use std::collections::{BTreeMap, BTreeSet};
use std::convert::Infallible;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use axum::response::sse::Event;
use bro_core::TaskId;
use bro_protocol::{RosterDelta, RosterSnapshotV1, RosterSummaryV1};
use fleet_core::FleetSnapshot;
use futures::{Stream, StreamExt};
use tokio::sync::broadcast;
use tokio_stream::wrappers::BroadcastStream;

#[derive(Debug, Clone)]
pub(crate) enum RosterSignal {
    Delta(Box<RosterDelta>),
    Resync {
        reason: String,
        skipped: Option<u64>,
    },
}

#[derive(Clone)]
pub(crate) struct RosterHub {
    snapshot: Arc<RwLock<RosterSnapshotV1>>,
    sender: broadcast::Sender<RosterSignal>,
}

impl RosterHub {
    pub(crate) fn new(snapshot: RosterSnapshotV1) -> Self {
        let (sender, _) = broadcast::channel(512);
        Self {
            snapshot: Arc::new(RwLock::new(snapshot)),
            sender,
        }
    }

    pub(crate) fn empty() -> Self {
        Self::new(RosterSnapshotV1 {
            version: 0,
            tasks: Vec::new(),
            daemon_version: Some(env!("CARGO_PKG_VERSION").to_string()),
            daemon_build_id: Some(fleet_build_id()),
        })
    }

    pub(crate) fn from_authority(snapshot: &FleetSnapshot) -> Self {
        Self::new(authority_roster(snapshot))
    }

    pub(crate) fn snapshot(&self) -> RosterSnapshotV1 {
        self.snapshot
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    pub(crate) fn replace(&self, next: RosterSnapshotV1) {
        let mut current = self
            .snapshot
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        *current = next;
        let _ = self.sender.send(RosterSignal::Resync {
            reason: "shadow snapshot replaced".into(),
            skipped: None,
        });
    }

    pub(crate) fn publish_authority(&self, next: &FleetSnapshot) {
        self.publish(authority_roster(next));
    }

    pub(crate) fn publish(&self, next: RosterSnapshotV1) {
        let mut current = self
            .snapshot
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if next.version < current.version {
            let _ = self.sender.send(RosterSignal::Resync {
                reason: "roster authority generation moved backwards".into(),
                skipped: Some(current.version.saturating_sub(next.version)),
            });
            return;
        }

        let previous_by_id = by_id(&current.tasks);
        let next_by_id = by_id(&next.tasks);
        let ids: BTreeSet<TaskId> = previous_by_id
            .keys()
            .chain(next_by_id.keys())
            .cloned()
            .collect();
        let mut sequence = current.version;
        let mut deltas = Vec::new();
        for task_id in ids {
            let previous = previous_by_id.get(&task_id);
            let updated = next_by_id.get(&task_id);
            if previous == updated {
                continue;
            }
            sequence = sequence.saturating_add(1);
            let delta = match (previous, updated) {
                (None, Some(task)) => RosterDelta::Added {
                    seq: sequence,
                    task: (*task).clone(),
                },
                (Some(_), Some(task)) => RosterDelta::Updated {
                    seq: sequence,
                    task: (*task).clone(),
                },
                (Some(_), None) => RosterDelta::Removed {
                    seq: sequence,
                    task_id,
                },
                (None, None) => continue,
            };
            deltas.push(delta);
        }

        *current = next.clone();
        drop(current);
        if sequence != next.version {
            let _ = self.sender.send(RosterSignal::Resync {
                reason: "roster changes did not cover the authority generation gap".into(),
                skipped: Some(next.version.saturating_sub(sequence)),
            });
            return;
        }
        for delta in deltas {
            let _ = self.sender.send(RosterSignal::Delta(Box::new(delta)));
        }
    }

    pub(crate) fn subscribe(
        &self,
    ) -> impl Stream<Item = Result<Event, Infallible>> + Send + 'static + use<> {
        BroadcastStream::new(self.sender.subscribe()).map(|item| {
            let event = match item {
                Ok(RosterSignal::Delta(delta)) => Event::default()
                    .event("roster")
                    .json_data(*delta)
                    .unwrap_or_else(|_| Event::default().event("resync").data("{}")),
                Ok(RosterSignal::Resync { reason, skipped }) => Event::default()
                    .event("resync")
                    .json_data(serde_json::json!({"reason": reason, "skipped": skipped}))
                    .unwrap_or_else(|_| Event::default().event("resync").data("{}")),
                Err(error) => Event::default()
                    .event("resync")
                    .json_data(serde_json::json!({
                        "reason": "roster stream subscriber lagged",
                        "skipped": error.to_string()
                    }))
                    .unwrap_or_else(|_| Event::default().event("resync").data("{}")),
            };
            Ok(event)
        })
    }

    pub(crate) async fn wait_for_terminal(
        &self,
        task_id: &TaskId,
        timeout: Duration,
    ) -> Option<(bool, RosterSummaryV1)> {
        let mut receiver = self.sender.subscribe();
        let current = || {
            self.snapshot()
                .tasks
                .into_iter()
                .find(|task| &task.task_id == task_id)
        };
        let first = current()?;
        if first.status.is_terminal() {
            return Some((true, first));
        }
        let waited = tokio::time::timeout(timeout, async {
            loop {
                match receiver.recv().await {
                    Ok(_) | Err(broadcast::error::RecvError::Lagged(_)) => {
                        let task = current()?;
                        if task.status.is_terminal() {
                            return Some(task);
                        }
                    }
                    Err(broadcast::error::RecvError::Closed) => return current(),
                }
            }
        })
        .await;
        match waited {
            Ok(Some(task)) => Some((task.status.is_terminal(), task)),
            Ok(None) => None,
            Err(_) => current().map(|task| (false, task)),
        }
    }
}

fn authority_roster(snapshot: &FleetSnapshot) -> RosterSnapshotV1 {
    RosterSnapshotV1 {
        version: snapshot.roster_generation,
        tasks: snapshot.roster.values().cloned().collect(),
        daemon_version: Some(env!("CARGO_PKG_VERSION").to_string()),
        daemon_build_id: Some(fleet_build_id()),
    }
}

fn by_id(tasks: &[RosterSummaryV1]) -> BTreeMap<TaskId, &RosterSummaryV1> {
    tasks
        .iter()
        .map(|task| (task.task_id.clone(), task))
        .collect()
}

pub(crate) fn fleet_build_id() -> String {
    env!("FLEETD_BUILD_ID").to_string()
}

#[cfg(test)]
mod tests {
    use bro_core::{Origin, Provider, SessionId};
    use bro_protocol::TaskStatus;

    use super::*;

    fn row(id: &str, status: TaskStatus) -> RosterSummaryV1 {
        RosterSummaryV1 {
            task_id: TaskId::new(id),
            status,
            provider: Provider::Glm,
            cost: None,
            turns: None,
            cwd: None,
            label: None,
            name: None,
            session_id: Some(SessionId::new(format!("session-{id}"))),
            last_message_snippet: None,
            model: None,
            report: None,
            last_event_at: None,
            origin: Origin::Unknown,
            managed_worktree: None,
            workflow_owned: false,
            started_at: None,
            agent_label: None,
            report_full: None,
            interrupted: false,
            error_teaser: None,
            transcript_path: None,
        }
    }

    #[tokio::test]
    async fn materialized_view_emits_contiguous_deltas() {
        let hub = RosterHub::empty();
        let mut receiver = hub.sender.subscribe();
        hub.publish(RosterSnapshotV1 {
            version: 1,
            tasks: vec![row("task-1", TaskStatus::Running)],
            daemon_version: None,
            daemon_build_id: None,
        });
        let RosterSignal::Delta(delta) = receiver.recv().await.unwrap() else {
            panic!("expected added delta");
        };
        let RosterDelta::Added { seq, .. } = *delta else {
            panic!("expected added delta");
        };
        assert_eq!(seq, 1);
    }
}
