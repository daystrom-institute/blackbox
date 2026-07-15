use bro_protocol::{BroReportV1, TaskStatus};
use fleet_core::{SessionEventDescriptor, TaskEventObservation, TaskEventProjection};
use serde_json::Value;

const MAX_LAST_MESSAGE_CHARS: usize = 32 * 1024;
const MAX_ERROR_CHARS: usize = 4 * 1024;

pub(crate) fn observation_for_event(
    event: &Value,
    occurred_at_unix_ms: u64,
) -> TaskEventObservation {
    let record = Some(SessionEventDescriptor::from_event(event));
    if event.get("type").and_then(Value::as_str) == Some("harness_milestone")
        && event.get("milestone").and_then(Value::as_str) == Some("session_snapshot_committed")
    {
        return TaskEventObservation {
            commit_deferred_terminal: true,
            record,
            ..TaskEventObservation::default()
        };
    }

    if event.get("type").and_then(Value::as_str) == Some("result") {
        let failed = result_failed(event);
        let last_message = result_text(event).map(|text| bounded(&text, MAX_LAST_MESSAGE_CHARS));
        let error_teaser = failed.then(|| {
            bounded(
                event
                    .get("error")
                    .and_then(Value::as_str)
                    .or_else(|| event.get("result").and_then(Value::as_str))
                    .unwrap_or("worker result reported failure"),
                MAX_ERROR_CHARS,
            )
        });
        return TaskEventObservation {
            projection: Some(TaskEventProjection {
                last_message: last_message.clone(),
                cost: event.get("total_cost_usd").and_then(Value::as_f64),
                turns: event.get("num_turns").and_then(Value::as_u64),
                ..TaskEventProjection::default()
            }),
            defer_terminal: Some(TaskEventProjection {
                status: Some(if failed {
                    TaskStatus::Failed
                } else {
                    TaskStatus::Completed
                }),
                last_message,
                cost: event.get("total_cost_usd").and_then(Value::as_f64),
                turns: event.get("num_turns").and_then(Value::as_u64),
                error_teaser,
                completed_at_unix_ms: Some(occurred_at_unix_ms),
                interrupted: Some(false),
                recoverable: Some(failed),
                ..TaskEventProjection::default()
            }),
            commit_deferred_terminal: false,
            record,
        };
    }

    let mut projection = TaskEventProjection::default();
    if let Some(model) = event
        .get("model")
        .and_then(Value::as_str)
        .or_else(|| event.pointer("/message/model").and_then(Value::as_str))
    {
        projection.model = Some(model.to_string());
    }
    if event.get("type").and_then(Value::as_str) == Some("assistant")
        && let Some(text) = assistant_text(event)
    {
        projection.last_message = Some(bounded(&text, MAX_LAST_MESSAGE_CHARS));
    }
    if event.get("type").and_then(Value::as_str) == Some("bro_report")
        && let Some(message) = event.get("message").and_then(Value::as_str)
    {
        projection.report = Some(BroReportV1 {
            message: bounded(message, MAX_LAST_MESSAGE_CHARS),
            needs: event
                .get("needs")
                .and_then(Value::as_str)
                .map(str::to_string),
            data: event.get("data").cloned().filter(|value| !value.is_null()),
            reported_at: event
                .get("reported_at")
                .or_else(|| event.get("reportedAt"))
                .and_then(Value::as_u64)
                .unwrap_or(occurred_at_unix_ms),
            reported_ago: "just now".into(),
        });
    }
    let has_projection = projection != TaskEventProjection::default();
    TaskEventObservation {
        projection: has_projection.then_some(projection),
        record,
        ..TaskEventObservation::default()
    }
}

fn result_failed(event: &Value) -> bool {
    event.get("is_error").and_then(Value::as_bool) == Some(true)
        || event.get("success").and_then(Value::as_bool) == Some(false)
        || event.get("error").is_some_and(|value| !value.is_null())
        || event
            .get("subtype")
            .and_then(Value::as_str)
            .is_some_and(|value| value.contains("error") || value.contains("fail"))
}

fn result_text(event: &Value) -> Option<String> {
    event
        .get("result")
        .and_then(Value::as_str)
        .map(str::to_string)
        .or_else(|| assistant_text(event))
}

fn assistant_text(event: &Value) -> Option<String> {
    let content = event.pointer("/message/content")?.as_array()?;
    let text = content
        .iter()
        .filter(|block| block.get("type").and_then(Value::as_str) == Some("text"))
        .filter_map(|block| block.get("text").and_then(Value::as_str))
        .collect::<Vec<_>>()
        .join("\n\n");
    (!text.is_empty()).then_some(text)
}

fn bounded(value: &str, limit: usize) -> String {
    let mut chars = value.chars();
    let bounded: String = chars.by_ref().take(limit).collect();
    if chars.next().is_some() {
        format!("{bounded}...")
    } else {
        bounded
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn result_is_staged_and_snapshot_marker_commits_it() {
        let result = observation_for_event(
            &json!({
                "type": "result",
                "result": "done",
                "total_cost_usd": 1.5,
                "num_turns": 2
            }),
            10,
        );
        assert_eq!(
            result.defer_terminal.as_ref().unwrap().status,
            Some(TaskStatus::Completed)
        );
        assert!(!result.commit_deferred_terminal);
        let marker = observation_for_event(
            &json!({
                "type": "harness_milestone",
                "milestone": "session_snapshot_committed"
            }),
            11,
        );
        assert!(marker.commit_deferred_terminal);
    }
}
