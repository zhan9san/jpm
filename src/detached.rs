use anyhow::{Context, Result};
use std::collections::HashMap;

/// Detached plugin metadata extracted from Jenkins core resources.
pub struct DetachedMetadata {
    /// plugin -> last core version that still contained the functionality.
    pub split_plugins: HashMap<String, String>,
    /// Edges to drop when checking cycles (ClassicPluginStrategy.BREAK_CYCLES).
    pub break_cycles: Vec<(String, String)>,
}

pub async fn fetch(client: &reqwest::Client, jenkins_version: &str) -> Result<DetachedMetadata> {
    let cache_dir = if let Ok(dir) = std::env::var("JPM_CACHE_DIR") {
        std::path::PathBuf::from(dir)
    } else {
        dirs::cache_dir()
            .unwrap_or_else(|| std::path::PathBuf::from(".cache"))
            .join("jpm")
    };
    tokio::fs::create_dir_all(&cache_dir).await?;

    let split_plugins_cache = cache_dir.join(format!("split-plugins-{jenkins_version}.txt"));
    let split_cycles_cache = cache_dir.join(format!("split-plugin-cycles-{jenkins_version}.txt"));

    let base = std::env::var("JPM_JENKINS_GH_BASE").unwrap_or_else(|_| {
        "https://raw.githubusercontent.com/jenkinsci/jenkins/jenkins-".to_string()
    });
    let split_plugins_url =
        format!("{base}{jenkins_version}/core/src/main/resources/jenkins/split-plugins.txt");
    let split_cycles_url =
        format!("{base}{jenkins_version}/core/src/main/resources/jenkins/split-plugin-cycles.txt");

    let (split_plugins_text, split_cycles_text) = tokio::try_join!(
        fetch_text_with_cache(client, &split_plugins_url, &split_plugins_cache),
        fetch_text_with_cache(client, &split_cycles_url, &split_cycles_cache),
    )?;

    Ok(DetachedMetadata {
        split_plugins: parse_split_plugins(&split_plugins_text),
        break_cycles: parse_break_cycles(&split_cycles_text),
    })
}

fn parse_split_plugins(text: &str) -> HashMap<String, String> {
    let mut out = HashMap::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let cols: Vec<&str> = line.split_whitespace().collect();
        if cols.len() >= 2 {
            out.insert(cols[0].to_string(), cols[1].to_string());
        }
    }
    out
}

fn parse_break_cycles(text: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let cols: Vec<&str> = line.split_whitespace().collect();
        if cols.len() >= 2 {
            out.push((cols[0].to_string(), cols[1].to_string()));
        }
    }
    out
}

async fn fetch_text_with_cache(
    client: &reqwest::Client,
    url: &str,
    cache_path: &std::path::Path,
) -> Result<String> {
    if let Ok(cached) = tokio::fs::read_to_string(cache_path).await {
        return Ok(cached);
    }

    eprintln!("  fetching {url}");
    let text = client
        .get(url)
        .send()
        .await
        .with_context(|| format!("GET {url}"))?
        .error_for_status()
        .with_context(|| format!("non-2xx from {url}"))?
        .text()
        .await?;

    tokio::fs::write(cache_path, &text)
        .await
        .with_context(|| format!("writing cache '{}'", cache_path.display()))?;
    Ok(text)
}
