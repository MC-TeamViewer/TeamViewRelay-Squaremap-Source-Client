use std::collections::HashMap;
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use clap::{Parser, ValueEnum};
use futures_util::{SinkExt, StreamExt};
use reqwest::header::{ACCEPT, CONTENT_TYPE, COOKIE, ETAG, IF_NONE_MATCH, REFERER, USER_AGENT};
use serde_json::{Value, json};
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use tokio::net::{TcpListener, TcpStream};
use tokio::process::{Child, Command};
use tokio::time::Instant;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async, tungstenite::Message};
use tracing::{info, warn};
use url::Url;

const DEFAULT_URL: &str = "https://map1.nodemc.cc/tiles/players.json";
const DEFAULT_HOST: &str = "127.0.0.1";
const DEFAULT_PORT: u16 = 8080;
const MAX_BODY_BYTES: usize = 2 * 1024 * 1024;
const UA: &str =
    "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 Chrome/126.0.0.0 Safari/537.36";

#[derive(Clone, Copy, Debug, ValueEnum)]
enum BrowserMode {
    Resident,
    OnDemand,
}

#[derive(Debug, Parser)]
#[command(version, about)]
struct Args {
    #[arg(long, help = "运行 HTTP sidecar")]
    serve: bool,
    #[arg(long, help = "执行一次取数并把 JSON 输出到 stdout")]
    once: bool,
    #[arg(long, help = "检查一个正在运行的 sidecar")]
    healthcheck: bool,
    #[arg(long, default_value = "http://127.0.0.1:8080/healthz")]
    healthcheck_url: String,
    #[arg(long, default_value = DEFAULT_URL)]
    url: String,
    #[arg(long, default_value = DEFAULT_HOST)]
    host: String,
    #[arg(long, default_value_t = DEFAULT_PORT)]
    port: u16,
    #[arg(long, default_value_t = 5.0)]
    poll_interval: f64,
    #[arg(long, default_value_t = 30.0)]
    max_stale_secs: f64,
    #[arg(long, value_enum, default_value_t = BrowserMode::Resident)]
    browser_mode: BrowserMode,
    #[arg(long, default_value = "/usr/bin/chromium")]
    browser_path: String,
    #[arg(long, default_value = UA)]
    user_agent: String,
    #[arg(long)]
    referer: Option<String>,
}

#[derive(Clone, Debug)]
struct CachedSnapshot {
    body: Arc<Vec<u8>>,
    etag: String,
    updated_at: Instant,
}

#[derive(Default)]
struct SnapshotCache {
    snapshot: Option<CachedSnapshot>,
    last_error: Option<String>,
}

impl SnapshotCache {
    fn update(&mut self, body: Vec<u8>) {
        let etag = compute_etag(&body);
        self.snapshot = Some(CachedSnapshot {
            body: Arc::new(body),
            etag,
            updated_at: Instant::now(),
        });
        self.last_error = None;
    }

    fn fail(&mut self, error: impl ToString) {
        self.last_error = Some(error.to_string());
    }

    fn current(&self, max_stale: Duration) -> (Option<CachedSnapshot>, Duration, Option<String>) {
        let Some(snapshot) = &self.snapshot else {
            return (None, Duration::MAX, self.last_error.clone());
        };
        let age = snapshot.updated_at.elapsed();
        let fresh = (age <= max_stale).then(|| snapshot.clone());
        (fresh, age, self.last_error.clone())
    }
}

#[derive(Debug)]
struct PollError {
    captcha: bool,
    detail: String,
}

impl PollError {
    fn captcha(detail: impl Into<String>) -> Self {
        Self {
            captcha: true,
            detail: detail.into(),
        }
    }

    fn other(detail: impl Into<String>) -> Self {
        Self {
            captcha: false,
            detail: detail.into(),
        }
    }
}

impl std::fmt::Display for PollError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.detail)
    }
}

struct Fetcher {
    target: Url,
    mode: BrowserMode,
    browser_path: String,
    user_agent: String,
    referer: String,
    client: reqwest::Client,
    browser: Option<BrowserSession>,
    cookie_header: Option<String>,
    etag: Option<String>,
    last_body: Option<Vec<u8>>,
    has_snapshot: bool,
}

impl Fetcher {
    async fn new(args: &Args) -> Result<Self> {
        let target = Url::parse(&args.url).context("invalid target URL")?;
        if !matches!(target.scheme(), "http" | "https") {
            bail!("target URL must use http or https");
        }
        let client = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(5))
            .timeout(Duration::from_secs(20))
            .build()?;
        Ok(Self {
            referer: args
                .referer
                .clone()
                .unwrap_or_else(|| target.as_str().to_owned()),
            target,
            mode: args.browser_mode,
            browser_path: args.browser_path.clone(),
            user_agent: args.user_agent.clone(),
            client,
            browser: None,
            cookie_header: None,
            etag: None,
            last_body: None,
            has_snapshot: false,
        })
    }

    async fn poll(&mut self) -> Result<Vec<u8>, PollError> {
        match self.mode {
            BrowserMode::Resident => {
                self.ensure_browser()
                    .await
                    .map_err(|e| PollError::other(e.to_string()))?;
                match self.browser_fetch().await {
                    Ok(body) => Ok(body),
                    Err(error) => {
                        self.close_browser().await;
                        Err(error)
                    }
                }
            }
            BrowserMode::OnDemand => match self.direct_fetch().await {
                Ok(body) => Ok(body),
                Err(error) if error.captcha => {
                    self.ensure_browser()
                        .await
                        .map_err(|e| PollError::other(e.to_string()))?;
                    let result = self.browser_fetch().await;
                    self.close_browser().await;
                    result
                }
                Err(error) => Err(error),
            },
        }
    }

    async fn direct_fetch(&mut self) -> Result<Vec<u8>, PollError> {
        let mut request = self
            .client
            .get(self.target.clone())
            .header(ACCEPT, "application/json, text/plain, */*")
            .header(USER_AGENT, &self.user_agent)
            .header(REFERER, &self.referer);
        if let Some(cookie) = &self.cookie_header {
            request = request.header(COOKIE, cookie);
        }
        if let Some(etag) = &self.etag {
            request = request.header(IF_NONE_MATCH, etag);
        }
        let response = request
            .send()
            .await
            .map_err(|e| PollError::other(e.to_string()))?;
        if response.status() == reqwest::StatusCode::NOT_MODIFIED && self.has_snapshot {
            return self
                .last_body
                .clone()
                .ok_or_else(|| PollError::other("304 received before an initial snapshot"));
        }
        if !response.status().is_success() {
            return Err(PollError::other(format!("HTTP {}", response.status())));
        }
        let content_type = response
            .headers()
            .get(CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_owned();
        let response_etag = response
            .headers()
            .get(ETAG)
            .and_then(|v| v.to_str().ok())
            .map(ToOwned::to_owned);
        let body = read_limited(response)
            .await
            .map_err(|e| PollError::other(e.to_string()))?;
        if looks_like_challenge(&content_type, &body) {
            return Err(PollError::captcha(format!(
                "TencentEdgeOne returned a human-verification page (content-type: {}, body-prefix: {})",
                if content_type.is_empty() {
                    "unknown"
                } else {
                    &content_type
                },
                body_preview(&body),
            )));
        }
        validate_json(&body).map_err(|e| {
            if looks_like_challenge("", &body) {
                PollError::captcha(format!(
                    "TencentEdgeOne returned a non-JSON verification page (body-prefix: {})",
                    body_preview(&body),
                ))
            } else {
                PollError::other(format!(
                    "{} (content-type: {}, body-prefix: {})",
                    e,
                    if content_type.is_empty() {
                        "unknown"
                    } else {
                        &content_type
                    },
                    body_preview(&body)
                ))
            }
        })?;
        self.etag = response_etag.or_else(|| Some(compute_etag(&body)));
        self.last_body = Some(body.clone());
        self.has_snapshot = true;
        Ok(body)
    }

    async fn ensure_browser(&mut self) -> Result<()> {
        if self.browser.is_none() {
            self.browser = Some(BrowserSession::start(&self.browser_path, &self.user_agent).await?);
        }
        Ok(())
    }

    async fn close_browser(&mut self) {
        if let Some(mut browser) = self.browser.take() {
            browser.close().await;
        }
    }

    async fn browser_fetch(&mut self) -> Result<Vec<u8>, PollError> {
        let browser = self
            .browser
            .as_mut()
            .ok_or_else(|| PollError::other("browser is not running"))?;
        let body = browser.fetch(&self.target).await?;
        self.cookie_header = browser.cookie_header().await.ok();
        validate_json(&body).map_err(|e| PollError::other(e.to_string()))?;
        self.etag = Some(compute_etag(&body));
        self.last_body = Some(body.clone());
        self.has_snapshot = true;
        Ok(body)
    }
}

struct BrowserSession {
    child: Child,
    ws: WebSocketStream<MaybeTlsStream<TcpStream>>,
    next_id: u64,
    session_id: String,
}

impl BrowserSession {
    async fn start(path: &str, user_agent: &str) -> Result<Self> {
        let browser_path = resolve_browser_path(path).ok_or_else(|| {
            anyhow!("Chromium executable not found; use --browser-path /path/to/chrome")
        })?;
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let port = listener.local_addr()?.port();
        drop(listener);
        let mut command = Command::new(&browser_path);
        command.args([
            "--headless=new",
            // The sidecar runs as an unprivileged, capability-free container user;
            // Chromium's setuid/user-namespace sandbox cannot initialize there.
            // Container isolation remains enforced by cap_drop/no-new-privileges.
            "--no-sandbox",
            "--disable-gpu",
            "--disable-dev-shm-usage",
            "--no-first-run",
            "--no-default-browser-check",
            "--disable-background-networking",
            "--disable-component-update",
            "--disable-sync",
            "--disable-extensions",
            "--disable-blink-features=AutomationControlled",
            "--window-size=1600,900",
            "--remote-debugging-address=127.0.0.1",
        ]);
        command.arg(format!("--remote-debugging-port={port}"));
        command.arg(format!(
            "--user-data-dir=/tmp/pass-cdn-chromium-{}",
            std::process::id()
        ));
        command.arg(format!("--user-agent={user_agent}"));
        command.arg("about:blank");
        command
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let child = command
            .spawn()
            .with_context(|| format!("start Chromium at {}", browser_path.display()))?;
        let version_url = format!("http://127.0.0.1:{port}/json/version");
        let http = reqwest::Client::new();
        let deadline = Instant::now() + Duration::from_secs(15);
        let ws_url = loop {
            if Instant::now() >= deadline {
                bail!("Chromium DevTools endpoint did not become ready");
            }
            match http.get(&version_url).send().await {
                Ok(response) if response.status().is_success() => {
                    let value: Value = response.json().await?;
                    if let Some(url) = value.get("webSocketDebuggerUrl").and_then(Value::as_str) {
                        break url.to_owned();
                    }
                }
                _ => {}
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        };
        let (ws, _) = connect_async(ws_url).await?;
        let mut session = Self {
            child,
            ws,
            next_id: 1,
            session_id: String::new(),
        };
        let target = session
            .command("Target.createTarget", json!({"url":"about:blank"}), None)
            .await?;
        let target_id = target
            .get("targetId")
            .and_then(Value::as_str)
            .context("missing targetId")?;
        let attached = session
            .command(
                "Target.attachToTarget",
                json!({"targetId":target_id,"flatten":true}),
                None,
            )
            .await?;
        session.session_id = attached
            .get("sessionId")
            .and_then(Value::as_str)
            .context("missing sessionId")?
            .to_owned();
        let session_id = session.session_id.clone();
        session
            .command("Page.enable", json!({}), Some(&session_id))
            .await?;
        session
            .command("Runtime.enable", json!({}), Some(&session_id))
            .await?;
        session
            .command("Network.enable", json!({}), Some(&session_id))
            .await?;
        Ok(session)
    }

    async fn command(
        &mut self,
        method: &str,
        params: Value,
        session_id: Option<&str>,
    ) -> Result<Value> {
        let id = self.next_id;
        self.next_id += 1;
        let mut message = json!({"id": id, "method": method, "params": params});
        if let Some(session_id) = session_id {
            message["sessionId"] = Value::String(session_id.to_owned());
        }
        self.ws
            .send(Message::Text(message.to_string().into()))
            .await?;
        while let Some(message) = self.ws.next().await {
            let message = message?;
            let text = match message {
                Message::Text(text) => text.to_string(),
                Message::Binary(bytes) => String::from_utf8(bytes.to_vec())?,
                Message::Close(_) => bail!("Chromium CDP websocket closed"),
                _ => continue,
            };
            let value: Value = serde_json::from_str(&text)?;
            if value.get("id").and_then(Value::as_u64) != Some(id) {
                continue;
            }
            if let Some(error) = value.get("error") {
                bail!("CDP {method} failed: {error}");
            }
            return Ok(value.get("result").cloned().unwrap_or(Value::Null));
        }
        bail!("Chromium CDP websocket ended")
    }

    async fn evaluate(&mut self, expression: &str, context_id: Option<u64>) -> Result<Value> {
        let mut params = json!({"expression":expression,"returnByValue":true,"awaitPromise":true});
        if let Some(context_id) = context_id {
            params["contextId"] = json!(context_id);
        }
        let session_id = self.session_id.clone();
        let result = self
            .command("Runtime.evaluate", params, Some(&session_id))
            .await?;
        Ok(result
            .get("result")
            .and_then(|v| v.get("value"))
            .cloned()
            .unwrap_or(Value::Null))
    }

    async fn fetch(&mut self, target: &Url) -> Result<Vec<u8>, PollError> {
        let session_id = self.session_id.clone();
        self.command(
            "Page.navigate",
            json!({"url":target.as_str()}),
            Some(&session_id),
        )
        .await
        .map_err(|e| PollError::other(e.to_string()))?;
        if let Some(body) = self.wait_json(5).await {
            return Ok(body);
        }
        self.solve_challenge()
            .await
            .map_err(|e| PollError::captcha(e.to_string()))?;
        if let Some(body) = self.wait_json(20).await {
            return Ok(body);
        }
        if let Ok(body) = self.fetch_current_url().await {
            return Ok(body);
        }
        let session_id = self.session_id.clone();
        self.command(
            "Page.navigate",
            json!({"url":target.as_str()}),
            Some(&session_id),
        )
        .await
        .map_err(|e| PollError::other(e.to_string()))?;
        if let Some(body) = self.wait_json(20).await {
            return Ok(body);
        }
        let expression = format!(
            "fetch({}).then(response=>response.text())",
            serde_json::to_string(target.as_str()).unwrap_or_else(|_| "''".to_owned())
        );
        if let Ok(value) = self.evaluate(&expression, None).await
            && let Some(text) = value.as_str()
        {
            let body = text.trim().as_bytes().to_vec();
            if validate_json(&body).is_ok() {
                return Ok(body);
            }
        }
        let diagnostic = self
            .evaluate(
                "JSON.stringify({url:location.href,title:document.title,body:(document.body?.innerText||'').slice(0,500),cookies:document.cookie})",
                None,
            )
            .await
            .ok()
            .and_then(|value| value.as_str().map(ToOwned::to_owned))
            .unwrap_or_else(|| "unavailable".to_owned());
        Err(PollError::captcha(format!(
            "verification completed but players JSON was not returned; page={diagnostic}"
        )))
    }

    async fn fetch_current_url(&mut self) -> Result<Vec<u8>, PollError> {
        let location = self
            .evaluate("location.href", None)
            .await
            .map_err(|e| PollError::other(e.to_string()))?;
        let location = location
            .as_str()
            .filter(|value| value.starts_with("http://") || value.starts_with("https://"))
            .ok_or_else(|| PollError::other("browser current URL is unavailable"))?;
        let cookies = self
            .cookie_header()
            .await
            .map_err(|e| PollError::other(e.to_string()))?;
        let response = reqwest::Client::new()
            .get(location)
            .header(ACCEPT, "application/json, text/plain, */*")
            .header(USER_AGENT, UA)
            .header(COOKIE, cookies)
            .send()
            .await
            .map_err(|e| PollError::other(e.to_string()))?;
        if !response.status().is_success() {
            return Err(PollError::other(format!("HTTP {}", response.status())));
        }
        let body = read_limited(response)
            .await
            .map_err(|e| PollError::other(e.to_string()))?;
        if looks_like_challenge("", &body) {
            return Err(PollError::captcha(format!(
                "browser current URL still returned verification HTML (body-prefix: {})",
                body_preview(&body)
            )));
        }
        validate_json(&body).map_err(|e| PollError::other(e.to_string()))?;
        Ok(body)
    }

    async fn wait_json(&mut self, timeout_secs: u64) -> Option<Vec<u8>> {
        let deadline = Instant::now() + Duration::from_secs(timeout_secs);
        while Instant::now() < deadline {
            if let Ok(value) = self
                .evaluate("document.body ? document.body.innerText : ''", None)
                .await
                && let Some(text) = value.as_str()
            {
                let body = text.trim().as_bytes().to_vec();
                if validate_json(&body).is_ok() {
                    return Some(body);
                }
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
        None
    }

    async fn solve_challenge(&mut self) -> Result<()> {
        let deadline = Instant::now() + Duration::from_secs(20);
        let rect = loop {
            let value = self
                .evaluate(
                    "(()=>{const e=document.querySelector('iframe#tcaptcha_iframe_eo,iframe[src*=captcha],iframe[name*=captcha]'); if(!e)return null; const r=e.getBoundingClientRect(); return {x:r.x,y:r.y,width:r.width,height:r.height}})()",
                    None,
                )
                .await?;
            if value
                .get("width")
                .and_then(Value::as_f64)
                .is_some_and(|width| width > 0.0)
            {
                break value;
            }
            if Instant::now() >= deadline {
                bail!("verification iframe element did not become visible");
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        };
        let iframe_x = rect.get("x").and_then(Value::as_f64).unwrap_or(0.0);
        let iframe_y = rect.get("y").and_then(Value::as_f64).unwrap_or(0.0);
        let iframe_width = rect.get("width").and_then(Value::as_f64).unwrap_or(330.0);
        let iframe_height = rect.get("height").and_then(Value::as_f64).unwrap_or(70.0);
        // The EdgeOne widget's checkbox is on the left side of its cross-origin iframe.
        let x = iframe_x + (iframe_width * 0.11).clamp(22.0, 48.0);
        let y = iframe_y + iframe_height * 0.5;
        for i in 0..=12 {
            let t = i as f64 / 12.0;
            self.input_mouse("mouseMoved", x * t, y * t).await?;
        }
        self.input_mouse("mousePressed", x, y).await?;
        self.input_mouse("mouseReleased", x, y).await?;
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            let cookies = self.cookie_header().await.unwrap_or_default();
            if cookies.contains("EO-Bot-Captcha-Token=") {
                return Ok(());
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
        bail!("verification did not produce EO-Bot-Captcha-Token")
    }

    async fn input_mouse(&mut self, event: &str, x: f64, y: f64) -> Result<()> {
        let session_id = self.session_id.clone();
        self.command(
            "Input.dispatchMouseEvent",
            json!({"type":event,"x":x,"y":y,"button":"left","clickCount":1}),
            Some(&session_id),
        )
        .await?;
        Ok(())
    }

    async fn cookie_header(&mut self) -> Result<String> {
        let session_id = self.session_id.clone();
        let result = self
            .command("Network.getAllCookies", json!({}), Some(&session_id))
            .await?;
        let cookies = result
            .get("cookies")
            .and_then(Value::as_array)
            .context("CDP returned no cookies")?;
        let values = cookies
            .iter()
            .filter_map(|cookie| {
                let name = cookie.get("name")?.as_str()?;
                let value = cookie.get("value")?.as_str()?;
                Some(format!("{name}={value}"))
            })
            .collect::<Vec<_>>();
        Ok(values.join("; "))
    }

    async fn close(&mut self) {
        let _ = self.ws.close(None).await;
        let _ = self.child.kill().await;
    }
}

fn resolve_browser_path(configured: &str) -> Option<PathBuf> {
    let configured_path = Path::new(configured);
    let mut candidates = Vec::new();
    if configured_path.is_dir() {
        candidates.push(configured_path.join("chrome"));
        candidates.push(configured_path.join("chromium"));
    } else {
        candidates.push(configured_path.to_owned());
    }
    candidates.extend([
        PathBuf::from("/usr/bin/chromium"),
        PathBuf::from("/usr/bin/chromium-browser"),
        PathBuf::from("/usr/bin/google-chrome"),
    ]);
    candidates.into_iter().find(|path| path.is_file())
}

async fn read_limited(response: reqwest::Response) -> Result<Vec<u8>> {
    if response
        .content_length()
        .is_some_and(|length| length > MAX_BODY_BYTES as u64)
    {
        bail!("response body too large");
    }
    let mut body = Vec::new();
    let mut stream = response;
    while let Some(chunk) = stream.chunk().await? {
        if body.len().saturating_add(chunk.len()) > MAX_BODY_BYTES {
            bail!("response body too large");
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

fn validate_json(body: &[u8]) -> Result<()> {
    let value: Value = serde_json::from_slice(body).context("invalid JSON")?;
    if !value.is_object() {
        bail!("players JSON root is not an object");
    }
    Ok(())
}

fn compute_etag(body: &[u8]) -> String {
    let mut hasher = DefaultHasher::new();
    body.hash(&mut hasher);
    format!("\"rust-hash-{:x}-{}\"", hasher.finish(), body.len())
}

fn looks_like_challenge(content_type: &str, body: &[u8]) -> bool {
    let sample = String::from_utf8_lossy(&body[..body.len().min(64 * 1024)]).to_ascii_lowercase();
    content_type.to_ascii_lowercase().contains("text/html")
        || sample.trim_start().starts_with("<!doctype html")
        || sample.trim_start().starts_with("<html")
        || [
            "tencentedgeone",
            "eo-bot",
            "captcha",
            "human verification",
            "人机验证",
            "tcaptcha_iframe_eo",
        ]
        .iter()
        .any(|marker| sample.contains(marker))
}

fn body_preview(body: &[u8]) -> String {
    String::from_utf8_lossy(&body[..body.len().min(160)]).replace(['\r', '\n', '\t'], " ")
}

async fn run_worker(mut fetcher: Fetcher, cache: Arc<Mutex<SnapshotCache>>, interval: Duration) {
    loop {
        match fetcher.poll().await {
            Ok(body) => {
                if let Ok(mut cache) = cache.lock() {
                    cache.update(body);
                }
                info!("players.json updated");
            }
            Err(error) if error.detail == "not_modified" => {}
            Err(error) => {
                warn!(captcha = error.captcha, detail = %error, "players.json fetch failed");
                if let Ok(mut cache) = cache.lock() {
                    cache.fail(&error);
                }
            }
        }
        tokio::time::sleep(interval).await;
    }
}

async fn serve(args: Args) -> Result<()> {
    let cache = Arc::new(Mutex::new(SnapshotCache::default()));
    let fetcher = Fetcher::new(&args).await?;
    let worker_cache = Arc::clone(&cache);
    let worker = tokio::spawn(run_worker(
        fetcher,
        worker_cache,
        Duration::from_secs_f64(args.poll_interval),
    ));
    let listener = TcpListener::bind((args.host.as_str(), args.port)).await?;
    info!(
        "sidecar listening on http://{}:{}/tiles/players.json",
        args.host, args.port
    );
    loop {
        tokio::select! {
            result = listener.accept() => {
                let (stream, _) = result?;
                let cache = Arc::clone(&cache);
                let max_stale = Duration::from_secs_f64(args.max_stale_secs);
                tokio::spawn(async move { if let Err(error) = handle_http(stream, cache, max_stale).await { warn!(%error, "HTTP client failed"); } });
            }
            _ = tokio::signal::ctrl_c() => break,
        }
    }
    worker.abort();
    Ok(())
}

async fn handle_http(
    mut stream: TcpStream,
    cache: Arc<Mutex<SnapshotCache>>,
    max_stale: Duration,
) -> Result<()> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut request = vec![0_u8; 16 * 1024];
    let count = stream.read(&mut request).await?;
    let request = String::from_utf8_lossy(&request[..count]);
    let mut lines = request.split("\r\n");
    let first = lines.next().unwrap_or("");
    let mut parts = first.split_whitespace();
    let method = parts.next().unwrap_or("");
    let path = parts.next().unwrap_or("").split('?').next().unwrap_or("");
    let mut headers = HashMap::new();
    for line in lines {
        if line.is_empty() {
            break;
        }
        if let Some((name, value)) = line.split_once(':') {
            headers.insert(name.trim().to_ascii_lowercase(), value.trim().to_owned());
        }
    }
    let (snapshot, age, error) = cache
        .lock()
        .map_err(|_| anyhow!("cache lock poisoned"))?
        .current(max_stale);
    let (status, body, extra) = if path == "/healthz" {
        let payload = json!({"ok":snapshot.is_some(),"age_secs":(age != Duration::MAX).then_some(age.as_secs_f64()),"last_error":error}).to_string().into_bytes();
        (if snapshot.is_some() { 200 } else { 503 }, payload, None)
    } else if path == "/tiles/players.json" {
        match snapshot {
            Some(snapshot) if headers.get("if-none-match") == Some(&snapshot.etag) => (304, Vec::new(), Some(snapshot.etag)),
            Some(snapshot) => (200, (*snapshot.body).clone(), Some(snapshot.etag)),
            None => (503, json!({"error":"snapshot_unavailable","age_secs":(age != Duration::MAX).then_some(age.as_secs_f64()),"detail":error}).to_string().into_bytes(), None),
        }
    } else {
        (404, b"{\"error\":\"not_found\"}".to_vec(), None)
    };
    let reason = match status {
        200 => "OK",
        304 => "Not Modified",
        404 => "Not Found",
        503 => "Service Unavailable",
        _ => "Error",
    };
    let mut response = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json; charset=utf-8\r\nContent-Length: {}\r\nCache-Control: no-store\r\n",
        body.len()
    );
    if let Some(etag) = extra {
        response.push_str(&format!("ETag: {etag}\r\n"));
    }
    response.push_str("Connection: close\r\n\r\n");
    if method != "HEAD" {
        response.push_str(&String::from_utf8_lossy(&body));
    }
    stream.write_all(response.as_bytes()).await?;
    Ok(())
}

async fn healthcheck(url: &str) -> Result<()> {
    let response = reqwest::Client::new().get(url).send().await?;
    if response.status() != reqwest::StatusCode::OK {
        bail!("healthcheck returned {}", response.status());
    }
    Ok(())
}

async fn once(args: &Args) -> Result<()> {
    let mut fetcher = Fetcher::new(args).await?;
    let body = fetcher.poll().await.map_err(|e| anyhow!(e.to_string()))?;
    println!("{}", String::from_utf8(body)?);
    fetcher.close_browser().await;
    Ok(())
}

// A cloud VM can expose hundreds of host CPUs even when this container is
// limited to a fraction of one CPU. Keep the sidecar's runtime footprint
// bounded instead of letting Tokio create one worker per host CPU.
#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(std::env::var("RUST_LOG").unwrap_or_else(|_| "info".to_owned()))
        .init();
    let args = Args::parse();
    if args.poll_interval <= 0.0 || args.max_stale_secs <= 0.0 {
        bail!("poll interval and max stale seconds must be positive");
    }
    if args.healthcheck {
        return healthcheck(&args.healthcheck_url).await;
    }
    if args.serve && args.once {
        bail!("--serve and --once cannot be combined");
    }
    if args.serve {
        serve(args).await
    } else {
        once(&args).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_object_json_only() {
        assert!(validate_json(br#"{"players":[]}"#).is_ok());
        assert!(validate_json(br#"[]"#).is_err());
        assert!(validate_json(b"not-json").is_err());
    }

    #[test]
    fn detects_edgeone_challenge_html() {
        assert!(looks_like_challenge(
            "text/html; charset=utf-8",
            b"<html>TencentEdgeOne captcha</html>"
        ));
        assert!(!looks_like_challenge(
            "application/json",
            br#"{"players":[]}"#
        ));
    }

    #[test]
    fn cache_expires_after_max_stale() {
        let mut cache = SnapshotCache::default();
        cache.update(br#"{"players":[]}"#.to_vec());
        assert!(cache.current(Duration::from_secs(1)).0.is_some());
        assert!(cache.current(Duration::ZERO).0.is_none());
    }
}
