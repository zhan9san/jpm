mod bundled;
mod installer;
mod lockfile;
mod parser;
mod resolver;
mod update_center;
mod version;

use anyhow::{Context, Result};
use clap::{Args, Parser, Subcommand};
use std::collections::HashMap;
use std::path::PathBuf;

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
    #[arg(
        short = 'd',
        long,
        value_name = "DIR",
        default_value = "plugins"
    )]
    plugin_dir: PathBuf,

    /// Warn on individual download failures instead of aborting.
    #[arg(long)]
    skip_failed: bool,

    /// Print what would be installed without downloading anything.
    #[arg(long)]
    dry_run: bool,
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
    }
}

async fn run_lock(client: &reqwest::Client, args: LockArgs) -> Result<()> {
    println!("jpm lock: resolving plugins for Jenkins {}", args.jenkins_version);

    let manifest_text = std::fs::read_to_string(&args.plugin_file)
        .with_context(|| format!("reading manifest '{}'", args.plugin_file.display()))?;
    let requests =
        parser::parse_plugins_txt(&manifest_text).context("parsing plugins.txt")?;

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

    let (uc, bundled) = tokio::try_join!(
        update_center::UpdateCenter::fetch(client, &args.jenkins_version),
        bundled_fut,
    )
    .context("fetching remote data")?;

    if !bundled.is_empty() {
        println!("  {} plugin(s) bundled in Jenkins WAR", bundled.len());
    }

    println!("resolving dependencies...");
    let resolved = resolver::resolve(&requests, &uc, &bundled);

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
            if locked_hash != lockfile::manifest_hash(&manifest_text) {
                eprintln!(
                    "  warning: '{}' is out of date — manifest changed since last lock",
                    args.output.display()
                );
            }
        }
    }

    let lock_content = lockfile::render(&resolved, &args.jenkins_version, &manifest_text);
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
