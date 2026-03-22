use std::collections::{HashMap, HashSet};
use std::io::Read;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};

use crate::lockfile;

pub struct DoctorOptions {
    pub lock_file: PathBuf,
    pub plugin_dir: PathBuf,
    pub strict: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Severity {
    Warning,
    Error,
}

impl Severity {
    fn as_str(self) -> &'static str {
        match self {
            Severity::Warning => "warning",
            Severity::Error => "error",
        }
    }
}

#[derive(Debug, Clone)]
struct Finding {
    code: &'static str,
    severity: Severity,
    plugin: String,
    detail: String,
    remediation: &'static str,
}

#[derive(Debug, Clone)]
struct PluginFile {
    path: PathBuf,
    plugin_name: String,
    version: String,
    ext: String,
    disabled_marker: bool,
}

pub fn run(opts: &DoctorOptions) -> Result<()> {
    let lock_content = std::fs::read_to_string(&opts.lock_file)
        .with_context(|| format!("reading lock file '{}'", opts.lock_file.display()))?;
    let lock_entries = lockfile::parse(&lock_content);

    let plugin_files = scan_plugin_files(&opts.plugin_dir)
        .with_context(|| format!("scanning '{}'", opts.plugin_dir.display()))?;

    let findings = collect_findings(&lock_entries, &plugin_files);

    println!(
        "jpm doctor: {} lock plugin(s), {} archive(s) scanned",
        lock_entries.len(),
        plugin_files.len()
    );

    if findings.is_empty() {
        println!("  no findings");
        return Ok(());
    }

    for f in &findings {
        println!(
            "  [{}] {} plugin={} {}",
            f.severity.as_str(),
            f.code,
            f.plugin,
            f.detail
        );
        println!("       remediation: {}", f.remediation);
    }

    let errors = findings
        .iter()
        .filter(|f| matches!(f.severity, Severity::Error))
        .count();
    let warnings = findings.len() - errors;

    println!();
    println!(
        "summary: {} finding(s) ({} error, {} warning)",
        findings.len(),
        errors,
        warnings
    );

    if opts.strict {
        bail!(
            "doctor strict mode: found {} issue(s), please remediate before proceeding",
            findings.len()
        );
    }

    Ok(())
}

fn collect_findings(
    lock_entries: &HashMap<String, (String, Option<String>)>,
    plugin_files: &[PluginFile],
) -> Vec<Finding> {
    let mut findings = Vec::new();

    let mut by_name: HashMap<String, Vec<&PluginFile>> = HashMap::new();
    for pf in plugin_files {
        by_name.entry(pf.plugin_name.clone()).or_default().push(pf);
    }

    // JPM001 duplicate_suffix
    for (name, files) in &by_name {
        let exts: HashSet<&str> = files.iter().map(|f| f.ext.as_str()).collect();
        if exts.contains("hpi") && exts.contains("jpi") {
            findings.push(Finding {
                code: "JPM001",
                severity: Severity::Error,
                plugin: name.clone(),
                detail: "both .hpi and .jpi exist".to_string(),
                remediation:
                    "keep the canonical .jpi archive and remove duplicate suffix artifacts",
            });
        }
    }

    // JPM002 version_drift
    for (name, (lock_version, _)) in lock_entries {
        if let Some(files) = by_name.get(name) {
            let preferred = files
                .iter()
                .find(|f| f.ext == "jpi")
                .copied()
                .or_else(|| files.first().copied());
            if let Some(installed) = preferred {
                if installed.version != *lock_version {
                    findings.push(Finding {
                        code: "JPM002",
                        severity: Severity::Error,
                        plugin: name.clone(),
                        detail: format!(
                            "on-disk version {} differs from lock {}",
                            installed.version, lock_version
                        ),
                        remediation: "reconcile lock and disk state (reinstall from lock, or regenerate lock from intended manifest)",
                    });
                }
            }
        }
    }

    // JPM003 unmanaged_plugin
    for name in by_name.keys() {
        if !lock_entries.contains_key(name) {
            findings.push(Finding {
                code: "JPM003",
                severity: Severity::Warning,
                plugin: name.clone(),
                detail: "plugin exists on disk but is not present in lock file".to_string(),
                remediation: "add plugin to plugins.txt and regenerate lock, or remove it from plugin directory",
            });
        }
    }

    // JPM004 disabled_marker
    for pf in plugin_files {
        if pf.disabled_marker {
            findings.push(Finding {
                code: "JPM004",
                severity: Severity::Warning,
                plugin: pf.plugin_name.clone(),
                detail: format!("disabled marker exists for {}", file_name(&pf.path)),
                remediation: "remove .disabled if plugin should be active, or document this intentional exception",
            });
        }
    }

    findings.sort_by(|a, b| a.code.cmp(b.code).then(a.plugin.cmp(&b.plugin)));
    findings
}

fn file_name(path: &Path) -> String {
    path.file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("?")
        .to_string()
}

fn scan_plugin_files(dir: &Path) -> Result<Vec<PluginFile>> {
    let mut out = Vec::new();

    let read_dir = match std::fs::read_dir(dir) {
        Ok(rd) => rd,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(out),
        Err(e) => return Err(e).with_context(|| format!("reading '{}'", dir.display())),
    };

    for entry in read_dir {
        let path = entry?.path();
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("")
            .to_string();
        if ext != "hpi" && ext != "jpi" {
            continue;
        }

        let (plugin_name, version) = match read_plugin_manifest(&path) {
            Ok(v) => v,
            Err(e) => {
                eprintln!("  warning: skipping '{}': {e}", path.display());
                continue;
            }
        };

        let disabled_marker = path
            .file_name()
            .and_then(|n| n.to_str())
            .map(|n| path.with_file_name(format!("{n}.disabled")).exists())
            .unwrap_or(false);

        out.push(PluginFile {
            path,
            plugin_name,
            version,
            ext,
            disabled_marker,
        });
    }

    Ok(out)
}

fn read_plugin_manifest(path: &Path) -> Result<(String, String)> {
    let file =
        std::fs::File::open(path).with_context(|| format!("opening '{}'", path.display()))?;
    let mut archive = zip::ZipArchive::new(file)
        .with_context(|| format!("'{}' is not a valid ZIP/HPI", path.display()))?;

    let mut entry = archive
        .by_name("META-INF/MANIFEST.MF")
        .with_context(|| format!("no META-INF/MANIFEST.MF in '{}'", path.display()))?;

    let mut content = String::new();
    entry.read_to_string(&mut content)?;

    let headers = parse_manifest_headers(&content);
    let plugin_name = headers
        .get("Short-Name")
        .cloned()
        .or_else(|| path.file_stem().and_then(|s| s.to_str()).map(str::to_owned))
        .with_context(|| format!("cannot determine plugin name from '{}'", path.display()))?;
    let version = headers
        .get("Plugin-Version")
        .cloned()
        .with_context(|| format!("no Plugin-Version in '{}'", path.display()))?;

    Ok((plugin_name, version))
}

fn parse_manifest_headers(content: &str) -> HashMap<String, String> {
    let mut map: HashMap<String, String> = HashMap::new();
    let mut current_key: Option<String> = None;
    let mut current_val = String::new();

    for line in content.lines() {
        if let Some(rest) = line.strip_prefix(' ') {
            current_val.push_str(rest);
        } else {
            if let Some(key) = current_key.take() {
                map.insert(key, current_val.trim_end().to_string());
                current_val.clear();
            }
            if let Some((k, v)) = line.split_once(':') {
                current_key = Some(k.to_string());
                current_val = v.trim_start().to_string();
            }
        }
    }
    if let Some(key) = current_key {
        map.insert(key, current_val.trim_end().to_string());
    }

    map
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use zip::write::SimpleFileOptions;

    fn write_plugin(dir: &Path, file_name: &str, short_name: &str, version: &str) {
        let path = dir.join(file_name);
        let file = std::fs::File::create(&path).expect("create plugin archive");
        let mut zip = zip::ZipWriter::new(file);
        let opts = SimpleFileOptions::default();
        zip.start_file("META-INF/MANIFEST.MF", opts)
            .expect("start manifest");
        write!(
            zip,
            "Manifest-Version: 1.0\r\nShort-Name: {short_name}\r\nPlugin-Version: {version}\r\n"
        )
        .expect("write manifest");
        zip.finish().expect("finish zip");
    }

    #[test]
    fn doctor_collects_expected_findings() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let dir = tmp.path();

        write_plugin(dir, "git.jpi", "git", "1.0");
        write_plugin(dir, "git.hpi", "git", "0.9");
        write_plugin(dir, "extra.jpi", "extra", "3.0");
        write_plugin(dir, "cred.jpi", "cred", "2.0");
        std::fs::write(dir.join("cred.jpi.disabled"), "").expect("write disabled marker");

        let lock_entries = HashMap::from([
            ("git".to_string(), ("1.1".to_string(), None)),
            ("cred".to_string(), ("2.0".to_string(), None)),
        ]);
        let plugin_files = scan_plugin_files(dir).expect("scan plugin files");

        let findings = collect_findings(&lock_entries, &plugin_files);
        let codes: Vec<&str> = findings.iter().map(|f| f.code).collect();

        assert!(codes.contains(&"JPM001"));
        assert!(codes.contains(&"JPM002"));
        assert!(codes.contains(&"JPM003"));
        assert!(codes.contains(&"JPM004"));
    }
}
