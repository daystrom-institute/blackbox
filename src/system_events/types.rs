use std::fmt;

use serde::de::{self, Visitor};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::util::now_iso;

pub fn new_event_id() -> String {
    format!("evt-{}", uuid::Uuid::new_v4())
}

pub fn new_outbox_id() -> String {
    format!("outbox-{}", uuid::Uuid::new_v4())
}

macro_rules! known_kinds {
    ($($variant:ident => $wire:expr),* $(,)?) => {
        #[derive(Debug, Clone, PartialEq, Eq, Hash)]
        pub enum SystemEventKind {
            $($variant,)*
            Unknown(String),
        }

        impl SystemEventKind {
            pub fn to_wire(&self) -> &str {
                match self {
                    $(Self::$variant => $wire,)*
                    Self::Unknown(s) => s.as_str(),
                }
            }

            pub fn from_wire(s: &str) -> Self {
                match s {
                    $($wire => Self::$variant,)*
                    other => Self::Unknown(other.to_string()),
                }
            }
        }

        impl fmt::Display for SystemEventKind {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(self.to_wire())
            }
        }
    }
}

known_kinds! {
    BroIdentityRequired => "bro.identity.required",
    BroIdentityProvisioned => "bro.identity.provisioned",
    BroIdentityProvisionFailed => "bro.identity.provision_failed",
    TaskStarted => "task.started",
    TaskProgress => "task.progress",
    TaskCompleted => "task.completed",
    TaskFailed => "task.failed",
    TaskCancelled => "task.cancelled",
    WorkflowArcStarted => "workflow.arc.started",
    WorkflowArcNodeStarted => "workflow.arc.node_started",
    WorkflowArcNodeCompleted => "workflow.arc.node_completed",
    WorkflowArcWaitRegistered => "workflow.arc.wait_registered",
    WorkflowArcSignalReceived => "workflow.arc.signal_received",
    WorkflowArcCompleted => "workflow.arc.completed",
    WorkflowArcFailed => "workflow.arc.failed",
    WorkflowArcCancelled => "workflow.arc.cancelled",
    CoordinationIssueLinked => "coordination.issue.linked",
    CoordinationPrOpened => "coordination.pr.opened",
    CoordinationReviewPosted => "coordination.review.posted",
    CoordinationStatusChanged => "coordination.status.changed",
    CoordinationAuditCommentRequested => "coordination.audit_comment.requested",
    WhiteboardPhaseChanged => "whiteboard.phase_changed",
    WhiteboardVoteRecorded => "whiteboard.vote_recorded",
    CouncilPosted => "council.posted",
    CouncilMention => "council.mention",
}

impl Serialize for SystemEventKind {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(self.to_wire())
    }
}

impl<'de> Deserialize<'de> for SystemEventKind {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        d.deserialize_str(SystemEventKindVisitor)
    }
}

struct SystemEventKindVisitor;

impl Visitor<'_> for SystemEventKindVisitor {
    type Value = SystemEventKind;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("a system event kind string")
    }

    fn visit_str<E: de::Error>(self, v: &str) -> Result<Self::Value, E> {
        Ok(SystemEventKind::from_wire(v))
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct EventPrincipal {
    pub kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bro: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct EventSubject {
    pub kind: String,
    pub id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SystemEvent {
    pub id: String,
    pub kind: SystemEventKind,
    pub occurred_at: String,
    pub producer: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub principal: Option<EventPrincipal>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subject: Option<EventSubject>,
    #[serde(default)]
    pub correlation: serde_json::Map<String, serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub causation_id: Option<String>,
    #[serde(default)]
    pub payload: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct JournalEnvelope {
    pub schema: String,
    pub event: SystemEvent,
}

impl JournalEnvelope {
    pub const SCHEMA: &'static str = "system-event/v1";

    pub fn wrap(event: SystemEvent) -> Self {
        Self {
            schema: Self::SCHEMA.to_string(),
            event,
        }
    }
}

pub fn make_event(
    kind: SystemEventKind,
    producer: &str,
    project: Option<String>,
    principal: Option<EventPrincipal>,
    subject: Option<EventSubject>,
    correlation: serde_json::Map<String, serde_json::Value>,
    causation_id: Option<String>,
    payload: serde_json::Value,
) -> SystemEvent {
    SystemEvent {
        id: new_event_id(),
        kind,
        occurred_at: now_iso(),
        producer: producer.to_string(),
        project,
        principal,
        subject,
        correlation,
        causation_id,
        payload,
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum OutboxStatus {
    Pending,
    Claimed,
    Succeeded,
    RetryAt,
    DeadLettered,
}

impl Default for OutboxStatus {
    fn default() -> Self {
        Self::Pending
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct OutboxRecord {
    pub id: String,
    pub event_id: String,
    pub reaction_name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub idempotency_key: Option<String>,
    #[serde(default)]
    pub status: OutboxStatus,
    #[serde(default)]
    pub attempt_count: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_attempt_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub claimed_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub claimed_by: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dead_letter_reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response_summary: Option<serde_json::Value>,
    pub created_at: String,
    pub updated_at: String,
}

impl OutboxRecord {
    pub fn new(event_id: &str, reaction_name: &str, idempotency_key: Option<String>) -> Self {
        let now = now_iso();
        Self {
            id: new_outbox_id(),
            event_id: event_id.to_string(),
            reaction_name: reaction_name.to_string(),
            idempotency_key,
            status: OutboxStatus::Pending,
            attempt_count: 0,
            next_attempt_at: None,
            claimed_at: None,
            claimed_by: None,
            last_error: None,
            dead_letter_reason: None,
            response_summary: None,
            created_at: now.clone(),
            updated_at: now,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum ReactionAction {
    HttpJson { args: serde_json::Value },
    McpCall { args: serde_json::Value },
    AtomInvoke { args: serde_json::Value },
    StartWorkflow { args: serde_json::Value },
    EmitEvent { args: serde_json::Value },
}

impl ReactionAction {
    pub fn op_name(&self) -> &'static str {
        match self {
            Self::HttpJson { .. } => "http_json",
            Self::McpCall { .. } => "mcp_call",
            Self::AtomInvoke { .. } => "atom_invoke",
            Self::StartWorkflow { .. } => "start_workflow",
            Self::EmitEvent { .. } => "emit_event",
        }
    }

    pub fn requires_idempotency_key(&self) -> bool {
        !matches!(self, Self::EmitEvent { .. })
    }
}

fn default_true() -> bool {
    true
}

fn default_max_attempts() -> u32 {
    5
}

fn default_backoff() -> String {
    "exponential".to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RetryPolicy {
    #[serde(default = "default_max_attempts")]
    pub max_attempts: u32,
    #[serde(default = "default_backoff")]
    pub backoff: String,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            max_attempts: default_max_attempts(),
            backoff: default_backoff(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum FailurePolicy {
    DeadLetter,
}

impl Default for FailurePolicy {
    fn default() -> Self {
        Self::DeadLetter
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ActionStatus {
    Succeeded,
    PermanentFailure,
    RetryableFailure,
    SkippedByGate,
}

#[derive(Debug, Clone)]
pub struct ActionOutcome {
    pub status: ActionStatus,
    pub response_summary: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ReactionSpec {
    #[serde(rename = "_contract")]
    pub contract: String,
    pub name: String,
    pub version: u32,
    #[serde(default = "default_true")]
    pub enabled: bool,
    pub event_kinds: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub when: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub idempotency_key: Option<String>,
    pub action: ReactionAction,
    #[serde(default)]
    pub retry: RetryPolicy,
    #[serde(default)]
    pub on_failure: FailurePolicy,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn system_event_serde_roundtrip() {
        let event = SystemEvent {
            id: "evt-test123".to_string(),
            kind: SystemEventKind::BroIdentityRequired,
            occurred_at: "2026-05-13T12:34:56Z".to_string(),
            producer: "orchestration.dispatch".to_string(),
            project: Some("/home/user/repos/test".to_string()),
            principal: Some(EventPrincipal {
                kind: "bro".to_string(),
                bro: Some("keystone-review".to_string()),
                provider: Some("claude".to_string()),
                model: Some("haiku-4.5".to_string()),
                effort: Some("medium".to_string()),
            }),
            subject: Some(EventSubject {
                kind: "bro".to_string(),
                id: "bro:keystone-review".to_string(),
            }),
            correlation: {
                let mut m = serde_json::Map::new();
                m.insert("task_id".to_string(), json!("task-abc"));
                m
            },
            causation_id: None,
            payload: json!({"identity_scope": "forgejo"}),
        };
        let json_str = serde_json::to_string(&event).unwrap();
        let back: SystemEvent = serde_json::from_str(&json_str).unwrap();
        assert_eq!(event, back);
    }

    #[test]
    fn unknown_event_kind_roundtrips_without_panic() {
        let json_str = r#"{"id":"evt-x","kind":"future.unknown.kind","occurred_at":"2026-01-01T00:00:00Z","producer":"test","payload":null}"#;
        let event: SystemEvent = serde_json::from_str(json_str).unwrap();
        assert!(
            matches!(event.kind, SystemEventKind::Unknown(ref s) if s == "future.unknown.kind")
        );
        let back = serde_json::to_string(&event).unwrap();
        assert!(back.contains("future.unknown.kind"));
        let again: SystemEvent = serde_json::from_str(&back).unwrap();
        assert_eq!(event.kind, again.kind);
    }

    #[test]
    fn all_known_kinds_roundtrip() {
        let kinds = vec![
            SystemEventKind::BroIdentityRequired,
            SystemEventKind::BroIdentityProvisioned,
            SystemEventKind::BroIdentityProvisionFailed,
            SystemEventKind::TaskStarted,
            SystemEventKind::TaskProgress,
            SystemEventKind::TaskCompleted,
            SystemEventKind::TaskFailed,
            SystemEventKind::TaskCancelled,
            SystemEventKind::WorkflowArcStarted,
            SystemEventKind::WorkflowArcNodeStarted,
            SystemEventKind::WorkflowArcNodeCompleted,
            SystemEventKind::WorkflowArcWaitRegistered,
            SystemEventKind::WorkflowArcSignalReceived,
            SystemEventKind::WorkflowArcCompleted,
            SystemEventKind::WorkflowArcFailed,
            SystemEventKind::WorkflowArcCancelled,
            SystemEventKind::CoordinationIssueLinked,
            SystemEventKind::CoordinationPrOpened,
            SystemEventKind::CoordinationReviewPosted,
            SystemEventKind::CoordinationStatusChanged,
            SystemEventKind::CoordinationAuditCommentRequested,
            SystemEventKind::WhiteboardPhaseChanged,
            SystemEventKind::WhiteboardVoteRecorded,
            SystemEventKind::CouncilPosted,
            SystemEventKind::CouncilMention,
        ];
        for kind in &kinds {
            let json = serde_json::to_string(kind).unwrap();
            let back: SystemEventKind = serde_json::from_str(&json).unwrap();
            assert_eq!(*kind, back, "roundtrip failed for {:?}", kind);
        }
    }

    #[test]
    fn reaction_spec_serde_roundtrip() {
        let spec = ReactionSpec {
            contract: "reaction/v1".to_string(),
            name: "test-reaction".to_string(),
            version: 1,
            enabled: true,
            event_kinds: vec!["bro.identity.required".to_string()],
            when: Some("domain:system-event/forgejo-identity".to_string()),
            idempotency_key: Some("forgejo:${event.payload.instance}".to_string()),
            action: ReactionAction::HttpJson {
                args: json!({
                    "method": "POST",
                    "url": "https://example.com/api",
                    "body": {}
                }),
            },
            retry: RetryPolicy::default(),
            on_failure: FailurePolicy::DeadLetter,
        };
        let json_str = serde_json::to_string_pretty(&spec).unwrap();
        let back: ReactionSpec = serde_json::from_str(&json_str).unwrap();
        assert_eq!(spec, back);
    }

    #[test]
    fn reaction_action_tag_serializes_as_op() {
        let action = ReactionAction::McpCall {
            args: json!({"tool": "bbox_search", "args": {}}),
        };
        let v = serde_json::to_value(&action).unwrap();
        assert_eq!(v["op"], "mcp_call");
        assert!(v["args"].is_object());
    }

    #[test]
    fn outbox_record_new_defaults() {
        let rec = OutboxRecord::new("evt-1", "my-reaction", Some("key-1".to_string()));
        assert_eq!(rec.status, OutboxStatus::Pending);
        assert_eq!(rec.attempt_count, 0);
        assert_eq!(rec.event_id, "evt-1");
        assert_eq!(rec.reaction_name, "my-reaction");
        assert_eq!(rec.idempotency_key, Some("key-1".to_string()));
    }

    #[test]
    fn journal_envelope_schema() {
        let event = make_event(
            SystemEventKind::TaskStarted,
            "test",
            None,
            None,
            None,
            serde_json::Map::new(),
            None,
            json!({}),
        );
        let envelope = JournalEnvelope::wrap(event);
        assert_eq!(envelope.schema, "system-event/v1");
        let v = serde_json::to_value(&envelope).unwrap();
        assert_eq!(v["schema"], "system-event/v1");
        assert!(v["event"]["id"].is_string());
    }

    #[test]
    fn new_event_ids_are_unique() {
        let mut ids = std::collections::HashSet::new();
        for _ in 0..100 {
            let id = new_event_id();
            assert!(ids.insert(id), "duplicate event id generated");
        }
    }
}
