use anyhow::{Context, Result};
use serde_json::Value;
use std::path::Path;
use std::time::{Duration, SystemTime};

const CACHE_TTL: Duration = Duration::from_secs(3600);

const UC_STABLE_URL: &str = "https://updates.jenkins.io/update-center.actual.json?version=";
const UC_EXPERIMENTAL_URL: &str =
    "https://updates.jenkins.io/experimental/update-center.actual.json";
const UC_PLUGIN_VERSIONS_URL: &str = "https://updates.jenkins.io/current/plugin-versions.json";

/// Holds parsed data from the Jenkins Update Center.
pub struct UpdateCenter {
    pub stable: Value,
    pub experimental: Value,
    pub plugin_versions: Value,
}

impl UpdateCenter {
    /// Fetch all three UC endpoints concurrently, using the disk cache when fresh.
    pub async fn fetch(client: &reqwest::Client, jenkins_version: &str) -> Result<Self> {
        let cache_dir = cache_dir();
        tokio::fs::create_dir_all(&cache_dir).await?;

        let stable_url = format!("{UC_STABLE_URL}{jenkins_version}");
        let stable_cache = cache_dir.join(format!("uc-{jenkins_version}.json"));
        let experimental_cache = cache_dir.join("uc-experimental.json");
        let plugin_versions_cache = cache_dir.join("plugin-versions.json");

        let (stable, experimental, plugin_versions) = tokio::try_join!(
            fetch_json(client, &stable_url, &stable_cache),
            fetch_json(client, UC_EXPERIMENTAL_URL, &experimental_cache),
            fetch_json(client, UC_PLUGIN_VERSIONS_URL, &plugin_versions_cache),
        )
        .context("fetching Update Center endpoints")?;

        Ok(UpdateCenter {
            stable,
            experimental,
            plugin_versions,
        })
    }

    /// Look up the latest version string of a plugin in the stable UC.
    pub fn latest_version(&self, plugin: &str) -> Option<&str> {
        self.stable["plugins"][plugin]["version"].as_str()
    }

    /// Look up the latest version string of a plugin in the experimental UC.
    pub fn experimental_version(&self, plugin: &str) -> Option<&str> {
        self.experimental["plugins"][plugin]["version"].as_str()
    }

    /// Look up dependencies for a specific pinned version of a plugin.
    /// Returns a list of `(name, version, optional)` tuples.
    pub fn dependencies_for(&self, plugin: &str, version: &str) -> Vec<(String, String, bool)> {
        let deps = &self.plugin_versions["plugins"][plugin][version]["dependencies"];
        parse_deps(deps)
    }

    /// Look up the SHA-256 checksum for a specific pinned version of a plugin.
    /// Returns `None` if the UC does not provide one.
    pub fn sha256_for(&self, plugin: &str, version: &str) -> Option<&str> {
        self.plugin_versions["plugins"][plugin][version]["sha256"].as_str()
    }

    /// Look up dependencies for the latest version of a plugin in the stable UC.
    pub fn latest_dependencies(&self, plugin: &str) -> Vec<(String, String, bool)> {
        let deps = &self.stable["plugins"][plugin]["dependencies"];
        parse_deps(deps)
    }

    /// Look up dependencies for the latest version of a plugin in the experimental UC.
    pub fn experimental_dependencies(&self, plugin: &str) -> Vec<(String, String, bool)> {
        let deps = &self.experimental["plugins"][plugin]["dependencies"];
        parse_deps(deps)
    }
}

fn parse_deps(deps: &Value) -> Vec<(String, String, bool)> {
    let Some(arr) = deps.as_array() else {
        return vec![];
    };
    arr.iter()
        .filter_map(|d| {
            let name = d["name"].as_str()?.to_string();
            let version = d["version"].as_str()?.to_string();
            let optional = d["optional"].as_bool().unwrap_or(false);
            Some((name, version, optional))
        })
        .collect()
}

/// Fetch JSON from `url`, returning a cached copy if it exists and is fresh.
async fn fetch_json(client: &reqwest::Client, url: &str, cache_path: &Path) -> Result<Value> {
    if let Some(cached) = try_load_cache(cache_path).await {
        return Ok(cached);
    }

    eprintln!("  fetching {url}");
    let text = client
        .get(url)
        .send()
        .await
        .with_context(|| format!("GET {url}"))?
        .error_for_status()
        .with_context(|| format!("non-2xx from {url}"))?
        .text()
        .await?;

    let json: Value =
        serde_json::from_str(&text).with_context(|| format!("parsing JSON from {url}"))?;

    tokio::fs::write(cache_path, &text).await?;
    Ok(json)
}

async fn try_load_cache(path: &Path) -> Option<Value> {
    let meta = tokio::fs::metadata(path).await.ok()?;
    let modified = meta.modified().ok()?;
    if SystemTime::now().duration_since(modified).ok()? > CACHE_TTL {
        return None;
    }
    let text = tokio::fs::read_to_string(path).await.ok()?;
    serde_json::from_str(&text).ok()
}

fn cache_dir() -> std::path::PathBuf {
    dirs::cache_dir()
        .unwrap_or_else(|| std::path::PathBuf::from(".cache"))
        .join("jpm")
}
