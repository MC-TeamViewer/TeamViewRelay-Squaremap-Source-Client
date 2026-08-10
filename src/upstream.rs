use std::time::Duration;

use reqwest::header::{ETAG, IF_MODIFIED_SINCE, IF_NONE_MATCH, LAST_MODIFIED};
use reqwest::{Client, StatusCode};
use url::Url;

use crate::model::{Snapshot, parse_snapshot};

const MAX_BODY_BYTES: usize = 2 * 1024 * 1024;

#[derive(Debug)]
pub struct PollError {
    pub code: &'static str,
    pub detail: String,
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
    etag: Option<String>,
    last_modified: Option<String>,
    has_snapshot: bool,
    normalize_dimensions: bool,
}

impl UpstreamClient {
    pub fn new(url: Url, normalize_dimensions: bool) -> anyhow::Result<Self> {
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
            url,
            etag: None,
            last_modified: None,
            has_snapshot: false,
            normalize_dimensions,
        })
    }

    pub async fn poll(&mut self) -> Result<PollOutcome, PollError> {
        let mut request = self.client.get(self.url.clone());
        if let Some(etag) = &self.etag {
            request = request.header(IF_NONE_MATCH, etag);
        }
        if let Some(last_modified) = &self.last_modified {
            request = request.header(IF_MODIFIED_SINCE, last_modified);
        }

        let mut response = request.send().await.map_err(|error| PollError {
            code: if error.is_timeout() {
                "timeout"
            } else {
                "network"
            },
            detail: error.to_string(),
        })?;

        if response.status() == StatusCode::NOT_MODIFIED {
            if !self.has_snapshot {
                return Err(PollError {
                    code: "schema",
                    detail: "304 received before an initial snapshot".to_owned(),
                });
            }
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
            return Ok(PollOutcome::NotModified);
        }
        if !response.status().is_success() {
            return Err(PollError {
                code: "http_status",
                detail: response.status().to_string(),
            });
        }
        if response
            .content_length()
            .is_some_and(|length| length > MAX_BODY_BYTES as u64)
        {
            return Err(PollError {
                code: "schema",
                detail: "response body too large".to_owned(),
            });
        }

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
        while let Some(chunk) = response.chunk().await.map_err(|error| PollError {
            code: "network",
            detail: error.to_string(),
        })? {
            if body.len().saturating_add(chunk.len()) > MAX_BODY_BYTES {
                return Err(PollError {
                    code: "schema",
                    detail: "response body too large".to_owned(),
                });
            }
            body.extend_from_slice(&chunk);
        }
        let snapshot =
            parse_snapshot(&body, self.normalize_dimensions).map_err(|error| PollError {
                code: if error.downcast_ref::<serde_json::Error>().is_some() {
                    "json"
                } else {
                    "schema"
                },
                detail: error.to_string(),
            })?;

        self.etag = response_etag;
        self.last_modified = response_last_modified;
        self.has_snapshot = true;
        Ok(PollOutcome::Snapshot(snapshot))
    }
}
