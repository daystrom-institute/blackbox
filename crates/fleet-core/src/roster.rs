use bro_protocol::{RosterDelta, RosterSummaryV1};

use crate::model::TaskRecord;

const REPORT_TEASER_CHARS: usize = 80;

pub fn project_task(task: &TaskRecord) -> RosterSummaryV1 {
    RosterSummaryV1 {
        task_id: task.task_id.clone(),
        status: task.status,
        provider: task.provider,
        cost: task.cost,
        turns: task.turns,
        cwd: task.cwd.clone(),
        label: task.label.clone(),
        name: task.name.clone(),
        session_id: Some(task.session_id.clone()),
        last_message_snippet: task.last_message.as_deref().map(bounded_message),
        model: task.model.clone(),
        report: task
            .report
            .as_ref()
            .map(|report| bounded_chars(&report.message, REPORT_TEASER_CHARS)),
        last_event_at: Some(task.last_event_at_unix_ms),
        origin: task.origin,
        managed_worktree: task.managed_worktree.clone(),
        workflow_owned: task.workflow_owned,
        started_at: Some(task.started_at_unix_ms),
        agent_label: task.agent_label.clone(),
        report_full: task.report.clone(),
        interrupted: task.interrupted,
        error_teaser: task.error_teaser.as_deref().map(bounded_error),
        transcript_path: task.transcript_path.clone(),
    }
}

pub fn roster_delta(
    sequence: u64,
    previous: Option<&RosterSummaryV1>,
    next: Option<&RosterSummaryV1>,
) -> Option<RosterDelta> {
    match (previous, next) {
        (None, Some(task)) => Some(RosterDelta::Added {
            seq: sequence,
            task: task.clone(),
        }),
        (Some(previous), Some(task)) if previous != task => Some(RosterDelta::Updated {
            seq: sequence,
            task: task.clone(),
        }),
        (Some(task), None) => Some(RosterDelta::Removed {
            seq: sequence,
            task_id: task.task_id.clone(),
        }),
        _ => None,
    }
}

fn bounded_message(value: &str) -> String {
    bounded_chars(value.trim(), 240)
}

fn bounded_error(value: &str) -> String {
    let last_line = value
        .lines()
        .rev()
        .find(|line| !line.trim().is_empty())
        .unwrap_or("");
    bounded_chars(last_line.trim(), 200)
}

fn bounded_chars(value: &str, maximum: usize) -> String {
    let mut characters = value.chars();
    let bounded: String = characters.by_ref().take(maximum).collect();
    if characters.next().is_some() {
        format!("{bounded}…")
    } else {
        bounded
    }
}

#[cfg(test)]
mod tests {
    use bro_core::{Origin, Provider, SessionId, TaskId};
    use bro_protocol::TaskStatus;

    use super::*;

    fn task() -> TaskRecord {
        TaskRecord {
            task_id: TaskId::new("task-1"),
            session_id: SessionId::new("session-1"),
            attempt_id: None,
            worker_id: None,
            status: TaskStatus::Running,
            provider: Provider::Glm,
            model: Some("model".into()),
            cost: None,
            turns: None,
            cwd: None,
            managed_worktree: None,
            transcript_path: Some("/tmp/transcript".into()),
            transcript_cursor: None,
            last_message: Some("message".into()),
            report: None,
            error_teaser: None,
            label: None,
            name: None,
            agent_label: None,
            origin: Origin::Unknown,
            workflow_owned: false,
            interrupted: false,
            cancellation_requested_at_unix_ms: None,
            recoverable: false,
            started_at_unix_ms: 10,
            last_event_at_unix_ms: 20,
            completed_at_unix_ms: None,
        }
    }

    #[test]
    fn projection_is_bounded_and_contains_no_events() {
        let mut task = task();
        task.last_message = Some("x".repeat(300));
        task.error_teaser = Some("old\n{}".replace("{}", &"e".repeat(240)));
        let summary = project_task(&task);
        assert!(
            summary
                .last_message_snippet
                .as_ref()
                .unwrap()
                .chars()
                .count()
                <= 241
        );
        assert!(summary.error_teaser.as_ref().unwrap().chars().count() <= 201);
        let value = serde_json::to_value(summary).unwrap();
        assert!(value.get("events").is_none());
    }

    #[test]
    fn delta_distinguishes_add_update_remove_and_noop() {
        let first = project_task(&task());
        assert!(matches!(
            roster_delta(1, None, Some(&first)),
            Some(RosterDelta::Added { seq: 1, .. })
        ));
        assert!(roster_delta(2, Some(&first), Some(&first)).is_none());
        let mut changed = first.clone();
        changed.turns = Some(3);
        assert!(matches!(
            roster_delta(2, Some(&first), Some(&changed)),
            Some(RosterDelta::Updated { seq: 2, .. })
        ));
        assert!(matches!(
            roster_delta(3, Some(&changed), None),
            Some(RosterDelta::Removed { seq: 3, .. })
        ));
    }
}
