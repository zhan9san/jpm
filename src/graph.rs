use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use anyhow::{bail, Context, Result};

use crate::bundled;
use crate::detached;
use crate::lockfile;
use crate::parser;
use crate::resolver::{self, ResolvedPlugin};
use crate::update_center::UpdateCenter;

#[derive(Debug)]
pub struct GraphArgs {
    pub jenkins_version: String,
    pub plugin_file: Option<PathBuf>,
    pub lock_file: Option<PathBuf>,
    pub output: PathBuf,
    pub skip_bundled: bool,
    pub allow_cycle: bool,
}

pub async fn run(client: &reqwest::Client, args: GraphArgs) -> Result<()> {
    if args.plugin_file.is_some() == args.lock_file.is_some() {
        bail!("exactly one of --file or --lock must be specified");
    }

    println!(
        "jpm graph: building dependency graph for Jenkins {}",
        args.jenkins_version
    );

    let bundled_fut = async {
        if args.skip_bundled {
            return Ok(HashMap::new());
        }
        match bundled::fetch_bundled_plugins(client, &args.jenkins_version).await {
            Ok(map) => Ok(map),
            Err(e) => {
                eprintln!("  warning: could not fetch bundled plugins: {e}");
                Ok(HashMap::new())
            }
        }
    };

    let (uc, bundled, detached) = tokio::try_join!(
        UpdateCenter::fetch(client, &args.jenkins_version),
        bundled_fut,
        detached::fetch(client, &args.jenkins_version),
    )
    .context("fetching remote data")?;

    let resolved = if let Some(plugin_file) = &args.plugin_file {
        let content = std::fs::read_to_string(plugin_file)
            .with_context(|| format!("reading manifest '{}'", plugin_file.display()))?;
        let requests = parser::parse_plugins_txt(&content).context("parsing plugins.txt")?;
        resolver::resolve(&requests, &uc)
    } else {
        let lock_file = args.lock_file.as_ref().expect("lock_file present");
        let content = std::fs::read_to_string(lock_file)
            .with_context(|| format!("reading lock '{}'", lock_file.display()))?;
        if let Some(v) = lockfile::parse_jenkins_version(&content) {
            if v != args.jenkins_version {
                eprintln!(
                    "  warning: lock Jenkins version is {v}, but --jenkins-version is {}",
                    args.jenkins_version
                );
            }
        }
        lockfile::parse(&content)
            .into_iter()
            .map(|(name, (version, sha256))| {
                (
                    name.clone(),
                    ResolvedPlugin {
                        name,
                        version,
                        sha256,
                        is_direct: false,
                    },
                )
            })
            .collect()
    };

    let adjacency = resolver::cycle_adjacency(&resolved, &uc, &bundled, &detached);
    let cycle = resolver::detect_cycle(&resolved, &uc, &bundled, &detached);
    let dot = render_dot(&adjacency, cycle.as_ref());
    std::fs::write(&args.output, dot)
        .with_context(|| format!("writing graph '{}'", args.output.display()))?;

    println!(
        "  wrote graph '{}' ({} nodes)",
        args.output.display(),
        adjacency.len()
    );

    if let Some(c) = cycle {
        eprintln!(
            "error: found cycle in plugin dependencies: {}",
            c.join(" -> ")
        );
        if !args.allow_cycle {
            bail!(
                "cycle detected (graph was written). Re-run with --allow-cycle to keep zero exit code"
            );
        }
    }

    Ok(())
}

fn render_dot(adjacency: &HashMap<String, Vec<String>>, cycle: Option<&Vec<String>>) -> String {
    let mut out = String::new();
    out.push_str("digraph jpm {\n");
    out.push_str("  rankdir=LR;\n");
    out.push_str("  node [shape=box, fontsize=10];\n");

    let mut cycle_nodes: HashSet<String> = HashSet::new();
    let mut cycle_edges: HashSet<(String, String)> = HashSet::new();
    if let Some(c) = cycle {
        for n in c {
            cycle_nodes.insert(n.clone());
        }
        for w in c.windows(2) {
            if let [a, b] = &w {
                cycle_edges.insert(((*a).clone(), (*b).clone()));
            }
        }
    }

    let mut nodes: Vec<&String> = adjacency.keys().collect();
    nodes.sort();
    for n in nodes {
        if cycle_nodes.contains(n.as_str()) {
            out.push_str(&format!(
                "  \"{}\" [style=filled, fillcolor=\"#ffd1d1\"];\n",
                escape(n)
            ));
        } else {
            out.push_str(&format!("  \"{}\";\n", escape(n)));
        }
    }

    let mut edges: Vec<(String, String)> = Vec::new();
    for (from, tos) in adjacency {
        for to in tos {
            edges.push((from.clone(), to.clone()));
        }
    }
    edges.sort();

    for (from, to) in edges {
        if cycle_edges.contains(&(from.clone(), to.clone())) {
            out.push_str(&format!(
                "  \"{}\" -> \"{}\" [color=red, penwidth=2.0];\n",
                escape(&from),
                escape(&to)
            ));
        } else {
            out.push_str(&format!(
                "  \"{}\" -> \"{}\";\n",
                escape(&from),
                escape(&to)
            ));
        }
    }

    out.push_str("}\n");
    out
}

fn escape(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_dot_highlights_cycle() {
        let adjacency = HashMap::from([
            ("a".to_string(), vec!["b".to_string()]),
            ("b".to_string(), vec!["a".to_string()]),
        ]);
        let cycle = vec!["a".to_string(), "b".to_string(), "a".to_string()];
        let dot = render_dot(&adjacency, Some(&cycle));
        assert!(dot.contains("\"a\" [style=filled"));
        assert!(dot.contains("\"a\" -> \"b\" [color=red"));
        assert!(dot.contains("\"b\" -> \"a\" [color=red"));
    }
}
