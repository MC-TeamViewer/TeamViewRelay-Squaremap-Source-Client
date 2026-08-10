use std::time::Duration;

use tokio::time::Instant;

use crate::model::Snapshot;
use crate::proto::ExternalSourceHealth;

#[derive(Clone, Debug, PartialEq)]
pub enum PublishAction {
    Status {
        health: ExternalSourceHealth,
        failure_code: Option<String>,
    },
    Full {
        previous: Option<Snapshot>,
        current: Snapshot,
    },
    Patch {
        previous: Snapshot,
        current: Snapshot,
    },
    Keepalive(Vec<String>),
    Clear,
}

pub struct Publisher {
    health: ExternalSourceHealth,
    failure_grace: Duration,
    failure_since: Option<Instant>,
    failure_code: Option<String>,
    latest_snapshot: Option<Snapshot>,
    published_snapshot: Option<Snapshot>,
    relay_connected: bool,
    cleared_on_connection: bool,
}

impl Publisher {
    pub fn new(failure_grace: Duration) -> Self {
        Self {
            health: ExternalSourceHealth::Starting,
            failure_grace,
            failure_since: None,
            failure_code: None,
            latest_snapshot: None,
            published_snapshot: None,
            relay_connected: false,
            cleared_on_connection: false,
        }
    }

    pub fn relay_connected(&mut self) -> Vec<PublishAction> {
        self.relay_connected = true;
        self.published_snapshot = None;
        self.cleared_on_connection = false;

        let mut actions = vec![self.status_action()];
        match self.health {
            ExternalSourceHealth::Healthy => {
                if let Some(snapshot) = self.latest_snapshot.clone() {
                    actions.push(PublishAction::Full {
                        previous: None,
                        current: snapshot.clone(),
                    });
                    self.published_snapshot = Some(snapshot);
                }
            }
            ExternalSourceHealth::Unavailable => {
                actions.push(PublishAction::Clear);
                self.cleared_on_connection = true;
            }
            _ => {}
        }
        actions
    }

    pub fn relay_disconnected(&mut self) {
        self.relay_connected = false;
        self.published_snapshot = None;
        self.cleared_on_connection = false;
    }

    pub fn upstream_succeeded(&mut self, snapshot: Option<Snapshot>) -> Vec<PublishAction> {
        let was_healthy = self.health == ExternalSourceHealth::Healthy;
        if let Some(snapshot) = snapshot {
            self.latest_snapshot = Some(snapshot);
        }
        let Some(latest) = self.latest_snapshot.clone() else {
            return Vec::new();
        };

        self.health = ExternalSourceHealth::Healthy;
        self.failure_since = None;
        self.failure_code = None;

        if !self.relay_connected {
            return Vec::new();
        }

        let mut actions = Vec::new();
        if !was_healthy {
            actions.push(self.status_action());
            actions.push(PublishAction::Full {
                previous: self.published_snapshot.clone(),
                current: latest.clone(),
            });
            self.published_snapshot = Some(latest);
            return actions;
        }

        match self.published_snapshot.clone() {
            None => {
                actions.push(PublishAction::Full {
                    previous: None,
                    current: latest.clone(),
                });
                self.published_snapshot = Some(latest);
            }
            Some(published)
                if published.players != latest.players
                    || published.tab_players != latest.tab_players =>
            {
                actions.push(PublishAction::Patch {
                    previous: published,
                    current: latest.clone(),
                });
                self.published_snapshot = Some(latest);
            }
            Some(_) => {
                actions.push(PublishAction::Keepalive(
                    latest.players.keys().cloned().collect(),
                ));
            }
        }
        actions
    }

    pub fn upstream_failed(&mut self, code: &str, now: Instant) -> Vec<PublishAction> {
        if self.failure_since.is_none() {
            self.failure_since = Some(now);
        }
        self.failure_code = Some(code.to_owned());

        let unavailable = self
            .failure_since
            .is_some_and(|started| now.saturating_duration_since(started) >= self.failure_grace);
        self.health = if unavailable {
            ExternalSourceHealth::Unavailable
        } else {
            ExternalSourceHealth::Degraded
        };

        if !self.relay_connected {
            return Vec::new();
        }

        let mut actions = vec![self.status_action()];
        if unavailable && !self.cleared_on_connection {
            actions.push(PublishAction::Clear);
            self.cleared_on_connection = true;
            self.published_snapshot = None;
        }
        actions
    }

    fn status_action(&self) -> PublishAction {
        PublishAction::Status {
            health: self.health,
            failure_code: self.failure_code.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use tokio::time;

    use super::*;
    use crate::model::{PlayerRecord, TabRecord};

    fn snapshot(x: f64) -> Snapshot {
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
                    health: None,
                    armor: None,
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

    #[tokio::test(start_paused = true)]
    async fn degrades_clears_once_and_recovers_with_full_state() {
        let mut publisher = Publisher::new(Duration::from_secs(30));
        assert!(matches!(
            publisher.relay_connected().as_slice(),
            [PublishAction::Status {
                health: ExternalSourceHealth::Starting,
                ..
            }]
        ));

        let initial = publisher.upstream_succeeded(Some(snapshot(1.0)));
        assert!(matches!(
            initial.as_slice(),
            [PublishAction::Status { .. }, PublishAction::Full { .. }]
        ));

        let failed = publisher.upstream_failed("network", Instant::now());
        assert!(matches!(
            failed.as_slice(),
            [PublishAction::Status {
                health: ExternalSourceHealth::Degraded,
                ..
            }]
        ));
        assert!(
            !failed
                .iter()
                .any(|action| matches!(action, PublishAction::Keepalive(_)))
        );

        time::advance(Duration::from_secs(30)).await;
        let unavailable = publisher.upstream_failed("timeout", Instant::now());
        assert!(matches!(
            unavailable.as_slice(),
            [
                PublishAction::Status {
                    health: ExternalSourceHealth::Unavailable,
                    ..
                },
                PublishAction::Clear
            ]
        ));
        let repeated = publisher.upstream_failed("timeout", Instant::now());
        assert_eq!(
            repeated
                .iter()
                .filter(|action| matches!(action, PublishAction::Clear))
                .count(),
            0
        );

        let recovered = publisher.upstream_succeeded(None);
        assert!(matches!(
            recovered.as_slice(),
            [PublishAction::Status { .. }, PublishAction::Full { .. }]
        ));
    }

    #[tokio::test(start_paused = true)]
    async fn degraded_reconnect_never_republishes_stale_snapshot() {
        let mut publisher = Publisher::new(Duration::from_secs(30));
        publisher.relay_connected();
        publisher.upstream_succeeded(Some(snapshot(1.0)));
        publisher.upstream_failed("network", Instant::now());
        publisher.relay_disconnected();

        let reconnect = publisher.relay_connected();
        assert!(matches!(
            reconnect.as_slice(),
            [PublishAction::Status {
                health: ExternalSourceHealth::Degraded,
                ..
            }]
        ));
    }

    #[tokio::test(start_paused = true)]
    async fn healthy_unchanged_snapshot_uses_keepalive_and_changed_snapshot_uses_patch() {
        let mut publisher = Publisher::new(Duration::from_secs(30));
        publisher.relay_connected();
        publisher.upstream_succeeded(Some(snapshot(1.0)));

        assert!(matches!(
            publisher.upstream_succeeded(None).as_slice(),
            [PublishAction::Keepalive(_)]
        ));
        assert!(matches!(
            publisher.upstream_succeeded(Some(snapshot(2.0))).as_slice(),
            [PublishAction::Patch { .. }]
        ));
    }
}
