use std::collections::BTreeMap;

use anyhow::{Result, bail};
use serde_json::Value;
use uuid::Uuid;

#[derive(Clone, Debug, PartialEq)]
pub struct PlayerRecord {
    pub x: f64,
    pub y: f64,
    pub z: f64,
    pub dimension: String,
    pub name: String,
    pub uuid: String,
    pub health: Option<f64>,
    pub armor: Option<f64>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TabRecord {
    pub uuid: String,
    pub name: String,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Snapshot {
    pub players: BTreeMap<String, PlayerRecord>,
    pub tab_players: BTreeMap<String, TabRecord>,
    pub skipped_entries: usize,
}

fn finite_number(value: Option<&Value>) -> Option<f64> {
    let number = value?.as_f64()?;
    number.is_finite().then_some(number)
}

fn normalize_dimension(world: &str, enabled: bool) -> String {
    if !enabled || world.contains(':') {
        return world.to_owned();
    }
    match world.split_once('_') {
        Some((namespace, path)) if !namespace.is_empty() && !path.is_empty() => {
            format!("{namespace}:{path}")
        }
        _ => world.to_owned(),
    }
}

pub fn parse_snapshot(body: &[u8], normalize_dimensions: bool) -> Result<Snapshot> {
    let root: Value = serde_json::from_slice(body)?;
    let root_object = root
        .as_object()
        .ok_or_else(|| anyhow::anyhow!("root_not_object"))?;
    let raw_players = root_object
        .get("players")
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow::anyhow!("players_not_array"))?;

    let mut snapshot = Snapshot::default();
    let mut valid_identities = 0usize;

    for raw in raw_players {
        let Some(item) = raw.as_object() else {
            snapshot.skipped_entries += 1;
            continue;
        };
        let Some(raw_uuid) = item.get("uuid").and_then(Value::as_str) else {
            snapshot.skipped_entries += 1;
            continue;
        };
        let Ok(uuid) = Uuid::parse_str(raw_uuid.trim()) else {
            snapshot.skipped_entries += 1;
            continue;
        };
        let Some(name) = item
            .get("name")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|v| !v.is_empty())
        else {
            snapshot.skipped_entries += 1;
            continue;
        };

        valid_identities += 1;
        let uuid_text = uuid.hyphenated().to_string().to_lowercase();
        snapshot.tab_players.insert(
            uuid_text.clone(),
            TabRecord {
                uuid: uuid_text.clone(),
                name: name.to_owned(),
            },
        );

        snapshot.players.remove(&uuid_text);
        let world = item
            .get("world")
            .and_then(Value::as_str)
            .filter(|value| !value.trim().is_empty());
        let coordinates = (
            finite_number(item.get("x")),
            finite_number(item.get("y")),
            finite_number(item.get("z")),
        );
        if let (Some(dimension), (Some(x), Some(y), Some(z))) = (world, coordinates) {
            snapshot.players.insert(
                uuid_text.clone(),
                PlayerRecord {
                    x,
                    y,
                    z,
                    dimension: normalize_dimension(dimension, normalize_dimensions),
                    name: name.to_owned(),
                    uuid: uuid_text,
                    health: finite_number(item.get("health")),
                    armor: finite_number(item.get("armor")),
                },
            );
        }
    }

    if !raw_players.is_empty() && valid_identities == 0 {
        bail!("no_valid_player_identity");
    }
    Ok(snapshot)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_positions_and_tab_only_entries() {
        let snapshot = parse_snapshot(
            br#"{"max":790,"players":[{"world":"minecraft_overworld","name":"Moving","uuid":"276326acdbf74083ae6eba6e9eaca2e1","x":1,"y":2,"z":3,"health":23,"armor":5},{"world":"minecraft_overworld","name":"TabOnly","uuid":"228a1b7dc334419c8b50fecd6a6134ff"}]}"#,
            true,
        )
        .unwrap();

        assert_eq!(snapshot.players.len(), 1);
        assert_eq!(snapshot.tab_players.len(), 2);
        let player = snapshot.players.values().next().unwrap();
        assert_eq!(player.dimension, "minecraft:overworld");
        assert_eq!(player.health, Some(23.0));
    }

    #[test]
    fn rejects_nonempty_completely_invalid_schema() {
        assert!(parse_snapshot(br#"{"players":[{"uuid":"bad"}]}"#, true).is_err());
    }

    #[test]
    fn accepts_authoritative_empty_roster() {
        assert_eq!(
            parse_snapshot(br#"{"players":[]}"#, true).unwrap(),
            Snapshot::default()
        );
    }

    #[test]
    fn duplicate_uuid_uses_last_entry() {
        let snapshot = parse_snapshot(
            br#"{"players":[{"name":"Old","uuid":"276326acdbf74083ae6eba6e9eaca2e1","world":"a","x":1,"y":2,"z":3},{"name":"New","uuid":"276326acdbf74083ae6eba6e9eaca2e1","world":"b","x":4,"y":5,"z":6}]}"#,
            true,
        )
        .unwrap();
        assert_eq!(snapshot.players.values().next().unwrap().name, "New");
    }

    #[test]
    fn normalizes_uuid_and_skips_only_bad_entries() {
        let snapshot = parse_snapshot(
            br#"{"players":[{"name":"Bad","uuid":"bad"},{"name":"Good","uuid":"276326acdbf74083ae6eba6e9eaca2e1","world":" minecraft_overworld ","x":1,"y":2,"z":3}]}"#,
            false,
        )
        .unwrap();
        assert_eq!(snapshot.skipped_entries, 1);
        assert!(
            snapshot
                .players
                .contains_key("276326ac-dbf7-4083-ae6e-ba6e9eaca2e1")
        );
        assert_eq!(
            snapshot.players.values().next().unwrap().dimension,
            " minecraft_overworld "
        );
    }

    #[test]
    fn invalid_coordinates_keep_identity_in_tab_only() {
        let snapshot = parse_snapshot(
            br#"{"players":[{"name":"TabOnly","uuid":"276326acdbf74083ae6eba6e9eaca2e1","world":"minecraft_overworld","x":"NaN","y":2,"z":3}]}"#,
            true,
        )
        .unwrap();
        assert!(snapshot.players.is_empty());
        assert_eq!(snapshot.tab_players.len(), 1);
    }

    #[test]
    fn rejects_invalid_root_shapes() {
        assert!(parse_snapshot(br#"[]"#, true).is_err());
        assert!(parse_snapshot(br#"{"players":null}"#, true).is_err());
    }

    #[test]
    fn normalizes_only_the_squaremap_namespace_separator() {
        assert_eq!(
            normalize_dimension("minecraft_the_nether", true),
            "minecraft:the_nether"
        );
        assert_eq!(
            normalize_dimension("custom:path_with_underscores", true),
            "custom:path_with_underscores"
        );
        assert_eq!(normalize_dimension("plainworld", true), "plainworld");
        assert_eq!(
            normalize_dimension("minecraft_overworld", false),
            "minecraft_overworld"
        );
    }
}
