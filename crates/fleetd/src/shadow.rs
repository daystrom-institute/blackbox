use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex};

use bro_core::TaskId;
use bro_protocol::{RosterDelta, RosterSnapshotV1, RosterSummaryV1};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::roster::RosterHub;
use crate::{FleetdError, FleetdResult};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ParityMismatch {
    pub task_id: TaskId,
    pub field: String,
    pub expected: Value,
    pub observed: Value,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ParityReport {
    pub expected_version: u64,
    pub observed_version: u64,
    pub missing_tasks: Vec<TaskId>,
    pub unexpected_tasks: Vec<TaskId>,
    pub mismatches: Vec<ParityMismatch>,
}

impl ParityReport {
    pub fn is_match(&self) -> bool {
        self.expected_version == self.observed_version
            && self.missing_tasks.is_empty()
            && self.unexpected_tasks.is_empty()
            && self.mismatches.is_empty()
    }
}

#[derive(Clone)]
pub struct ShadowReplica {
    state: Arc<Mutex<RosterSnapshotV1>>,
    hub: RosterHub,
}

impl Default for ShadowReplica {
    fn default() -> Self {
        Self::new()
    }
}

impl ShadowReplica {
    pub fn new() -> Self {
        let hub = RosterHub::empty();
        Self {
            state: Arc::new(Mutex::new(hub.snapshot())),
            hub,
        }
    }

    pub fn snapshot(&self) -> RosterSnapshotV1 {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    pub fn apply_snapshot(&self, snapshot: RosterSnapshotV1) {
        *self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = snapshot.clone();
        self.hub.replace(snapshot);
    }

    pub fn apply_delta(&self, delta: RosterDelta) -> FleetdResult<()> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if delta.seq() <= state.version {
            return Ok(());
        }
        let expected = state.version.saturating_add(1);
        if delta.seq() != expected {
            return Err(FleetdError::Conflict(format!(
                "shadow roster sequence gap: expected {expected}, received {}",
                delta.seq()
            )));
        }
        let mut tasks: BTreeMap<TaskId, RosterSummaryV1> = state
            .tasks
            .drain(..)
            .map(|task| (task.task_id.clone(), task))
            .collect();
        match delta {
            RosterDelta::Added { task, seq } | RosterDelta::Updated { task, seq } => {
                tasks.insert(task.task_id.clone(), task);
                state.version = seq;
            }
            RosterDelta::Removed { task_id, seq } => {
                tasks.remove(&task_id);
                state.version = seq;
            }
        }
        state.tasks = tasks.into_values().collect();
        let snapshot = state.clone();
        drop(state);
        self.hub.publish(snapshot);
        Ok(())
    }

    pub fn compare(&self, expected: &RosterSnapshotV1) -> ParityReport {
        let observed = self.snapshot();
        let expected_by_id = rows_by_id(&expected.tasks);
        let observed_by_id = rows_by_id(&observed.tasks);
        let expected_ids: BTreeSet<TaskId> = expected_by_id.keys().cloned().collect();
        let observed_ids: BTreeSet<TaskId> = observed_by_id.keys().cloned().collect();
        let missing_tasks = expected_ids.difference(&observed_ids).cloned().collect();
        let unexpected_tasks = observed_ids.difference(&expected_ids).cloned().collect();
        let mut mismatches = Vec::new();
        for task_id in expected_ids.intersection(&observed_ids) {
            compare_row(
                task_id,
                expected_by_id[task_id],
                observed_by_id[task_id],
                &mut mismatches,
            );
        }
        ParityReport {
            expected_version: expected.version,
            observed_version: observed.version,
            missing_tasks,
            unexpected_tasks,
            mismatches,
        }
    }

    pub(crate) fn hub(&self) -> RosterHub {
        self.hub.clone()
    }
}

fn rows_by_id(rows: &[RosterSummaryV1]) -> BTreeMap<TaskId, &RosterSummaryV1> {
    rows.iter().map(|row| (row.task_id.clone(), row)).collect()
}

fn compare_row(
    task_id: &TaskId,
    expected: &RosterSummaryV1,
    observed: &RosterSummaryV1,
    mismatches: &mut Vec<ParityMismatch>,
) {
    compare_field(
        task_id,
        "status",
        expected.status,
        observed.status,
        mismatches,
    );
    compare_field(
        task_id,
        "session_id",
        &expected.session_id,
        &observed.session_id,
        mismatches,
    );
    compare_field(
        task_id,
        "transcript_path",
        &expected.transcript_path,
        &observed.transcript_path,
        mismatches,
    );
    compare_field(
        task_id,
        "last_event_at",
        expected.last_event_at,
        observed.last_event_at,
        mismatches,
    );
    compare_field(
        task_id,
        "interrupted",
        expected.interrupted,
        observed.interrupted,
        mismatches,
    );
    compare_field(
        task_id,
        "error_teaser",
        &expected.error_teaser,
        &observed.error_teaser,
        mismatches,
    );
    compare_field(
        task_id,
        "managed_worktree",
        &expected.managed_worktree,
        &observed.managed_worktree,
        mismatches,
    );
    compare_field(task_id, "full_row", expected, observed, mismatches);
}

fn compare_field<T>(
    task_id: &TaskId,
    field: &str,
    expected: T,
    observed: T,
    mismatches: &mut Vec<ParityMismatch>,
) where
    T: PartialEq + Serialize,
{
    if expected == observed {
        return;
    }
    mismatches.push(ParityMismatch {
        task_id: task_id.clone(),
        field: field.to_string(),
        expected: serde_json::to_value(expected).unwrap_or(Value::Null),
        observed: serde_json::to_value(observed).unwrap_or(Value::Null),
    });
}

#[cfg(test)]
mod tests {
    use bro_core::{Origin, Provider, SessionId};
    use bro_protocol::TaskStatus;

    use super::*;

    fn row(status: TaskStatus) -> RosterSummaryV1 {
        RosterSummaryV1 {
            task_id: TaskId::new("task-1"),
            status,
            provider: Provider::Glm,
            cost: None,
            turns: None,
            cwd: None,
            label: None,
            name: None,
            session_id: Some(SessionId::new("session-1")),
            last_message_snippet: None,
            model: None,
            report: None,
            last_event_at: Some(1),
            origin: Origin::Unknown,
            managed_worktree: None,
            workflow_owned: false,
            started_at: Some(1),
            agent_label: None,
            report_full: None,
            interrupted: false,
            error_teaser: None,
            transcript_path: Some("/tmp/transcript".into()),
        }
    }

    #[test]
    fn shadow_delta_requires_contiguous_sequence_and_reports_parity() {
        let replica = ShadowReplica::new();
        replica.apply_snapshot(RosterSnapshotV1 {
            version: 1,
            tasks: vec![row(bro_protocol::TaskStatus::Running)],
            daemon_version: None,
            daemon_build_id: None,
        });
        let mut completed = row(TaskStatus::Completed);
        completed.last_event_at = Some(2);
        replica
            .apply_delta(RosterDelta::Updated {
                seq: 2,
                task: completed.clone(),
            })
            .unwrap();
        assert!(
            replica
                .compare(&RosterSnapshotV1 {
                    version: 2,
                    tasks: vec![completed],
                    daemon_version: None,
                    daemon_build_id: None,
                })
                .is_match()
        );
        assert!(
            replica
                .apply_delta(RosterDelta::Removed {
                    seq: 4,
                    task_id: TaskId::new("task-1"),
                })
                .is_err()
        );
    }
}
