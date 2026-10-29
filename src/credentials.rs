//! Basic-auth credentials for private mirrors, in the format of
//! plugin-installation-manager-tool: a comma-separated list of
//! `host[:port]:username:password`.
//!
//! Credentials are only attached to requests whose URL host (and port, when
//! one is given) matches; reqwest drops the header again if a redirect leaves
//! that host.

use std::fmt;
use std::sync::OnceLock;

use anyhow::{bail, Context, Result};

#[derive(Clone, PartialEq, Eq)]
pub struct Credential {
    host: String,
    port: Option<u16>,
    username: String,
    password: String,
}

// Never print the password.
impl fmt::Debug for Credential {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Credential")
            .field("host", &self.host)
            .field("port", &self.port)
            .field("username", &self.username)
            .finish_non_exhaustive()
    }
}

impl Credential {
    /// Parse `host:username:password` or `host:port:username:password`.
    /// Like the Java tool, only the 4-field form allows `:` in the password.
    fn parse(value: &str) -> Result<Self> {
        let parts: Vec<&str> = value.splitn(4, ':').collect();
        let (host, port, username, password) = match parts.as_slice() {
            [host, user, pass] => (*host, None, *user, *pass),
            [host, port, user, pass] => {
                let port = port
                    .parse::<u16>()
                    .with_context(|| format!("invalid port '{port}' in credentials for {host}"))?;
                (*host, Some(port), *user, *pass)
            }
            _ => bail!("credentials must be host[:port]:username:password"),
        };
        if host.is_empty() || username.is_empty() {
            bail!("credentials must be host[:port]:username:password");
        }
        Ok(Credential {
            host: host.to_ascii_lowercase(),
            port,
            username: username.to_string(),
            password: password.to_string(),
        })
    }

    fn matches(&self, url: &reqwest::Url) -> bool {
        let port_matches = match self.port {
            None => true,
            Some(p) => url.port_or_known_default() == Some(p),
        };
        port_matches
            && url
                .host_str()
                .is_some_and(|h| h.eq_ignore_ascii_case(&self.host))
    }
}

/// Parse a comma-separated credentials list (empty entries are ignored).
pub fn parse_list(value: &str) -> Result<Vec<Credential>> {
    value
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(Credential::parse)
        .collect()
}

static CREDENTIALS: OnceLock<Vec<Credential>> = OnceLock::new();

/// Set the process-wide credentials. Only the first call has an effect.
pub fn init(credentials: Vec<Credential>) {
    let _ = CREDENTIALS.set(credentials);
}

/// GET `url`, with basic auth when a configured credential matches it.
pub fn get(client: &reqwest::Client, url: &str) -> reqwest::RequestBuilder {
    let request = client.get(url);
    match find(
        CREDENTIALS.get().map(Vec::as_slice).unwrap_or_default(),
        url,
    ) {
        Some(c) => request.basic_auth(&c.username, Some(&c.password)),
        None => request,
    }
}

fn find<'a>(credentials: &'a [Credential], url: &str) -> Option<&'a Credential> {
    let url = reqwest::Url::parse(url).ok()?;
    // Prefer an entry with an explicit port over a host-only one.
    credentials
        .iter()
        .filter(|c| c.matches(&url))
        .max_by_key(|c| c.port.is_some())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_host_and_optional_port() {
        let list = parse_list("repo.example:alice:s3cret, Mirror.Example:8443:bob:pa:ss").unwrap();
        assert_eq!(list.len(), 2);
        assert_eq!(list[0].host, "repo.example");
        assert_eq!(list[0].port, None);
        assert_eq!(list[0].password, "s3cret");
        assert_eq!(list[1].host, "mirror.example");
        assert_eq!(list[1].port, Some(8443));
        assert_eq!(list[1].username, "bob");
        assert_eq!(list[1].password, "pa:ss");
    }

    #[test]
    fn rejects_malformed_entries() {
        assert!(parse_list("host:user").is_err());
        assert!(parse_list("host:notaport:user:pass").is_err());
        assert!(parse_list(":user:pass").is_err());
        assert!(parse_list("").unwrap().is_empty());
    }

    #[test]
    fn matches_host_and_port_only() {
        let list = parse_list("a.example:u:p,b.example:8443:u2:p2").unwrap();
        assert_eq!(
            find(&list, "https://A.example/x").map(|c| c.username.as_str()),
            Some("u")
        );
        assert_eq!(
            find(&list, "http://a.example:8080/x").map(|c| c.username.as_str()),
            Some("u"),
            "host-only entry matches any port"
        );
        assert_eq!(
            find(&list, "https://b.example:8443/x").map(|c| c.username.as_str()),
            Some("u2")
        );
        assert!(
            find(&list, "https://b.example/x").is_none(),
            "port 443 != 8443"
        );
        assert!(find(&list, "https://c.example/x").is_none());
        assert!(find(&list, "https://evil.example/?a.example").is_none());
    }

    #[test]
    fn prefers_entry_with_explicit_port() {
        let list = parse_list("a.example:any:p,a.example:9000:specific:p").unwrap();
        assert_eq!(
            find(&list, "http://a.example:9000/").map(|c| c.username.as_str()),
            Some("specific")
        );
    }

    #[test]
    fn debug_hides_password() {
        let list = parse_list("a.example:u:topsecret").unwrap();
        assert!(!format!("{list:?}").contains("topsecret"));
    }
}
