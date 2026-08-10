mod config;
mod model;
mod proto;
mod publisher;
mod relay;
mod upstream;

use anyhow::Result;
use tokio::sync::mpsc;
use tokio::time::{Instant, MissedTickBehavior};
use tracing::{info, warn};
use tracing_subscriber::EnvFilter;

use crate::config::Config;
use crate::publisher::{PublishAction, Publisher};
use crate::relay::RelayEvent;
use crate::upstream::{PollError, PollOutcome, UpstreamClient};

enum PollEvent {
    Success(PollOutcome),
    Failed(PollError),
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    let config = Config::load()?;
    let source_id = config.source_id.hyphenated().to_string();
    info!(
        %source_id,
        relay = %config.relay_url,
        source = %config.source_url,
        normalize_dimensions = config.normalize_dimensions,
        "Squaremap source client starting"
    );

    let (poll_tx, mut poll_rx) = mpsc::channel(4);
    let upstream_config = config.clone();
    let poll_task = tokio::spawn(async move {
        let mut upstream = match UpstreamClient::new(
            upstream_config.source_url.clone(),
            upstream_config.normalize_dimensions,
        ) {
            Ok(client) => client,
            Err(error) => {
                warn!(%error, "Failed to initialize upstream HTTP client");
                return;
            }
        };
        let mut interval = tokio::time::interval(upstream_config.poll_interval);
        interval.set_missed_tick_behavior(MissedTickBehavior::Skip);
        interval.tick().await;
        loop {
            let event = match upstream.poll().await {
                Ok(outcome) => PollEvent::Success(outcome),
                Err(error) => PollEvent::Failed(error),
            };
            if poll_tx.send(event).await.is_err() {
                return;
            }
            interval.tick().await;
        }
    });

    let (relay_tx, relay_rx) = mpsc::unbounded_channel();
    let (event_tx, mut event_rx) = mpsc::unbounded_channel();
    let relay_config = config.clone();
    let relay_task = tokio::spawn(relay::run(relay_config, relay_rx, event_tx));
    let mut publisher = Publisher::new(config.failure_grace);
    let mut replace_supported = false;

    loop {
        tokio::select! {
            event = event_rx.recv() => {
                let Some(event) = event else {
                    break;
                };
                match event {
                    RelayEvent::Connected { replace_supported: supported } => {
                        replace_supported = supported;
                        send_actions(
                            &relay_tx,
                            &source_id,
                            publisher.relay_connected(),
                            replace_supported,
                        );
                    }
                    RelayEvent::Disconnected => publisher.relay_disconnected(),
                }
            }
            event = poll_rx.recv() => {
                let Some(event) = event else {
                    break;
                };
                let actions = match event {
                    PollEvent::Success(PollOutcome::Snapshot(snapshot)) => {
                        if snapshot.skipped_entries > 0 {
                            warn!(skipped = snapshot.skipped_entries, "Skipped invalid upstream player entries");
                        }
                        publisher.upstream_succeeded(Some(snapshot))
                    }
                    PollEvent::Success(PollOutcome::NotModified) => publisher.upstream_succeeded(None),
                    PollEvent::Failed(error) => {
                        warn!(code = error.code, detail = %error.detail, "Squaremap upstream poll failed");
                        publisher.upstream_failed(error.code, Instant::now())
                    }
                };
                send_actions(&relay_tx, &source_id, actions, replace_supported);
            }
            result = tokio::signal::ctrl_c() => {
                result?;
                info!("Shutdown requested");
                break;
            }
        }
    }

    poll_task.abort();
    drop(relay_tx);
    relay_task.abort();
    Ok(())
}

fn send_actions(
    relay_tx: &mpsc::UnboundedSender<Vec<u8>>,
    source_id: &str,
    actions: Vec<PublishAction>,
    replace_supported: bool,
) {
    for action in actions {
        for packet in relay::action_packets(source_id, action, replace_supported) {
            if relay_tx.send(packet).is_err() {
                return;
            }
        }
    }
}
