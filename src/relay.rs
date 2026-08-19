use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use futures_util::{SinkExt, StreamExt};
use prost::Message as ProstMessage;
use tokio::sync::mpsc;
use tokio::time::{MissedTickBehavior, timeout};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async};
use tracing::{info, warn};

use crate::config::Config;
use crate::history::{HistoryDelta, LastSeenRecord};
use crate::model::{PlayerRecord, Snapshot, TabRecord};
use crate::proto::wire_envelope::Payload;
use crate::proto::{
    ClientRole, ExternalSourceStatus, HandshakeAck, LastSeenPlayerData, LastSeenPlayerPatchScope,
    LastSeenPlayerUpsert, LastSeenPlayersReplace, PlayerData, PlayerDelta, PlayerHandshakeRequest,
    PlayerPatchScope, PlayerReportBundle, PlayerUpsert, PlayersReplace, SourceStateClear,
    StateKeepalive, TabPlayerEntry, TabPlayerUpsert, TabPlayersPatchScope, TabPlayersReplace,
    WireChannel, WireEnvelope,
};
use crate::publisher::PublishAction;

const NETWORK_PROTOCOL_VERSION: &str = "0.6.4";
const MINIMUM_COMPATIBLE_PROTOCOL_VERSION: &str = "0.6.1";
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

pub enum RelayEvent {
    Connected {
        replace_supported: bool,
        history_supported: bool,
    },
    Disconnected,
}

type RelaySocket = WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>;

pub async fn run(
    config: Config,
    mut commands: mpsc::UnboundedReceiver<Vec<u8>>,
    events: mpsc::UnboundedSender<RelayEvent>,
) {
    let mut retry_delay = Duration::from_secs(1);
    loop {
        match connect_and_handshake(&config).await {
            Ok((socket, replace_supported, history_supported)) => {
                retry_delay = Duration::from_secs(1);
                if events
                    .send(RelayEvent::Connected {
                        replace_supported,
                        history_supported,
                    })
                    .is_err()
                {
                    return;
                }
                if let Err(error) = run_connected(socket, &mut commands).await {
                    warn!(%error, "Relay connection closed");
                }
                if events.send(RelayEvent::Disconnected).is_err() {
                    return;
                }
            }
            Err(error) => warn!(%error, "Relay connection failed"),
        }

        tokio::select! {
            _ = tokio::time::sleep(retry_delay) => {}
            command = commands.recv() => {
                if command.is_none() {
                    return;
                }
            }
        }
        retry_delay = (retry_delay * 2).min(Duration::from_secs(30));
    }
}

async fn connect_and_handshake(config: &Config) -> Result<(RelaySocket, bool, bool)> {
    let (mut socket, _) = timeout(CONNECT_TIMEOUT, connect_async(config.relay_url.as_str()))
        .await
        .context("Relay connection timed out")?
        .context("connect Relay websocket")?;

    socket
        .send(Message::Binary(handshake_packet(config).into()))
        .await
        .context("send Relay handshake")?;

    let ack = timeout(HANDSHAKE_TIMEOUT, async {
        loop {
            match socket.next().await {
                Some(Ok(Message::Binary(bytes))) => {
                    let envelope = WireEnvelope::decode(bytes.as_ref())
                        .context("decode Relay handshake response")?;
                    if let Some(Payload::HandshakeAck(ack)) = envelope.payload {
                        return Ok::<HandshakeAck, anyhow::Error>(ack);
                    }
                }
                Some(Ok(Message::Ping(payload))) => {
                    socket
                        .send(Message::Pong(payload))
                        .await
                        .context("reply to Relay ping")?;
                }
                Some(Ok(Message::Close(frame))) => {
                    bail!("Relay closed during handshake: {frame:?}");
                }
                Some(Ok(_)) => {}
                Some(Err(error)) => {
                    return Err(anyhow!(error).context("read Relay handshake response"));
                }
                None => bail!("Relay closed before handshake acknowledgement"),
            }
        }
    })
    .await
    .context("Relay handshake timed out")??;

    if !ack.ready {
        bail!(
            "Relay rejected handshake: {}",
            ack.reject_reason
                .or(ack.error)
                .unwrap_or_else(|| "unknown reason".to_owned())
        );
    }
    let replace_supported = match ack.accepted_client_role {
        Some(role) if role == ClientRole::ExternalSource as i32 => true,
        Some(role) => {
            warn!(
                accepted_role = role,
                "Relay did not accept external-source role; compatibility mode enabled"
            );
            false
        }
        None => {
            warn!(
                "Relay did not return accepted_client_role; it is probably older than protocol 0.6.3"
            );
            false
        }
    };

    info!(
        relay = %config.relay_url,
        protocol = %ack.network_protocol_version,
        room = %ack.room_code,
        "Relay connected"
    );
    let history_supported =
        replace_supported && protocol_at_least(&ack.network_protocol_version, "0.6.4");
    if !history_supported {
        warn!(
            protocol = %ack.network_protocol_version,
            "Relay does not support last-seen player history; realtime reporting remains enabled"
        );
    }
    Ok((socket, replace_supported, history_supported))
}

async fn run_connected(
    socket: RelaySocket,
    commands: &mut mpsc::UnboundedReceiver<Vec<u8>>,
) -> Result<()> {
    let (mut sink, mut stream) = socket.split();
    let mut ping = tokio::time::interval(Duration::from_secs(20));
    ping.set_missed_tick_behavior(MissedTickBehavior::Skip);
    ping.tick().await;

    loop {
        tokio::select! {
            command = commands.recv() => {
                let Some(packet) = command else {
                    return Ok(());
                };
                sink.send(Message::Binary(packet.into())).await.context("send Relay packet")?;
            }
            incoming = stream.next() => {
                match incoming {
                    Some(Ok(Message::Ping(payload))) => {
                        sink.send(Message::Pong(payload)).await.context("reply to Relay ping")?;
                    }
                    Some(Ok(Message::Close(frame))) => bail!("Relay closed websocket: {frame:?}"),
                    Some(Ok(_)) => {}
                    Some(Err(error)) => return Err(anyhow!(error).context("read Relay websocket")),
                    None => bail!("Relay websocket ended"),
                }
            }
            _ = ping.tick() => {
                sink.send(Message::Ping(Vec::new().into())).await.context("ping Relay")?;
            }
        }
    }
}

fn handshake_packet(config: &Config) -> Vec<u8> {
    encode(Payload::PlayerHandshakeRequest(PlayerHandshakeRequest {
        network_protocol_version: NETWORK_PROTOCOL_VERSION.to_owned(),
        minimum_compatible_network_protocol_version: MINIMUM_COMPATIBLE_PROTOCOL_VERSION.to_owned(),
        local_program_version: env!("CARGO_PKG_VERSION").to_owned(),
        submit_player_id: config.source_id.hyphenated().to_string(),
        room_code: Some(config.room_code.clone()),
        preferred_report_interval_ticks: None,
        min_report_interval_ticks: None,
        max_report_interval_ticks: None,
        client_role: Some(ClientRole::ExternalSource as i32),
        client_display_name: Some(config.display_name.clone()),
    }))
}

pub fn action_packets(
    source_id: &str,
    action: PublishAction,
    replace_supported: bool,
) -> Vec<Vec<u8>> {
    let bundle = match action {
        PublishAction::Status {
            health,
            failure_code,
        } => PlayerReportBundle {
            submit_player_id: source_id.to_owned(),
            external_source_status: Some(ExternalSourceStatus {
                health: health as i32,
                failure_code,
            }),
            ..Default::default()
        },
        PublishAction::Full { previous, current } => {
            if replace_supported {
                full_bundle(source_id, current)
            } else {
                full_patch_bundle(source_id, previous, current)
            }
        }
        PublishAction::Patch { previous, current } => patch_bundle(source_id, previous, current),
        PublishAction::Keepalive(players) => PlayerReportBundle {
            submit_player_id: source_id.to_owned(),
            state_keepalive: Some(StateKeepalive {
                players,
                entities: Vec::new(),
                battle_chunks: Vec::new(),
            }),
            ..Default::default()
        },
        PublishAction::Clear => PlayerReportBundle {
            submit_player_id: source_id.to_owned(),
            source_state_clear: Some(SourceStateClear {
                scopes: vec!["players".to_owned(), "tab_players".to_owned()],
            }),
            ..Default::default()
        },
    };
    vec![encode(Payload::PlayerReportBundle(bundle))]
}

pub fn history_full_packet(
    source_id: &str,
    players: std::collections::BTreeMap<String, LastSeenRecord>,
) -> Vec<u8> {
    encode(Payload::PlayerReportBundle(PlayerReportBundle {
        submit_player_id: source_id.to_owned(),
        last_seen_players_replace: Some(LastSeenPlayersReplace {
            players: players
                .into_iter()
                .map(|(id, value)| (id, last_seen_player_data(value)))
                .collect(),
        }),
        ..Default::default()
    }))
}

pub fn history_patch_packet(source_id: &str, delta: HistoryDelta) -> Vec<u8> {
    encode(Payload::PlayerReportBundle(PlayerReportBundle {
        submit_player_id: source_id.to_owned(),
        last_seen_players_patch: Some(LastSeenPlayerPatchScope {
            upsert: delta
                .upsert
                .into_iter()
                .map(|(id, value)| LastSeenPlayerUpsert {
                    id,
                    data: Some(last_seen_player_data(value)),
                })
                .collect(),
            delete: delta.delete,
        }),
        ..Default::default()
    }))
}

fn full_bundle(source_id: &str, snapshot: Snapshot) -> PlayerReportBundle {
    PlayerReportBundle {
        submit_player_id: source_id.to_owned(),
        players_replace: Some(PlayersReplace {
            players: snapshot
                .players
                .into_iter()
                .map(|(id, player)| (id, player_data(player)))
                .collect(),
        }),
        tab_players_replace: Some(TabPlayersReplace {
            tab_players: snapshot
                .tab_players
                .into_values()
                .map(tab_player_entry)
                .collect(),
        }),
        ..Default::default()
    }
}

fn full_patch_bundle(
    source_id: &str,
    previous: Option<Snapshot>,
    current: Snapshot,
) -> PlayerReportBundle {
    let previous = previous.unwrap_or_default();
    PlayerReportBundle {
        submit_player_id: source_id.to_owned(),
        players_patch: Some(PlayerPatchScope {
            upsert: current
                .players
                .values()
                .cloned()
                .map(|player| PlayerUpsert {
                    id: player.uuid.clone(),
                    data: Some(player_delta(player)),
                })
                .collect(),
            delete: previous
                .players
                .keys()
                .filter(|id| !current.players.contains_key(*id))
                .cloned()
                .collect(),
        }),
        tab_players_patch: Some(TabPlayersPatchScope {
            upsert: current
                .tab_players
                .values()
                .cloned()
                .map(|player| TabPlayerUpsert {
                    key: player.uuid.clone(),
                    data: Some(tab_player_entry(player)),
                })
                .collect(),
            delete: previous
                .tab_players
                .keys()
                .filter(|id| !current.tab_players.contains_key(*id))
                .cloned()
                .collect(),
        }),
        ..Default::default()
    }
}

fn patch_bundle(source_id: &str, previous: Snapshot, current: Snapshot) -> PlayerReportBundle {
    let player_upserts = current
        .players
        .iter()
        .filter(|(id, value)| previous.players.get(*id) != Some(*value))
        .map(|(id, value)| PlayerUpsert {
            id: id.clone(),
            data: Some(player_delta(value.clone())),
        })
        .collect();
    let player_deletes = previous
        .players
        .keys()
        .filter(|id| !current.players.contains_key(*id))
        .cloned()
        .collect();
    let tab_upserts = current
        .tab_players
        .iter()
        .filter(|(id, value)| previous.tab_players.get(*id) != Some(*value))
        .map(|(id, value)| TabPlayerUpsert {
            key: id.clone(),
            data: Some(tab_player_entry(value.clone())),
        })
        .collect();
    let tab_deletes = previous
        .tab_players
        .keys()
        .filter(|id| !current.tab_players.contains_key(*id))
        .cloned()
        .collect();

    PlayerReportBundle {
        submit_player_id: source_id.to_owned(),
        players_patch: Some(PlayerPatchScope {
            upsert: player_upserts,
            delete: player_deletes,
        }),
        tab_players_patch: Some(TabPlayersPatchScope {
            upsert: tab_upserts,
            delete: tab_deletes,
        }),
        ..Default::default()
    }
}

fn player_data(player: PlayerRecord) -> PlayerData {
    PlayerData {
        x: player.x,
        y: player.y,
        z: player.z,
        dimension: player.dimension,
        player_name: Some(player.name),
        player_uuid: Some(player.uuid),
        health: player.health,
        armor: player.armor,
        ..Default::default()
    }
}

fn player_delta(player: PlayerRecord) -> PlayerDelta {
    PlayerDelta {
        x: Some(player.x),
        y: Some(player.y),
        z: Some(player.z),
        dimension: Some(player.dimension),
        player_name: Some(player.name),
        player_uuid: Some(player.uuid),
        health: player.health,
        armor: player.armor,
        ..Default::default()
    }
}

fn tab_player_entry(player: TabRecord) -> TabPlayerEntry {
    TabPlayerEntry {
        uuid: Some(player.uuid),
        name: Some(player.name),
        display_name: None,
        prefixed_name: None,
    }
}

fn last_seen_player_data(record: LastSeenRecord) -> LastSeenPlayerData {
    LastSeenPlayerData {
        x: record.x,
        y: record.y,
        z: record.z,
        dimension: record.dimension,
        player_name: record.player_name,
        player_uuid: record.player_uuid,
        last_seen_at_utc_ms: record.last_seen_at_utc_ms,
        position_observed_at_utc_ms: record.position_observed_at_utc_ms,
        offline_detected_at_utc_ms: record.offline_detected_at_utc_ms,
    }
}

fn protocol_at_least(current: &str, minimum: &str) -> bool {
    fn parts(version: &str) -> [u64; 3] {
        let mut result = [0; 3];
        for (index, part) in version.trim().split('.').take(3).enumerate() {
            result[index] = part.parse().unwrap_or(0);
        }
        result
    }
    parts(current) >= parts(minimum)
}

fn encode(payload: Payload) -> Vec<u8> {
    WireEnvelope {
        channel: WireChannel::Player as i32,
        payload: Some(payload),
    }
    .encode_to_vec()
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;
    use crate::model::{PlayerRecord, TabRecord};

    fn sample_snapshot(x: f64) -> Snapshot {
        let uuid = "276326ac-dbf7-4083-ae6e-ba6e9eaca2e1".to_owned();
        Snapshot {
            players: BTreeMap::from([(
                uuid.clone(),
                PlayerRecord {
                    x,
                    y: 2.0,
                    z: 3.0,
                    dimension: "minecraft_overworld".to_owned(),
                    name: "Player".to_owned(),
                    uuid: uuid.clone(),
                    health: Some(20.0),
                    armor: Some(5.0),
                },
            )]),
            tab_players: BTreeMap::from([(
                uuid.clone(),
                TabRecord {
                    uuid,
                    name: "Player".to_owned(),
                },
            )]),
            skipped_entries: 0,
        }
    }

    #[test]
    fn patch_contains_changed_player_and_tab_deletions() {
        let previous = sample_snapshot(1.0);
        let mut current = sample_snapshot(2.0);
        current.tab_players.clear();
        let packets = action_packets("source", PublishAction::Patch { previous, current }, true);
        let envelope = WireEnvelope::decode(packets[0].as_slice()).unwrap();
        let Some(Payload::PlayerReportBundle(bundle)) = envelope.payload else {
            panic!("expected report bundle");
        };
        assert_eq!(bundle.players_patch.unwrap().upsert.len(), 1);
        assert_eq!(bundle.tab_players_patch.unwrap().delete.len(), 1);
    }

    #[test]
    fn compatibility_full_patch_preserves_zero_coordinates() {
        let mut current = sample_snapshot(0.0);
        current.players.values_mut().next().unwrap().z = 0.0;
        let packets = action_packets(
            "source",
            PublishAction::Full {
                previous: None,
                current,
            },
            false,
        );
        let envelope = WireEnvelope::decode(packets[0].as_slice()).unwrap();
        let Some(Payload::PlayerReportBundle(bundle)) = envelope.payload else {
            panic!("expected report bundle");
        };
        let delta = bundle.players_patch.unwrap().upsert.remove(0).data.unwrap();
        assert_eq!(delta.x, Some(0.0));
        assert_eq!(delta.z, Some(0.0));
    }
}
