//! Read the Jenkins version, bundled plugins and split-plugin metadata from a
//! local `jenkins.war`, so `lock`, `graph` and `roots` need no GitHub access.

use std::collections::HashMap;
use std::io::{Cursor, Read, Seek};
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};

use crate::detached::{self, DetachedMetadata};
use crate::manifest;
use crate::version::JenkinsVersion;

/// What jpm needs from a Jenkins WAR.
pub struct WarInfo {
    /// `Jenkins-Version` from the WAR's `META-INF/MANIFEST.MF`.
    pub jenkins_version: String,
    /// Plugins shipped in `WEB-INF/detached-plugins/` (and the legacy
    /// `WEB-INF/plugins/`): `short-name → version`.
    pub bundled: HashMap<String, String>,
    /// `jenkins/split-plugins.txt` and `split-plugin-cycles.txt` from
    /// `WEB-INF/lib/jenkins-core-*.jar`.
    pub detached: DetachedMetadata,
}

/// Read `path` on a blocking thread (the WAR is a large ZIP).
pub async fn read(path: &Path) -> Result<WarInfo> {
    let owned: PathBuf = path.to_path_buf();
    tokio::task::spawn_blocking(move || read_blocking(&owned))
        .await
        .context("WAR reading task panicked")?
        .with_context(|| format!("reading Jenkins WAR '{}'", path.display()))
}

fn read_blocking(path: &Path) -> Result<WarInfo> {
    let file =
        std::fs::File::open(path).with_context(|| format!("opening '{}'", path.display()))?;
    read_from(file, &path.display().to_string())
}

fn read_from<R: Read + Seek>(reader: R, label: &str) -> Result<WarInfo> {
    let mut war =
        zip::ZipArchive::new(reader).with_context(|| format!("'{label}' is not a valid WAR"))?;

    let manifest = read_entry_text(&mut war, "META-INF/MANIFEST.MF")?
        .with_context(|| format!("no META-INF/MANIFEST.MF in '{label}'"))?;
    let jenkins_version = manifest::parse_headers(&manifest)
        .remove("Jenkins-Version")
        .filter(|v| !v.is_empty())
        .with_context(|| format!("no Jenkins-Version in the manifest of '{label}'"))?;

    let names: Vec<String> = war.file_names().map(str::to_owned).collect();

    let mut bundled = HashMap::new();
    for name in names.iter().filter(|n| is_bundled_plugin(n)) {
        let bytes = read_entry_bytes(&mut war, name)?;
        let headers = manifest::read_headers(Cursor::new(bytes), name)?;
        let short_name = headers
            .get("Short-Name")
            .cloned()
            .or_else(|| file_stem(name))
            .with_context(|| format!("cannot determine plugin name of '{name}'"))?;
        let version = headers
            .get("Plugin-Version")
            .cloned()
            .with_context(|| format!("no Plugin-Version in '{name}'"))?;
        bundled.insert(short_name, version);
    }

    let core_jar = names
        .iter()
        .find(|n| {
            n.strip_prefix("WEB-INF/lib/jenkins-core-")
                .is_some_and(|rest| rest.ends_with(".jar"))
        })
        .with_context(|| format!("no WEB-INF/lib/jenkins-core-*.jar in '{label}'"))?;
    let mut core = zip::ZipArchive::new(Cursor::new(read_entry_bytes(&mut war, core_jar)?))
        .with_context(|| format!("'{core_jar}' is not a valid JAR"))?;
    let Some(split_plugins) = read_entry_text(&mut core, "jenkins/split-plugins.txt")? else {
        bail!("no jenkins/split-plugins.txt in '{core_jar}'");
    };
    // Older cores have no cycle-break list.
    let split_cycles =
        read_entry_text(&mut core, "jenkins/split-plugin-cycles.txt")?.unwrap_or_default();

    Ok(WarInfo {
        jenkins_version,
        bundled,
        detached: DetachedMetadata {
            split_plugins: detached::parse_split_plugins(&split_plugins),
            break_cycles: detached::parse_break_cycles(&split_cycles),
        },
    })
}

/// Pick the Jenkins version from `-j` and/or `--war`; when both are given
/// they must agree.
pub fn jenkins_version(flag: Option<&str>, war: Option<&WarInfo>) -> Result<String> {
    match (flag, war) {
        (Some(v), Some(w)) if JenkinsVersion::new(v) != JenkinsVersion::new(&w.jenkins_version) => {
            bail!(
                "--jenkins-version {v} does not match the WAR, which is Jenkins {}",
                w.jenkins_version
            )
        }
        (Some(v), _) => Ok(v.to_string()),
        (None, Some(w)) => Ok(w.jenkins_version.clone()),
        (None, None) => bail!("either --jenkins-version (or JENKINS_VERSION) or --war is required"),
    }
}

fn is_bundled_plugin(name: &str) -> bool {
    let Some(file) = name
        .strip_prefix("WEB-INF/detached-plugins/")
        .or_else(|| name.strip_prefix("WEB-INF/plugins/"))
    else {
        return false;
    };
    !file.contains('/') && (file.ends_with(".hpi") || file.ends_with(".jpi"))
}

fn file_stem(name: &str) -> Option<String> {
    Path::new(name)
        .file_stem()
        .and_then(|s| s.to_str())
        .map(str::to_owned)
}

fn read_entry_bytes<R: Read + Seek>(zip: &mut zip::ZipArchive<R>, name: &str) -> Result<Vec<u8>> {
    let mut entry = zip
        .by_name(name)
        .with_context(|| format!("opening '{name}'"))?;
    let mut buf = Vec::with_capacity(usize::try_from(entry.size()).unwrap_or(0));
    entry
        .read_to_end(&mut buf)
        .with_context(|| format!("reading '{name}'"))?;
    Ok(buf)
}

/// Read a text entry, or `None` when the archive has no such entry.
fn read_entry_text<R: Read + Seek>(
    zip: &mut zip::ZipArchive<R>,
    name: &str,
) -> Result<Option<String>> {
    let mut entry = match zip.by_name(name) {
        Ok(entry) => entry,
        Err(zip::result::ZipError::FileNotFound) => return Ok(None),
        Err(e) => return Err(e).with_context(|| format!("opening '{name}'")),
    };
    let mut text = String::new();
    entry
        .read_to_string(&mut text)
        .with_context(|| format!("reading '{name}'"))?;
    Ok(Some(text))
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::io::Write;
    use zip::write::SimpleFileOptions;

    fn zip_bytes(entries: &[(&str, Vec<u8>)]) -> Vec<u8> {
        let mut buf = Cursor::new(Vec::new());
        {
            let mut zip = zip::ZipWriter::new(&mut buf);
            for (name, data) in entries {
                zip.start_file(*name, SimpleFileOptions::default()).unwrap();
                zip.write_all(data).unwrap();
            }
            zip.finish().unwrap();
        }
        buf.into_inner()
    }

    fn plugin(short_name: &str, version: &str) -> Vec<u8> {
        zip_bytes(&[(
            "META-INF/MANIFEST.MF",
            format!("Manifest-Version: 1.0\r\nShort-Name: {short_name}\r\nPlugin-Version: {version}\r\n")
                .into_bytes(),
        )])
    }

    /// A minimal WAR with the given core version.
    pub(crate) fn fake_war(version: &str) -> Vec<u8> {
        let core = zip_bytes(&[
            (
                "jenkins/split-plugins.txt",
                b"# comment\nsshd 2.281 3.236.ved5e1b_cb_50b_2\n".to_vec(),
            ),
            (
                "jenkins/split-plugin-cycles.txt",
                b"javax-activation-api sshd\n".to_vec(),
            ),
        ]);
        zip_bytes(&[
            (
                "META-INF/MANIFEST.MF",
                format!("Manifest-Version: 1.0\r\nJenkins-Version: {version}\r\n").into_bytes(),
            ),
            (
                "WEB-INF/detached-plugins/mailer.hpi",
                plugin("mailer", "472.vf7c289a_4b_c36"),
            ),
            ("WEB-INF/plugins/legacy.hpi", plugin("legacy-name", "1.0")),
            (
                "WEB-INF/detached-plugins/readme.txt",
                b"not a plugin".to_vec(),
            ),
            (&format!("WEB-INF/lib/jenkins-core-{version}.jar"), core),
        ])
    }

    #[test]
    fn reads_version_bundled_plugins_and_split_metadata() {
        let info = read_from(Cursor::new(fake_war("2.452.4")), "jenkins.war").unwrap();
        assert_eq!(info.jenkins_version, "2.452.4");
        assert_eq!(info.bundled.len(), 2);
        assert_eq!(info.bundled["mailer"], "472.vf7c289a_4b_c36");
        assert_eq!(info.bundled["legacy-name"], "1.0");
        assert_eq!(info.detached.split_plugins["sshd"], "2.281");
        assert_eq!(
            info.detached.break_cycles,
            vec![("javax-activation-api".to_string(), "sshd".to_string())]
        );
    }

    #[test]
    fn rejects_non_war_and_missing_version() {
        assert!(read_from(Cursor::new(b"nope".to_vec()), "x.war").is_err());
        let no_version = zip_bytes(&[(
            "META-INF/MANIFEST.MF",
            b"Manifest-Version: 1.0\r\n".to_vec(),
        )]);
        let err = read_from(Cursor::new(no_version), "x.war")
            .err()
            .unwrap()
            .to_string();
        assert!(err.contains("no Jenkins-Version"), "{err}");
    }

    #[test]
    fn jenkins_version_from_flag_war_or_both() {
        let war = read_from(Cursor::new(fake_war("2.452.4")), "jenkins.war").unwrap();
        assert_eq!(jenkins_version(Some("2.440.3"), None).unwrap(), "2.440.3");
        assert_eq!(jenkins_version(None, Some(&war)).unwrap(), "2.452.4");
        assert_eq!(
            jenkins_version(Some("2.452.4"), Some(&war)).unwrap(),
            "2.452.4"
        );
        assert!(jenkins_version(Some("2.440.3"), Some(&war)).is_err());
        assert!(jenkins_version(None, None).is_err());
    }
}
