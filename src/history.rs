use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use tracing::warn;

use crate::config::Config;
use crate::model::Snapshot;

const STATE_SCHEMA_VERSION: u32 = 1;
const CLEANUP_INTERVAL: Duration = Duration::from_secs(24 * 60 * 60);

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct LastSeenRecord {
    pub x: f64,
    pub y: f64,
    pub z: f64,
    pub dimension: String,
    pub player_name: String,
    pub player_uuid: String,
    pub last_seen_at_utc_ms: i64,
    pub position_observed_at_utc_ms: i64,
    pub offline_detected_at_utc_ms: i64,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct HistoryDelta {
    pub upsert: BTreeMap<String, LastSeenRecord>,
    pub delete: Vec<String>,
}

impl HistoryDelta {
    pub fn is_empty(&self) -> bool {
        self.upsert.is_empty() && self.delete.is_empty()
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct PositionState {
    x: f64,
    y: f64,
    z: f64,
    dimension: String,
    observed_at_utc_ms: i64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct PlayerHistory {
    uuid: String,
    name: String,
    last_seen_at_utc_ms: i64,
    position: Option<PositionState>,
    online: bool,
    offline_detected_at_utc_ms: Option<i64>,
}

#[derive(Debug, Serialize, Deserialize)]
struct StateFile {
    schema_version: u32,
    source_id: String,
    room_code: String,
    source_url: String,
    players: BTreeMap<String, PlayerHistory>,
}

pub struct HistoryStore {
    path: PathBuf,
    source_id: String,
    room_code: String,
    source_url: String,
    retention: Option<Duration>,
    flush_interval: Duration,
    players: BTreeMap<String, PlayerHistory>,
    dirty: bool,
    last_flush: Instant,
    last_cleanup: Instant,
}

impl HistoryStore {
    pub fn load(config: &Config) -> Result<Self> {
        let source_id = config.source_id.hyphenated().to_string();
        let source_url = config.source_url.as_str().to_owned();
        let players = load_state(&config.history_state_path, &source_id, &config.room_code)?;
        let mut store = Self {
            path: config.history_state_path.clone(),
            source_id,
            room_code: config.room_code.clone(),
            source_url,
            retention: (config.history_retention_days != 0).then(|| {
                Duration::from_secs(config.history_retention_days.saturating_mul(24 * 60 * 60))
            }),
            flush_interval: config.history_flush_interval,
            players,
            dirty: false,
            last_flush: Instant::now(),
            last_cleanup: Instant::now(),
        };
        store.cleanup_expired(utc_now_ms());
        store.flush_urgent()?;
        Ok(store)
    }

    pub fn observe_snapshot(&mut self, snapshot: &Snapshot, now_ms: i64) -> HistoryDelta {
        let current_ids: BTreeSet<&String> = snapshot.tab_players.keys().collect();
        let previously_online: Vec<String> = self
            .players
            .iter()
            .filter(|(_, value)| value.online)
            .map(|(id, _)| id.clone())
            .collect();
        let mut delta = HistoryDelta::default();

        for (id, tab) in &snapshot.tab_players {
            let entry = self
                .players
                .entry(id.clone())
                .or_insert_with(|| PlayerHistory {
                    uuid: id.clone(),
                    name: tab.name.clone(),
                    last_seen_at_utc_ms: now_ms,
                    position: None,
                    online: true,
                    offline_detected_at_utc_ms: None,
                });
            if !entry.online && entry.position.is_some() {
                delta.delete.push(id.clone());
            }
            entry.name = tab.name.clone();
            entry.last_seen_at_utc_ms = now_ms.max(entry.last_seen_at_utc_ms);
            entry.online = true;
            entry.offline_detected_at_utc_ms = None;
            if let Some(player) = snapshot.players.get(id) {
                entry.position = Some(PositionState {
                    x: player.x,
                    y: player.y,
                    z: player.z,
                    dimension: player.dimension.clone(),
                    observed_at_utc_ms: now_ms.max(
                        entry
                            .position
                            .as_ref()
                            .map_or(0, |position| position.observed_at_utc_ms),
                    ),
                });
            }
        }

        for id in previously_online {
            if current_ids.contains(&id) {
                continue;
            }
            if let Some(entry) = self.players.get_mut(&id) {
                entry.online = false;
                let offline_at = now_ms.max(entry.last_seen_at_utc_ms);
                entry.offline_detected_at_utc_ms = Some(offline_at);
                if let Some(record) = to_last_seen(entry) {
                    delta.upsert.insert(id, record);
                }
            }
        }

        self.dirty = true;
        delta
    }

    pub fn confirm_not_modified(&mut self, now_ms: i64) {
        for entry in self.players.values_mut().filter(|entry| entry.online) {
            entry.last_seen_at_utc_ms = now_ms.max(entry.last_seen_at_utc_ms);
        }
        self.dirty = true;
    }

    pub fn snapshot(&self) -> BTreeMap<String, LastSeenRecord> {
        self.players
            .iter()
            .filter_map(|(id, entry)| to_last_seen(entry).map(|record| (id.clone(), record)))
            .collect()
    }

    pub fn maintenance(&mut self, now_ms: i64) -> Result<HistoryDelta> {
        let mut delta = HistoryDelta::default();
        if self.last_cleanup.elapsed() >= CLEANUP_INTERVAL {
            self.last_cleanup = Instant::now();
            delta.delete = self.cleanup_expired(now_ms);
        }
        self.flush_if_due()?;
        Ok(delta)
    }

    fn cleanup_expired(&mut self, now_ms: i64) -> Vec<String> {
        let Some(retention) = self.retention else {
            return Vec::new();
        };
        let cutoff = now_ms.saturating_sub(retention.as_millis().min(i64::MAX as u128) as i64);
        let mut deleted = Vec::new();
        self.players.retain(|id, entry| {
            let expired = !entry.online
                && entry
                    .offline_detected_at_utc_ms
                    .is_some_and(|offline_at| offline_at < cutoff);
            if expired {
                deleted.push(id.clone());
            }
            !expired
        });
        self.dirty |= !deleted.is_empty();
        deleted
    }

    pub fn flush_urgent(&mut self) -> Result<()> {
        self.flush(true)
    }

    pub fn flush_if_due(&mut self) -> Result<()> {
        self.flush(self.last_flush.elapsed() >= self.flush_interval)
    }

    fn flush(&mut self, force: bool) -> Result<()> {
        if !self.dirty || !force {
            return Ok(());
        }
        let state = StateFile {
            schema_version: STATE_SCHEMA_VERSION,
            source_id: self.source_id.clone(),
            room_code: self.room_code.clone(),
            source_url: self.source_url.clone(),
            players: self.players.clone(),
        };
        write_atomic(&self.path, &state)?;
        self.dirty = false;
        self.last_flush = Instant::now();
        Ok(())
    }
}

pub fn utc_now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(i64::MAX as u128) as i64
}

fn to_last_seen(entry: &PlayerHistory) -> Option<LastSeenRecord> {
    let position = entry.position.as_ref()?;
    let offline_at = entry.offline_detected_at_utc_ms?;
    (!entry.online).then(|| LastSeenRecord {
        x: position.x,
        y: position.y,
        z: position.z,
        dimension: position.dimension.clone(),
        player_name: entry.name.clone(),
        player_uuid: entry.uuid.clone(),
        last_seen_at_utc_ms: entry.last_seen_at_utc_ms,
        position_observed_at_utc_ms: position.observed_at_utc_ms,
        offline_detected_at_utc_ms: offline_at,
    })
}

fn load_state(
    path: &Path,
    source_id: &str,
    room_code: &str,
) -> Result<BTreeMap<String, PlayerHistory>> {
    let raw = match fs::read(path) {
        Ok(raw) => raw,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(BTreeMap::new()),
        Err(error) => return Err(error).with_context(|| format!("read {}", path.display())),
    };
    let state: StateFile = match serde_json::from_slice(&raw) {
        Ok(state) => state,
        Err(error) => {
            let corrupt_path = path.with_extension(format!("corrupt-{}", utc_now_ms()));
            fs::rename(path, &corrupt_path).with_context(|| {
                format!("preserve corrupt history as {}", corrupt_path.display())
            })?;
            warn!(%error, path = %corrupt_path.display(), "Preserved corrupt history state");
            return Ok(BTreeMap::new());
        }
    };
    if state.schema_version != STATE_SCHEMA_VERSION {
        bail!(
            "unsupported history schema version {}",
            state.schema_version
        );
    }
    if state.source_id != source_id || state.room_code != room_code {
        bail!("history state identity does not match source_id and room_code");
    }
    Ok(state.players)
}

fn write_atomic(path: &Path, state: &StateFile) -> Result<()> {
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty());
    if let Some(parent) = parent {
        fs::create_dir_all(parent)
            .with_context(|| format!("create history directory {}", parent.display()))?;
    }
    let temp = path.with_extension("tmp");
    {
        let file = File::create(&temp)
            .with_context(|| format!("create history temp file {}", temp.display()))?;
        let mut writer = BufWriter::new(file);
        serde_json::to_writer(&mut writer, state).context("serialize history state")?;
        writer.flush().context("flush history state")?;
        writer.get_ref().sync_all().context("sync history state")?;
    }
    #[cfg(windows)]
    if path.exists() {
        fs::remove_file(path)
            .with_context(|| format!("replace history file {}", path.display()))?;
    }
    fs::rename(&temp, path).with_context(|| format!("install history file {}", path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;
    use crate::model::{PlayerRecord, TabRecord};

    fn snapshot(with_position: bool) -> Snapshot {
        let id = "276326ac-dbf7-4083-ae6e-ba6e9eaca2e1".to_owned();
        Snapshot {
            players: if with_position {
                BTreeMap::from([(
                    id.clone(),
                    PlayerRecord {
                        x: 1.0,
                        y: 2.0,
                        z: 3.0,
                        dimension: "minecraft:overworld".to_owned(),
                        name: "Player".to_owned(),
                        uuid: id.clone(),
                        health: None,
                        armor: None,
                    },
                )])
            } else {
                BTreeMap::new()
            },
            tab_players: BTreeMap::from([(
                id.clone(),
                TabRecord {
                    uuid: id,
                    name: "Player".to_owned(),
                },
            )]),
            skipped_entries: 0,
        }
    }

    fn store(retention_days: u64) -> HistoryStore {
        HistoryStore {
            path: PathBuf::from("unused"),
            source_id: "source".to_owned(),
            room_code: "room".to_owned(),
            source_url: "https://example.test/players.json".to_owned(),
            retention: (retention_days != 0)
                .then(|| Duration::from_secs(retention_days * 24 * 60 * 60)),
            flush_interval: Duration::from_secs(60),
            players: BTreeMap::new(),
            dirty: false,
            last_flush: Instant::now(),
            last_cleanup: Instant::now(),
        }
    }

    #[test]
    fn records_offline_position_and_deletes_it_on_return() {
        let mut history = store(90);
        history.observe_snapshot(&snapshot(true), 1_000);
        let offline = history.observe_snapshot(&Snapshot::default(), 2_000);
        assert_eq!(offline.upsert.len(), 1);
        let record = offline.upsert.values().next().unwrap();
        assert_eq!(record.last_seen_at_utc_ms, 1_000);
        assert_eq!(record.offline_detected_at_utc_ms, 2_000);

        let online = history.observe_snapshot(&snapshot(false), 3_000);
        assert_eq!(online.delete.len(), 1);
        assert!(history.snapshot().is_empty());
    }

    #[test]
    fn tab_only_player_is_online_but_has_no_renderable_history() {
        let mut history = store(90);
        history.observe_snapshot(&snapshot(false), 1_000);
        let offline = history.observe_snapshot(&Snapshot::default(), 2_000);
        assert!(offline.is_empty());
    }

    #[test]
    fn not_modified_refreshes_online_confirmation_only() {
        let mut history = store(90);
        history.observe_snapshot(&snapshot(true), 1_000);
        history.confirm_not_modified(2_000);
        let offline = history.observe_snapshot(&Snapshot::default(), 3_000);
        let record = offline.upsert.values().next().unwrap();
        assert_eq!(record.last_seen_at_utc_ms, 2_000);
        assert_eq!(record.position_observed_at_utc_ms, 1_000);
    }

    #[test]
    fn retention_removes_only_old_offline_records() {
        let mut history = store(90);
        history.observe_snapshot(&snapshot(true), 1_000);
        history.observe_snapshot(&Snapshot::default(), 2_000);
        let after_90_days = 2_000 + 91 * 24 * 60 * 60 * 1_000;
        assert_eq!(history.cleanup_expired(after_90_days).len(), 1);
        assert!(history.snapshot().is_empty());
    }

    #[test]
    fn zero_retention_days_keeps_history_forever() {
        let mut history = store(0);
        history.observe_snapshot(&snapshot(true), 1_000);
        history.observe_snapshot(&Snapshot::default(), 2_000);
        assert!(history.cleanup_expired(i64::MAX).is_empty());
        assert_eq!(history.snapshot().len(), 1);
    }

    #[test]
    fn history_identity_survives_transport_url_change() {
        let path = std::env::temp_dir().join(format!(
            "teamviewrelay-history-url-change-{}-{}.json",
            std::process::id(),
            utc_now_ms()
        ));
        let state = StateFile {
            schema_version: STATE_SCHEMA_VERSION,
            source_id: "source".to_owned(),
            room_code: "room".to_owned(),
            source_url: "https://map1.nodemc.cc/tiles/players.json".to_owned(),
            players: BTreeMap::new(),
        };
        fs::write(&path, serde_json::to_vec(&state).unwrap()).unwrap();

        let loaded = load_state(&path, "source", "room").unwrap();
        assert!(loaded.is_empty());
        fs::remove_file(path).unwrap();
    }
}
