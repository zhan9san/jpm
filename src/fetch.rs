//! Shared HTTP fetching for remote metadata (Update Center JSON, war
//! `pom.xml`, split-plugin lists) and the on-disk cache location.

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};

/// Attempts per URL before giving up.
const MAX_ATTEMPTS: u32 = 3;

/// Back-off before the second attempt; doubled for each further attempt.
#[cfg(not(test))]
const RETRY_BASE: Duration = Duration::from_millis(300);
#[cfg(test)]
const RETRY_BASE: Duration = Duration::from_millis(10);

/// Directory for cached remote metadata: `$JPM_CACHE_DIR`, or `jpm` under the
/// platform cache directory.
pub fn cache_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("JPM_CACHE_DIR") {
        return PathBuf::from(dir);
    }
    dirs::cache_dir()
        .unwrap_or_else(|| PathBuf::from(".cache"))
        .join("jpm")
}

/// GET `url` as text, retrying network errors, 5xx and 429 responses with
/// exponential back-off. Other 4xx responses (e.g. a 404 for an unknown
/// Jenkins version) fail immediately.
pub async fn get_text(client: &reqwest::Client, url: &str) -> Result<String> {
    let mut attempt = 1;
    loop {
        match try_get_text(client, url).await {
            Ok(text) => return Ok(text),
            Err(failure) if failure.retryable && attempt < MAX_ATTEMPTS => {
                let wait = RETRY_BASE * 2u32.pow(attempt - 1);
                eprintln!(
                    "  retry {attempt}/{MAX_ATTEMPTS} for {url} (waiting {}ms): {:#}",
                    wait.as_millis(),
                    failure.error
                );
                tokio::time::sleep(wait).await;
                attempt += 1;
            }
            Err(failure) => return Err(failure.error),
        }
    }
}

struct Failure {
    error: anyhow::Error,
    retryable: bool,
}

async fn try_get_text(client: &reqwest::Client, url: &str) -> Result<String, Failure> {
    let transient = |error: anyhow::Error| Failure {
        error,
        retryable: true,
    };

    let response = client
        .get(url)
        .send()
        .await
        .with_context(|| format!("GET {url}"))
        .map_err(transient)?;

    let status = response.status();
    if !status.is_success() {
        return Err(Failure {
            error: anyhow!("HTTP {status} from {url}"),
            retryable: status.is_server_error() || status.as_u16() == 429,
        });
    }

    response
        .text()
        .await
        .with_context(|| format!("reading body of {url}"))
        .map_err(transient)
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[tokio::test]
    async fn get_text_retries_server_errors() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/x"))
            .respond_with(ResponseTemplate::new(503))
            .up_to_n_times(2)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/x"))
            .respond_with(ResponseTemplate::new(200).set_body_string("ok"))
            .mount(&server)
            .await;

        let client = reqwest::Client::new();
        let text = get_text(&client, &format!("{}/x", server.uri()))
            .await
            .unwrap();
        assert_eq!(text, "ok");
        assert_eq!(server.received_requests().await.unwrap().len(), 3);
    }

    #[tokio::test]
    async fn get_text_gives_up_after_max_attempts() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;

        let client = reqwest::Client::new();
        let err = get_text(&client, &format!("{}/x", server.uri()))
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("500"), "{err}");
        assert_eq!(
            server.received_requests().await.unwrap().len(),
            MAX_ATTEMPTS as usize
        );
    }

    #[tokio::test]
    async fn get_text_does_not_retry_not_found() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;

        let client = reqwest::Client::new();
        assert!(get_text(&client, &format!("{}/x", server.uri()))
            .await
            .is_err());
        assert_eq!(server.received_requests().await.unwrap().len(), 1);
    }
}
