//! Plugins sourced from a URL in `plugins.txt` (`name:version:url` or
//! `name::url`), e.g. in-house builds that are not in the Update Center.
//!
//! The archive is downloaded (`http(s)://`) or read (`file://`) at lock time.
//! Its manifest provides the version, `Jenkins-Version` and
//! `Plugin-Dependencies`, and the archive's SHA-256 is computed locally. The
//! plugin is then registered in the in-memory `plugin-versions.json` data, so
//! dependency resolution, the compatibility check and the cycle check treat it
//! like any Update Center plugin.

use std::collections::HashMap;
use std::io::Cursor;
use std::path::Path;

use anyhow::{bail, Context, Result};
use base64::Engine as _;
use sha2::Digest as _;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

use crate::fetch;
use crate::manifest;
use crate::parser::{PluginRequest, VersionSpec};
use crate::resolver::ResolvedPlugin;
use crate::update_center::UpdateCenter;
use crate::version::JenkinsVersion;

/// A plugin archive read from a URL.
#[derive(Debug, Clone)]
pub struct UrlPlugin {
    pub name: String,
    pub version: String,
    pub url: String,
    /// Base64 SHA-256 of the archive, like the Update Center publishes.
    pub sha256: String,
    /// `Jenkins-Version` (or legacy `Hudson-Version`) from the manifest.
    pub required_core: Option<String>,
    /// `(name, version, optional)` from `Plugin-Dependencies`.
    pub dependencies: Vec<(String, String, bool)>,
}

/// Download every URL-sourced request.
pub async fn load(
    client: &reqwest::Client,
    requests: &[PluginRequest],
) -> Result<HashMap<String, UrlPlugin>> {
    let mut out = HashMap::new();
    for req in requests {
        let Some(url) = &req.url else { continue };
        let expected = match &req.version {
            VersionSpec::Pinned(v) => Some(v.as_str()),
            _ => None,
        };
        let plugin = fetch_plugin(client, &req.name, url, expected).await?;
        println!("  {}:{} from {url}", plugin.name, plugin.version);
        out.insert(plugin.name.clone(), plugin);
    }
    Ok(out)
}

/// Register URL plugins in the Update Center data and pin their requests to
/// the archive version.
pub fn apply(
    uc: &mut UpdateCenter,
    requests: &mut [PluginRequest],
    plugins: &HashMap<String, UrlPlugin>,
) {
    for p in plugins.values() {
        uc.add_url_plugin(p);
    }
    for req in requests.iter_mut() {
        if let Some(p) = plugins.get(&req.name) {
            req.version = VersionSpec::Pinned(p.version.clone());
        }
    }
}

/// Fail when resolution replaced a URL plugin, i.e. another plugin needs a
/// newer version than the archive provides.
pub fn check_kept(
    resolved: &HashMap<String, ResolvedPlugin>,
    plugins: &HashMap<String, UrlPlugin>,
) -> Result<()> {
    let mut names: Vec<&String> = plugins.keys().collect();
    names.sort();
    for name in names {
        let p = &plugins[name];
        if let Some(r) = resolved.get(name) {
            if r.url.as_deref() != Some(p.url.as_str()) {
                bail!(
                    "{name} is pinned to {} from {}, but another plugin requires {name} >= {}; \
                     point plugins.txt at a newer build",
                    p.version,
                    p.url,
                    r.version
                );
            }
        }
    }
    Ok(())
}

/// Download (or read) the archive at `url` and check that it is plugin `name`
/// (and version `expected`, when the manifest pins one).
pub async fn fetch_plugin(
    client: &reqwest::Client,
    name: &str,
    url: &str,
    expected: Option<&str>,
) -> Result<UrlPlugin> {
    let bytes = read_source(client, url).await?;
    let sha256 = base64::engine::general_purpose::STANDARD.encode(sha2::Sha256::digest(&bytes));
    let headers = manifest::read_headers(Cursor::new(bytes), url)?;
    from_manifest(name, url, sha256, &headers, expected)
}

fn from_manifest(
    name: &str,
    url: &str,
    sha256: String,
    headers: &HashMap<String, String>,
    expected: Option<&str>,
) -> Result<UrlPlugin> {
    if let Some(short_name) = headers.get("Short-Name") {
        if short_name != name {
            bail!("{url} is plugin '{short_name}', not '{name}'");
        }
    }
    let version = headers
        .get("Plugin-Version")
        .cloned()
        .with_context(|| format!("no Plugin-Version in the manifest of {url}"))?;
    if let Some(expected) = expected {
        if JenkinsVersion::new(expected) != JenkinsVersion::new(&version) {
            bail!("plugins.txt pins {name}:{expected}, but {url} is version {version}");
        }
    }
    Ok(UrlPlugin {
        name: name.to_string(),
        version,
        url: url.to_string(),
        sha256,
        required_core: headers
            .get("Jenkins-Version")
            .or_else(|| headers.get("Hudson-Version"))
            .cloned(),
        dependencies: headers
            .get("Plugin-Dependencies")
            .map(|d| parse_plugin_dependencies(d))
            .unwrap_or_default(),
    })
}

/// Parse a `Plugin-Dependencies` manifest header:
/// `credentials:1371.0,git:5.2.0;resolution:=optional`.
pub fn parse_plugin_dependencies(value: &str) -> Vec<(String, String, bool)> {
    value
        .split(',')
        .filter_map(|dep| {
            let mut parts = dep.trim().split(';');
            let (name, version) = parts.next()?.trim().split_once(':')?;
            let optional = parts.any(|p| p.trim() == "resolution:=optional");
            Some((
                name.trim().to_string(),
                version.trim().to_string(),
                optional,
            ))
        })
        .collect()
}

/// Read the bytes behind an `http(s)://` or `file://` URL.
pub async fn read_source(client: &reqwest::Client, url: &str) -> Result<Vec<u8>> {
    match local_path(url)? {
        Some(path) => tokio::fs::read(&path)
            .await
            .with_context(|| format!("reading '{}'", path.display())),
        None => fetch::get_bytes(client, url).await,
    }
}

/// Copy a `file://` plugin into `dest`, returning its Base64 SHA-256.
pub async fn copy_local(src: &Path, dest: &Path) -> Result<String> {
    let mut input = tokio::fs::File::open(src)
        .await
        .with_context(|| format!("opening '{}'", src.display()))?;
    let mut output = tokio::fs::File::create(dest)
        .await
        .with_context(|| format!("creating '{}'", dest.display()))?;
    let mut hasher = sha2::Sha256::new();
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let n = input
            .read(&mut buf)
            .await
            .with_context(|| format!("reading '{}'", src.display()))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        output.write_all(&buf[..n]).await?;
    }
    output.flush().await?;
    Ok(base64::engine::general_purpose::STANDARD.encode(hasher.finalize()))
}

/// `Some(path)` for a `file://` URL, `None` for `http(s)://`; other schemes
/// are rejected.
pub fn local_path(url: &str) -> Result<Option<std::path::PathBuf>> {
    let parsed = reqwest::Url::parse(url).with_context(|| format!("invalid plugin URL '{url}'"))?;
    match parsed.scheme() {
        "http" | "https" => Ok(None),
        "file" => parsed
            .to_file_path()
            .map(Some)
            .map_err(|()| anyhow::anyhow!("'{url}' is not a valid local file URL")),
        other => bail!("unsupported URL scheme '{other}' in '{url}' (use http, https or file)"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn headers(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn parses_plugin_dependencies_header() {
        let deps = parse_plugin_dependencies(
            "credentials:1371.0, git:5.2.0;resolution:=optional,workflow-api:1.0;x=y",
        );
        assert_eq!(
            deps,
            vec![
                ("credentials".into(), "1371.0".into(), false),
                ("git".into(), "5.2.0".into(), true),
                ("workflow-api".into(), "1.0".into(), false),
            ]
        );
        assert!(parse_plugin_dependencies("").is_empty());
    }

    #[test]
    fn from_manifest_reads_version_core_and_dependencies() {
        let h = headers(&[
            ("Short-Name", "my-plugin"),
            ("Plugin-Version", "1.2.3"),
            ("Hudson-Version", "2.440.3"),
            ("Plugin-Dependencies", "credentials:1371.0"),
        ]);
        let p = from_manifest("my-plugin", "file:///x.hpi", "SHA".into(), &h, None).unwrap();
        assert_eq!(p.version, "1.2.3");
        assert_eq!(p.required_core.as_deref(), Some("2.440.3"));
        assert_eq!(p.dependencies.len(), 1);
        assert!(from_manifest("my-plugin", "u", "S".into(), &h, Some("1.2.3")).is_ok());
    }

    #[test]
    fn from_manifest_rejects_wrong_name_or_pinned_version() {
        let h = headers(&[("Short-Name", "other"), ("Plugin-Version", "1.0")]);
        let err = from_manifest("my-plugin", "u", "S".into(), &h, None)
            .unwrap_err()
            .to_string();
        assert!(err.contains("is plugin 'other'"), "{err}");

        let h = headers(&[("Plugin-Version", "1.0")]);
        let err = from_manifest("my-plugin", "u", "S".into(), &h, Some("2.0"))
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("pins my-plugin:2.0, but u is version 1.0"),
            "{err}"
        );
    }

    #[test]
    fn local_path_accepts_only_http_and_file() {
        assert!(local_path("https://example.com/p.hpi").unwrap().is_none());
        let expected = if cfg!(windows) {
            "file:///C:/tmp/p.hpi"
        } else {
            "file:///tmp/p.hpi"
        };
        assert!(local_path(expected).unwrap().is_some());
        assert!(local_path("ftp://example.com/p.hpi").is_err());
        assert!(local_path("not a url").is_err());
    }
}
