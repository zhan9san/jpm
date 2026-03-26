mod bundled;
mod detached;
mod doctor;
mod graph;
mod installer;
mod lockfile;
mod parser;
mod resolver;
mod roots;
mod update_center;
mod version;

use anyhow::{Context, Result};
use clap::{Args, Parser, Subcommand};
use std::collections::HashMap;
use std::path::PathBuf;

use version::JenkinsVersion;

/// Jenkins Plugin Manager — reproducible plugin management for Jenkins.
#[derive(Parser, Debug)]
#[command(name = "jpm", version, about, long_about = None)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Resolve plugins.txt and generate a plugins-lock.txt.
    Lock(LockArgs),
    /// Install plugins from a lock file into a plugin directory.
    Install(InstallArgs),
    /// Validate plugin directory state against a lock file.
    Doctor(DoctorArgs),
    /// Generate dependency graph from plugins.txt or plugins-lock.txt.
    Graph(GraphArgs),
    /// Minimize plugins.txt to root-only direct intents.
    Roots(RootsArgs),
}

// ── jpm lock ──────────────────────────────────────────────────────────────────

#[derive(Args, Debug)]
struct LockArgs {
    /// Jenkins version to target (e.g. `2.452.4`).
    #[arg(short = 'j', long, value_name = "VERSION")]
    jenkins_version: String,

    /// Path to the input `plugins.txt` manifest.
    #[arg(short = 'f', long, value_name = "FILE", default_value = "plugins.txt")]
    plugin_file: PathBuf,

    /// Path to write the generated lock file.
    #[arg(
        short = 'o',
        long,
        value_name = "FILE",
        default_value = "plugins-lock.txt"
    )]
    output: PathBuf,

    /// Skip fetching bundled plugin versions from the Jenkins WAR pom.xml.
    /// Faster, but may include plugins that are already bundled in the WAR.
    #[arg(long)]
    skip_bundled: bool,

    /// Rewrite plugins.txt, replacing versions of incompatible plugins with
    /// the highest version whose `requiredCore` ≤ `--jenkins-version`.
    #[arg(long)]
    fix: bool,

    /// Rewrite plugins.txt, replacing ALL pinned versions with the highest
    /// version whose `requiredCore` ≤ `--jenkins-version` (superset of --fix).
    #[arg(long)]
    upgrade: bool,
}

// ── jpm install ───────────────────────────────────────────────────────────────

#[derive(Args, Debug)]
struct InstallArgs {
    /// Path to the lock file to install from.
    #[arg(
        short = 'l',
        long,
        value_name = "FILE",
        default_value = "plugins-lock.txt"
    )]
    lock: PathBuf,

    /// Directory to install plugins into.
    #[arg(short = 'd', long, value_name = "DIR", default_value = "plugins")]
    plugin_dir: PathBuf,

    /// Warn on individual download failures instead of aborting.
    #[arg(long)]
    skip_failed: bool,

    /// Print what would be installed without downloading anything.
    #[arg(long)]
    dry_run: bool,
}

// ── jpm doctor ────────────────────────────────────────────────────────────────

#[derive(Args, Debug)]
struct DoctorArgs {
    /// Path to the lock file to validate against.
    #[arg(
        short = 'l',
        long,
        value_name = "FILE",
        default_value = "plugins-lock.txt"
    )]
    lock: PathBuf,

    /// Directory containing installed plugin archives.
    #[arg(short = 'd', long, value_name = "DIR", default_value = "plugins")]
    plugin_dir: PathBuf,

    /// Exit non-zero when any finding is detected.
    #[arg(long)]
    strict: bool,
}

// ── jpm graph ─────────────────────────────────────────────────────────────────

#[derive(Args, Debug)]
struct GraphArgs {
    /// Jenkins version to target (e.g. `2.452.4`).
    #[arg(short = 'j', long, value_name = "VERSION")]
    jenkins_version: String,

    /// Path to input plugins.txt manifest.
    #[arg(short = 'f', long, value_name = "FILE")]
    plugin_file: Option<PathBuf>,

    /// Path to input plugins-lock.txt.
    #[arg(short = 'l', long, value_name = "FILE")]
    lock_file: Option<PathBuf>,

    /// DOT output file path.
    #[arg(
        short = 'o',
        long,
        value_name = "FILE",
        default_value = "plugins-graph.dot"
    )]
    output: PathBuf,

    /// Skip fetching bundled plugin versions from the Jenkins WAR pom.xml.
    #[arg(long)]
    skip_bundled: bool,

    /// Return zero exit code even when a cycle exists.
    #[arg(long)]
    allow_cycle: bool,
}

// ── jpm roots ─────────────────────────────────────────────────────────────────

#[derive(Args, Debug)]
struct RootsArgs {
    /// Jenkins version to target (e.g. `2.452.4`).
    #[arg(short = 'j', long, value_name = "VERSION")]
    jenkins_version: String,

    /// Path to input plugins.txt manifest.
    #[arg(short = 'f', long, value_name = "FILE", default_value = "plugins.txt")]
    plugin_file: PathBuf,

    /// Rewrite plugins.txt in-place instead of writing plugins-roots.txt.
    #[arg(long)]
    write: bool,

    /// Keep plugin in roots output even if transitively reachable.
    #[arg(long, value_name = "PLUGIN")]
    keep: Vec<String>,
}

// ── Entry point ───────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    let client = reqwest::Client::builder()
        .user_agent(concat!("jpm/", env!("CARGO_PKG_VERSION")))
        .build()?;

    match cli.command {
        Command::Lock(args) => run_lock(&client, args).await,
        Command::Install(args) => run_install(&client, args).await,
        Command::Doctor(args) => run_doctor(args),
        Command::Graph(args) => run_graph(&client, args).await,
        Command::Roots(args) => run_roots(&client, args).await,
    }
}

async fn run_lock(client: &reqwest::Client, args: LockArgs) -> Result<()> {
    println!(
        "jpm lock: resolving plugins for Jenkins {}",
        args.jenkins_version
    );

    let manifest_text = std::fs::read_to_string(&args.plugin_file)
        .with_context(|| format!("reading manifest '{}'", args.plugin_file.display()))?;
    let mut requests = parser::parse_plugins_txt(&manifest_text).context("parsing plugins.txt")?;

    println!("  {} plugin(s) in manifest", requests.len());

    println!("fetching Update Center data and bundled plugin list...");

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
        update_center::UpdateCenter::fetch(client, &args.jenkins_version),
        bundled_fut,
        detached::fetch(client, &args.jenkins_version),
    )
    .context("fetching remote data")?;

    if !bundled.is_empty() {
        println!("  {} plugin(s) bundled in Jenkins WAR", bundled.len());
    }

    println!("resolving dependencies...");
    let mut resolved = resolver::resolve(&requests, &uc);

    // ── Jenkins version compatibility check ───────────────────────────────────
    let compat_issues = resolver::check_compat(&resolved, &uc, &args.jenkins_version);

    if !compat_issues.is_empty() && !args.fix && !args.upgrade {
        for issue in &compat_issues {
            eprintln!(
                "error: {}:{} requires Jenkins >= {} (target: {})",
                issue.name, issue.version, issue.required_core, args.jenkins_version
            );
            match &issue.suggestion {
                Some(v) => eprintln!("       → highest compatible version: {}:{}", issue.name, v),
                None => eprintln!("       → no compatible version found in Update Center"),
            }
        }
        eprintln!(
            "       → run `jpm lock --fix` to auto-correct plugins.txt ({} issue(s))",
            compat_issues.len()
        );
        anyhow::bail!(
            "{} plugin(s) incompatible with Jenkins {}",
            compat_issues.len(),
            args.jenkins_version
        );
    }

    // ── Build version update map (--fix and/or --upgrade) ────────────────────
    if args.fix || args.upgrade {
        let target = JenkinsVersion::new(&args.jenkins_version);
        let mut updates: HashMap<String, String> = HashMap::new();

        // --fix: fix only plugins that fail the compat check.
        for issue in &compat_issues {
            match &issue.suggestion {
                Some(v) => {
                    updates.insert(issue.name.clone(), v.clone());
                }
                None => {
                    anyhow::bail!(
                        "{}:{} requires Jenkins >= {} and no compatible version exists — \
                         cannot auto-fix",
                        issue.name,
                        issue.version,
                        issue.required_core
                    );
                }
            }
        }

        // --upgrade: also update compatible plugins to the highest available version.
        if args.upgrade {
            for plugin in resolved.values() {
                if updates.contains_key(&plugin.name) {
                    continue;
                }
                if let Some(best) = uc.highest_compatible_version(&plugin.name, &target) {
                    if JenkinsVersion::new(&best) > JenkinsVersion::new(&plugin.version) {
                        updates.insert(plugin.name.clone(), best);
                    }
                }
            }
        }

        if !updates.is_empty() {
            let mut sorted: Vec<_> = updates.iter().collect();
            sorted.sort_by_key(|(k, _)| k.as_str());
            for (name, new_ver) in &sorted {
                let old_ver = resolved
                    .get(*name)
                    .map(|p| p.version.as_str())
                    .unwrap_or("?");
                println!("  fixed: {name}:{old_ver} → {name}:{new_ver}");
            }
            println!(
                "  {} plugin(s) updated — rewriting '{}'",
                updates.len(),
                args.plugin_file.display()
            );

            let new_manifest = parser::rewrite_versions(&manifest_text, &updates);
            std::fs::write(&args.plugin_file, &new_manifest)
                .with_context(|| format!("rewriting '{}'", args.plugin_file.display()))?;

            requests =
                parser::parse_plugins_txt(&new_manifest).context("re-parsing plugins.txt")?;
        }

        if args.upgrade {
            // Keep upgrade floors for transitive plugins too; otherwise
            // transitive upgrades can be lost after manifest rewrite.
            let mut upgrade_floors = updates.clone();
            loop {
                let candidate =
                    resolver::resolve_with_min_versions(&requests, &uc, &upgrade_floors);
                let mut changed = false;
                for plugin in candidate.values() {
                    if let Some(best) = uc.highest_compatible_version(&plugin.name, &target) {
                        if JenkinsVersion::new(&best) > JenkinsVersion::new(&plugin.version) {
                            let should_update = match upgrade_floors.get(&plugin.name) {
                                Some(existing) => {
                                    JenkinsVersion::new(&best) > JenkinsVersion::new(existing)
                                }
                                None => true,
                            };
                            if should_update {
                                upgrade_floors.insert(plugin.name.clone(), best);
                                changed = true;
                            }
                        }
                    }
                }
                resolved = candidate;
                if !changed {
                    break;
                }
            }
        } else if !updates.is_empty() {
            // --fix only: re-resolve with rewritten direct pins.
            resolved = resolver::resolve(&requests, &uc);
        }
    }

    // ── Dependency cycle check ────────────────────────────────────────────────
    if let Some(cycle) = resolver::detect_cycle(&resolved, &uc, &bundled, &detached) {
        anyhow::bail!(
            "found cycle in plugin dependencies: {}\n       -> adjust pinned versions or run `jpm lock --fix` and re-resolve",
            cycle.join(" -> ")
        );
    }

    // ── Summary + lock file write ─────────────────────────────────────────────
    let direct = resolved.values().filter(|p| p.is_direct).count();
    let transitive = resolved.len() - direct;
    println!(
        "  resolved {} plugin(s) total ({} direct + {} transitive)",
        resolved.len(),
        direct,
        transitive
    );

    // Warn when the existing lock was generated from a different manifest.
    if let Ok(existing) = std::fs::read_to_string(&args.output) {
        if let Some(locked_hash) = lockfile::parse_manifest_hash(&existing) {
            let current_manifest = std::fs::read_to_string(&args.plugin_file)
                .unwrap_or_else(|_| manifest_text.clone());
            if locked_hash != lockfile::manifest_hash(&current_manifest) {
                eprintln!(
                    "  warning: '{}' is out of date — manifest changed since last lock",
                    args.output.display()
                );
            }
        }
    }

    let current_manifest =
        std::fs::read_to_string(&args.plugin_file).unwrap_or_else(|_| manifest_text.clone());
    let lock_content = lockfile::render(&resolved, &args.jenkins_version, &current_manifest);
    std::fs::write(&args.output, &lock_content)
        .with_context(|| format!("writing lock file '{}'", args.output.display()))?;

    println!("wrote '{}'", args.output.display());
    Ok(())
}

async fn run_install(client: &reqwest::Client, args: InstallArgs) -> Result<()> {
    installer::install(
        client,
        &installer::InstallOptions {
            lock_file: args.lock,
            plugin_dir: args.plugin_dir,
            skip_failed: args.skip_failed,
            dry_run: args.dry_run,
        },
    )
    .await
}

fn run_doctor(args: DoctorArgs) -> Result<()> {
    doctor::run(&doctor::DoctorOptions {
        lock_file: args.lock,
        plugin_dir: args.plugin_dir,
        strict: args.strict,
    })
}

async fn run_graph(client: &reqwest::Client, args: GraphArgs) -> Result<()> {
    graph::run(
        client,
        graph::GraphArgs {
            jenkins_version: args.jenkins_version,
            plugin_file: args.plugin_file,
            lock_file: args.lock_file,
            output: args.output,
            skip_bundled: args.skip_bundled,
            allow_cycle: args.allow_cycle,
        },
    )
    .await
}

async fn run_roots(client: &reqwest::Client, args: RootsArgs) -> Result<()> {
    roots::run(
        client,
        roots::RootsOptions {
            jenkins_version: args.jenkins_version,
            plugin_file: args.plugin_file,
            write: args.write,
            keep: args.keep,
        },
    )
    .await
}
