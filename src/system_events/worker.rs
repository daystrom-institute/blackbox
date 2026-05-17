use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use serde_json::json;

use super::executors;
use super::gate;
use super::hub::EventHub;
use super::types::*;
use crate::server::state::SharedState;

const WORKER_TICK_SECS: u64 = 2;
const INITIAL_BACKOFF_SECS: u64 = 5;
const MAX_BACKOFF_SECS: u64 = 600;
const MAX_CAUSATION_DEPTH: usize = 4;

pub async fn run_worker(state: std::sync::Arc<SharedState>) {
    let hub = &state.system_events;
    let pid = hub.process_instance_id().to_string();
    loop {
        tokio::time::sleep(Duration::from_secs(WORKER_TICK_SECS)).await;
        match tick(state.clone(), hub, &pid).await {
            Ok(Processed::DidWork) => {}
            Ok(Processed::Idle) => {}
            Err(e) => {
                tracing::error!("outbox worker tick error: {e:#}");
            }
        }
    }
}

#[derive(Debug, serde::Serialize)]
pub struct ReactionExecuteReport {
    pub mode: String,
    pub force: bool,
    pub dry_run: gate::DryRunResult,
    pub outbox_record: OutboxRecord,
}

pub async fn execute_reaction_once(
    state: Arc<SharedState>,
    event: &SystemEvent,
    reaction: &ReactionSpec,
    force: bool,
) -> Result<ReactionExecuteReport> {
    let hub = &state.system_events;
    let dry_run = {
        let outbox_records = hub.outbox_store().load_all()?;
        let packets = state.packets.read();
        gate::dry_run_replay(reaction, event, &packets, &outbox_records)?
    };
    let created = hub.outbox_store().create_record(
        &event.id,
        &reaction.name,
        dry_run.rendered_idempotency_key.clone(),
    )?;
    let now = crate::util::now_iso();
    let claimed = hub
        .outbox_store()
        .claim_record(&created.id, &now, hub.process_instance_id())?
        .ok_or_else(|| anyhow::anyhow!("failed to claim replay outbox record {}", created.id))?;
    let result = process_record_with_options(
        state.clone(),
        hub,
        &claimed,
        ProcessOptions {
            force_idempotency: force,
        },
    )
    .await;
    handle_tick_result(&state, hub, &claimed, result, reaction.retry.max_attempts)?;
    let outbox_record = hub
        .outbox_store()
        .get_record(&created.id)?
        .ok_or_else(|| anyhow::anyhow!("replay outbox record {} disappeared", created.id))?;
    Ok(ReactionExecuteReport {
        mode: if force { "force" } else { "execute" }.to_string(),
        force,
        dry_run,
        outbox_record,
    })
}

enum Processed {
    DidWork,
    Idle,
}

async fn tick(state: Arc<SharedState>, hub: &EventHub, pid: &str) -> Result<Processed> {
    let now = crate::util::now_iso();
    let record = match hub.outbox_store().claim_next(&now, pid)? {
        Some(r) => r,
        None => return Ok(Processed::Idle),
    };

    let max_attempts = hub
        .get_reaction(&record.reaction_name)
        .await
        .map(|r| r.retry.max_attempts)
        .unwrap_or(1);

    let result = process_record(state.clone(), hub, &record).await;
    handle_tick_result(&state, hub, &record, result, max_attempts)?;
    Ok(Processed::DidWork)
}

fn handle_tick_result(
    state: &Arc<SharedState>,
    hub: &EventHub,
    record: &OutboxRecord,
    result: ProcessResult,
    max_attempts: u32,
) -> Result<()> {
    match result {
        ProcessResult::Suppressed => {
            hub.outbox_store().mark_succeeded(
                &record.id,
                Some(json!({"reason": "idempotency_suppressed"})),
            )?;
        }
        ProcessResult::GateSkipped => {
            hub.outbox_store()
                .mark_succeeded(&record.id, Some(json!({"reason": "skipped_by_gate"})))?;
        }
        ProcessResult::PreCheckErr(WorkerError::Permanent(reason)) => {
            let redacted = redact_string(&reason);
            let project = event_project(hub, &record.event_id);
            write_blocked_note(state, record, &redacted, project.as_deref());
            hub.outbox_store()
                .mark_dead_lettered(&record.id, &redacted, Some(&redacted))?;
        }
        ProcessResult::PreCheckErr(WorkerError::NonRetryable(reason)) => {
            let redacted = redact_string(&reason);
            let project = event_project(hub, &record.event_id);
            write_blocked_note(state, record, &redacted, project.as_deref());
            hub.outbox_store()
                .mark_dead_lettered(&record.id, &redacted, Some(&redacted))?;
        }
        ProcessResult::Action(outcome) => match outcome.status {
            ActionStatus::Succeeded => {
                hub.outbox_store()
                    .mark_succeeded(&record.id, Some(outcome.response_summary))?;
            }
            ActionStatus::SkippedByGate => {
                hub.outbox_store()
                    .mark_succeeded(&record.id, Some(outcome.response_summary))?;
            }
            ActionStatus::PermanentFailure => {
                let reason = serde_json::to_string(&outcome.response_summary)?;
                let redacted = redact_string(&reason);
                let project = event_project(hub, &record.event_id);
                write_blocked_note(state, record, &redacted, project.as_deref());
                hub.outbox_store()
                    .mark_dead_lettered(&record.id, &redacted, Some(&redacted))?;
            }
            ActionStatus::RetryableFailure => {
                let reason = serde_json::to_string(&outcome.response_summary)?;
                let redacted = redact_string(&reason);
                if record.attempt_count >= max_attempts {
                    let reason = format!(
                        "max attempts ({}) exceeded: {redacted}",
                        record.attempt_count
                    );
                    let project = event_project(hub, &record.event_id);
                    write_blocked_note(state, record, &reason, project.as_deref());
                    hub.outbox_store()
                        .mark_dead_lettered(&record.id, &reason, Some(&reason))?;
                } else {
                    let next = compute_next_attempt(record);
                    hub.outbox_store()
                        .mark_retry_at(&record.id, &next, &redacted)?;
                }
            }
        },
    }
    Ok(())
}

enum ProcessResult {
    Suppressed,
    GateSkipped,
    PreCheckErr(WorkerError),
    Action(ActionOutcome),
}

#[derive(Debug)]
enum WorkerError {
    Permanent(String),
    NonRetryable(String),
}

impl WorkerError {
    fn permanent(reason: impl Into<String>) -> Self {
        WorkerError::Permanent(reason.into())
    }

    fn non_retryable(reason: impl Into<String>) -> Self {
        WorkerError::NonRetryable(reason.into())
    }
}

async fn process_record(
    state: Arc<SharedState>,
    hub: &EventHub,
    record: &OutboxRecord,
) -> ProcessResult {
    process_record_with_options(state, hub, record, ProcessOptions::default()).await
}

#[derive(Debug, Clone, Copy, Default)]
struct ProcessOptions {
    force_idempotency: bool,
}

async fn process_record_with_options(
    state: Arc<SharedState>,
    hub: &EventHub,
    record: &OutboxRecord,
    options: ProcessOptions,
) -> ProcessResult {
    let event = match hub.open_event(&record.event_id) {
        Ok(Some(e)) => e,
        Ok(None) => return ProcessResult::PreCheckErr(WorkerError::permanent("event not found")),
        Err(e) => {
            return ProcessResult::PreCheckErr(WorkerError::permanent(format!(
                "event load error: {e:#}"
            )));
        }
    };

    let reaction = match hub.get_reaction(&record.reaction_name).await {
        Some(r) => r,
        None => return ProcessResult::PreCheckErr(WorkerError::permanent("reaction not found")),
    };

    if !options.force_idempotency
        && let Some(ref key) = record.idempotency_key
    {
        match hub.outbox_store().load_all() {
            Ok(all_outbox) => {
                let suppressed = all_outbox.iter().any(|r| {
                    r.id != record.id
                        && r.reaction_name == record.reaction_name
                        && r.idempotency_key.as_deref() == Some(key.as_str())
                        && r.status == OutboxStatus::Succeeded
                });
                if suppressed {
                    return ProcessResult::Suppressed;
                }
            }
            Err(e) => {
                return ProcessResult::PreCheckErr(WorkerError::permanent(format!(
                    "outbox load error: {e:#}"
                )));
            }
        }
    }

    {
        let packets = state.packets.read();
        match gate::reaction_gate_allows(&packets, &reaction, &event) {
            Ok(decision) => {
                if !decision.allowed {
                    return ProcessResult::GateSkipped;
                }
            }
            Err(e) => {
                return ProcessResult::PreCheckErr(WorkerError::non_retryable(format!(
                    "gate evaluation error: {e:#}"
                )));
            }
        }
    }

    if let Err(e) = check_repeated_pair(hub, record, &event) {
        return ProcessResult::PreCheckErr(e);
    }

    {
        match hub.causation_chain_for(&event.id) {
            Ok(chain) => {
                if chain.len() >= MAX_CAUSATION_DEPTH {
                    return ProcessResult::PreCheckErr(WorkerError::permanent(format!(
                        "causation depth {} exceeds maximum",
                        chain.len()
                    )));
                }
            }
            Err(e) => {
                return ProcessResult::PreCheckErr(WorkerError::permanent(format!(
                    "causation chain error: {e:#}"
                )));
            }
        }
    }

    let outcome = executors::execute_action(state, hub, &reaction, &event).await;
    ProcessResult::Action(outcome)
}

fn check_repeated_pair(
    hub: &EventHub,
    record: &OutboxRecord,
    event: &SystemEvent,
) -> std::result::Result<(), WorkerError> {
    let Some(ref cid) = event.causation_id else {
        return Ok(());
    };
    let chain = hub
        .causation_chain_for(cid)
        .map_err(|e| WorkerError::permanent(format!("causation lookup: {e:#}")))?;
    for ancestor in &chain {
        if ancestor.kind == event.kind {
            let outbox = hub
                .outbox_store()
                .list_by_event(&ancestor.id)
                .map_err(|e| WorkerError::permanent(format!("outbox lookup: {e:#}")))?;
            let repeated = outbox
                .iter()
                .any(|r| r.reaction_name == record.reaction_name);
            if repeated {
                return Err(WorkerError::permanent(format!(
                    "repeated pair ({}, {}) in ancestry",
                    event.kind.to_wire(),
                    record.reaction_name
                )));
            }
        }
    }
    Ok(())
}

fn compute_next_attempt(record: &OutboxRecord) -> String {
    let attempt = record.attempt_count;
    let base_secs = INITIAL_BACKOFF_SECS * 2u64.saturating_pow(attempt.saturating_sub(1));
    let capped = base_secs.min(MAX_BACKOFF_SECS);
    let jitter = record
        .id
        .bytes()
        .fold(0u64, |acc, b| acc.wrapping_add(b as u64))
        % 3;
    let delay_secs = capped + jitter;
    let now = chrono::Utc::now();
    let next = now + chrono::Duration::seconds(delay_secs as i64);
    next.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

fn event_project(hub: &EventHub, event_id: &str) -> Option<String> {
    hub.open_event(event_id).ok()?.and_then(|e| e.project)
}

#[cfg(test)]
pub(crate) fn redact_string_for_test(s: &str) -> String {
    redact_string(s)
}

fn redact_string(s: &str) -> String {
    let mut result = String::with_capacity(s.len());
    let mut i = 0;
    let bytes = s.as_bytes();
    while i < bytes.len() {
        if s[i..].starts_with("secret:") {
            result.push_str("[REDACTED]");
            i += 7;
            while i < bytes.len() {
                let c = bytes[i];
                if c.is_ascii_whitespace() || c == b'"' || c == b'\'' {
                    break;
                }
                i += 1;
            }
        } else if s[i..].starts_with("Bearer ") {
            result.push_str("Bearer ");
            i += 7;
            let val_start = i;
            while i < bytes.len() {
                let c = bytes[i];
                if c.is_ascii_whitespace() || c == b'"' || c == b'\'' {
                    break;
                }
                i += 1;
            }
            if i > val_start {
                result.push_str("[REDACTED]");
            }
        } else {
            result.push(bytes[i] as char);
            i += 1;
        }
    }
    result
}

fn write_blocked_note(
    state: &Arc<SharedState>,
    record: &OutboxRecord,
    reason: &str,
    project: Option<&str>,
) {
    let body = format!(
        "outbox record {} (reaction '{}', event {}) dead-lettered: {}",
        record.id, record.reaction_name, record.event_id, reason
    );
    let params = crate::notes::NoteParams {
        kind: "blocked".to_string(),
        body,
        task_id: None,
        session_id: None,
        project: project.map(|p| p.to_string()),
        thread_id: None,
        provider: None,
        bro: None,
    };
    if let Err(e) = state.notes.write().create(&params) {
        tracing::warn!(
            "failed to write blocked note for outbox {}: {e:#}",
            record.id
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::system_events::{OutboxStore, SystemEventDraft};

    async fn setup_for_retry_test(hub: &EventHub, max_attempts: u32) -> (OutboxRecord, String) {
        let event = emit_event(hub, "task.completed", None).await;
        let spec = ReactionSpec {
            contract: "reaction/v1".to_string(),
            name: "retry-path-test".to_string(),
            version: 1,
            enabled: true,
            event_kinds: vec!["task.completed".to_string()],
            when: None,
            idempotency_key: Some("rp-${event.id}".to_string()),
            action: ReactionAction::EmitEvent {
                args: json!({"kind": "derived.event"}),
            },
            retry: RetryPolicy {
                max_attempts,
                backoff: "exponential".to_string(),
            },
            on_failure: FailurePolicy::DeadLetter,
        };
        hub.install_reaction(spec, false).await.unwrap();
        let rec = hub
            .outbox_store()
            .create_record(&event.id, "retry-path-test", Some("rp-key".to_string()))
            .unwrap();
        (rec, event.id)
    }

    async fn claim_record(hub: &EventHub, _record: &OutboxRecord) {
        let now = crate::util::now_iso();
        hub.outbox_store()
            .claim_next(&now, hub.process_instance_id())
            .unwrap();
    }

    #[tokio::test]
    async fn retryable_below_max_marks_retry_at() {
        let (state, _dir) = test_state();
        let hub = &state.system_events;
        let (rec, _eid) = setup_for_retry_test(hub, 3).await;
        claim_record(hub, &rec).await;

        let loaded = hub.outbox_store().get_record(&rec.id).unwrap().unwrap();
        assert_eq!(loaded.attempt_count, 1);

        let result = ProcessResult::Action(ActionOutcome {
            status: ActionStatus::RetryableFailure,
            response_summary: serde_json::json!({"error": "transient failure with secret:inner-token"}),
        });
        handle_tick_result(&state, hub, &loaded, result, 3).unwrap();

        let after = hub.outbox_store().get_record(&rec.id).unwrap().unwrap();
        assert_eq!(after.status, OutboxStatus::RetryAt, "should be retry_at");
        assert!(
            after.next_attempt_at.is_some(),
            "should have next_attempt_at"
        );
        let err = after.last_error.as_deref().unwrap();
        assert!(
            !err.contains("secret:inner-token"),
            "last_error should be redacted: {err}"
        );
        assert!(
            err.contains("[REDACTED]"),
            "last_error should have redaction marker: {err}"
        );
    }

    #[tokio::test]
    async fn retryable_at_max_dead_letters_with_blocked_note() {
        let (state, _dir) = test_state();
        let hub = &state.system_events;
        let (rec, _eid) = setup_for_retry_test(hub, 2).await;

        let now = crate::util::now_iso();
        let pid = hub.process_instance_id().to_string();
        hub.outbox_store().claim_next(&now, &pid).unwrap();
        let after_claim = hub.outbox_store().get_record(&rec.id).unwrap().unwrap();
        assert_eq!(after_claim.attempt_count, 1);

        let result = ProcessResult::Action(ActionOutcome {
            status: ActionStatus::RetryableFailure,
            response_summary: serde_json::json!({"error": "first failure"}),
        });
        handle_tick_result(&state, hub, &after_claim, result, 2).unwrap();
        let after_first = hub.outbox_store().get_record(&rec.id).unwrap().unwrap();
        assert_eq!(after_first.status, OutboxStatus::RetryAt);

        let later = {
            let t = chrono::Utc::now() + chrono::Duration::seconds(600);
            t.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
        };
        hub.outbox_store().claim_next(&later, &pid).unwrap();
        let after_second_claim = hub.outbox_store().get_record(&rec.id).unwrap().unwrap();
        assert_eq!(after_second_claim.attempt_count, 2);

        let result2 = ProcessResult::Action(ActionOutcome {
            status: ActionStatus::RetryableFailure,
            response_summary: serde_json::json!({"error": "second failure secret:key-x"}),
        });
        handle_tick_result(&state, hub, &after_second_claim, result2, 2).unwrap();

        let final_rec = hub.outbox_store().get_record(&rec.id).unwrap().unwrap();
        assert_eq!(
            final_rec.status,
            OutboxStatus::DeadLettered,
            "should be dead_lettered at max_attempts=2"
        );
        let reason = final_rec.dead_letter_reason.as_deref().unwrap();
        assert!(
            reason.contains("max attempts (2) exceeded"),
            "should mention max attempts: {reason}"
        );
        assert!(
            !reason.contains("secret:key-x"),
            "reason should be redacted: {reason}"
        );

        let notes_guard = state.notes.read();
        let all_notes = notes_guard.all();
        let blocked: Vec<_> = all_notes
            .iter()
            .filter(|n| n.kind == crate::notes::NoteKind::Blocked && n.body.contains(&rec.id))
            .collect();
        assert_eq!(blocked.len(), 1, "should have one blocked note");
        assert!(
            !blocked[0].body.contains("secret:key-x"),
            "blocked note should be redacted"
        );
    }

    #[test]
    fn compute_next_attempt_increases() {
        let mut rec = OutboxRecord::new("evt-1", "react", Some("key".to_string()));
        rec.attempt_count = 1;
        let t1 = compute_next_attempt(&rec);
        rec.attempt_count = 2;
        let t2 = compute_next_attempt(&rec);
        rec.attempt_count = 3;
        let t3 = compute_next_attempt(&rec);
        assert!(t1 < t2, "backoff should increase: {t1} >= {t2}");
        assert!(t2 < t3, "backoff should increase: {t2} >= {t3}");
    }

    #[test]
    fn compute_next_attempt_caps_at_max() {
        let mut rec = OutboxRecord::new("evt-1", "react", Some("key".to_string()));
        rec.attempt_count = 20;
        let next = compute_next_attempt(&rec);
        let now = chrono::Utc::now();
        let parsed = chrono::DateTime::parse_from_rfc3339(&next).unwrap();
        let delta = parsed.signed_duration_since(now);
        assert!(
            delta.num_seconds() <= (MAX_BACKOFF_SECS + 3) as i64,
            "backoff should be capped at ~{MAX_BACKOFF_SECS}s, got {}s",
            delta.num_seconds()
        );
    }

    fn test_state() -> (Arc<SharedState>, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let state = SharedState::for_test(dir.path());
        (Arc::new(state), dir)
    }

    async fn emit_event(hub: &EventHub, kind: &str, causation_id: Option<&str>) -> SystemEvent {
        hub.emit(SystemEventDraft {
            kind: SystemEventKind::from_wire(kind),
            producer: "worker-test".to_string(),
            project: Some("/test/project".to_string()),
            principal: None,
            subject: None,
            correlation: serde_json::Map::new(),
            causation_id: causation_id.map(|s| s.to_string()),
            payload: json!({}),
        })
        .await
        .unwrap()
        .event
    }

    async fn install_emit_reaction(hub: &EventHub, name: &str, event_kind: &str) {
        let spec = ReactionSpec {
            contract: "reaction/v1".to_string(),
            name: name.to_string(),
            version: 1,
            enabled: true,
            event_kinds: vec![event_kind.to_string()],
            when: None,
            idempotency_key: Some("key:${event.id}".to_string()),
            action: ReactionAction::EmitEvent {
                args: json!({"kind": "derived.event"}),
            },
            retry: RetryPolicy::default(),
            on_failure: FailurePolicy::DeadLetter,
        };
        hub.install_reaction(spec, false).await.unwrap();
    }

    #[tokio::test]
    async fn worker_tick_processes_claimed_record() {
        let (state, _dir) = test_state();
        let hub = &state.system_events;
        let event = emit_event(hub, "task.completed", None).await;
        install_emit_reaction(hub, "emit-derived", "task.completed").await;

        hub.outbox_store()
            .create_record(&event.id, "emit-derived", Some("key-1".to_string()))
            .unwrap();

        let pid = hub.process_instance_id().to_string();
        tick(state.clone(), hub, &pid).await.unwrap();

        let records = hub.outbox_store().load_all().unwrap();
        let original = records.iter().find(|r| r.event_id == event.id).unwrap();
        assert_eq!(original.status, OutboxStatus::Succeeded);

        let derived_events = hub.list_events(None, None, None, None).unwrap();
        assert_eq!(
            derived_events.len(),
            2,
            "should have original + derived event"
        );
    }

    #[tokio::test]
    async fn reaction_execute_audits_and_suppresses_duplicate_by_default() {
        let (state, _dir) = test_state();
        let hub = &state.system_events;
        let event = emit_event(hub, "task.completed", None).await;
        install_emit_reaction(hub, "execute-emit", "task.completed").await;
        let reaction = hub.get_reaction("execute-emit").await.unwrap();

        let first = execute_reaction_once(state.clone(), &event, &reaction, false)
            .await
            .unwrap();
        assert_eq!(first.outbox_record.status, OutboxStatus::Succeeded);

        let second = execute_reaction_once(state.clone(), &event, &reaction, false)
            .await
            .unwrap();
        assert_eq!(second.outbox_record.status, OutboxStatus::Succeeded);
        assert_eq!(
            second.outbox_record.response_summary,
            Some(json!({"reason": "idempotency_suppressed"}))
        );
        assert!(
            second.dry_run.idempotency_suppressed,
            "dry-run preview should report suppression before execute"
        );

        let derived = hub
            .list_events(None, Some("derived.event"), None, None)
            .unwrap();
        assert_eq!(
            derived.len(),
            1,
            "second execute should not emit a second derived event"
        );
    }

    #[tokio::test]
    async fn reaction_execute_force_bypasses_idempotency_suppression_only() {
        let (state, _dir) = test_state();
        let hub = &state.system_events;
        let event = emit_event(hub, "task.completed", None).await;
        install_emit_reaction(hub, "force-emit", "task.completed").await;
        let reaction = hub.get_reaction("force-emit").await.unwrap();

        execute_reaction_once(state.clone(), &event, &reaction, false)
            .await
            .unwrap();
        let forced = execute_reaction_once(state.clone(), &event, &reaction, true)
            .await
            .unwrap();
        assert_eq!(forced.outbox_record.status, OutboxStatus::Succeeded);
        assert_eq!(forced.mode, "force");
        assert!(
            forced.dry_run.idempotency_suppressed,
            "force still reports the previewed suppression"
        );

        let derived = hub
            .list_events(None, Some("derived.event"), None, None)
            .unwrap();
        assert_eq!(
            derived.len(),
            2,
            "force should execute despite prior succeeded idempotency key"
        );
    }

    #[tokio::test]
    async fn worker_http_json_connection_error_retries() {
        let (state, _dir) = test_state();
        let hub = &state.system_events;
        let event = emit_event(hub, "task.completed", None).await;

        let spec = ReactionSpec {
            contract: "reaction/v1".to_string(),
            name: "http-test".to_string(),
            version: 1,
            enabled: true,
            event_kinds: vec!["task.completed".to_string()],
            when: None,
            idempotency_key: Some("key-${event.id}".to_string()),
            action: ReactionAction::HttpJson {
                args: json!({"url": "http://127.0.0.1:1/does-not-exist"}),
            },
            retry: RetryPolicy {
                max_attempts: 3,
                backoff: "exponential".to_string(),
            },
            on_failure: FailurePolicy::DeadLetter,
        };
        hub.install_reaction(spec, false).await.unwrap();

        hub.outbox_store()
            .create_record(&event.id, "http-test", Some("key-h".to_string()))
            .unwrap();

        let pid = hub.process_instance_id().to_string();
        tick(state.clone(), hub, &pid).await.unwrap();

        let records = hub.outbox_store().load_all().unwrap();
        let rec = records.iter().find(|r| r.event_id == event.id).unwrap();
        let status = rec.status.clone();
        assert!(
            status == OutboxStatus::RetryAt || status == OutboxStatus::DeadLettered,
            "connection error should be retryable or dead-lettered, got {status:?}"
        );
    }

    #[tokio::test]
    async fn worker_idempotency_suppresses_duplicate() {
        let (state, _dir) = test_state();
        let hub = &state.system_events;
        let event = emit_event(hub, "task.completed", None).await;
        install_emit_reaction(hub, "idem-test", "task.completed").await;

        let rec1 = hub
            .outbox_store()
            .create_record(&event.id, "idem-test", Some("same-key".to_string()))
            .unwrap();
        hub.outbox_store().mark_succeeded(&rec1.id, None).unwrap();

        hub.outbox_store()
            .create_record(&event.id, "idem-test", Some("same-key".to_string()))
            .unwrap();

        let pid = hub.process_instance_id().to_string();
        tick(state.clone(), hub, &pid).await.unwrap();

        let records = hub.outbox_store().load_all().unwrap();
        let second = records.iter().find(|r| r.id != rec1.id).unwrap();
        assert_eq!(second.status, OutboxStatus::Succeeded);

        let events = hub.list_events(None, None, None, None).unwrap();
        assert_eq!(
            events.len(),
            1,
            "emit_event should not fire for suppressed record"
        );
    }

    #[tokio::test]
    async fn worker_succeeds_within_causation_depth_limit() {
        let (state, _dir) = test_state();
        let hub = &state.system_events;

        let e0 = emit_event(hub, "root.event", None).await;
        let e1 = emit_event(hub, "chain.1", Some(&e0.id)).await;
        let e2 = emit_event(hub, "chain.2", Some(&e1.id)).await;

        let chain = hub.causation_chain_for(&e2.id).unwrap();
        assert_eq!(chain.len(), 3, "e2 chain should be depth 3");

        let spec = ReactionSpec {
            contract: "reaction/v1".to_string(),
            name: "depth-ok-test".to_string(),
            version: 1,
            enabled: true,
            event_kinds: vec!["chain.2".to_string()],
            when: None,
            idempotency_key: Some("depth-${event.id}".to_string()),
            action: ReactionAction::EmitEvent {
                args: json!({"kind": "chain.3"}),
            },
            retry: RetryPolicy::default(),
            on_failure: FailurePolicy::DeadLetter,
        };
        hub.install_reaction(spec, false).await.unwrap();

        hub.outbox_store()
            .create_record(&e2.id, "depth-ok-test", Some("dkey".to_string()))
            .unwrap();

        let pid = hub.process_instance_id().to_string();
        tick(state.clone(), hub, &pid).await.unwrap();

        let records = hub.outbox_store().load_all().unwrap();
        let rec = records.iter().find(|r| r.event_id == e2.id).unwrap();
        assert_eq!(
            rec.status,
            OutboxStatus::Succeeded,
            "depth-3 chain should succeed"
        );
    }

    #[tokio::test]
    async fn worker_causation_depth_precheck_dead_letters() {
        let (state, _dir) = test_state();
        let hub = &state.system_events;

        let e0 = emit_event(hub, "root.event", None).await;
        let e1 = emit_event(hub, "chain.1", Some(&e0.id)).await;
        let e2 = emit_event(hub, "chain.2", Some(&e1.id)).await;
        let e3 = emit_event(hub, "chain.3", Some(&e2.id)).await;

        let chain = hub.causation_chain_for(&e3.id).unwrap();
        assert_eq!(chain.len(), 4, "e3 chain should be depth 4");

        let spec = ReactionSpec {
            contract: "reaction/v1".to_string(),
            name: "depth-exceed".to_string(),
            version: 1,
            enabled: true,
            event_kinds: vec!["chain.3".to_string()],
            when: None,
            idempotency_key: Some("dexc-${event.id}".to_string()),
            action: ReactionAction::EmitEvent {
                args: json!({"kind": "chain.4"}),
            },
            retry: RetryPolicy::default(),
            on_failure: FailurePolicy::DeadLetter,
        };
        hub.install_reaction(spec, false).await.unwrap();

        hub.outbox_store()
            .create_record(&e3.id, "depth-exceed", Some("dk2".to_string()))
            .unwrap();

        let pid = hub.process_instance_id().to_string();
        tick(state.clone(), hub, &pid).await.unwrap();

        let records = hub.outbox_store().load_all().unwrap();
        let rec = records.iter().find(|r| r.event_id == e3.id).unwrap();
        assert_eq!(
            rec.status,
            OutboxStatus::DeadLettered,
            "depth-4 chain should dead-letter on first tick"
        );
        let reason = rec.dead_letter_reason.as_deref().unwrap();
        assert!(
            reason.contains("causation depth"),
            "expected causation depth, got: {reason}"
        );
        assert_eq!(rec.attempt_count, 1, "should not become retry_at");
    }

    #[tokio::test]
    async fn worker_repeated_pair_blocked() {
        let (state, _dir) = test_state();
        let hub = &state.system_events;

        let e0 = emit_event(hub, "trigger.event", None).await;
        let e1 = emit_event(hub, "trigger.event", Some(&e0.id)).await;

        let spec = ReactionSpec {
            contract: "reaction/v1".to_string(),
            name: "pair-test".to_string(),
            version: 1,
            enabled: true,
            event_kinds: vec!["trigger.event".to_string()],
            when: None,
            idempotency_key: Some("pair-${event.id}".to_string()),
            action: ReactionAction::EmitEvent {
                args: json!({"kind": "trigger.event"}),
            },
            retry: RetryPolicy::default(),
            on_failure: FailurePolicy::DeadLetter,
        };
        hub.install_reaction(spec, false).await.unwrap();

        let rec0 = hub
            .outbox_store()
            .create_record(&e0.id, "pair-test", Some("p0".to_string()))
            .unwrap();
        hub.outbox_store().mark_succeeded(&rec0.id, None).unwrap();

        hub.outbox_store()
            .create_record(&e1.id, "pair-test", Some("p1".to_string()))
            .unwrap();

        let pid = hub.process_instance_id().to_string();
        tick(state.clone(), hub, &pid).await.unwrap();

        let records = hub.outbox_store().load_all().unwrap();
        let rec1 = records.iter().find(|r| r.event_id == e1.id).unwrap();
        assert_eq!(rec1.status, OutboxStatus::DeadLettered);
        let reason = rec1.dead_letter_reason.as_deref().unwrap();
        assert!(
            reason.contains("repeated pair"),
            "expected repeated pair error, got: {reason}"
        );
    }

    #[tokio::test]
    async fn worker_permanent_failure_dead_letters() {
        let (state, _dir) = test_state();
        let hub = &state.system_events;
        let event = emit_event(hub, "task.completed", None).await;

        let spec = ReactionSpec {
            contract: "reaction/v1".to_string(),
            name: "permanent-fail".to_string(),
            version: 1,
            enabled: true,
            event_kinds: vec!["task.completed".to_string()],
            when: None,
            idempotency_key: Some("exh-${event.id}".to_string()),
            action: ReactionAction::HttpJson {
                args: json!({"url": "http://127.0.0.1:1/bad"}),
            },
            retry: RetryPolicy {
                max_attempts: 1,
                backoff: "exponential".to_string(),
            },
            on_failure: FailurePolicy::DeadLetter,
        };
        hub.install_reaction(spec, false).await.unwrap();

        hub.outbox_store()
            .create_record(&event.id, "permanent-fail", Some("exh-key".to_string()))
            .unwrap();

        let pid = hub.process_instance_id().to_string();
        tick(state.clone(), hub, &pid).await.unwrap();

        let records = hub.outbox_store().load_all().unwrap();
        let rec = records.iter().find(|r| r.event_id == event.id).unwrap();
        assert_eq!(rec.status, OutboxStatus::DeadLettered);
    }

    #[tokio::test]
    async fn worker_retry_policy_respects_reaction_max_attempts() {
        let (state, _dir) = test_state();
        let hub = &state.system_events;
        let event = emit_event(hub, "task.completed", None).await;

        let spec = ReactionSpec {
            contract: "reaction/v1".to_string(),
            name: "low-retry".to_string(),
            version: 1,
            enabled: true,
            event_kinds: vec!["task.completed".to_string()],
            when: None,
            idempotency_key: Some("lr-${event.id}".to_string()),
            action: ReactionAction::HttpJson {
                args: json!({"url": "http://127.0.0.1:1/fail"}),
            },
            retry: RetryPolicy {
                max_attempts: 2,
                backoff: "exponential".to_string(),
            },
            on_failure: FailurePolicy::DeadLetter,
        };
        hub.install_reaction(spec, false).await.unwrap();

        let rec = hub
            .outbox_store()
            .create_record(&event.id, "low-retry", Some("lr-key".to_string()))
            .unwrap();

        let pid = hub.process_instance_id().to_string();
        tick(state.clone(), hub, &pid).await.unwrap();

        let loaded = hub.outbox_store().get_record(&rec.id).unwrap().unwrap();
        assert!(
            loaded.status == OutboxStatus::RetryAt || loaded.status == OutboxStatus::DeadLettered,
            "first attempt should be retryable or dead-lettered, got {:?}",
            loaded.status
        );
    }

    #[tokio::test]
    async fn worker_redacts_secrets_in_dead_letter() {
        let (state, _dir) = test_state();
        let hub = &state.system_events;
        let event = emit_event(hub, "task.completed", None).await;

        let spec = ReactionSpec {
            contract: "reaction/v1".to_string(),
            name: "leaky-reaction".to_string(),
            version: 1,
            enabled: true,
            event_kinds: vec!["task.completed".to_string()],
            when: None,
            idempotency_key: Some("leak-${event.id}".to_string()),
            action: ReactionAction::EmitEvent {
                args: json!({"kind": "derived.event"}),
            },
            retry: RetryPolicy::default(),
            on_failure: FailurePolicy::DeadLetter,
        };
        hub.install_reaction(spec, false).await.unwrap();

        let rec = hub
            .outbox_store()
            .create_record(&event.id, "leaky-reaction", Some("lk-key".to_string()))
            .unwrap();

        let now = crate::util::now_iso();
        hub.outbox_store()
            .claim_next(&now, hub.process_instance_id())
            .unwrap();
        let loaded = hub.outbox_store().get_record(&rec.id).unwrap().unwrap();

        let result = ProcessResult::Action(ActionOutcome {
            status: ActionStatus::RetryableFailure,
            response_summary: serde_json::json!({
                "error": "failed with Bearer secret:my-api-key-123"
            }),
        });
        handle_tick_result(&state, hub, &loaded, result, 1).unwrap();

        let records = hub.outbox_store().load_all().unwrap();
        let after = records.iter().find(|r| r.event_id == event.id).unwrap();
        assert_eq!(after.status, OutboxStatus::DeadLettered);

        let reason = after.dead_letter_reason.as_deref().unwrap();
        let error = after.last_error.as_deref().unwrap();
        assert!(
            !reason.contains("secret:my-api-key-123"),
            "dead_letter_reason should not contain secret ref: {reason}"
        );
        assert!(
            !error.contains("Bearer secret:"),
            "last_error should not contain Bearer secret: {error}"
        );
        assert!(
            !error.contains("my-api-key-123"),
            "last_error should not contain secret value: {error}"
        );
    }

    #[test]
    fn crash_recovery_requeues_idempotent_claims() {
        let dir = tempfile::tempdir().unwrap();
        let store = OutboxStore::new(dir.path().to_path_buf()).unwrap();
        let mut rec = OutboxRecord::new("evt-1", "react", Some("key".to_string()));
        rec.status = OutboxStatus::Claimed;
        rec.claimed_at = Some("2026-01-01T00:00:00Z".to_string());
        rec.claimed_by = Some("pid-old".to_string());
        store.append(&rec).unwrap();

        let report = store.recover_stale_claims();
        assert_eq!(report.requeued, 1);
        assert_eq!(report.dead_lettered, 0);

        let loaded = store.load_all().unwrap();
        assert_eq!(loaded[0].status, OutboxStatus::Pending);
    }

    #[test]
    fn crash_recovery_dead_letters_non_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let store = OutboxStore::new(dir.path().to_path_buf()).unwrap();
        let mut rec = OutboxRecord::new("evt-2", "react", None);
        rec.status = OutboxStatus::Claimed;
        rec.claimed_at = Some("2026-01-01T00:00:00Z".to_string());
        rec.claimed_by = Some("pid-old".to_string());
        store.append(&rec).unwrap();

        let report = store.recover_stale_claims();
        assert_eq!(report.requeued, 0);
        assert_eq!(report.dead_lettered, 1);

        let loaded = store.load_all().unwrap();
        assert_eq!(loaded[0].status, OutboxStatus::DeadLettered);
        let reason = loaded[0].dead_letter_reason.as_deref().unwrap();
        assert_eq!(reason, "crash_recovery_non_idempotent");
    }

    #[test]
    fn redact_string_removes_secret_refs() {
        let input = "failed with token=secret:my-key-123 and more";
        let out = redact_string(input);
        assert!(
            !out.contains("secret:my-key-123"),
            "secret ref should be redacted"
        );
        assert!(
            out.contains("[REDACTED]"),
            "should contain redaction marker"
        );
        assert!(out.contains("and more"), "non-secret text preserved");
    }

    #[test]
    fn redact_string_removes_bearer_tokens() {
        let input = "header Authorization: Bearer secret:api-token-xyz failed";
        let out = redact_string(input);
        assert!(
            !out.contains("api-token-xyz"),
            "bearer secret value should be redacted: {out}"
        );
        assert!(
            out.contains("[REDACTED]"),
            "should contain redaction marker: {out}"
        );
    }

    #[test]
    fn redact_string_preserves_clean_strings() {
        let input = "action 'http_json' not yet supported";
        assert_eq!(redact_string(input), input);
    }

    #[test]
    fn redact_string_handles_multiple_occurrences() {
        let input = "token=secret:key-a and secret:key-b Bearer tok123 Bearer tok456 end";
        let out = redact_string(input);
        assert!(
            !out.contains("secret:key-a"),
            "first secret should be redacted: {out}"
        );
        assert!(
            !out.contains("secret:key-b"),
            "second secret should be redacted: {out}"
        );
        assert!(
            !out.contains("tok123"),
            "first bearer value should be redacted: {out}"
        );
        assert!(
            !out.contains("tok456"),
            "second bearer value should be redacted: {out}"
        );
        assert!(
            out.contains("[REDACTED]"),
            "should contain redaction markers: {out}"
        );
    }

    // -----------------------------------------------------------------------
    // Forgejo ensure-user worker integration: retry / dead-letter / recovery
    // These tests use handle_tick_result directly so they exercise the outbox
    // worker state machine with the exact error shapes that
    // run_forgejo_ensure_user produces, without needing secrets or a live
    // Forgejo server (those are covered by system_events::forgejo tests).
    // -----------------------------------------------------------------------

    async fn install_forgejo_reaction(hub: &EventHub, max_attempts: u32) {
        let spec = ReactionSpec {
            contract: "reaction/v1".to_string(),
            name: "forgejo-ensure-bro-user".to_string(),
            version: 1,
            enabled: true,
            event_kinds: vec!["bro.identity.required".to_string()],
            when: None,
            idempotency_key: Some(
                "forgejo:${event.payload.instance}:${event.subject.id}".to_string(),
            ),
            action: ReactionAction::AtomInvoke {
                args: json!({
                    "atom": "atom:forgejo-ensure-user@v1",
                    "args": {
                        "instance": "test-forgejo",
                        "bro": "${event.payload.bro}",
                        "provider": "${event.payload.provider}",
                        "model": "${event.payload.model}",
                        "username": "${event.payload.username}",
                        "display_name": "${event.payload.display_name}",
                        "email": "${event.payload.email}",
                        "token_secret_name": "forgejo-bro-${event.payload.username}",
                        "admin_token_ref": "secret:forgejo-admin-token",
                        "base_url": "http://127.0.0.1:1"
                    }
                }),
            },
            retry: RetryPolicy {
                max_attempts,
                backoff: "exponential".to_string(),
            },
            on_failure: FailurePolicy::DeadLetter,
        };
        hub.install_reaction(spec, false).await.unwrap();
    }

    fn claim_outbox_record(hub: &EventHub, rec_id: &str) -> OutboxRecord {
        let now = crate::util::now_iso();
        hub.outbox_store()
            .claim_next(&now, hub.process_instance_id())
            .unwrap();
        hub.outbox_store().get_record(rec_id).unwrap().unwrap()
    }

    fn claim_at(hub: &EventHub, ts: &str) -> Option<OutboxRecord> {
        hub.outbox_store()
            .claim_next(ts, hub.process_instance_id())
            .unwrap()
    }

    #[tokio::test]
    async fn forgejo_worker_500_response_schedules_retry() {
        let (state, _dir) = test_state();
        let hub = &state.system_events;

        install_forgejo_reaction(hub, 5).await;
        let event = emit_event(hub, "bro.identity.required", None).await;

        let rec = hub
            .outbox_store()
            .create_record(
                &event.id,
                "forgejo-ensure-bro-user",
                Some("fgj-retry-key".to_string()),
            )
            .unwrap();

        let claimed = claim_outbox_record(hub, &rec.id);
        assert_eq!(claimed.attempt_count, 1);

        // Simulate what run_forgejo_ensure_user returns on a 500 from Forgejo.
        let outcome = ActionOutcome {
            status: ActionStatus::RetryableFailure,
            response_summary: json!({"error": "Forgejo server error getting user: HTTP 500"}),
        };
        handle_tick_result(&state, hub, &claimed, ProcessResult::Action(outcome), 5).unwrap();

        let after = hub.outbox_store().get_record(&rec.id).unwrap().unwrap();
        assert_eq!(
            after.status,
            OutboxStatus::RetryAt,
            "500 from Forgejo should schedule retry, got {:?}",
            after.status
        );
        assert!(after.next_attempt_at.is_some(), "must have next_attempt_at");
        let err = after.last_error.as_deref().unwrap();
        assert!(err.contains("500"), "last_error should mention 500: {err}");
    }

    #[tokio::test]
    async fn forgejo_worker_max_attempts_dead_letters() {
        let (state, _dir) = test_state();
        let hub = &state.system_events;

        install_forgejo_reaction(hub, 2).await;
        let event = emit_event(hub, "bro.identity.required", None).await;

        let rec = hub
            .outbox_store()
            .create_record(
                &event.id,
                "forgejo-ensure-bro-user",
                Some("fgj-dl-key".to_string()),
            )
            .unwrap();

        // First attempt
        let claimed1 = claim_outbox_record(hub, &rec.id);
        handle_tick_result(
            &state,
            hub,
            &claimed1,
            ProcessResult::Action(ActionOutcome {
                status: ActionStatus::RetryableFailure,
                response_summary: json!({"error": "Forgejo server error: HTTP 500"}),
            }),
            2,
        )
        .unwrap();
        assert_eq!(
            hub.outbox_store()
                .get_record(&rec.id)
                .unwrap()
                .unwrap()
                .status,
            OutboxStatus::RetryAt
        );

        // Second attempt (max_attempts=2, attempt_count will be 2)
        let future_ts = {
            let t = chrono::Utc::now() + chrono::Duration::seconds(600);
            t.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
        };
        let claimed2 = claim_at(hub, &future_ts).unwrap();
        assert_eq!(claimed2.attempt_count, 2, "should be on attempt 2");

        handle_tick_result(
            &state,
            hub,
            &claimed2,
            ProcessResult::Action(ActionOutcome {
                status: ActionStatus::RetryableFailure,
                response_summary: json!({"error": "Forgejo server error: HTTP 500"}),
            }),
            2,
        )
        .unwrap();

        let final_rec = hub.outbox_store().get_record(&rec.id).unwrap().unwrap();
        assert_eq!(
            final_rec.status,
            OutboxStatus::DeadLettered,
            "should dead-letter after 2 attempts, got {:?}",
            final_rec.status
        );
        let reason = final_rec.dead_letter_reason.as_deref().unwrap();
        assert!(
            reason.contains("max attempts (2) exceeded"),
            "reason should mention max attempts: {reason}"
        );

        // A blocked note should be written
        let notes_guard = state.notes.read();
        let all_notes = notes_guard.all();
        let blocked: Vec<_> = all_notes
            .iter()
            .filter(|n| n.kind == crate::notes::NoteKind::Blocked)
            .collect();
        assert_eq!(
            blocked.len(),
            1,
            "one blocked note expected from dead-letter"
        );
    }

    #[tokio::test]
    async fn forgejo_worker_retry_dead_lettered_then_recovery() {
        let (state, _dir) = test_state();
        let hub = &state.system_events;

        install_forgejo_reaction(hub, 1).await;
        let event = emit_event(hub, "bro.identity.required", None).await;

        let rec = hub
            .outbox_store()
            .create_record(
                &event.id,
                "forgejo-ensure-bro-user",
                Some("fgj-recover-key".to_string()),
            )
            .unwrap();

        // First tick: max_attempts=1, RetryableFailure → DeadLettered
        let claimed = claim_outbox_record(hub, &rec.id);
        handle_tick_result(
            &state,
            hub,
            &claimed,
            ProcessResult::Action(ActionOutcome {
                status: ActionStatus::RetryableFailure,
                response_summary: json!({"error": "connection refused"}),
            }),
            1,
        )
        .unwrap();
        assert_eq!(
            hub.outbox_store()
                .get_record(&rec.id)
                .unwrap()
                .unwrap()
                .status,
            OutboxStatus::DeadLettered,
            "max_attempts=1 on first RetryableFailure should dead-letter"
        );

        // Operator calls retry_dead_lettered after Forgejo recovers
        let requeued = hub.outbox_store().retry_dead_lettered(&rec.id).unwrap();
        assert!(requeued, "retry_dead_lettered should return true");
        let after_requeue = hub.outbox_store().get_record(&rec.id).unwrap().unwrap();
        assert_eq!(
            after_requeue.status,
            OutboxStatus::Pending,
            "should be Pending after retry_dead_lettered"
        );

        // Recovery tick: Forgejo responds, provisioning succeeds
        let future_ts = {
            let t = chrono::Utc::now() + chrono::Duration::seconds(5);
            t.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
        };
        let recovered = claim_at(hub, &future_ts).unwrap();
        handle_tick_result(
            &state,
            hub,
            &recovered,
            ProcessResult::Action(ActionOutcome {
                status: ActionStatus::Succeeded,
                response_summary: json!({
                    "action": "provisioned",
                    "username": "bro-keystone",
                    "user_id": "42",
                    "token_ref": "secret:forgejo-bro-bro-keystone"
                }),
            }),
            1,
        )
        .unwrap();

        let final_rec = hub.outbox_store().get_record(&rec.id).unwrap().unwrap();
        assert_eq!(
            final_rec.status,
            OutboxStatus::Succeeded,
            "recovered tick should succeed, got {:?}",
            final_rec.status
        );
        let summary = final_rec.response_summary.as_ref().unwrap();
        assert_eq!(summary["action"], "provisioned");
        // The token_ref in the summary should NOT contain a raw token value
        let summary_str = serde_json::to_string(summary).unwrap();
        assert!(
            !summary_str.contains("sha1"),
            "summary must not contain raw token: {summary_str}"
        );
    }

    async fn spawn_json_handler(
        status_code: u16,
        body: serde_json::Value,
    ) -> (String, tokio::task::JoinHandle<()>) {
        let app = axum::Router::new().route(
            "/test",
            axum::routing::post(move || {
                let code = status_code;
                let body = body.clone();
                async move {
                    let s = axum::http::StatusCode::from_u16(code)
                        .unwrap_or(axum::http::StatusCode::INTERNAL_SERVER_ERROR);
                    (s, axum::Json(body))
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let port = addr.port();
        let handle = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        (format!("http://127.0.0.1:{port}/test"), handle)
    }

    async fn install_http_reaction(hub: &EventHub, name: &str, url: &str, max_attempts: u32) {
        let spec = ReactionSpec {
            contract: "reaction/v1".to_string(),
            name: name.to_string(),
            version: 1,
            enabled: true,
            event_kinds: vec!["task.completed".to_string()],
            when: None,
            idempotency_key: Some("hk-${event.id}".to_string()),
            action: ReactionAction::HttpJson {
                args: json!({"url": url, "method": "POST", "body": {"test": 1}}),
            },
            retry: RetryPolicy {
                max_attempts,
                backoff: "exponential".to_string(),
            },
            on_failure: FailurePolicy::DeadLetter,
        };
        hub.install_reaction(spec, false).await.unwrap();
    }

    #[tokio::test]
    async fn worker_http_json_200_succeeds() {
        let (url, _handle) = spawn_json_handler(200, json!({"ok": true})).await;
        let (state, _dir) = test_state();
        let hub = &state.system_events;
        let event = emit_event(hub, "task.completed", None).await;
        install_http_reaction(hub, "http-ok", &url, 3).await;

        hub.outbox_store()
            .create_record(&event.id, "http-ok", Some("hk-1".to_string()))
            .unwrap();

        let pid = hub.process_instance_id().to_string();
        tick(state.clone(), hub, &pid).await.unwrap();

        let records = hub.outbox_store().load_all().unwrap();
        let rec = records.iter().find(|r| r.event_id == event.id).unwrap();
        assert_eq!(rec.status, OutboxStatus::Succeeded, "200 should succeed");
        let summary = rec.response_summary.as_ref().unwrap();
        assert_eq!(summary["status"].as_u64(), Some(200));
    }

    #[tokio::test]
    async fn worker_http_json_500_schedules_retry() {
        let (url, _handle) = spawn_json_handler(500, json!({"error": "internal"})).await;
        let (state, _dir) = test_state();
        let hub = &state.system_events;
        let event = emit_event(hub, "task.completed", None).await;
        install_http_reaction(hub, "http-500", &url, 3).await;

        hub.outbox_store()
            .create_record(&event.id, "http-500", Some("hk-2".to_string()))
            .unwrap();

        let pid = hub.process_instance_id().to_string();
        tick(state.clone(), hub, &pid).await.unwrap();

        let records = hub.outbox_store().load_all().unwrap();
        let rec = records.iter().find(|r| r.event_id == event.id).unwrap();
        assert_eq!(rec.status, OutboxStatus::RetryAt, "500 should be retry_at");
        assert!(rec.next_attempt_at.is_some());
    }

    #[tokio::test]
    async fn worker_http_json_400_dead_letters() {
        let (url, _handle) = spawn_json_handler(400, json!({"error": "bad"})).await;
        let (state, _dir) = test_state();
        let hub = &state.system_events;
        let event = emit_event(hub, "task.completed", None).await;
        install_http_reaction(hub, "http-400", &url, 3).await;

        hub.outbox_store()
            .create_record(&event.id, "http-400", Some("hk-3".to_string()))
            .unwrap();

        let pid = hub.process_instance_id().to_string();
        tick(state.clone(), hub, &pid).await.unwrap();

        let records = hub.outbox_store().load_all().unwrap();
        let rec = records.iter().find(|r| r.event_id == event.id).unwrap();
        assert_eq!(
            rec.status,
            OutboxStatus::DeadLettered,
            "400 should dead-letter"
        );
        let reason = rec.dead_letter_reason.as_deref().unwrap();
        assert!(
            reason.contains("400"),
            "reason should mention 400: {reason}"
        );
    }
}
