use std::fs;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use clap::Parser;
use serde::Deserialize;
use url::Url;
use uuid::Uuid;

#[derive(Debug, Parser)]
#[command(version, about)]
pub struct Args {
    #[arg(long, default_value = "config.toml")]
    config: PathBuf,
    #[arg(long, env = "TEAMVIEWRELAY_RELAY_URL")]
    relay_url: Option<String>,
    #[arg(long, env = "TEAMVIEWRELAY_ROOM_CODE")]
    room_code: Option<String>,
    #[arg(long, env = "TEAMVIEWRELAY_SOURCE_URL")]
    source_url: Option<String>,
    #[arg(long, env = "TEAMVIEWRELAY_DISPLAY_NAME")]
    display_name: Option<String>,
    #[arg(long, env = "TEAMVIEWRELAY_POLL_INTERVAL_SECS")]
    poll_interval_secs: Option<u64>,
    #[arg(long, env = "TEAMVIEWRELAY_FAILURE_GRACE_SECS")]
    failure_grace_secs: Option<u64>,
    #[arg(
        long,
        env = "TEAMVIEWRELAY_NORMALIZE_DIMENSIONS",
        action = clap::ArgAction::Set
    )]
    normalize_dimensions: Option<bool>,
    #[arg(long, env = "TEAMVIEWRELAY_SOURCE_ID")]
    source_id: Option<Uuid>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct FileConfig {
    relay_url: Option<String>,
    room_code: Option<String>,
    source_url: Option<String>,
    display_name: Option<String>,
    poll_interval_secs: Option<u64>,
    failure_grace_secs: Option<u64>,
    normalize_dimensions: Option<bool>,
    source_id: Option<Uuid>,
}

#[derive(Clone, Debug)]
pub struct Config {
    pub relay_url: Url,
    pub room_code: String,
    pub source_url: Url,
    pub display_name: String,
    pub poll_interval: Duration,
    pub failure_grace: Duration,
    pub normalize_dimensions: bool,
    pub source_id: Uuid,
}

impl Config {
    pub fn load() -> Result<Self> {
        let args = Args::parse();
        let file = if args.config.is_file() {
            let raw = fs::read_to_string(&args.config)
                .with_context(|| format!("read config {}", args.config.display()))?;
            toml::from_str::<FileConfig>(&raw)
                .with_context(|| format!("parse config {}", args.config.display()))?
        } else {
            FileConfig::default()
        };

        let relay_raw = args
            .relay_url
            .or(file.relay_url)
            .context("relay_url is required (config, --relay-url, or TEAMVIEWRELAY_RELAY_URL)")?;
        let relay_url = Url::parse(&relay_raw).context("invalid relay_url")?;
        if !matches!(relay_url.scheme(), "ws" | "wss") {
            bail!("relay_url must use ws or wss");
        }

        let source_raw = args.source_url.or(file.source_url).context(
            "source_url is required (config, --source-url, or TEAMVIEWRELAY_SOURCE_URL)",
        )?;
        let source_url = Url::parse(&source_raw).context("invalid source_url")?;
        if !matches!(source_url.scheme(), "http" | "https") {
            bail!("source_url must use http or https");
        }

        let room_code = args
            .room_code
            .or(file.room_code)
            .unwrap_or_else(|| "default".to_owned())
            .trim()
            .to_owned();
        if room_code.is_empty() {
            bail!("room_code must not be empty");
        }

        let display_name = args
            .display_name
            .or(file.display_name)
            .unwrap_or_else(|| "Squaremap".to_owned())
            .trim()
            .to_owned();
        if display_name.is_empty() {
            bail!("display_name must not be empty");
        }

        let poll_secs = args
            .poll_interval_secs
            .or(file.poll_interval_secs)
            .unwrap_or(5);
        let grace_secs = args
            .failure_grace_secs
            .or(file.failure_grace_secs)
            .unwrap_or(30);
        if poll_secs == 0 || grace_secs == 0 {
            bail!("poll_interval_secs and failure_grace_secs must be positive");
        }
        let normalize_dimensions = args
            .normalize_dimensions
            .or(file.normalize_dimensions)
            .unwrap_or(true);

        let source_id = args.source_id.or(file.source_id).unwrap_or_else(|| {
            let seed = format!("{}|{}|{}", relay_url, room_code, source_url);
            Uuid::new_v5(&Uuid::NAMESPACE_URL, seed.as_bytes())
        });

        Ok(Self {
            relay_url,
            room_code,
            source_url,
            display_name,
            poll_interval: Duration::from_secs(poll_secs),
            failure_grace: Duration::from_secs(grace_secs),
            normalize_dimensions,
            source_id,
        })
    }
}
