use std::collections::HashMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use base64::Engine as _;
use sha2::Digest as _;
use tokio::io::AsyncWriteExt as _;
use tokio::sync::Semaphore;

use crate::lockfile;
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
    let mut to_download: Vec<(String, String, Option<String>)> = Vec::new();
    let mut up_to_date = 0usize;

    for (name, (version, sha256)) in &entries {
        let needs_update = match installed.get(name) {
            Some(inst) => JenkinsVersion::new(inst) < JenkinsVersion::new(version),
            None => true,
        };
        if needs_update {
            to_download.push((name.clone(), version.clone(), sha256.clone()));
        } else {
            up_to_date += 1;
        }
    }

    // Sort for deterministic, readable output.
    to_download.sort_by(|a, b| a.0.cmp(&b.0));

    println!("  {up_to_date} plugin(s) already up to date");
    println!("  {} plugin(s) to download", to_download.len());

    if opts.dry_run {
        for (name, version, _) in &to_download {
            println!("  would install {name}:{version}");
        }
        return Ok(());
    }

    if to_download.is_empty() {
        return Ok(());
    }

    // 5. Spawn one tokio task per plugin; a semaphore caps how many
    //    downloads run at the same time.
    println!("downloading...");

    let permits = Arc::new(Semaphore::new(opts.concurrency.max(1)));
    let mut handles = Vec::with_capacity(to_download.len());

    for (name, version, sha256) in to_download {
        let client = client.clone();
        let dir = opts.plugin_dir.clone();
        let permits = Arc::clone(&permits);

        handles.push(tokio::spawn(async move {
            let _permit = permits
                .acquire_owned()
                .await
                .expect("semaphore is never closed");
            let result =
                download_and_install(&client, &name, &version, sha256.as_deref(), &dir).await;
            (name, result)
        }));
    }

    // 6. Collect results — wait for all tasks regardless of individual failures.
    let mut installed_count = 0usize;
    let mut failed: Vec<String> = Vec::new();

    for handle in handles {
        let (name, result) = handle.await.context("download task panicked")?;
        match result {
            Ok(()) => {
                println!("  installed {name}");
                installed_count += 1;
            }
            Err(e) => {
                eprintln!("  failed {name}: {e:#}");
                failed.push(name);
            }
        }
    }

    // 7. Summary.
    println!();
    println!("installed {} plugin(s)", installed_count);

    if !failed.is_empty() {
        eprintln!(
            "failed    {} plugin(s): {}",
            failed.len(),
            failed.join(", ")
        );
        if !opts.skip_failed {
            bail!("{} plugin(s) failed to install", failed.len());
        }
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
        match read_plugin_manifest(&path) {
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

/// Open an `.hpi`/`.jpi` ZIP archive and return `(short-name, plugin-version)`
/// extracted from `META-INF/MANIFEST.MF`.
fn read_plugin_manifest(path: &Path) -> Result<(String, String)> {
    let file =
        std::fs::File::open(path).with_context(|| format!("opening '{}'", path.display()))?;
    let mut archive = zip::ZipArchive::new(file)
        .with_context(|| format!("'{}' is not a valid ZIP/HPI", path.display()))?;

    let mut entry = archive
        .by_name("META-INF/MANIFEST.MF")
        .with_context(|| format!("no META-INF/MANIFEST.MF in '{}'", path.display()))?;

    let mut content = String::new();
    entry.read_to_string(&mut content)?;

    let headers = parse_manifest_headers(&content);

    // `Short-Name` is the artifact ID; fall back to the filename stem.
    let name = headers
        .get("Short-Name")
        .cloned()
        .or_else(|| path.file_stem().and_then(|s| s.to_str()).map(str::to_owned))
        .with_context(|| format!("cannot determine name from '{}'", path.display()))?;

    let version = headers
        .get("Plugin-Version")
        .cloned()
        .with_context(|| format!("no Plugin-Version in '{}'", path.display()))?;

    Ok((name, version))
}

/// Parse a Java JAR manifest into a `key → value` map.
///
/// Lines that start with a single space are continuations of the previous
/// header value (JAR manifest specification §3.5).
fn parse_manifest_headers(content: &str) -> HashMap<String, String> {
    let mut map: HashMap<String, String> = HashMap::new();
    let mut current_key: Option<String> = None;
    let mut current_val = String::new();

    for line in content.lines() {
        if let Some(rest) = line.strip_prefix(' ') {
            // Continuation: append (strip the leading space).
            current_val.push_str(rest);
        } else {
            // Flush previous header.
            if let Some(key) = current_key.take() {
                map.insert(key, current_val.trim_end().to_string());
                current_val.clear();
            }
            if let Some((k, v)) = line.split_once(':') {
                current_key = Some(k.to_string());
                current_val = v.trim_start().to_string();
            }
        }
    }
    // Flush the final header.
    if let Some(key) = current_key {
        map.insert(key, current_val.trim_end().to_string());
    }

    map
}

// ── Downloading ───────────────────────────────────────────────────────────────

/// Download one plugin, verify its checksum, and move it to `plugin_dir`.
///
/// The archive is streamed to `<plugin_dir>/.jpm-<name>.tmp` and only renamed
/// to `<name>.jpi` once it passed verification, so Jenkins never sees a
/// partial or unverified file.
async fn download_and_install(
    client: &reqwest::Client,
    name: &str,
    version: &str,
    expected_sha256: Option<&str>,
    plugin_dir: &Path,
) -> Result<()> {
    let tmp_path = plugin_dir.join(format!(".jpm-{name}.tmp"));
    let result = async {
        let computed = download_with_retry(client, name, version, &tmp_path).await?;
        verify_sha256(&computed, expected_sha256, name)?;
        move_into_place(plugin_dir, name, &tmp_path).await
    }
    .await;
    if result.is_err() {
        tokio::fs::remove_file(&tmp_path).await.ok();
    }
    result
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
    let primary_url = primary_download_url(
        std::env::var("JENKINS_UC_DOWNLOAD_URL").ok().as_deref(),
        std::env::var("JENKINS_UC_DOWNLOAD").ok().as_deref(),
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

/// Build the primary download URL, honouring the mirror overrides of the Java
/// plugin manager:
///
/// - `JENKINS_UC_DOWNLOAD_URL` is the full base: `<base>/<n>/<v>/<n>.hpi`
/// - `JENKINS_UC_DOWNLOAD` (deprecated) gets `/plugins` appended:
///   `<base>/plugins/<n>/<v>/<n>.hpi`
///
/// `JENKINS_UC_DOWNLOAD_URL` wins when both are set; empty values are ignored.
fn primary_download_url(
    download_url: Option<&str>,
    legacy_download: Option<&str>,
    name: &str,
    version: &str,
) -> String {
    fn non_empty(v: Option<&str>) -> Option<&str> {
        v.map(|s| s.trim_end_matches('/')).filter(|s| !s.is_empty())
    }
    let base = match (non_empty(download_url), non_empty(legacy_download)) {
        (Some(base), _) => base.to_string(),
        (None, Some(legacy)) => format!("{legacy}/plugins"),
        (None, None) => PRIMARY_BASE.to_string(),
    };
    format!("{base}/{name}/{version}/{name}.hpi")
}

/// Stream the response body of `url` into `dest` (truncating it), hashing it
/// on the way so the archive never has to be held in memory. Returns the
/// Base64 SHA-256 of what was written.
async fn download_to_file(client: &reqwest::Client, url: &str, dest: &Path) -> Result<String> {
    let mut response = client
        .get(url)
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

/// Move the verified archive at `tmp_path` to `<plugin_dir>/<name>.jpi`.
///
/// An existing `<name>.jpi` is kept as `<name>.bak`, which is the file the
/// Jenkins plugin manager offers for "Downgrade to …".
///
/// Uses `tokio::fs::rename` (atomic on the same filesystem). Falls back to
/// copy + delete when source and destination are on different devices.
async fn move_into_place(plugin_dir: &Path, name: &str, tmp_path: &Path) -> Result<()> {
    let final_path = plugin_dir.join(format!("{name}.jpi"));

    backup_existing(plugin_dir, name).await?;

    if tokio::fs::rename(tmp_path, &final_path).await.is_err() {
        // Cross-device fallback: copy then delete the temp file.
        tokio::fs::copy(tmp_path, &final_path)
            .await
            .with_context(|| format!("moving {name}.jpi into plugin directory"))?;
        tokio::fs::remove_file(tmp_path).await.ok();
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
    fn parses_simple_manifest() {
        let manifest = "Manifest-Version: 1.0\r\nShort-Name: git\r\nPlugin-Version: 5.7.0\r\n";
        let headers = parse_manifest_headers(manifest);
        assert_eq!(headers["Short-Name"], "git");
        assert_eq!(headers["Plugin-Version"], "5.7.0");
    }

    #[test]
    fn parses_continuation_lines() {
        let manifest = "Long-Name: Git plugin with a very lon\r\n g name continued here\r\nPlugin-Version: 5.7.0\r\n";
        let headers = parse_manifest_headers(manifest);
        assert_eq!(
            headers["Long-Name"],
            "Git plugin with a very long name continued here"
        );
        assert_eq!(headers["Plugin-Version"], "5.7.0");
    }

    #[test]
    fn download_url_defaults_to_update_center() {
        assert_eq!(
            primary_download_url(None, None, "git", "5.7.0"),
            "https://updates.jenkins.io/download/plugins/git/5.7.0/git.hpi"
        );
    }

    #[test]
    fn download_url_uses_full_base_from_download_url_env() {
        assert_eq!(
            primary_download_url(Some("https://m.example/dl/plugins/"), None, "git", "5.7.0"),
            "https://m.example/dl/plugins/git/5.7.0/git.hpi"
        );
    }

    #[test]
    fn download_url_appends_plugins_to_legacy_env() {
        assert_eq!(
            primary_download_url(None, Some("https://m.example/download"), "git", "5.7.0"),
            "https://m.example/download/plugins/git/5.7.0/git.hpi"
        );
    }

    #[test]
    fn download_url_prefers_download_url_over_legacy_and_skips_empty() {
        assert_eq!(
            primary_download_url(
                Some("https://new.example/p"),
                Some("https://old.example"),
                "git",
                "5.7.0"
            ),
            "https://new.example/p/git/5.7.0/git.hpi"
        );
        assert_eq!(
            primary_download_url(Some(""), Some("https://old.example"), "git", "5.7.0"),
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
