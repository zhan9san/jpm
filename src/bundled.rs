use anyhow::{Context, Result};
use quick_xml::events::Event;
use quick_xml::Reader;
use std::collections::HashMap;
use std::path::Path;

/// Fetch the list of plugins bundled inside the Jenkins WAR for a given
/// Jenkins version by parsing the `war/pom.xml` from GitHub.
///
/// The pom.xml for a tagged Jenkins version is immutable, so it is cached
/// permanently (no TTL) under `~/.cache/jpm/pom-<version>.xml`.
pub async fn fetch_bundled_plugins(
    client: &reqwest::Client,
    jenkins_version: &str,
) -> Result<HashMap<String, String>> {
    let cache_dir = if let Ok(dir) = std::env::var("JPM_CACHE_DIR") {
        std::path::PathBuf::from(dir)
    } else {
        dirs::cache_dir()
            .unwrap_or_else(|| std::path::PathBuf::from(".cache"))
            .join("jpm")
    };
    tokio::fs::create_dir_all(&cache_dir).await?;

    let cache_path = cache_dir.join(format!("pom-{jenkins_version}.xml"));

    let xml = if let Some(cached) = try_load_cached_xml(&cache_path).await {
        cached
    } else {
        let pom_base = std::env::var("JPM_POM_BASE_URL").unwrap_or_else(|_| {
            "https://raw.githubusercontent.com/jenkinsci/jenkins/jenkins-".to_string()
        });
        let url = format!("{pom_base}{jenkins_version}/war/pom.xml");
        eprintln!("  fetching bundled plugins from {url}");
        let text = client
            .get(&url)
            .send()
            .await
            .with_context(|| format!("GET {url}"))?
            .error_for_status()
            .with_context(|| format!("non-2xx fetching pom.xml for Jenkins {jenkins_version}"))?
            .text()
            .await?;
        tokio::fs::write(&cache_path, &text).await?;
        text
    };

    parse_bundled_from_pom(&xml)
}

async fn try_load_cached_xml(path: &Path) -> Option<String> {
    tokio::fs::read_to_string(path).await.ok()
}

/// Parse `<artifactItem>` elements with `<type>hpi</type>` from the pom XML.
///
/// The relevant section looks like:
/// ```xml
/// <artifactItem>
///   <artifactId>mailer</artifactId>
///   <version>525.v2458b_d8a_1a_71</version>
///   <type>hpi</type>
/// </artifactItem>
/// ```
fn parse_bundled_from_pom(xml: &str) -> Result<HashMap<String, String>> {
    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(true);

    let mut plugins: HashMap<String, String> = HashMap::new();
    let mut properties: HashMap<String, String> = HashMap::new();

    // State machine: track whether we are inside an <artifactItem> block
    // and collect artifactId / version / type fields.
    // Also collect `<properties>` so `${...}` versions can be resolved.
    let mut in_item = false;
    let mut in_properties = false;
    let mut artifact_id = String::new();
    let mut version = String::new();
    let mut item_type = String::new();
    let mut current_tag = String::new();

    loop {
        match reader.read_event() {
            Ok(Event::Start(e)) => {
                let tag = std::str::from_utf8(e.name().as_ref())
                    .unwrap_or("")
                    .to_string();
                match tag.as_str() {
                    "artifactItem" => {
                        in_item = true;
                        artifact_id.clear();
                        version.clear();
                        item_type.clear();
                    }
                    "properties" => {
                        in_properties = true;
                    }
                    _ if in_item => {
                        current_tag = tag;
                    }
                    _ if in_properties => {
                        current_tag = tag;
                    }
                    _ => {}
                }
            }
            Ok(Event::Text(e)) if in_item || in_properties => {
                let text = e.unescape().unwrap_or_default();
                if in_item {
                    match current_tag.as_str() {
                        "artifactId" => artifact_id = text.to_string(),
                        "version" => version = text.to_string(),
                        "type" => item_type = text.to_string(),
                        _ => {}
                    }
                } else if in_properties && !current_tag.is_empty() {
                    properties.insert(current_tag.clone(), text.to_string());
                }
            }
            Ok(Event::End(e)) => {
                let name_bytes = e.name();
                let tag = std::str::from_utf8(name_bytes.as_ref()).unwrap_or("");
                if tag == "artifactItem" && in_item {
                    if item_type == "hpi" && !artifact_id.is_empty() && !version.is_empty() {
                        let resolved_version = resolve_property_ref(&version, &properties)
                            .unwrap_or_else(|| version.clone());
                        plugins.insert(artifact_id.clone(), resolved_version);
                    }
                    in_item = false;
                    current_tag.clear();
                } else if tag == "properties" {
                    in_properties = false;
                    current_tag.clear();
                }
            }
            Ok(Event::Eof) => break,
            Err(e) => return Err(anyhow::anyhow!("XML parse error: {e}")),
            _ => {}
        }
    }

    Ok(plugins)
}

fn resolve_property_ref(raw: &str, properties: &HashMap<String, String>) -> Option<String> {
    if !(raw.starts_with("${") && raw.ends_with('}')) {
        return Some(raw.to_string());
    }
    let key = &raw[2..raw.len() - 1];
    properties.get(key).cloned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_hpi_artifacts() {
        let xml = r#"<?xml version="1.0"?>
<project>
  <build>
    <plugins>
      <plugin>
        <configuration>
          <artifactItems>
            <artifactItem>
              <artifactId>mailer</artifactId>
              <version>525.v2458b_d8a_1a_71</version>
              <type>hpi</type>
            </artifactItem>
            <artifactItem>
              <artifactId>script-security</artifactId>
              <version>1336.vf33a_a_9863911</version>
              <type>hpi</type>
            </artifactItem>
            <artifactItem>
              <artifactId>not-a-plugin</artifactId>
              <version>1.0</version>
              <type>jar</type>
            </artifactItem>
          </artifactItems>
        </configuration>
      </plugin>
    </plugins>
  </build>
</project>"#;

        let bundled = parse_bundled_from_pom(xml).unwrap();
        assert_eq!(
            bundled.get("mailer").map(String::as_str),
            Some("525.v2458b_d8a_1a_71")
        );
        assert_eq!(
            bundled.get("script-security").map(String::as_str),
            Some("1336.vf33a_a_9863911")
        );
        assert!(!bundled.contains_key("not-a-plugin"));
    }

    #[test]
    fn resolves_version_property_references() {
        let xml = r#"<?xml version="1.0"?>
<project>
  <properties>
    <mina-sshd-api.version>2.14.0-138.v6341ee58e1df</mina-sshd-api.version>
  </properties>
  <build>
    <plugins>
      <plugin>
        <configuration>
          <artifactItems>
            <artifactItem>
              <artifactId>mina-sshd-api-core</artifactId>
              <version>${mina-sshd-api.version}</version>
              <type>hpi</type>
            </artifactItem>
          </artifactItems>
        </configuration>
      </plugin>
    </plugins>
  </build>
</project>"#;

        let bundled = parse_bundled_from_pom(xml).unwrap();
        assert_eq!(
            bundled.get("mina-sshd-api-core").map(String::as_str),
            Some("2.14.0-138.v6341ee58e1df")
        );
    }
}
