mod bundled;
mod lockfile;
mod parser;
mod resolver;
mod update_center;
mod version;

use anyhow::{Context, Result};
use clap::Parser;
use std::collections::HashMap;
use std::path::PathBuf;

/// Jenkins Plugin Manager — generate reproducible plugin lock files.
///
/// Reads a `plugins.txt` manifest and a Jenkins version, resolves all
/// transitive plugin dependencies against the Jenkins Update Center, and
/// writes a `plugins-lock.txt` with every plugin pinned to an exact version.
#[derive(Parser, Debug)]
#[command(name = "jpm", version, about, long_about = None)]
struct Cli {
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

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    println!(
        "jpm: resolving plugins for Jenkins {}",
        cli.jenkins_version
    );

    // Read and parse the plugin manifest.
    let manifest_text = std::fs::read_to_string(&cli.plugin_file)
        .with_context(|| format!("reading plugin manifest '{}'", cli.plugin_file.display()))?;
    let requests = parser::parse_plugins_txt(&manifest_text).context("parsing plugins.txt")?;

    println!("  {} plugin(s) in manifest", requests.len());

    // Build HTTP client.
    let client = reqwest::Client::builder()
        .user_agent(concat!("jpm/", env!("CARGO_PKG_VERSION")))
        .build()?;

    // Fetch Update Center data and bundled plugins concurrently.
    println!("fetching Update Center data and bundled plugin list...");

    let bundled_fut = async {
        if cli.skip_bundled {
            return Ok(HashMap::new());
        }
        match bundled::fetch_bundled_plugins(&client, &cli.jenkins_version).await {
            Ok(map) => Ok(map),
            Err(e) => {
                eprintln!("  warning: could not fetch bundled plugins: {e}");
                Ok(HashMap::new())
            }
        }
    };

    let (uc, bundled) = tokio::try_join!(
        update_center::UpdateCenter::fetch(&client, &cli.jenkins_version),
        bundled_fut,
    )
    .context("fetching remote data")?;

    if !bundled.is_empty() {
        println!("  {} plugin(s) bundled in Jenkins WAR", bundled.len());
    }

    // Resolve all transitive dependencies (synchronous BFS).
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

    // Warn if an existing lock file was generated from a different manifest.
    if let Ok(existing_lock) = std::fs::read_to_string(&cli.output) {
        if let Some(locked_hash) = lockfile::parse_manifest_hash(&existing_lock) {
            let current_hash = lockfile::manifest_hash(&manifest_text);
            if locked_hash != current_hash {
                eprintln!(
                    "  warning: '{}' is out of date — manifest changed since last lock",
                    cli.output.display()
                );
            }
        }
    }

    // Write the lock file.
    let lock_content = lockfile::render(&resolved, &cli.jenkins_version, &manifest_text);
    std::fs::write(&cli.output, &lock_content)
        .with_context(|| format!("writing lock file '{}'", cli.output.display()))?;

    println!("wrote '{}'", cli.output.display());

    Ok(())
}
