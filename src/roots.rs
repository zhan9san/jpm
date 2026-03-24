use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use anyhow::{bail, Context, Result};

use crate::parser;
use crate::resolver;
use crate::update_center::UpdateCenter;

#[derive(Debug)]
pub struct RootsOptions {
    pub jenkins_version: String,
    pub plugin_file: PathBuf,
    pub write: bool,
    pub keep: Vec<String>,
}

pub async fn run(client: &reqwest::Client, opts: RootsOptions) -> Result<()> {
    println!(
        "jpm roots: minimizing manifest for Jenkins {}",
        opts.jenkins_version
    );

    let manifest_text = std::fs::read_to_string(&opts.plugin_file)
        .with_context(|| format!("reading manifest '{}'", opts.plugin_file.display()))?;
    let requests = parser::parse_plugins_txt(&manifest_text).context("parsing plugins.txt")?;
    println!("  {} plugin(s) in manifest", requests.len());

    let uc = UpdateCenter::fetch(client, &opts.jenkins_version)
        .await
        .context("fetching update center data")?;
    let resolved = resolver::resolve(&requests, &uc);

    let compat_issues = resolver::check_compat(&resolved, &uc, &opts.jenkins_version);
    if !compat_issues.is_empty() {
        for issue in &compat_issues {
            eprintln!(
                "error: {}:{} requires Jenkins >= {} (target: {})",
                issue.name, issue.version, issue.required_core, opts.jenkins_version
            );
        }
        bail!(
            "{} plugin(s) incompatible with Jenkins {}",
            compat_issues.len(),
            opts.jenkins_version
        );
    }

    let selected = selected_in_order(&requests);
    let resolved_names: HashSet<String> = resolved.keys().cloned().collect();
    let unknown: HashSet<String> = selected
        .iter()
        .filter(|name| !resolved_names.contains(*name))
        .cloned()
        .collect();
    for name in &unknown {
        eprintln!("  warning: '{name}' could not be resolved; keeping it in roots output");
    }

    let adjacency = required_adjacency(&resolved, &uc);
    let force_keep: HashSet<String> = opts.keep.into_iter().collect();
    let roots = compute_roots(&selected, &adjacency, &force_keep, &unknown);
    let out_text = parser::filter_plugins(&manifest_text, &roots);

    let output = if opts.write {
        opts.plugin_file.clone()
    } else {
        opts.plugin_file.with_file_name("plugins-roots.txt")
    };
    std::fs::write(&output, out_text).with_context(|| format!("writing '{}'", output.display()))?;
    println!(
        "  minimized {} -> {} root plugin(s)",
        selected.len(),
        roots.len()
    );
    println!("wrote '{}'", output.display());
    Ok(())
}

fn required_adjacency(
    resolved: &HashMap<String, resolver::ResolvedPlugin>,
    uc: &UpdateCenter,
) -> HashMap<String, Vec<String>> {
    let mut out: HashMap<String, Vec<String>> = HashMap::new();
    for plugin in resolved.values() {
        let deps = uc.dependencies_for(&plugin.name, &plugin.version);
        let mut req: Vec<String> = deps
            .into_iter()
            .filter_map(|(name, _version, optional)| {
                if optional || !resolved.contains_key(&name) {
                    None
                } else {
                    Some(name)
                }
            })
            .collect();
        req.sort();
        req.dedup();
        out.insert(plugin.name.clone(), req);
    }
    out
}

fn selected_in_order(requests: &[parser::PluginRequest]) -> Vec<String> {
    let mut seen: HashSet<String> = HashSet::new();
    let mut out = Vec::new();
    for r in requests {
        if seen.insert(r.name.clone()) {
            out.push(r.name.clone());
        }
    }
    out
}

fn compute_roots(
    selected: &[String],
    adjacency: &HashMap<String, Vec<String>>,
    force_keep: &HashSet<String>,
    unknown: &HashSet<String>,
) -> HashSet<String> {
    let mut reach: HashMap<String, HashSet<String>> = HashMap::new();
    for s in selected {
        reach.insert(s.clone(), reachable_from(s, adjacency));
    }

    let mut cycle_members: HashSet<String> = HashSet::new();
    for a in selected {
        for b in selected {
            if a == b {
                continue;
            }
            let a_to_b = reach.get(a).is_some_and(|set| set.contains(b));
            let b_to_a = reach.get(b).is_some_and(|set| set.contains(a));
            if a_to_b && b_to_a {
                cycle_members.insert(a.clone());
                cycle_members.insert(b.clone());
            }
        }
    }
    if !cycle_members.is_empty() {
        eprintln!(
            "  warning: cycle among selected plugins detected ({}); keeping cycle members",
            cycle_members.iter().cloned().collect::<Vec<_>>().join(",")
        );
    }

    let mut roots: HashSet<String> = selected.iter().cloned().collect();
    for p in selected {
        if force_keep.contains(p) || unknown.contains(p) || cycle_members.contains(p) {
            continue;
        }
        let covered = selected.iter().any(|q| {
            q != p
                && reach
                    .get(q)
                    .is_some_and(|reachable| reachable.contains(p.as_str()))
        });
        if covered {
            roots.remove(p);
        }
    }
    roots
}

fn reachable_from(start: &str, adjacency: &HashMap<String, Vec<String>>) -> HashSet<String> {
    let mut visited: HashSet<String> = HashSet::new();
    let mut stack: Vec<String> = Vec::new();
    if let Some(nexts) = adjacency.get(start) {
        stack.extend(nexts.iter().cloned());
    }
    while let Some(node) = stack.pop() {
        if !visited.insert(node.clone()) {
            continue;
        }
        if let Some(nexts) = adjacency.get(&node) {
            stack.extend(nexts.iter().cloned());
        }
    }
    visited
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compute_roots_removes_transitive_selected_plugins() {
        let selected = vec!["a".to_string(), "b".to_string(), "c".to_string()];
        let adjacency = HashMap::from([
            ("a".to_string(), vec!["b".to_string()]),
            ("b".to_string(), vec![]),
            ("c".to_string(), vec![]),
        ]);
        let roots = compute_roots(&selected, &adjacency, &HashSet::new(), &HashSet::new());
        assert!(roots.contains("a"));
        assert!(roots.contains("c"));
        assert!(!roots.contains("b"));
    }

    #[test]
    fn compute_roots_keeps_cycle_members() {
        let selected = vec!["a".to_string(), "b".to_string()];
        let adjacency = HashMap::from([
            ("a".to_string(), vec!["b".to_string()]),
            ("b".to_string(), vec!["a".to_string()]),
        ]);
        let roots = compute_roots(&selected, &adjacency, &HashSet::new(), &HashSet::new());
        assert!(roots.contains("a"));
        assert!(roots.contains("b"));
    }
}
