use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use base64::Engine as _;
use sha2::Digest as _;
use tokio::io::AsyncWriteExt as _;
use tokio::sync::Semaphore;

use crate::lockfile::{self, LockEntry};
use crate::manifest;
use crate::url_plugins;
use crate::version::JenkinsVersion;

const PRIMARY_BASE: &str = "https://updates.jenkins.io/download/plugins";
const FALLBACK_BASE: &str = "https://archives.jenkins.io/plugins";
const MAX_RETRIES: usize = 3;

pub struct InstallOptions {
    pub lock_file: PathBuf,
    pub plugin_dir: PathBuf,
    pub skip_failed: bool,
    pub dry_run: bool,
    /// Maximum number of plugins downloaded at the same time.
    pub concurrency: usize,
}

/// Top-level entry point: read lock file, scan existing plugins, download what
/// is missing or outdated, verify integrity, move to the plugin directory.
pub async fn install(client: &reqwest::Client, opts: &InstallOptions) -> Result<()> {
    // 1. Parse lock file — abort if missing (run `jpm lock` first).
    let lock_content = std::fs::read_to_string(&opts.lock_file).with_context(|| {
        format!(
            "lock file '{}' not found — run `jpm lock` first",
            opts.lock_file.display()
        )
    })?;

    let entries = lockfile::parse(&lock_content);
    println!("jpm install: {} plugin(s) in lock file", entries.len());

    // 2. Ensure the plugin directory exists.
    std::fs::create_dir_all(&opts.plugin_dir)
        .with_context(|| format!("creating plugin directory '{}'", opts.plugin_dir.display()))?;

    // 3. Scan already-installed plugins (blocking, but fast).
    let installed = scan_installed(&opts.plugin_dir)
        .with_context(|| format!("scanning '{}'", opts.plugin_dir.display()))?;

    // 4. Partition: up-to-date vs needs download.
    let mut to_download: Vec<(String, LockEntry)> = Vec::new();
    let mut up_to_date = 0usize;

    for (name, entry) in &entries {
        let needs_update = match installed.get(name) {
            Some(inst) => JenkinsVersion::new(inst) < JenkinsVersion::new(&entry.version),
            None => true,
        };
        if needs_update {
            to_download.push((name.clone(), entry.clone()));
        } else {
            up_to_date += 1;
        }
    }

    // Sort for deterministic, readable output.
    to_download.sort_by(|a, b| a.0.cmp(&b.0));

    println!("  {up_to_date} plugin(s) already up to date");
    println!("  {} plugin(s) to download", to_download.len());

    if opts.dry_run {
        for (name, entry) in &to_download {
            println!("  would install {name}:{}", entry.version);
        }
        return Ok(());
    }

    if to_download.is_empty() {
        return Ok(());
    }

    // 5. Download everything into a staging directory inside the plugin
    //    directory (same filesystem, so the final rename is atomic). Nothing
    //    in the plugin directory changes until every download is verified.
    println!("downloading...");

    let staging = tempfile::Builder::new()
        .prefix(".jpm-staging-")
        .tempdir_in(&opts.plugin_dir)
        .with_context(|| {
            format!(
                "creating staging directory in '{}'",
                opts.plugin_dir.display()
            )
        })?;

    // One tokio task per plugin; a semaphore caps how many downloads run at
    // the same time.
    let permits = Arc::new(Semaphore::new(opts.concurrency.max(1)));
    let mut handles = Vec::with_capacity(to_download.len());

    for (name, entry) in to_download {
        let client = client.clone();
        let dir = staging.path().to_path_buf();
        let permits = Arc::clone(&permits);

        handles.push(tokio::spawn(async move {
            let _permit = permits
                .acquire_owned()
                .await
                .expect("semaphore is never closed");
            let result = download_and_stage(&client, &name, &entry, &dir).await;
            (name, result)
        }));
    }

    // 6. Collect results — wait for all tasks regardless of individual failures.
    let mut staged: Vec<(String, PathBuf)> = Vec::new();
    let mut failed: Vec<String> = Vec::new();

    for handle in handles {
        let (name, result) = handle.await.context("download task panicked")?;
        match result {
            Ok(path) => staged.push((name, path)),
            Err(e) => {
                eprintln!("  failed {name}: {e:#}");
                failed.push(name);
            }
        }
    }
    staged.sort();

    if !failed.is_empty() {
        eprintln!(
            "failed    {} plugin(s): {}",
            failed.len(),
            failed.join(", ")
        );
        if !opts.skip_failed {
            // `staging` is removed on drop, so the plugin directory is left
            // exactly as it was.
            bail!(
                "{} plugin(s) failed to download; '{}' was left unchanged",
                failed.len(),
                opts.plugin_dir.display()
            );
        }
    }

    // 7. Commit: move every verified archive into the plugin directory.
    for (name, path) in &staged {
        move_into_place(&opts.plugin_dir, name, path).await?;
        println!("  installed {name}");
    }

    // 8. Summary.
    println!();
    println!("installed {} plugin(s)", staged.len());
    if !failed.is_empty() {
        eprintln!("skipped   {} failed plugin(s)", failed.len());
    }

    Ok(())
}

// ── Scanning ─────────────────────────────────────────────────────────────────

/// Walk `dir` and return a map of `short-name → installed-version` for every
/// `.hpi` / `.jpi` file found.
pub fn scan_installed(dir: &Path) -> Result<HashMap<String, String>> {
    let mut map = HashMap::new();

    let read_dir = match std::fs::read_dir(dir) {
        Ok(rd) => rd,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(map),
        Err(e) => return Err(e).with_context(|| format!("reading '{}'", dir.display())),
    };

    for entry in read_dir {
        let path = entry?.path();
        let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
        if ext != "hpi" && ext != "jpi" {
            continue;
        }
        match manifest::read_plugin_manifest(&path) {
            Ok((name, version)) => {
                map.insert(name, version);
            }
            Err(e) => {
                eprintln!("  warning: skipping '{}': {e}", path.display());
            }
        }
    }

    Ok(map)
}

// ── Downloading ───────────────────────────────────────────────────────────────

/// Download one plugin into `staging_dir/<name>.jpi` and verify it.
///
/// Returns the path of the verified archive. On failure the partial file is
/// removed, so only verified archives are left in the staging directory.
async fn download_and_stage(
    client: &reqwest::Client,
    name: &str,
    entry: &LockEntry,
    staging_dir: &Path,
) -> Result<PathBuf> {
    let path = staging_dir.join(format!("{name}.jpi"));
    let result = async {
        let computed = match &entry.url {
            Some(url) => fetch_from_url(client, name, url, &path).await?,
            None => download_with_retry(client, name, &entry.version, &path).await?,
        };
        verify_sha256(&computed, entry.sha256.as_deref(), name)?;
        verify_archive(&path, name, &entry.version).await
    }
    .await;
    match result {
        Ok(()) => Ok(path),
        Err(e) => {
            tokio::fs::remove_file(&path).await.ok();
            Err(e)
        }
    }
}

/// Download a plugin into `dest`, retrying the primary URL up to `MAX_RETRIES`
/// times with exponential back-off. Falls back to `archives.jenkins.io` if
/// all primary attempts fail. Returns the Base64 SHA-256 of the file.
async fn download_with_retry(
    client: &reqwest::Client,
    name: &str,
    version: &str,
    dest: &Path,
) -> Result<String> {
    let env = crate::fetch::env_non_empty;
    let primary_url = primary_download_url(
        env("JENKINS_UC_DOWNLOAD_URL").as_deref(),
        env("JENKINS_UC_DOWNLOAD").as_deref(),
        env("JENKINS_UC").as_deref(),
        name,
        version,
    );

    let mut last_err = None;

    for attempt in 1..=MAX_RETRIES {
        match download_to_file(client, &primary_url, dest).await {
            Ok(sha256) => return Ok(sha256),
            Err(e) => {
                last_err = Some(e);
                if attempt < MAX_RETRIES {
                    let wait = Duration::from_millis(300 * (1 << (attempt - 1)));
                    eprintln!(
                        "  retry {attempt}/{MAX_RETRIES} for {name} (waiting {}ms)",
                        wait.as_millis()
                    );
                    tokio::time::sleep(wait).await;
                }
            }
        }
    }

    // Primary failed — try the archive mirror once.
    let fallback_url = format!("{FALLBACK_BASE}/{name}/{version}/{name}.hpi");
    eprintln!("  primary failed for {name}, trying fallback");

    download_to_file(client, &fallback_url, dest)
        .await
        .with_context(|| match last_err {
            Some(e) => format!("downloading {name}:{version} failed (primary: {e})"),
            None => format!("downloading {name}:{version} failed"),
        })
}

/// Fetch a plugin whose lock entry has its own `url:` (a `file://` path is
/// copied, `http(s)://` downloaded with retries). There is no archive
/// fallback: the plugin is not in the Update Center.
async fn fetch_from_url(
    client: &reqwest::Client,
    name: &str,
    url: &str,
    dest: &Path,
) -> Result<String> {
    if let Some(path) = url_plugins::local_path(url)? {
        return url_plugins::copy_local(&path, dest).await;
    }
    let mut attempt = 1;
    loop {
        match download_to_file(client, url, dest).await {
            Ok(sha256) => return Ok(sha256),
            Err(e) if attempt < MAX_RETRIES => {
                let wait = Duration::from_millis(300 * (1 << (attempt - 1)));
                eprintln!(
                    "  retry {attempt}/{MAX_RETRIES} for {name} (waiting {}ms): {e:#}",
                    wait.as_millis()
                );
                tokio::time::sleep(wait).await;
                attempt += 1;
            }
            Err(e) => return Err(e).with_context(|| format!("downloading {name} from {url}")),
        }
    }
}

/// Build the primary download URL, honouring the mirror overrides of the Java
/// plugin manager, in this order:
///
/// 1. `JENKINS_UC_DOWNLOAD_URL` is the full base: `<base>/<n>/<v>/<n>.hpi`
/// 2. `JENKINS_UC_DOWNLOAD` (deprecated) gets `/plugins` appended:
///    `<base>/plugins/<n>/<v>/<n>.hpi`
/// 3. `JENKINS_UC`: the scheme, host and port of the Update Center, plus
///    `/download/plugins/<n>/<v>/<n>.hpi`
///
/// Empty values are ignored.
fn primary_download_url(
    download_url: Option<&str>,
    legacy_download: Option<&str>,
    update_center: Option<&str>,
    name: &str,
    version: &str,
) -> String {
    fn non_empty(v: Option<&str>) -> Option<&str> {
        v.map(|s| s.trim_end_matches('/')).filter(|s| !s.is_empty())
    }
    let uc_origin = || {
        let url = reqwest::Url::parse(non_empty(update_center)?).ok()?;
        let origin = url.origin();
        origin
            .is_tuple()
            .then(|| format!("{}/download/plugins", origin.ascii_serialization()))
    };
    let base = match (non_empty(download_url), non_empty(legacy_download)) {
        (Some(base), _) => base.to_string(),
        (None, Some(legacy)) => format!("{legacy}/plugins"),
        (None, None) => uc_origin().unwrap_or_else(|| PRIMARY_BASE.to_string()),
    };
    format!("{base}/{name}/{version}/{name}.hpi")
}

/// Stream the response body of `url` into `dest` (truncating it), hashing it
/// on the way so the archive never has to be held in memory. Returns the
/// Base64 SHA-256 of what was written.
async fn download_to_file(client: &reqwest::Client, url: &str, dest: &Path) -> Result<String> {
    let mut response = crate::credentials::get(client, url)
        .send()
        .await
        .with_context(|| format!("GET {url}"))?
        .error_for_status()
        .with_context(|| format!("non-2xx from {url}"))?;

    let mut file = tokio::fs::File::create(dest)
        .await
        .with_context(|| format!("creating '{}'", dest.display()))?;
    let mut hasher = sha2::Sha256::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .with_context(|| format!("reading body of {url}"))?
    {
        hasher.update(&chunk);
        file.write_all(&chunk)
            .await
            .with_context(|| format!("writing '{}'", dest.display()))?;
    }
    file.flush().await?;

    Ok(base64::engine::general_purpose::STANDARD.encode(hasher.finalize()))
}

// ── Integrity & write ─────────────────────────────────────────────────────────

/// Compare the computed SHA-256 (Base64-standard-encoded) against the value
/// stored in the lock file. The Jenkins Update Center publishes checksums as
/// standard Base64 with `=` padding, not hex.
fn verify_sha256(computed: &str, expected: Option<&str>, name: &str) -> Result<()> {
    let Some(expected) = expected else {
        eprintln!("  warning: no sha256 for {name}, skipping integrity check");
        return Ok(());
    };

    if computed != expected {
        bail!("{name}: integrity check failed\n  expected: {expected}\n  computed: {computed}");
    }
    Ok(())
}

/// Check that the file at `path` is a plugin archive for `name:version`.
///
/// The sha256 check above is skipped when the lock has no checksum, so this
/// is what catches a mirror answering with an HTML error page or the wrong
/// file: the archive must be a valid ZIP whose `META-INF/MANIFEST.MF` has a
/// matching `Short-Name` (when present) and `Plugin-Version`.
async fn verify_archive(path: &Path, name: &str, version: &str) -> Result<()> {
    let path = path.to_path_buf();
    let headers = tokio::task::spawn_blocking(move || manifest::read_headers_from_path(&path))
        .await
        .context("archive check task panicked")?
        .with_context(|| format!("{name}: downloaded file is not a plugin archive"))?;
    check_manifest(&headers, name, version)
}

fn check_manifest(headers: &HashMap<String, String>, name: &str, version: &str) -> Result<()> {
    if let Some(short_name) = headers.get("Short-Name") {
        if short_name != name {
            bail!("{name}: downloaded archive is plugin '{short_name}', not '{name}'");
        }
    }
    let Some(plugin_version) = headers.get("Plugin-Version") else {
        bail!("{name}: downloaded archive has no Plugin-Version in its manifest");
    };
    if JenkinsVersion::new(plugin_version) != JenkinsVersion::new(version) {
        bail!("{name}: downloaded archive is version {plugin_version}, lock file says {version}");
    }
    Ok(())
}

/// Move the verified archive at `staged` to `<plugin_dir>/<name>.jpi`.
///
/// An existing `<name>.jpi` is kept as `<name>.bak`, which is the file the
/// Jenkins plugin manager offers for "Downgrade to …".
///
/// Uses `tokio::fs::rename` (atomic on the same filesystem). Falls back to
/// copy + delete when source and destination are on different devices.
async fn move_into_place(plugin_dir: &Path, name: &str, staged: &Path) -> Result<()> {
    let final_path = plugin_dir.join(format!("{name}.jpi"));

    backup_existing(plugin_dir, name).await?;

    if tokio::fs::rename(staged, &final_path).await.is_err() {
        // Cross-device fallback: copy then delete the staged file.
        tokio::fs::copy(staged, &final_path)
            .await
            .with_context(|| format!("moving {name}.jpi into plugin directory"))?;
        tokio::fs::remove_file(staged).await.ok();
    }

    Ok(())
}

/// Move `<plugin_dir>/<name>.jpi` to `<name>.bak`, replacing an older backup.
/// Does nothing when the plugin is not installed yet.
async fn backup_existing(plugin_dir: &Path, name: &str) -> Result<()> {
    let current = plugin_dir.join(format!("{name}.jpi"));
    if !tokio::fs::try_exists(&current).await.unwrap_or(false) {
        return Ok(());
    }
    let backup = plugin_dir.join(format!("{name}.bak"));
    match tokio::fs::remove_file(&backup).await {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e).with_context(|| format!("removing old backup for {name}")),
    }
    tokio::fs::rename(&current, &backup)
        .await
        .with_context(|| format!("backing up {name}.jpi to {name}.bak"))
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn download_url_defaults_to_update_center() {
        assert_eq!(
            primary_download_url(None, None, None, "git", "5.7.0"),
            "https://updates.jenkins.io/download/plugins/git/5.7.0/git.hpi"
        );
    }

    #[test]
    fn download_url_uses_full_base_from_download_url_env() {
        assert_eq!(
            primary_download_url(
                Some("https://m.example/dl/plugins/"),
                None,
                None,
                "git",
                "5.7.0"
            ),
            "https://m.example/dl/plugins/git/5.7.0/git.hpi"
        );
    }

    #[test]
    fn download_url_appends_plugins_to_legacy_env() {
        assert_eq!(
            primary_download_url(
                None,
                Some("https://m.example/download"),
                None,
                "git",
                "5.7.0"
            ),
            "https://m.example/download/plugins/git/5.7.0/git.hpi"
        );
    }

    #[test]
    fn download_url_derives_from_jenkins_uc_origin() {
        assert_eq!(
            primary_download_url(
                None,
                None,
                Some("https://mirror.example:8443/jenkins/update-center.json"),
                "git",
                "5.7.0"
            ),
            "https://mirror.example:8443/download/plugins/git/5.7.0/git.hpi"
        );
        assert_eq!(
            primary_download_url(
                None,
                Some("https://old.example"),
                Some("https://uc.example"),
                "git",
                "5.7.0"
            ),
            "https://old.example/plugins/git/5.7.0/git.hpi"
        );
        assert_eq!(
            primary_download_url(None, None, Some("not a url"), "git", "5.7.0"),
            "https://updates.jenkins.io/download/plugins/git/5.7.0/git.hpi"
        );
    }

    #[test]
    fn download_url_prefers_download_url_over_legacy_and_skips_empty() {
        assert_eq!(
            primary_download_url(
                Some("https://new.example/p"),
                Some("https://old.example"),
                Some("https://uc.example"),
                "git",
                "5.7.0"
            ),
            "https://new.example/p/git/5.7.0/git.hpi"
        );
        assert_eq!(
            primary_download_url(Some(""), Some("https://old.example"), None, "git", "5.7.0"),
            "https://old.example/plugins/git/5.7.0/git.hpi"
        );
    }

    #[tokio::test]
    async fn move_into_place_keeps_previous_archive_as_bak() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        let staged = dir.join(".jpm-git.tmp");

        std::fs::write(&staged, b"v1").unwrap();
        move_into_place(dir, "git", &staged).await.unwrap();
        assert!(!dir.join("git.bak").exists(), "fresh install has no backup");
        assert!(!staged.exists());

        std::fs::write(&staged, b"v2").unwrap();
        move_into_place(dir, "git", &staged).await.unwrap();
        assert_eq!(std::fs::read(dir.join("git.jpi")).unwrap(), b"v2");
        assert_eq!(std::fs::read(dir.join("git.bak")).unwrap(), b"v1");

        std::fs::write(&staged, b"v3").unwrap();
        move_into_place(dir, "git", &staged).await.unwrap();
        assert_eq!(std::fs::read(dir.join("git.jpi")).unwrap(), b"v3");
        assert_eq!(std::fs::read(dir.join("git.bak")).unwrap(), b"v2");
    }

    fn headers(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn check_manifest_accepts_matching_archive() {
        let h = headers(&[("Short-Name", "git"), ("Plugin-Version", "5.7.0")]);
        assert!(check_manifest(&h, "git", "5.7.0").is_ok());
        // Versions compare like Jenkins core, and Short-Name is optional.
        let h = headers(&[("Plugin-Version", "1.0.9.0")]);
        assert!(check_manifest(&h, "mapdb-api", "1.0.9").is_ok());
    }

    #[test]
    fn check_manifest_rejects_wrong_plugin_or_version() {
        let h = headers(&[("Short-Name", "mailer"), ("Plugin-Version", "5.7.0")]);
        let err = check_manifest(&h, "git", "5.7.0").unwrap_err().to_string();
        assert!(err.contains("is plugin 'mailer'"), "{err}");

        let h = headers(&[("Short-Name", "git"), ("Plugin-Version", "5.6.0")]);
        let err = check_manifest(&h, "git", "5.7.0").unwrap_err().to_string();
        assert!(err.contains("is version 5.6.0"), "{err}");

        let h = headers(&[("Short-Name", "git")]);
        assert!(check_manifest(&h, "git", "5.7.0").is_err());
    }

    #[test]
    fn verify_sha256_mismatch_errors() {
        let result = verify_sha256("BBBB", Some("AAAA"), "test-plugin");
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(msg.contains("integrity check failed"));
    }

    #[test]
    fn verify_sha256_none_warns_but_passes() {
        assert!(verify_sha256("AAAA", None, "test-plugin").is_ok());
    }

    #[test]
    fn verify_sha256_correct_hash() {
        assert!(verify_sha256("AAAA", Some("AAAA"), "test-plugin").is_ok());
    }
}
