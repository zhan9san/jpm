use std::collections::{HashMap, VecDeque};

use crate::parser::{PluginRequest, VersionSpec};
use crate::update_center::UpdateCenter;
use crate::version::JenkinsVersion;

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
            eprintln!(
                "  upgrading {} {} → {} (conflict resolution)",
                entry.name, existing.version, entry.version
            );
        }

        // If the WAR bundles an equal-or-newer version, prefer the bundled one.
        let effective_version = match bundled.get(&entry.name) {
            Some(bundled_ver) if JenkinsVersion::new(bundled_ver) >= new_ver => {
                bundled_ver.clone()
            }
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
