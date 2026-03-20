use anyhow::{bail, Result};

/// The version specifier for a requested plugin.
#[derive(Debug, Clone, PartialEq)]
pub enum VersionSpec {
    /// Resolve to latest in the stable update center.
    Latest,
    /// Resolve to latest in the experimental update center.
    Experimental,
    /// Use an exact version string.
    Pinned(String),
}

impl std::fmt::Display for VersionSpec {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            VersionSpec::Latest => write!(f, "latest"),
            VersionSpec::Experimental => write!(f, "experimental"),
            VersionSpec::Pinned(v) => write!(f, "{v}"),
        }
    }
}

/// A single entry from the user-supplied `plugins.txt` manifest.
#[derive(Debug, Clone)]
pub struct PluginRequest {
    /// The plugin artifact ID (e.g. `git`, `credentials`).
    pub name: String,
    /// Version specifier from the manifest.
    pub version: VersionSpec,
    /// Optional download URL override (reserved for future use — dependency
    /// resolution is skipped for URL-sourced plugins).
    #[allow(dead_code)]
    pub url: Option<String>,
}

/// Parse the contents of a `plugins.txt` file into a list of plugin requests.
///
/// Format per line (after stripping inline `#` comments and blank lines):
/// ```
/// artifactId[:version[:url]]
/// ```
///
/// Special version values:
/// - omitted / `latest`  → `VersionSpec::Latest`
/// - `experimental`      → `VersionSpec::Experimental`
/// - anything else       → `VersionSpec::Pinned`
pub fn parse_plugins_txt(content: &str) -> Result<Vec<PluginRequest>> {
    let mut plugins = Vec::new();

    for (lineno, raw) in content.lines().enumerate() {
        let line = strip_comment(raw).trim().to_string();
        if line.is_empty() {
            continue;
        }

        let parts: Vec<&str> = line.splitn(3, ':').collect();
        let name = parts[0].trim();
        if name.is_empty() {
            bail!("line {}: plugin name is empty", lineno + 1);
        }

        let version = match parts.get(1).map(|s| s.trim()) {
            None | Some("") | Some("latest") => VersionSpec::Latest,
            Some("experimental") => VersionSpec::Experimental,
            Some(v) => VersionSpec::Pinned(v.to_string()),
        };

        let url = parts
            .get(2)
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string());

        plugins.push(PluginRequest {
            name: name.to_string(),
            version,
            url,
        });
    }

    Ok(plugins)
}

fn strip_comment(s: &str) -> &str {
    match s.find('#') {
        Some(i) => &s[..i],
        None => s,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_various_formats() {
        let input = r#"
# full line comment
git
kubernetes:4285.v50ed5f624918
junit:experimental
blueocean:latest
script-security::http://example.com/plugin.hpi
credentials:1415.v831096eb_5534:http://override.example.com/cred.hpi
docker # inline comment
"#;
        let plugins = parse_plugins_txt(input).unwrap();
        assert_eq!(plugins.len(), 7);

        assert_eq!(plugins[0].name, "git");
        assert_eq!(plugins[0].version, VersionSpec::Latest);

        assert_eq!(plugins[1].name, "kubernetes");
        assert!(matches!(&plugins[1].version, VersionSpec::Pinned(v) if v == "4285.v50ed5f624918"));

        assert_eq!(plugins[2].version, VersionSpec::Experimental);

        assert_eq!(plugins[3].version, VersionSpec::Latest);

        assert_eq!(plugins[4].name, "script-security");
        assert_eq!(plugins[4].version, VersionSpec::Latest);
        assert_eq!(
            plugins[4].url.as_deref(),
            Some("http://example.com/plugin.hpi")
        );

        assert_eq!(plugins[5].name, "credentials");
        assert!(
            matches!(&plugins[5].version, VersionSpec::Pinned(v) if v == "1415.v831096eb_5534")
        );
        assert_eq!(
            plugins[5].url.as_deref(),
            Some("http://override.example.com/cred.hpi")
        );

        assert_eq!(plugins[6].name, "docker");
        assert_eq!(plugins[6].version, VersionSpec::Latest);
    }
}
