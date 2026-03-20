use std::collections::{HashMap, VecDeque};

use crate::parser::{PluginRequest, VersionSpec};
use crate::update_center::UpdateCenter;
use crate::version::JenkinsVersion;

/// A plugin whose resolved version is incompatible with the target Jenkins.
#[derive(Debug, Clone)]
pub struct CompatIssue {
    pub name: String,
    pub version: String,
    pub required_core: String,
    /// Highest version of this plugin whose `requiredCore` ≤ target, if any.
    pub suggestion: Option<String>,
}

/// A fully resolved plugin with its exact pinned version.
#[derive(Debug, Clone)]
pub struct ResolvedPlugin {
    pub name: String,
    pub version: String,
    /// SHA-256 checksum of the `.hpi` file, sourced from the Update Center.
    /// `None` when the UC does not provide one (e.g. for bundled-only plugins).
    pub sha256: Option<String>,
    /// True if this plugin was requested directly in `plugins.txt`.
    pub is_direct: bool,
}

/// Source channel used when the plugin was enqueued, so we know which UC
/// endpoint to query for its dependency list.
#[derive(Clone)]
enum Channel {
    /// Use `plugin-versions.json` for this exact version.
    Pinned,
    /// Use stable UC's `plugins.<name>.dependencies`.
    Latest,
    /// Use experimental UC's `plugins.<name>.dependencies`.
    Experimental,
}

struct QueueEntry {
    name: String,
    version: String,
    channel: Channel,
    is_direct: bool,
}

/// Resolve the full transitive dependency graph for the requested plugins.
///
/// Algorithm:
/// 1. Seed the BFS queue with all directly requested plugins.
/// 2. For each plugin in the queue, look up its dependencies in the UC.
/// 3. For each dependency, if not yet seen add it; if seen, keep higher version
///    and re-enqueue if upgraded (so its own deps are re-visited).
/// 4. Return the final map of `name → ResolvedPlugin`.
pub fn resolve(
    requests: &[PluginRequest],
    uc: &UpdateCenter,
    bundled: &HashMap<String, String>,
) -> HashMap<String, ResolvedPlugin> {
    let mut resolved: HashMap<String, ResolvedPlugin> = HashMap::new();
    let mut queue: VecDeque<QueueEntry> = VecDeque::new();

    // Track which plugins were explicitly pinned in plugins.txt so we can warn
    // when a transitive dependency forces an upgrade past the pin.
    let direct_pins: HashMap<String, String> = requests
        .iter()
        .filter_map(|r| match &r.version {
            VersionSpec::Pinned(v) => Some((r.name.clone(), v.clone())),
            _ => None,
        })
        .collect();

    // Seed from the explicit requests.
    for req in requests {
        let (version, channel) = match resolve_version(req, uc) {
            Some(pair) => pair,
            None => {
                eprintln!(
                    "  warning: could not resolve version for '{}', skipping",
                    req.name
                );
                continue;
            }
        };
        queue.push_back(QueueEntry {
            name: req.name.clone(),
            version,
            channel,
            is_direct: true,
        });
    }

    while let Some(entry) = queue.pop_front() {
        let new_ver = JenkinsVersion::new(&entry.version);

        if let Some(existing) = resolved.get(&entry.name) {
            let existing_ver = JenkinsVersion::new(&existing.version);
            if new_ver <= existing_ver {
                continue;
            }
            // Warn when a transitive dep overrides an explicit pin from plugins.txt.
            if existing.is_direct {
                if let Some(pinned_ver) = direct_pins.get(&entry.name) {
                    eprintln!(
                        "  warning: {} pinned to {} in plugins.txt but upgraded to {}",
                        entry.name, pinned_ver, entry.version
                    );
                }
            }
            eprintln!(
                "  upgrading {} {} → {} (conflict resolution)",
                entry.name, existing.version, entry.version
            );
        }

        // If the WAR bundles an equal-or-newer version, prefer the bundled one.
        let effective_version = match bundled.get(&entry.name) {
            Some(bundled_ver) if JenkinsVersion::new(bundled_ver) >= new_ver => bundled_ver.clone(),
            _ => entry.version.clone(),
        };

        let sha256 = uc
            .sha256_for(&entry.name, &effective_version)
            .map(str::to_owned);

        resolved.insert(
            entry.name.clone(),
            ResolvedPlugin {
                name: entry.name.clone(),
                version: effective_version.clone(),
                sha256,
                is_direct: entry.is_direct,
            },
        );

        // Fetch dependencies for this plugin.
        let deps = match entry.channel {
            Channel::Latest => uc.latest_dependencies(&entry.name),
            Channel::Experimental => uc.experimental_dependencies(&entry.name),
            Channel::Pinned => uc.dependencies_for(&entry.name, &effective_version),
        };

        for (dep_name, dep_version, optional) in deps {
            if optional {
                continue;
            }
            queue.push_back(QueueEntry {
                name: dep_name,
                version: dep_version,
                channel: Channel::Pinned,
                is_direct: false,
            });
        }
    }

    resolved
}

/// Check every resolved plugin against the target Jenkins version.
///
/// Returns a list of `CompatIssue`s for plugins whose `requiredCore` exceeds
/// the target. Each issue includes the highest compatible version, if one exists.
pub fn check_compat(
    resolved: &HashMap<String, ResolvedPlugin>,
    uc: &UpdateCenter,
    jenkins_version: &str,
) -> Vec<CompatIssue> {
    let target = JenkinsVersion::new(jenkins_version);
    let mut issues: Vec<CompatIssue> = resolved
        .values()
        .filter_map(|plugin| {
            let rc = uc.required_core_for(&plugin.name, &plugin.version)?;
            if JenkinsVersion::new(rc) <= target {
                return None;
            }
            Some(CompatIssue {
                name: plugin.name.clone(),
                version: plugin.version.clone(),
                required_core: rc.to_owned(),
                suggestion: uc.highest_compatible_version(&plugin.name, &target),
            })
        })
        .collect();
    issues.sort_by(|a, b| a.name.cmp(&b.name));
    issues
}

/// Resolve a `VersionSpec` into an actual version string and channel.
fn resolve_version(req: &PluginRequest, uc: &UpdateCenter) -> Option<(String, Channel)> {
    match &req.version {
        VersionSpec::Latest => {
            let v = uc.latest_version(&req.name)?.to_owned();
            Some((v, Channel::Latest))
        }
        VersionSpec::Experimental => {
            // Call experimental_version once and branch on the result.
            match uc.experimental_version(&req.name) {
                Some(v) => Some((v.to_owned(), Channel::Experimental)),
                None => {
                    let v = uc.latest_version(&req.name)?.to_owned();
                    Some((v, Channel::Latest))
                }
            }
        }
        VersionSpec::Pinned(v) => Some((v.clone(), Channel::Pinned)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn make_uc_for_compat(plugin_versions: serde_json::Value) -> UpdateCenter {
        UpdateCenter {
            stable: json!({}),
            experimental: json!({}),
            plugin_versions,
        }
    }

    fn resolved_plugin(name: &str, version: &str) -> ResolvedPlugin {
        ResolvedPlugin {
            name: name.to_string(),
            version: version.to_string(),
            sha256: None,
            is_direct: true,
        }
    }

    #[test]
    fn check_compat_no_issues() {
        let uc = make_uc_for_compat(json!({
            "plugins": { "git": { "5.5.0": { "requiredCore": "2.440.3" } } }
        }));
        let resolved = HashMap::from([("git".to_string(), resolved_plugin("git", "5.5.0"))]);
        let issues = check_compat(&resolved, &uc, "2.452.4");
        assert!(issues.is_empty());
    }

    #[test]
    fn check_compat_detects_incompatible_plugin() {
        let uc = make_uc_for_compat(json!({
            "plugins": {
                "git": {
                    "5.5.0": { "requiredCore": "2.440.3" },
                    "5.9.0": { "requiredCore": "2.504.3" }
                }
            }
        }));
        // git:5.9.0 requires Jenkins 2.504.3, but target is 2.452.4.
        let resolved = HashMap::from([("git".to_string(), resolved_plugin("git", "5.9.0"))]);
        let issues = check_compat(&resolved, &uc, "2.452.4");
        assert_eq!(issues.len(), 1);
        assert_eq!(issues[0].name, "git");
        assert_eq!(issues[0].version, "5.9.0");
        assert_eq!(issues[0].required_core, "2.504.3");
        assert_eq!(issues[0].suggestion.as_deref(), Some("5.5.0"));
    }

    #[test]
    fn check_compat_no_suggestion_when_all_incompatible() {
        let uc = make_uc_for_compat(json!({
            "plugins": {
                "git": { "5.9.0": { "requiredCore": "2.504.3" } }
            }
        }));
        let resolved = HashMap::from([("git".to_string(), resolved_plugin("git", "5.9.0"))]);
        let issues = check_compat(&resolved, &uc, "2.387.3");
        assert_eq!(issues.len(), 1);
        assert_eq!(issues[0].suggestion, None);
    }

    #[test]
    fn check_compat_skips_missing_required_core() {
        // Plugin entry has no requiredCore field → treated as compatible.
        let uc = make_uc_for_compat(json!({
            "plugins": { "git": { "5.5.0": {} } }
        }));
        let resolved = HashMap::from([("git".to_string(), resolved_plugin("git", "5.5.0"))]);
        let issues = check_compat(&resolved, &uc, "2.100.0");
        assert!(issues.is_empty());
    }

    #[test]
    fn check_compat_multiple_issues_sorted() {
        let uc = make_uc_for_compat(json!({
            "plugins": {
                "mailer":      { "1.0.0": { "requiredCore": "2.504.3" } },
                "credentials": { "2.0.0": { "requiredCore": "2.504.3" } }
            }
        }));
        let resolved = HashMap::from([
            ("mailer".to_string(), resolved_plugin("mailer", "1.0.0")),
            (
                "credentials".to_string(),
                resolved_plugin("credentials", "2.0.0"),
            ),
        ]);
        let issues = check_compat(&resolved, &uc, "2.387.3");
        assert_eq!(issues.len(), 2);
        assert_eq!(issues[0].name, "credentials");
        assert_eq!(issues[1].name, "mailer");
    }
}
