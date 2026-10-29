use anyhow::{Context, Result};
use serde_json::Value;
use std::collections::HashMap;
use std::path::Path;
use std::time::{Duration, SystemTime};

use crate::fetch;
use crate::url_plugins::UrlPlugin;
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
    /// `(version, source URL)` of each plugin registered with
    /// [`add_url_plugin`](Self::add_url_plugin), by name.
    pub url_plugins: HashMap<String, (String, String)>,
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

        // Mirror overrides: the `JPM_*` variables first, then the ones
        // plugin-installation-manager-tool reads.
        let env = fetch::env_non_empty;
        let stable_url = stable_url(
            env("JPM_UC_STABLE_URL").as_deref(),
            env("JENKINS_UC").as_deref(),
            jenkins_version,
        )?;
        let experimental_url = env("JPM_UC_EXPERIMENTAL_URL")
            .or_else(|| env("JENKINS_UC_EXPERIMENTAL").map(|uc| update_center_json_url(&uc)))
            .unwrap_or_else(|| UC_EXPERIMENTAL_URL.to_string());
        let plugin_versions_url = env("JPM_UC_PLUGIN_VERSIONS_URL")
            .or_else(|| env("JENKINS_PLUGIN_INFO"))
            .unwrap_or_else(|| UC_PLUGIN_VERSIONS_URL.to_string());

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
            url_plugins: HashMap::new(),
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

    /// Look up the source URL of a plugin registered with
    /// [`add_url_plugin`](Self::add_url_plugin). Update Center plugins have
    /// none, even though `plugin-versions.json` lists a `url` for each version.
    pub fn url_for(&self, plugin: &str, version: &str) -> Option<&str> {
        match self.url_plugins.get(plugin) {
            Some((v, url)) if v == version => Some(url),
            _ => None,
        }
    }

    /// Register a plugin read from a URL as if `plugin-versions.json` listed
    /// it, so resolution, compatibility and cycle checks see its manifest.
    pub fn add_url_plugin(&mut self, p: &UrlPlugin) {
        let dependencies: Vec<Value> = p
            .dependencies
            .iter()
            .map(|(name, version, optional)| {
                serde_json::json!({ "name": name, "version": version, "optional": optional })
            })
            .collect();
        let mut entry = serde_json::json!({
            "sha256": p.sha256,
            "dependencies": dependencies,
        });
        if let Some(core) = &p.required_core {
            entry["requiredCore"] = Value::from(core.as_str());
        }
        self.plugin_versions["plugins"][&p.name][&p.version] = entry;
        self.url_plugins
            .insert(p.name.clone(), (p.version.clone(), p.url.clone()));
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
            url_plugins: Default::default(),
        }
    }

    #[test]
    fn add_url_plugin_is_visible_through_lookups() {
        let mut uc = make_uc(json!({ "plugins": {} }));
        uc.add_url_plugin(&UrlPlugin {
            name: "my-plugin".into(),
            version: "1.0".into(),
            url: "file:///p.hpi".into(),
            sha256: "SHA=".into(),
            required_core: Some("2.440.3".into()),
            dependencies: vec![("credentials".into(), "1371.0".into(), false)],
        });
        assert_eq!(uc.url_for("my-plugin", "1.0"), Some("file:///p.hpi"));
        assert_eq!(uc.url_for("my-plugin", "2.0"), None);
        assert_eq!(uc.sha256_for("my-plugin", "1.0"), Some("SHA="));
        assert_eq!(uc.required_core_for("my-plugin", "1.0"), Some("2.440.3"));
        assert_eq!(
            uc.dependencies_for("my-plugin", "1.0"),
            vec![("credentials".to_string(), "1371.0".to_string(), false)]
        );
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
    fn stable_url_prefers_jpm_prefix_then_jenkins_uc() {
        assert_eq!(
            stable_url(None, None, "2.452.4").unwrap(),
            "https://updates.jenkins.io/update-center.actual.json?version=2.452.4"
        );
        assert_eq!(
            stable_url(Some("http://x/uc?version="), Some("http://y"), "2.452.4").unwrap(),
            "http://x/uc?version=2.452.4"
        );
        assert_eq!(
            stable_url(None, Some("https://mirror.example/jenkins/"), "2.452.4").unwrap(),
            "https://mirror.example/jenkins/update-center.json?version=2.452.4"
        );
        assert_eq!(
            stable_url(
                None,
                Some("https://mirror.example/uc.actual.json?site=a"),
                "2.452.4"
            )
            .unwrap(),
            "https://mirror.example/uc.actual.json?site=a&version=2.452.4"
        );
        assert!(stable_url(None, Some("not a url"), "2.452.4").is_err());
    }

    #[test]
    fn update_center_json_url_appends_file_to_base_urls() {
        assert_eq!(
            update_center_json_url("https://updates.jenkins.io"),
            "https://updates.jenkins.io/update-center.json"
        );
        assert_eq!(
            update_center_json_url("https://m.example/experimental/update-center.actual.json"),
            "https://m.example/experimental/update-center.actual.json"
        );
        assert_eq!(
            update_center_json_url("https://m.example/jenkins/?token=x"),
            "https://m.example/jenkins/update-center.json?token=x"
        );
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

/// URL of the stable Update Center for `jenkins_version`.
///
/// - `JPM_UC_STABLE_URL` is a prefix the version is appended to.
/// - `JENKINS_UC` (plugin-installation-manager-tool) is either a `.json` URL
///   or a base URL that `/update-center.json` is appended to; the version is
///   added as a `version` query parameter.
fn stable_url(
    jpm_prefix: Option<&str>,
    jenkins_uc: Option<&str>,
    jenkins_version: &str,
) -> Result<String> {
    if let Some(prefix) = jpm_prefix {
        return Ok(format!("{prefix}{jenkins_version}"));
    }
    let Some(uc) = jenkins_uc else {
        return Ok(format!("{UC_STABLE_URL}{jenkins_version}"));
    };
    let url = update_center_json_url(uc);
    let mut parsed =
        reqwest::Url::parse(&url).with_context(|| format!("invalid JENKINS_UC URL '{url}'"))?;
    parsed
        .query_pairs_mut()
        .append_pair("version", jenkins_version);
    Ok(parsed.into())
}

/// Like plugin-installation-manager-tool: a URL whose path does not end in
/// `json` is a base URL, so `/update-center.json` is appended to the path.
/// Strings that are not URLs are returned unchanged.
pub fn update_center_json_url(uc: &str) -> String {
    match reqwest::Url::parse(uc) {
        Ok(mut url) if !url.path().ends_with("json") => {
            let path = format!("{}/update-center.json", url.path().trim_end_matches('/'));
            url.set_path(&path);
            url.into()
        }
        _ => uc.to_string(),
    }
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
