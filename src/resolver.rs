use std::collections::{HashMap, VecDeque};

use crate::detached::DetachedMetadata;
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
pub fn resolve(requests: &[PluginRequest], uc: &UpdateCenter) -> HashMap<String, ResolvedPlugin> {
    resolve_with_min_versions(requests, uc, &HashMap::new())
}

/// Resolve with optional minimum versions ("floors") applied to plugins.
///
/// When a floor exists for a plugin, the resolver will use at least that
/// version for both direct and transitive occurrences.
pub fn resolve_with_min_versions(
    requests: &[PluginRequest],
    uc: &UpdateCenter,
    min_versions: &HashMap<String, String>,
) -> HashMap<String, ResolvedPlugin> {
    let mut resolved: HashMap<String, ResolvedPlugin> = HashMap::new();
    let mut queue: VecDeque<QueueEntry> = VecDeque::new();
    let mut optional_min_versions: HashMap<String, String> = HashMap::new();

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
        let version = match min_versions.get(&req.name) {
            Some(floor) if JenkinsVersion::new(floor) > JenkinsVersion::new(&version) => {
                floor.clone()
            }
            _ => version,
        };
        queue.push_back(QueueEntry {
            name: req.name.clone(),
            version,
            channel,
            is_direct: true,
        });
    }

    while let Some(mut entry) = queue.pop_front() {
        // Optional dependencies can still impose a minimum version when the
        // target plugin is present in the effective graph.
        if let Some(min_version) = optional_min_versions.get(&entry.name) {
            if JenkinsVersion::new(min_version) > JenkinsVersion::new(&entry.version) {
                entry.version = min_version.clone();
            }
        }
        if let Some(min_version) = min_versions.get(&entry.name) {
            if JenkinsVersion::new(min_version) > JenkinsVersion::new(&entry.version) {
                entry.version = min_version.clone();
            }
        }

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

        let effective_version = entry.version.clone();

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
                let floor_changed = match optional_min_versions.get(&dep_name) {
                    Some(existing_floor) => {
                        JenkinsVersion::new(&dep_version) > JenkinsVersion::new(existing_floor)
                    }
                    None => true,
                };
                if floor_changed {
                    optional_min_versions.insert(dep_name.clone(), dep_version.clone());
                }

                if let Some(existing) = resolved.get(&dep_name) {
                    if JenkinsVersion::new(&dep_version) > JenkinsVersion::new(&existing.version) {
                        queue.push_back(QueueEntry {
                            name: dep_name,
                            version: dep_version,
                            channel: Channel::Pinned,
                            is_direct: false,
                        });
                    }
                }
                continue;
            }

            let dep_effective_version = match optional_min_versions.get(&dep_name) {
                Some(floor) if JenkinsVersion::new(floor) > JenkinsVersion::new(&dep_version) => {
                    floor.clone()
                }
                _ => dep_version,
            };
            let dep_effective_version = match min_versions.get(&dep_name) {
                Some(floor)
                    if JenkinsVersion::new(floor) > JenkinsVersion::new(&dep_effective_version) =>
                {
                    floor.clone()
                }
                _ => dep_effective_version,
            };
            queue.push_back(QueueEntry {
                name: dep_name,
                version: dep_effective_version,
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

/// Detect one dependency cycle in the effective plugin graph.
///
/// Returns a cycle path like `a -> b -> c -> a` when found, otherwise `None`.
pub fn detect_cycle(
    resolved: &HashMap<String, ResolvedPlugin>,
    uc: &UpdateCenter,
    bundled: &HashMap<String, String>,
    detached: &DetachedMetadata,
) -> Option<Vec<String>> {
    let adjacency = cycle_adjacency(resolved, uc, bundled, detached);

    fn dfs(
        node: &str,
        adjacency: &HashMap<String, Vec<String>>,
        state: &mut HashMap<String, u8>,
        stack: &mut Vec<String>,
        stack_pos: &mut HashMap<String, usize>,
    ) -> Option<Vec<String>> {
        state.insert(node.to_string(), 1);
        stack.push(node.to_string());
        stack_pos.insert(node.to_string(), stack.len() - 1);

        if let Some(neighbors) = adjacency.get(node) {
            for neigh in neighbors {
                let neigh_state = *state.get(neigh).unwrap_or(&0);
                if neigh_state == 0 {
                    if let Some(cycle) = dfs(neigh, adjacency, state, stack, stack_pos) {
                        return Some(cycle);
                    }
                } else if neigh_state == 1 {
                    let start = *stack_pos.get(neigh)?;
                    let mut cycle = stack[start..].to_vec();
                    cycle.push(neigh.clone());
                    return Some(cycle);
                }
            }
        }

        stack.pop();
        stack_pos.remove(node);
        state.insert(node.to_string(), 2);
        None
    }

    let mut state: HashMap<String, u8> = HashMap::new(); // 0=unvisited, 1=visiting, 2=done
    let mut stack: Vec<String> = Vec::new();
    let mut stack_pos: HashMap<String, usize> = HashMap::new();

    let mut names: Vec<&String> = adjacency.keys().collect();
    names.sort();
    for name in names {
        if *state.get(name).unwrap_or(&0) == 0 {
            if let Some(cycle) = dfs(name, &adjacency, &mut state, &mut stack, &mut stack_pos) {
                return Some(cycle);
            }
        }
    }
    None
}

pub fn cycle_adjacency(
    resolved: &HashMap<String, ResolvedPlugin>,
    uc: &UpdateCenter,
    bundled: &HashMap<String, String>,
    detached: &DetachedMetadata,
) -> HashMap<String, Vec<String>> {
    // Runtime-active plugins include resolved plugins plus bundled plugins.
    let mut active: HashMap<String, String> = bundled.clone();
    for plugin in resolved.values() {
        active.insert(plugin.name.clone(), plugin.version.clone());
    }

    let mut adjacency: HashMap<String, Vec<String>> = HashMap::new();

    for (name, version) in &active {
        let deps = uc.dependencies_for(name, version);
        let mut edges: Vec<String> = deps
            .into_iter()
            .filter_map(|(dep_name, _dep_version, _optional)| {
                // Optional deps are active when the target plugin is present.
                let dep_present = active.contains_key(&dep_name);
                if !dep_present {
                    None
                } else {
                    Some(dep_name)
                }
            })
            .collect();

        // Split/detached plugins can be implied by required Jenkins core.
        // If a plugin targets an old core, Jenkins may add an implicit
        // dependency on detached plugins present at runtime.
        let plugin_required_core = uc.required_core_for(name, version).unwrap_or("0");
        for (detached_name, split_core) in &detached.split_plugins {
            if detached_name == name || !active.contains_key(detached_name) {
                continue;
            }
            if JenkinsVersion::new(plugin_required_core) <= JenkinsVersion::new(split_core) {
                edges.push(detached_name.clone());
            }
        }

        edges.sort();
        edges.dedup();

        // Match Jenkins BREAK_CYCLES overrides for split plugins.
        for (from, to) in &detached.break_cycles {
            if from == name {
                if let Some(i) = edges.iter().position(|e| e == to) {
                    edges.remove(i);
                }
            }
        }
        adjacency.insert(name.clone(), edges);
    }
    adjacency
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

    #[test]
    fn detect_cycle_finds_back_edge_cycle() {
        let uc = make_uc_for_compat(json!({
            "plugins": {
                "a": { "1.0.0": { "dependencies": [{ "name": "b", "version": "1.0.0", "optional": false }] } },
                "b": { "1.0.0": { "dependencies": [{ "name": "c", "version": "1.0.0", "optional": false }] } },
                "c": { "1.0.0": { "dependencies": [{ "name": "a", "version": "1.0.0", "optional": false }] } }
            }
        }));
        let resolved = HashMap::from([
            ("a".to_string(), resolved_plugin("a", "1.0.0")),
            ("b".to_string(), resolved_plugin("b", "1.0.0")),
            ("c".to_string(), resolved_plugin("c", "1.0.0")),
        ]);
        let detached = DetachedMetadata {
            split_plugins: HashMap::new(),
            break_cycles: vec![],
        };

        let bundled: HashMap<String, String> = HashMap::new();
        let cycle = detect_cycle(&resolved, &uc, &bundled, &detached).expect("expected a cycle");
        assert!(cycle.len() >= 2);
        assert_eq!(cycle.first(), cycle.last());
    }

    #[test]
    fn detect_cycle_ignores_optional_edge_when_target_absent() {
        let uc = make_uc_for_compat(json!({
            "plugins": {
                "a": { "1.0.0": { "dependencies": [{ "name": "b", "version": "1.0.0", "optional": true }] } }
            }
        }));
        let resolved = HashMap::from([("a".to_string(), resolved_plugin("a", "1.0.0"))]);
        let detached = DetachedMetadata {
            split_plugins: HashMap::new(),
            break_cycles: vec![],
        };
        let bundled: HashMap<String, String> = HashMap::new();

        assert!(detect_cycle(&resolved, &uc, &bundled, &detached).is_none());
    }

    #[test]
    fn detect_cycle_activates_optional_edge_when_target_present() {
        let uc = make_uc_for_compat(json!({
            "plugins": {
                "a": { "1.0.0": { "dependencies": [{ "name": "b", "version": "1.0.0", "optional": true }] } },
                "b": { "1.0.0": { "dependencies": [{ "name": "a", "version": "1.0.0", "optional": false }] } }
            }
        }));
        let resolved = HashMap::from([
            ("a".to_string(), resolved_plugin("a", "1.0.0")),
            ("b".to_string(), resolved_plugin("b", "1.0.0")),
        ]);
        let detached = DetachedMetadata {
            split_plugins: HashMap::new(),
            break_cycles: vec![],
        };
        let bundled: HashMap<String, String> = HashMap::new();

        let cycle = detect_cycle(&resolved, &uc, &bundled, &detached).expect("expected a cycle");
        assert_eq!(cycle.first(), cycle.last());
    }

    #[test]
    fn detect_cycle_includes_bundled_plugins() {
        let uc = make_uc_for_compat(json!({
            "plugins": {
                "a": { "1.0.0": { "dependencies": [{ "name": "b", "version": "1.0.0", "optional": true }] } },
                "b": { "1.0.0": { "dependencies": [{ "name": "a", "version": "1.0.0", "optional": false }] } }
            }
        }));
        let resolved = HashMap::from([("a".to_string(), resolved_plugin("a", "1.0.0"))]);
        let bundled = HashMap::from([("b".to_string(), "1.0.0".to_string())]);
        let detached = DetachedMetadata {
            split_plugins: HashMap::new(),
            break_cycles: vec![],
        };

        let cycle = detect_cycle(&resolved, &uc, &bundled, &detached).expect("expected a cycle");
        assert_eq!(cycle.first(), cycle.last());
    }

    #[test]
    fn detect_cycle_returns_none_for_dag() {
        let uc = make_uc_for_compat(json!({
            "plugins": {
                "a": { "1.0.0": { "dependencies": [{ "name": "b", "version": "1.0.0", "optional": false }] } },
                "b": { "1.0.0": { "dependencies": [{ "name": "c", "version": "1.0.0", "optional": false }] } },
                "c": { "1.0.0": { "dependencies": [] } }
            }
        }));
        let resolved = HashMap::from([
            ("a".to_string(), resolved_plugin("a", "1.0.0")),
            ("b".to_string(), resolved_plugin("b", "1.0.0")),
            ("c".to_string(), resolved_plugin("c", "1.0.0")),
        ]);

        let bundled: HashMap<String, String> = HashMap::new();
        let detached = DetachedMetadata {
            split_plugins: HashMap::new(),
            break_cycles: vec![],
        };
        assert!(detect_cycle(&resolved, &uc, &bundled, &detached).is_none());
    }

    #[test]
    fn resolve_preserves_pinned_version_even_if_bundled_is_newer() {
        let uc = make_uc_for_compat(json!({
            "plugins": {
                "a": {
                    "1.0.0": { "dependencies": [] }
                }
            }
        }));
        let requests = vec![PluginRequest {
            name: "a".to_string(),
            version: VersionSpec::Pinned("1.0.0".to_string()),
            url: None,
        }];
        let resolved = resolve(&requests, &uc);
        assert_eq!(resolved["a"].version, "1.0.0");
    }

    #[test]
    fn resolve_upgrades_present_plugin_to_optional_dependency_floor() {
        let uc = make_uc_for_compat(json!({
            "plugins": {
                "a": {
                    "1.0.0": {
                        "dependencies": [
                            { "name": "b", "version": "1.0.0", "optional": false }
                        ]
                    }
                },
                "b": {
                    "1.0.0": { "dependencies": [] },
                    "2.0.0": { "dependencies": [] }
                },
                "c": {
                    "1.0.0": {
                        "dependencies": [
                            { "name": "b", "version": "2.0.0", "optional": true }
                        ]
                    }
                }
            }
        }));

        let requests = vec![
            PluginRequest {
                name: "a".to_string(),
                version: VersionSpec::Pinned("1.0.0".to_string()),
                url: None,
            },
            PluginRequest {
                name: "c".to_string(),
                version: VersionSpec::Pinned("1.0.0".to_string()),
                url: None,
            },
        ];

        let resolved = resolve(&requests, &uc);
        assert_eq!(resolved["b"].version, "2.0.0");
    }
}
