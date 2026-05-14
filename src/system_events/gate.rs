use anyhow::Result;
use serde_json::{Value, json};

use super::types::{ReactionSpec, SystemEvent};

#[derive(Debug, Clone, serde::Serialize)]
pub struct GateDecision {
    pub allowed: bool,
    pub verdict: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub packet_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub warning: Option<String>,
}

pub fn event_packet_entity(event: &SystemEvent) -> Value {
    let mut entity = serde_json::Map::new();
    entity.insert("kind".to_string(), json!(event.kind.to_wire()));
    entity.insert("producer".to_string(), json!(event.producer));
    if let Some(ref project) = event.project {
        entity.insert("project".to_string(), json!(project));
    }
    if let Some(ref principal) = event.principal {
        entity.insert(
            "principal".to_string(),
            serde_json::to_value(principal).unwrap_or_default(),
        );
    }
    if let Some(ref subject) = event.subject {
        entity.insert(
            "subject".to_string(),
            serde_json::to_value(subject).unwrap_or_default(),
        );
    }
    if !event.correlation.is_empty() {
        entity.insert(
            "correlation".to_string(),
            Value::Object(event.correlation.clone()),
        );
    }
    if !event.payload.is_null() {
        entity.insert("payload".to_string(), event.payload.clone());
    }
    Value::Object(entity)
}

pub fn reaction_gate_allows(
    packets_store: &crate::packets::Packets,
    reaction: &ReactionSpec,
    event: &SystemEvent,
) -> Result<GateDecision> {
    let Some(ref packet_ref) = reaction.when else {
        return Ok(GateDecision {
            allowed: true,
            verdict: None,
            packet_id: None,
            warning: Some("no when clause; reaction fires unconditionally".to_string()),
        });
    };

    let packet = packets_store
        .load(packet_ref)
        .map_err(|e| anyhow::anyhow!("loading gate packet '{}': {e:#}", packet_ref))?;
    let entity = event_packet_entity(event);
    let prediction = crate::packets::apply_with(&packet, &entity, packets_store);

    match prediction {
        Some(p) => {
            let allowed = is_allow_verdict(&p.classification);
            Ok(GateDecision {
                allowed,
                verdict: Some(p.classification),
                packet_id: Some(packet_ref.clone()),
                warning: None,
            })
        }
        None => Ok(GateDecision {
            allowed: false,
            verdict: None,
            packet_id: Some(packet_ref.clone()),
            warning: Some("no rule matched in gate packet".to_string()),
        }),
    }
}

fn is_allow_verdict(verdict: &str) -> bool {
    matches!(
        verdict,
        "allow" | "pass" | "proceed" | "fire" | "ok" | "delete" | "keep" | "yes" | "true"
    )
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct DryRunResult {
    pub rendered_idempotency_key: Option<String>,
    pub packet_entity: Value,
    pub gate: GateDecision,
    pub rendered_action_args: Value,
    pub idempotency_suppressed: bool,
}

pub fn dry_run_replay(
    reaction: &ReactionSpec,
    event: &SystemEvent,
    packets_store: &crate::packets::Packets,
    outbox_records: &[crate::system_events::types::OutboxRecord],
) -> Result<DryRunResult> {
    let roots = build_event_roots(event);
    let rendered_key = match &reaction.idempotency_key {
        Some(tpl) => Some(super::template::render_template_with_roots(
            tpl,
            &roots,
            super::template::UnresolvedPolicy::HardError,
        )?),
        None => None,
    };

    let gate = reaction_gate_allows(packets_store, reaction, event)?;

    let rendered_action_args = render_action_args(&reaction.action, &roots)?;
    let redacted_args = redact_values(rendered_action_args);

    let idempotency_suppressed = match &rendered_key {
        Some(key) => outbox_records.iter().any(|r| {
            r.reaction_name == reaction.name
                && r.idempotency_key.as_deref() == Some(key.as_str())
                && matches!(
                    r.status,
                    crate::system_events::types::OutboxStatus::Succeeded
                )
        }),
        None => false,
    };

    Ok(DryRunResult {
        rendered_idempotency_key: rendered_key,
        packet_entity: event_packet_entity(event),
        gate,
        rendered_action_args: redacted_args,
        idempotency_suppressed,
    })
}

fn build_event_roots(event: &SystemEvent) -> serde_json::Map<String, Value> {
    let mut roots = serde_json::Map::new();
    roots.insert(
        "event".to_string(),
        serde_json::to_value(event).unwrap_or_default(),
    );
    roots
}

fn render_action_args(
    action: &crate::system_events::types::ReactionAction,
    roots: &serde_json::Map<String, Value>,
) -> Result<Value> {
    let args = match action {
        crate::system_events::types::ReactionAction::HttpJson { args }
        | crate::system_events::types::ReactionAction::McpCall { args }
        | crate::system_events::types::ReactionAction::AtomInvoke { args }
        | crate::system_events::types::ReactionAction::StartWorkflow { args }
        | crate::system_events::types::ReactionAction::EmitEvent { args } => args,
    };
    render_json_value(args, roots)
}

fn render_json_value(val: &Value, roots: &serde_json::Map<String, Value>) -> Result<Value> {
    match val {
        Value::String(s) => {
            let rendered = super::template::render_template_with_roots(
                s,
                roots,
                super::template::UnresolvedPolicy::HardError,
            )?;
            Ok(Value::String(rendered))
        }
        Value::Object(map) => {
            let mut out = serde_json::Map::with_capacity(map.len());
            for (k, v) in map {
                out.insert(k.clone(), render_json_value(v, roots)?);
            }
            Ok(Value::Object(out))
        }
        Value::Array(items) => {
            let rendered: Result<Vec<Value>> =
                items.iter().map(|i| render_json_value(i, roots)).collect();
            Ok(Value::Array(rendered?))
        }
        other => Ok(other.clone()),
    }
}

pub fn redact_values(val: Value) -> Value {
    match val {
        Value::Object(map) => {
            let mut out = serde_json::Map::with_capacity(map.len());
            for (k, v) in map {
                if k.eq_ignore_ascii_case("authorization") {
                    out.insert(k, Value::String("[REDACTED]".to_string()));
                } else {
                    out.insert(k, redact_values(v));
                }
            }
            Value::Object(out)
        }
        Value::String(s) => {
            if s.contains("secret:") {
                Value::String("[REDACTED]".to_string())
            } else {
                Value::String(s)
            }
        }
        Value::Array(items) => Value::Array(items.into_iter().map(redact_values).collect()),
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::system_events::types::*;
    use serde_json::json;

    fn test_event() -> SystemEvent {
        SystemEvent {
            id: "evt-test123".to_string(),
            kind: SystemEventKind::TaskCompleted,
            occurred_at: "2026-05-13T12:00:00Z".to_string(),
            producer: "orchestration.dispatch".to_string(),
            project: Some("/repo/test".to_string()),
            principal: Some(EventPrincipal {
                kind: "bro".to_string(),
                bro: Some("reviewer".to_string()),
                provider: None,
                model: None,
                effort: None,
            }),
            subject: None,
            correlation: {
                let mut m = serde_json::Map::new();
                m.insert("task_id".to_string(), json!("task-abc"));
                m
            },
            causation_id: None,
            payload: json!({"instance": "forgejo-bro-reviewer", "elapsed": 120}),
        }
    }

    #[test]
    fn packet_entity_contains_payload_and_correlation() {
        let event = test_event();
        let entity = event_packet_entity(&event);
        let obj = entity.as_object().unwrap();
        assert!(obj.contains_key("payload"), "entity must contain payload");
        assert!(
            obj.contains_key("correlation"),
            "entity must contain correlation"
        );
        assert!(!obj.contains_key("id"), "entity must NOT contain event id");
        assert!(
            !obj.contains_key("occurred_at"),
            "entity must NOT contain timestamps"
        );
        assert!(obj.contains_key("principal"));
        assert!(obj.contains_key("producer"));
        assert!(obj.contains_key("kind"));
    }

    #[test]
    fn packet_entity_excludes_id_and_timestamps() {
        let event = test_event();
        let entity = event_packet_entity(&event);
        let s = serde_json::to_string(&entity).unwrap();
        assert!(!s.contains("evt-test123"), "event id must not appear");
        assert!(!s.contains("2026-05-13"), "timestamp must not appear");
    }

    #[test]
    fn gate_allows_when_no_when_clause() {
        let reaction = ReactionSpec {
            contract: "reaction/v1".to_string(),
            name: "test".to_string(),
            version: 1,
            enabled: true,
            event_kinds: vec!["task.completed".to_string()],
            when: None,
            idempotency_key: None,
            action: ReactionAction::EmitEvent { args: json!({}) },
            retry: RetryPolicy::default(),
            on_failure: FailurePolicy::DeadLetter,
        };
        let tmp = tempfile::tempdir().unwrap();
        let packets = crate::packets::Packets::open(tmp.path()).unwrap();
        let event = test_event();
        let decision = reaction_gate_allows(&packets, &reaction, &event).unwrap();
        assert!(decision.allowed);
        assert!(decision.warning.is_some());
    }
}
