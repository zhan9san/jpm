//! Reading `META-INF/MANIFEST.MF` from plugin archives (`.hpi` / `.jpi`).

use std::collections::HashMap;
use std::io::{Read, Seek};
use std::path::Path;

use anyhow::{Context, Result};

/// Read and parse `META-INF/MANIFEST.MF` from the ZIP archive in `reader`.
/// `label` names the archive in error messages.
pub fn read_headers<R: Read + Seek>(reader: R, label: &str) -> Result<HashMap<String, String>> {
    let mut archive = zip::ZipArchive::new(reader)
        .with_context(|| format!("'{label}' is not a valid ZIP/HPI"))?;

    let mut entry = archive
        .by_name("META-INF/MANIFEST.MF")
        .with_context(|| format!("no META-INF/MANIFEST.MF in '{label}'"))?;

    let mut content = String::new();
    entry
        .read_to_string(&mut content)
        .with_context(|| format!("reading META-INF/MANIFEST.MF in '{label}'"))?;

    Ok(parse_headers(&content))
}

/// Read and parse `META-INF/MANIFEST.MF` from the archive at `path`.
pub fn read_headers_from_path(path: &Path) -> Result<HashMap<String, String>> {
    let file =
        std::fs::File::open(path).with_context(|| format!("opening '{}'", path.display()))?;
    read_headers(file, &path.display().to_string())
}

/// Open an `.hpi`/`.jpi` ZIP archive and return `(short-name, plugin-version)`
/// extracted from `META-INF/MANIFEST.MF`.
pub fn read_plugin_manifest(path: &Path) -> Result<(String, String)> {
    let headers = read_headers_from_path(path)?;

    // `Short-Name` is the artifact ID; fall back to the filename stem.
    let name = headers
        .get("Short-Name")
        .cloned()
        .or_else(|| path.file_stem().and_then(|s| s.to_str()).map(str::to_owned))
        .with_context(|| format!("cannot determine plugin name from '{}'", path.display()))?;

    let version = headers
        .get("Plugin-Version")
        .cloned()
        .with_context(|| format!("no Plugin-Version in '{}'", path.display()))?;

    Ok((name, version))
}

/// Parse a Java JAR manifest into a `key → value` map.
///
/// Lines that start with a single space are continuations of the previous
/// header value (JAR manifest specification §3.5).
pub fn parse_headers(content: &str) -> HashMap<String, String> {
    let mut map: HashMap<String, String> = HashMap::new();
    let mut current_key: Option<String> = None;
    let mut current_val = String::new();

    for line in content.lines() {
        if let Some(rest) = line.strip_prefix(' ') {
            // Continuation: append (strip the leading space).
            current_val.push_str(rest);
        } else {
            // Flush previous header.
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
    // Flush the final header.
    if let Some(key) = current_key {
        map.insert(key, current_val.trim_end().to_string());
    }

    map
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_simple_manifest() {
        let manifest = "Manifest-Version: 1.0\r\nShort-Name: git\r\nPlugin-Version: 5.7.0\r\n";
        let headers = parse_headers(manifest);
        assert_eq!(headers["Short-Name"], "git");
        assert_eq!(headers["Plugin-Version"], "5.7.0");
    }

    #[test]
    fn parses_continuation_lines() {
        let manifest = "Long-Name: Git plugin with a very lon\r\n g name continued here\r\nPlugin-Version: 5.7.0\r\n";
        let headers = parse_headers(manifest);
        assert_eq!(
            headers["Long-Name"],
            "Git plugin with a very long name continued here"
        );
        assert_eq!(headers["Plugin-Version"], "5.7.0");
    }

    #[test]
    fn read_headers_rejects_non_zip() {
        let err = read_headers(std::io::Cursor::new(b"<html>oops</html>".to_vec()), "x.hpi")
            .unwrap_err()
            .to_string();
        assert!(err.contains("not a valid ZIP"), "{err}");
    }
}
