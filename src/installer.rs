use std::collections::HashMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use base64::Engine as _;
use sha2::Digest as _;

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

    // 5. Spawn one tokio task per plugin and download concurrently.
    println!("downloading...");

    let mut handles = Vec::with_capacity(to_download.len());

    for (name, version, sha256) in to_download {
        let client = client.clone();
        let dir = opts.plugin_dir.clone();

        handles.push(tokio::spawn(async move {
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
async fn download_and_install(
    client: &reqwest::Client,
    name: &str,
    version: &str,
    expected_sha256: Option<&str>,
    plugin_dir: &Path,
) -> Result<()> {
    let bytes = download_with_retry(client, name, version).await?;
    verify_sha256(&bytes, expected_sha256, name)?;
    write_atomically(plugin_dir, name, &bytes).await
}

/// Download plugin bytes from the primary URL, retrying up to `MAX_RETRIES`
/// times with exponential back-off. Falls back to `archives.jenkins.io` if
/// all primary attempts fail.
async fn download_with_retry(
    client: &reqwest::Client,
    name: &str,
    version: &str,
) -> Result<Vec<u8>> {
    // Honour the mirror override used by the Java plugin manager.
    let base = std::env::var("JENKINS_UC_DOWNLOAD").unwrap_or_else(|_| PRIMARY_BASE.to_string());
    let primary_url = format!("{base}/{name}/{version}/{name}.hpi");

    let mut last_err = None;

    for attempt in 1..=MAX_RETRIES {
        match fetch_bytes(client, &primary_url).await {
            Ok(bytes) => return Ok(bytes),
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

    fetch_bytes(client, &fallback_url)
        .await
        .with_context(|| match last_err {
            Some(e) => format!("downloading {name}:{version} failed (primary: {e})"),
            None => format!("downloading {name}:{version} failed"),
        })
}

async fn fetch_bytes(client: &reqwest::Client, url: &str) -> Result<Vec<u8>> {
    Ok(client
        .get(url)
        .send()
        .await
        .with_context(|| format!("GET {url}"))?
        .error_for_status()
        .with_context(|| format!("non-2xx from {url}"))?
        .bytes()
        .await?
        .to_vec())
}

// ── Integrity & write ─────────────────────────────────────────────────────────

/// Verify the SHA-256 of `bytes` (Base64-standard-encoded) against the value
/// stored in the lock file. The Jenkins Update Center publishes checksums as
/// standard Base64 with `=` padding, not hex.
fn verify_sha256(bytes: &[u8], expected: Option<&str>, name: &str) -> Result<()> {
    let Some(expected) = expected else {
        eprintln!("  warning: no sha256 for {name}, skipping integrity check");
        return Ok(());
    };

    let computed = base64::engine::general_purpose::STANDARD.encode(sha2::Sha256::digest(bytes));

    if computed != expected {
        bail!("{name}: integrity check failed\n  expected: {expected}\n  computed: {computed}");
    }
    Ok(())
}

/// Write `bytes` to `<plugin_dir>/<name>.hpi` via a temporary file to avoid
/// Jenkins picking up an incomplete download.
///
/// Uses `tokio::fs::rename` (atomic on the same filesystem). Falls back to
/// copy + delete when source and destination are on different devices.
async fn write_atomically(plugin_dir: &Path, name: &str, bytes: &[u8]) -> Result<()> {
    let tmp_path = plugin_dir.join(format!(".jpm-{name}.tmp"));
    let final_path = plugin_dir.join(format!("{name}.hpi"));

    tokio::fs::write(&tmp_path, bytes)
        .await
        .with_context(|| format!("writing temp file for {name}"))?;

    if tokio::fs::rename(&tmp_path, &final_path).await.is_err() {
        // Cross-device fallback: copy then delete the temp file.
        tokio::fs::copy(&tmp_path, &final_path)
            .await
            .with_context(|| format!("moving {name}.hpi into plugin directory"))?;
        tokio::fs::remove_file(&tmp_path).await.ok();
    }

    Ok(())
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
    fn verify_sha256_mismatch_errors() {
        let bytes = b"fake plugin content";
        let result = verify_sha256(bytes, Some("AAAA"), "test-plugin");
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(msg.contains("integrity check failed"));
    }

    #[test]
    fn verify_sha256_none_warns_but_passes() {
        let bytes = b"no checksum";
        assert!(verify_sha256(bytes, None, "test-plugin").is_ok());
    }

    #[test]
    fn verify_sha256_correct_hash() {
        let bytes = b"hello";
        let hash = sha2::Sha256::digest(bytes);
        let encoded = base64::engine::general_purpose::STANDARD.encode(hash);
        assert!(verify_sha256(bytes, Some(&encoded), "test-plugin").is_ok());
    }
}
