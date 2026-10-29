//! Plugin security warnings published in the Update Center `warnings[]`.
//!
//! Each `type: plugin` warning names a plugin and lists `versions[].pattern`,
//! regular expressions that must match the *whole* version string (Jenkins
//! core uses `Pattern.matcher(version).matches()`).

use std::collections::HashMap;

use regex::Regex;
use serde_json::Value;

use crate::resolver::ResolvedPlugin;
use crate::update_center::UpdateCenter;
use crate::version::JenkinsVersion;

/// One security advisory that applies to some versions of a plugin.
#[derive(Debug)]
pub struct Advisory {
    pub id: String,
    pub message: String,
    pub url: String,
    patterns: Vec<VersionPattern>,
}

impl Advisory {
    fn affects(&self, version: &str) -> bool {
        self.patterns.iter().any(|p| p.matches(version))
    }
}

/// A Java `versions[].pattern`, matched against the whole version string.
///
/// The `regex` crate has no look-around, but some advisories start with a
/// negative lookahead to carve out fixed versions, e.g.
/// `(?!2[.]61[.].*)(1|2[.]([0-9]|[1-5][0-9]|6[01]))(|[.-].*)`. Anchored at
/// the start, `(?!X)Y` matches exactly when `Y` matches the whole version and
/// `X` does not match a prefix of it, so such patterns are split in two.
#[derive(Debug)]
struct VersionPattern {
    include: Regex,
    exclude: Option<Regex>,
}

impl VersionPattern {
    fn parse(pattern: &str) -> Option<Self> {
        let (exclude, include) = match split_leading_negative_lookahead(pattern) {
            Some((excluded, rest)) => (Some(Regex::new(&format!("^(?:{excluded})")).ok()?), rest),
            None => (None, pattern),
        };
        Some(VersionPattern {
            include: Regex::new(&format!("^(?:{include})$")).ok()?,
            exclude,
        })
    }

    fn matches(&self, version: &str) -> bool {
        self.include.is_match(version)
            && !self.exclude.as_ref().is_some_and(|x| x.is_match(version))
    }
}

/// Split `(?!X)Y` into `(X, Y)`. Returns `None` when the pattern does not
/// start with a negative lookahead (or its parentheses do not balance).
fn split_leading_negative_lookahead(pattern: &str) -> Option<(&str, &str)> {
    let inner = pattern.strip_prefix("(?!")?;
    let mut depth = 1;
    let mut in_class = false;
    let mut escaped = false;
    for (i, c) in inner.char_indices() {
        match c {
            _ if escaped => escaped = false,
            '\\' => escaped = true,
            '[' if !in_class => in_class = true,
            ']' if in_class => in_class = false,
            '(' if !in_class => depth += 1,
            ')' if !in_class => {
                depth -= 1;
                if depth == 0 {
                    return Some((&inner[..i], &inner[i + 1..]));
                }
            }
            _ => {}
        }
    }
    None
}

/// Plugin advisories indexed by plugin name.
#[derive(Debug, Default)]
pub struct SecurityWarnings {
    by_plugin: HashMap<String, Vec<Advisory>>,
    /// Patterns that are not valid regular expressions for the `regex` crate.
    pub unparsed_patterns: usize,
}

/// A locked plugin version affected by one or more advisories.
#[derive(Debug)]
pub struct Finding<'a> {
    pub name: String,
    pub version: String,
    pub advisories: Vec<&'a Advisory>,
    /// Highest version compatible with the target Jenkins that no advisory
    /// affects, when it is newer than the locked one.
    pub fixed_in: Option<String>,
}

impl SecurityWarnings {
    /// Parse the `warnings` array of an Update Center JSON document.
    pub fn from_update_center(uc_json: &Value) -> Self {
        let mut out = SecurityWarnings::default();
        let Some(warnings) = uc_json["warnings"].as_array() else {
            return out;
        };
        for w in warnings {
            if w["type"].as_str() != Some("plugin") {
                continue;
            }
            let Some(name) = w["name"].as_str() else {
                continue;
            };
            let mut patterns = Vec::new();
            for v in w["versions"].as_array().into_iter().flatten() {
                let Some(pattern) = v["pattern"].as_str() else {
                    continue;
                };
                match VersionPattern::parse(pattern) {
                    Some(p) => patterns.push(p),
                    None => out.unparsed_patterns += 1,
                }
            }
            let text = |key: &str| w[key].as_str().unwrap_or_default().to_string();
            out.by_plugin
                .entry(name.to_string())
                .or_default()
                .push(Advisory {
                    id: text("id"),
                    message: text("message"),
                    url: text("url"),
                    patterns,
                });
        }
        out
    }

    /// Advisories that affect `name` at `version`.
    pub fn affecting(&self, name: &str, version: &str) -> Vec<&Advisory> {
        self.by_plugin
            .get(name)
            .map(|list| list.iter().filter(|a| a.affects(version)).collect())
            .unwrap_or_default()
    }

    /// Check every resolved plugin and return the affected ones, sorted by
    /// name, each with the newest unaffected version that still supports
    /// `jenkins_version`, if there is one.
    pub fn check<'a>(
        &'a self,
        resolved: &HashMap<String, ResolvedPlugin>,
        uc: &UpdateCenter,
        jenkins_version: &str,
    ) -> Vec<Finding<'a>> {
        let target = JenkinsVersion::new(jenkins_version);
        let mut findings: Vec<Finding> = resolved
            .values()
            .filter_map(|p| {
                let advisories = self.affecting(&p.name, &p.version);
                if advisories.is_empty() {
                    return None;
                }
                Some(Finding {
                    name: p.name.clone(),
                    version: p.version.clone(),
                    advisories,
                    fixed_in: self.newest_unaffected(uc, &p.name, &p.version, &target),
                })
            })
            .collect();
        findings.sort_by(|a, b| a.name.cmp(&b.name));
        findings
    }

    fn newest_unaffected(
        &self,
        uc: &UpdateCenter,
        name: &str,
        current: &str,
        target: &JenkinsVersion,
    ) -> Option<String> {
        let current = JenkinsVersion::new(current);
        uc.plugin_versions["plugins"][name]
            .as_object()?
            .keys()
            .filter(|v| JenkinsVersion::new(v.as_str()) > current)
            .filter(|v| {
                let rc = uc.required_core_for(name, v).unwrap_or("1.0");
                JenkinsVersion::new(rc) <= *target
            })
            .filter(|v| self.affecting(name, v).is_empty())
            .max_by(|a, b| JenkinsVersion::new(a.as_str()).cmp(&JenkinsVersion::new(b.as_str())))
            .cloned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn uc(plugin_versions: Value) -> UpdateCenter {
        UpdateCenter {
            stable: json!({}),
            experimental: json!({}),
            plugin_versions,
        }
    }

    fn resolved(name: &str, version: &str) -> HashMap<String, ResolvedPlugin> {
        HashMap::from([(
            name.to_string(),
            ResolvedPlugin {
                name: name.to_string(),
                version: version.to_string(),
                sha256: None,
                is_direct: true,
            },
        )])
    }

    fn warnings() -> SecurityWarnings {
        SecurityWarnings::from_update_center(&json!({
            "warnings": [
                {
                    "type": "plugin",
                    "id": "SECURITY-1",
                    "name": "script-security",
                    "message": "Sandbox bypass",
                    "url": "https://www.jenkins.io/security/advisory/1/",
                    "versions": [ { "lastVersion": "1.2", "pattern": "1[.][012](|[.-].*)" } ]
                },
                {
                    "type": "core",
                    "id": "SECURITY-2",
                    "name": "core",
                    "message": "Core issue",
                    "url": "https://example.invalid/",
                    "versions": [ { "pattern": ".*" } ]
                },
                {
                    "type": "plugin",
                    "id": "SECURITY-3",
                    "name": "broken",
                    "message": "Bad pattern",
                    "url": "",
                    "versions": [ { "pattern": "(?<=x)1" } ]
                }
            ]
        }))
    }

    #[test]
    fn patterns_must_match_the_whole_version() {
        let w = warnings();
        assert_eq!(w.affecting("script-security", "1.2").len(), 1);
        assert_eq!(w.affecting("script-security", "1.1-beta").len(), 1);
        assert!(w.affecting("script-security", "1.3").is_empty());
        assert!(w.affecting("script-security", "11.2").is_empty());
        assert!(w.affecting("git", "1.0").is_empty());
    }

    #[test]
    fn leading_negative_lookahead_excludes_fixed_versions() {
        // Real pattern from SECURITY-1266 (workflow-cps).
        let p = VersionPattern::parse("(?!2[.]61[.].*)(1|2[.]([0-9]|[1-5][0-9]|6[01]))(|[.-].*)")
            .expect("pattern parses");
        assert!(p.matches("2.60"));
        assert!(p.matches("2.61"));
        assert!(!p.matches("2.61.1"), "backported fix is excluded");
        assert!(!p.matches("2.62"));

        // Nested groups inside the lookahead (SECURITY-1658, script-security).
        let p = VersionPattern::parse(
            "(?!(1[.](57[.][4-9]|66[.][1-9])))(1[.]([0-9]|[1-5][0-9]|6[0-7]))(|[-.].*)",
        )
        .expect("pattern parses");
        assert!(p.matches("1.57"));
        assert!(!p.matches("1.57.4"));
        assert!(!p.matches("1.66.2"));
        assert!(p.matches("1.66"));
        assert!(!p.matches("1.68"));
    }

    #[test]
    fn split_negative_lookahead_handles_classes_and_escapes() {
        assert_eq!(
            split_leading_negative_lookahead("(?![)]\\))a"),
            Some(("[)]\\)", "a"))
        );
        assert_eq!(split_leading_negative_lookahead("1[.]2"), None);
        assert_eq!(split_leading_negative_lookahead("(?!(1)"), None);
    }

    #[test]
    fn core_warnings_are_ignored_and_bad_patterns_counted() {
        let w = warnings();
        assert!(w.affecting("core", "2.452.4").is_empty());
        assert_eq!(w.unparsed_patterns, 1);
    }

    #[test]
    fn check_suggests_newest_unaffected_compatible_version() {
        let uc = uc(json!({
            "plugins": {
                "script-security": {
                    "1.2": { "requiredCore": "2.400" },
                    "1.3": { "requiredCore": "2.400" },
                    "1.4": { "requiredCore": "2.440" },
                    "1.5": { "requiredCore": "2.500" }
                }
            }
        }));
        let w = warnings();
        let findings = w.check(&resolved("script-security", "1.2"), &uc, "2.452.4");
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].advisories[0].id, "SECURITY-1");
        assert_eq!(findings[0].fixed_in.as_deref(), Some("1.4"));
    }

    #[test]
    fn check_reports_no_fix_when_none_is_compatible() {
        let uc = uc(json!({
            "plugins": {
                "script-security": {
                    "1.2": { "requiredCore": "2.400" },
                    "1.3": { "requiredCore": "2.500" }
                }
            }
        }));
        let w = warnings();
        let findings = w.check(&resolved("script-security", "1.2"), &uc, "2.452.4");
        assert_eq!(findings[0].fixed_in, None);
        assert!(w
            .check(&resolved("script-security", "1.3"), &uc, "2.452.4")
            .is_empty());
    }
}
