use anyhow::{Context, Result};
use serde_json::Value;
use std::path::Path;
use std::time::{Duration, SystemTime};

use crate::fetch;
use crate::version::JenkinsVersion;

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
    /// Fetch the UC endpoints concurrently, using the disk cache when fresh.
    ///
    /// The experimental UC is only fetched when `with_experimental` is set,
    /// i.e. when some plugin is requested as `:experimental`.
    pub async fn fetch(
        client: &reqwest::Client,
        jenkins_version: &str,
        with_experimental: bool,
    ) -> Result<Self> {
        let cache_dir = fetch::cache_dir();
        tokio::fs::create_dir_all(&cache_dir).await?;

        // Allow test / mirror overrides via environment variables.
        let stable_base =
            std::env::var("JPM_UC_STABLE_URL").unwrap_or_else(|_| UC_STABLE_URL.to_string());
        let experimental_url = std::env::var("JPM_UC_EXPERIMENTAL_URL")
            .unwrap_or_else(|_| UC_EXPERIMENTAL_URL.to_string());
        let plugin_versions_url = std::env::var("JPM_UC_PLUGIN_VERSIONS_URL")
            .unwrap_or_else(|_| UC_PLUGIN_VERSIONS_URL.to_string());

        let stable_url = format!("{stable_base}{jenkins_version}");
        let stable_cache = cache_dir.join(format!("uc-{jenkins_version}.json"));
        let experimental_cache = cache_dir.join("uc-experimental.json");
        let plugin_versions_cache = cache_dir.join("plugin-versions.json");

        let experimental_fut = async {
            if with_experimental {
                fetch_json(client, &experimental_url, &experimental_cache).await
            } else {
                Ok(Value::Null)
            }
        };

        let (stable, experimental, plugin_versions) = tokio::try_join!(
            fetch_json(client, &stable_url, &stable_cache),
            experimental_fut,
            fetch_json(client, &plugin_versions_url, &plugin_versions_cache),
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

    /// Return the `requiredCore` for a specific plugin version.
    pub fn required_core_for(&self, name: &str, version: &str) -> Option<&str> {
        self.plugin_versions["plugins"][name][version]["requiredCore"].as_str()
    }

    /// Return the highest version of `name` whose `requiredCore` ≤ `target`.
    /// Returns `None` if the plugin has no entries in `plugin-versions.json`.
    pub fn highest_compatible_version(
        &self,
        name: &str,
        target: &JenkinsVersion,
    ) -> Option<String> {
        let versions = self.plugin_versions["plugins"][name].as_object()?;
        versions
            .keys()
            .filter(|v| {
                let rc = self.required_core_for(name, v).unwrap_or("1.0");
                JenkinsVersion::new(rc) <= *target
            })
            .max_by(|a, b| JenkinsVersion::new(a.as_str()).cmp(&JenkinsVersion::new(b.as_str())))
            .cloned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn make_uc(plugin_versions: Value) -> UpdateCenter {
        UpdateCenter {
            stable: json!({}),
            experimental: json!({}),
            plugin_versions,
        }
    }

    #[test]
    fn required_core_for_present() {
        let uc = make_uc(json!({
            "plugins": { "git": { "5.7.0": { "requiredCore": "2.479.1" } } }
        }));
        assert_eq!(uc.required_core_for("git", "5.7.0"), Some("2.479.1"));
    }

    #[test]
    fn required_core_for_missing_version() {
        let uc = make_uc(json!({ "plugins": { "git": {} } }));
        assert_eq!(uc.required_core_for("git", "9.9.9"), None);
    }

    #[test]
    fn highest_compatible_picks_max_within_target() {
        let uc = make_uc(json!({
            "plugins": {
                "git": {
                    "5.2.0": { "requiredCore": "2.387.3" },
                    "5.5.0": { "requiredCore": "2.440.3" },
                    "5.9.0": { "requiredCore": "2.504.3" }
                }
            }
        }));
        // Target Jenkins 2.452.4: 5.5.0 is compatible, 5.9.0 is not.
        let best = uc.highest_compatible_version("git", &JenkinsVersion::new("2.452.4"));
        assert_eq!(best.as_deref(), Some("5.5.0"));
    }

    #[test]
    fn highest_compatible_all_too_new() {
        let uc = make_uc(json!({
            "plugins": {
                "git": {
                    "5.5.0": { "requiredCore": "2.440.3" },
                    "5.9.0": { "requiredCore": "2.504.3" }
                }
            }
        }));
        // Target Jenkins 2.387.3: neither is compatible.
        let best = uc.highest_compatible_version("git", &JenkinsVersion::new("2.387.3"));
        assert_eq!(best, None);
    }

    #[test]
    fn highest_compatible_exact_match_on_required_core() {
        let uc = make_uc(json!({
            "plugins": {
                "git": {
                    "5.2.0": { "requiredCore": "2.387.3" },
                    "5.5.0": { "requiredCore": "2.440.3" }
                }
            }
        }));
        // Target exactly 2.440.3: 5.5.0 should be included (≤, not <).
        let best = uc.highest_compatible_version("git", &JenkinsVersion::new("2.440.3"));
        assert_eq!(best.as_deref(), Some("5.5.0"));
    }

    #[test]
    fn strip_jsonp_wrapper_unwraps_update_center_post() {
        let wrapped = "updateCenter.post(\n{\"plugins\": {}}\n);\n";
        assert_eq!(strip_jsonp_wrapper(wrapped).trim(), "{\"plugins\": {}}");
        assert_eq!(
            strip_jsonp_wrapper("updateCenter.post({\"a\":1})"),
            "{\"a\":1}"
        );
    }

    #[test]
    fn strip_jsonp_wrapper_leaves_plain_json_alone() {
        let plain = "{\"plugins\": {}}";
        assert_eq!(strip_jsonp_wrapper(plain), plain);
    }

    #[test]
    fn format_age_picks_largest_unit() {
        assert_eq!(format_age(Duration::from_secs(42)), "42s");
        assert_eq!(format_age(Duration::from_secs(150)), "2m");
        assert_eq!(format_age(Duration::from_secs(7300)), "2h");
        assert_eq!(format_age(Duration::from_secs(3 * 86400)), "3d");
    }

    #[test]
    fn highest_compatible_unknown_plugin() {
        let uc = make_uc(json!({ "plugins": {} }));
        let best = uc.highest_compatible_version("unknown", &JenkinsVersion::new("2.452.4"));
        assert_eq!(best, None);
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
///
/// When every download attempt fails, an expired cache file is used instead
/// (with a warning), so a flaky network or an offline run can still work
/// from the last good copy.
async fn fetch_json(client: &reqwest::Client, url: &str, cache_path: &Path) -> Result<Value> {
    if let Some(cached) = try_load_cache(cache_path).await {
        return Ok(cached);
    }

    eprintln!("  fetching {url}");
    let text = match fetch::get_text(client, url).await {
        Ok(text) => text,
        Err(e) => {
            return match load_stale_cache(cache_path).await {
                Some((json, age)) => {
                    eprintln!(
                        "  warning: {e:#}; using cached copy from {} ago ({})",
                        format_age(age),
                        cache_path.display()
                    );
                    Ok(json)
                }
                None => Err(e),
            };
        }
    };
    let text = strip_jsonp_wrapper(&text);

    let json: Value =
        serde_json::from_str(text).with_context(|| format!("parsing JSON from {url}"))?;

    tokio::fs::write(cache_path, text).await?;
    Ok(json)
}

/// Load a cache file regardless of its age, returning its parsed content and
/// how old it is.
async fn load_stale_cache(path: &Path) -> Option<(Value, Duration)> {
    let modified = tokio::fs::metadata(path).await.ok()?.modified().ok()?;
    let age = SystemTime::now()
        .duration_since(modified)
        .unwrap_or_default();
    let text = tokio::fs::read_to_string(path).await.ok()?;
    Some((serde_json::from_str(&text).ok()?, age))
}

fn format_age(age: Duration) -> String {
    let secs = age.as_secs();
    match secs {
        0..=59 => format!("{secs}s"),
        60..=3599 => format!("{}m", secs / 60),
        3600..=86399 => format!("{}h", secs / 3600),
        _ => format!("{}d", secs / 86400),
    }
}

/// `update-center.json` (unlike `update-center.actual.json`) is JSONP:
/// `updateCenter.post({...});`. Return the JSON inside the wrapper, or the
/// input unchanged when there is no wrapper.
fn strip_jsonp_wrapper(text: &str) -> &str {
    let trimmed = text.trim();
    trimmed
        .strip_prefix("updateCenter.post(")
        .and_then(|rest| {
            let rest = rest.trim_end();
            let rest = rest.strip_suffix(';').unwrap_or(rest).trim_end();
            rest.strip_suffix(')')
        })
        .unwrap_or(text)
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
