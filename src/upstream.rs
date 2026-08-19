use std::fs;
use std::path::PathBuf;
use std::time::Duration;

use reqwest::header::{
    ACCEPT, COOKIE, ETAG, HeaderValue, IF_MODIFIED_SINCE, IF_NONE_MATCH, LAST_MODIFIED, REFERER,
    USER_AGENT,
};
use reqwest::{Client, StatusCode};
use url::Url;

use crate::model::{Snapshot, parse_snapshot};

const MAX_BODY_BYTES: usize = 2 * 1024 * 1024;
const MAX_COOKIE_BYTES: u64 = 32 * 1024;
const DEFAULT_ACCEPT: &str = "application/json, text/plain, */*";

#[derive(Debug)]
pub struct PollError {
    pub code: String,
    pub detail: String,
}

impl PollError {
    fn new(code: impl Into<String>, detail: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            detail: detail.into(),
        }
    }
}

impl std::fmt::Display for PollError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.code, self.detail)
    }
}

impl std::error::Error for PollError {}

pub enum PollOutcome {
    Snapshot(Snapshot),
    NotModified,
}

pub struct UpstreamClient {
    client: Client,
    url: Url,
    cookie_file: Option<PathBuf>,
    user_agent: Option<String>,
    referer: String,
    etag: Option<String>,
    last_modified: Option<String>,
    has_snapshot: bool,
    normalize_dimensions: bool,
}

impl UpstreamClient {
    pub fn new(
        url: Url,
        normalize_dimensions: bool,
        cookie_file: Option<PathBuf>,
        user_agent: Option<String>,
        referer: Option<String>,
    ) -> anyhow::Result<Self> {
        let client = Client::builder()
            .connect_timeout(Duration::from_secs(5))
            .timeout(Duration::from_secs(10))
            .user_agent(concat!(
                "TeamViewRelay-Squaremap-Source/",
                env!("CARGO_PKG_VERSION")
            ))
            .build()?;
        Ok(Self {
            client,
            referer: referer.unwrap_or_else(|| url.as_str().to_owned()),
            url,
            cookie_file,
            user_agent,
            etag: None,
            last_modified: None,
            has_snapshot: false,
            normalize_dimensions,
        })
    }

    pub async fn poll(&mut self) -> Result<PollOutcome, PollError> {
        let mut request = self
            .client
            .get(self.url.clone())
            .header(ACCEPT, DEFAULT_ACCEPT);
        if let Some(user_agent) = &self.user_agent {
            request = request.header(USER_AGENT, user_agent);
        }
        request = request.header(REFERER, &self.referer);
        if let Some(cookie) = self.read_cookie()? {
            request = request.header(COOKIE, cookie);
        }
        if let Some(etag) = &self.etag {
            request = request.header(IF_NONE_MATCH, etag);
        }
        if let Some(last_modified) = &self.last_modified {
            request = request.header(IF_MODIFIED_SINCE, last_modified);
        }

        let mut response = request.send().await.map_err(|error| {
            PollError::new(
                if error.is_timeout() {
                    "timeout"
                } else {
                    "network"
                },
                error.to_string(),
            )
        })?;

        if response.status() == StatusCode::NOT_MODIFIED {
            if !self.has_snapshot {
                return Err(PollError::new(
                    "not_modified_before_snapshot",
                    "304 received before an initial snapshot",
                ));
            }
            self.update_cache_headers(&response);
            return Ok(PollOutcome::NotModified);
        }
        if !response.status().is_success() {
            return Err(PollError::new(
                format!("http_status_{}", response.status().as_u16()),
                response.status().to_string(),
            ));
        }
        if response
            .content_length()
            .is_some_and(|length| length > MAX_BODY_BYTES as u64)
        {
            return Err(PollError::new(
                "response_too_large",
                "response body too large",
            ));
        }

        let content_type = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned);
        let response_etag = response
            .headers()
            .get(ETAG)
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned);
        let response_last_modified = response
            .headers()
            .get(LAST_MODIFIED)
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned);
        let mut body = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|error| PollError::new("network", error.to_string()))?
        {
            if body.len().saturating_add(chunk.len()) > MAX_BODY_BYTES {
                return Err(PollError::new(
                    "response_too_large",
                    "response body too large",
                ));
            }
            body.extend_from_slice(&chunk);
        }

        if looks_like_edgeone_challenge(content_type.as_deref(), &body) {
            return Err(PollError::new(
                "captcha_required",
                "TencentEdgeOne returned a human-verification HTML page",
            ));
        }
        if content_type
            .as_deref()
            .is_some_and(|value| value.to_ascii_lowercase().contains("text/html"))
        {
            return Err(PollError::new(
                "upstream_not_json",
                format!(
                    "successful response has HTML content-type instead of players JSON ({})",
                    content_type.as_deref().unwrap_or("unknown content-type")
                ),
            ));
        }

        let snapshot = parse_snapshot(&body, self.normalize_dimensions).map_err(|error| {
            PollError::new(
                if error.downcast_ref::<serde_json::Error>().is_some() {
                    "invalid_json"
                } else {
                    "invalid_players_schema"
                },
                error.to_string(),
            )
        })?;

        self.etag = response_etag;
        self.last_modified = response_last_modified;
        self.has_snapshot = true;
        Ok(PollOutcome::Snapshot(snapshot))
    }

    fn read_cookie(&self) -> Result<Option<HeaderValue>, PollError> {
        let Some(path) = &self.cookie_file else {
            return Ok(None);
        };
        let metadata = fs::metadata(path).map_err(|error| {
            PollError::new(
                "cookie_file_error",
                format!("read cookie file metadata {}: {error}", path.display()),
            )
        })?;
        if metadata.len() > MAX_COOKIE_BYTES {
            return Err(PollError::new(
                "cookie_file_error",
                format!("cookie file exceeds {MAX_COOKIE_BYTES} bytes"),
            ));
        }
        let cookie = fs::read_to_string(path).map_err(|error| {
            PollError::new(
                "cookie_file_error",
                format!("read cookie file {}: {error}", path.display()),
            )
        })?;
        let cookie = cookie.trim();
        if cookie.is_empty() {
            return Err(PollError::new("cookie_file_error", "cookie file is empty"));
        }
        if cookie.contains(['\r', '\n']) {
            return Err(PollError::new(
                "cookie_file_error",
                "cookie file must contain one Cookie header value",
            ));
        }
        HeaderValue::from_str(cookie).map(Some).map_err(|_| {
            PollError::new(
                "cookie_file_error",
                "cookie value is not a valid HTTP header",
            )
        })
    }

    fn update_cache_headers(&mut self, response: &reqwest::Response) {
        if let Some(etag) = response
            .headers()
            .get(ETAG)
            .and_then(|value| value.to_str().ok())
        {
            self.etag = Some(etag.to_owned());
        }
        if let Some(last_modified) = response
            .headers()
            .get(LAST_MODIFIED)
            .and_then(|value| value.to_str().ok())
        {
            self.last_modified = Some(last_modified.to_owned());
        }
    }
}

fn looks_like_edgeone_challenge(content_type: Option<&str>, body: &[u8]) -> bool {
    let is_html =
        content_type.is_some_and(|value| value.to_ascii_lowercase().contains("text/html"));
    if !is_html {
        return false;
    }
    let sample_len = body.len().min(64 * 1024);
    let sample = String::from_utf8_lossy(&body[..sample_len]).to_ascii_lowercase();
    [
        "tencentedgeone",
        "eo-bot",
        "captcha",
        "human verification",
        "人机验证",
    ]
    .iter()
    .any(|marker| sample.contains(marker))
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};

    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;
    use url::Url;

    use super::{PollOutcome, UpstreamClient, looks_like_edgeone_challenge};

    #[test]
    fn recognizes_edgeone_html_challenge() {
        assert!(looks_like_edgeone_challenge(
            Some("text/html; charset=utf-8"),
            b"<html><title>TencentEdgeOne captcha</title></html>"
        ));
    }

    #[test]
    fn does_not_mark_json_as_challenge() {
        assert!(!looks_like_edgeone_challenge(
            Some("application/json"),
            br#"{"players":[]}"#
        ));
    }

    #[tokio::test]
    async fn poll_sends_cookie_and_browser_headers() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let response = concat!(
            "HTTP/1.1 200 OK\r\n",
            "Content-Type: application/json\r\n",
            "ETag: test-etag\r\n",
            "Content-Length: 14\r\n",
            "Connection: close\r\n",
            "\r\n",
            "{\"players\":[]}"
        );
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            loop {
                let mut chunk = [0_u8; 1024];
                let count = socket.read(&mut chunk).await.unwrap();
                if count == 0 {
                    break;
                }
                request.extend_from_slice(&chunk[..count]);
                if request.windows(4).any(|window| window == b"\r\n\r\n") {
                    break;
                }
            }
            socket.write_all(response.as_bytes()).await.unwrap();
            String::from_utf8(request).unwrap()
        });

        let cookie_path = std::env::temp_dir().join(format!(
            "teamviewrelay-cookie-test-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::write(&cookie_path, "EO_Bot_Ssid=test; EO-Bot-Captcha-Token=token").unwrap();
        let url = Url::parse(&format!("http://{address}/tiles/players.json?")).unwrap();
        let mut client = UpstreamClient::new(
            url,
            true,
            Some(cookie_path.clone()),
            Some("Mozilla/5.0 Test Browser".to_owned()),
            Some("https://example.test/players.json".to_owned()),
        )
        .unwrap();

        assert!(matches!(
            client.poll().await.unwrap(),
            PollOutcome::Snapshot(_)
        ));
        let request = server.await.unwrap();
        assert!(request.contains("cookie: EO_Bot_Ssid=test; EO-Bot-Captcha-Token=token"));
        assert!(request.contains("user-agent: Mozilla/5.0 Test Browser"));
        assert!(request.contains("referer: https://example.test/players.json"));
        fs::remove_file(cookie_path).unwrap();
    }

    #[tokio::test]
    async fn poll_reports_edgeone_challenge_instead_of_json_error() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = [0_u8; 4096];
            let _ = socket.read(&mut request).await.unwrap();
            socket
                .write_all(
                    concat!(
                        "HTTP/1.1 200 OK\r\n",
                        "Content-Type: text/html; charset=utf-8\r\n",
                        "Connection: close\r\n",
                        "\r\n",
                        "<html>TencentEdgeOne captcha verification</html>"
                    )
                    .as_bytes(),
                )
                .await
                .unwrap();
        });
        let url = Url::parse(&format!("http://{address}/tiles/players.json")).unwrap();
        let mut client = UpstreamClient::new(url, true, None, None, None).unwrap();
        let error = match client.poll().await {
            Ok(_) => panic!("expected the EdgeOne challenge to fail"),
            Err(error) => error,
        };
        assert_eq!(error.code, "captcha_required");
        assert!(error.detail.contains("TencentEdgeOne"));
        server.await.unwrap();
    }
}
