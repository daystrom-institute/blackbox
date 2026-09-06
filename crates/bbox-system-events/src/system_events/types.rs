use std::fmt;

use serde::de::{self, Visitor};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use bbox_util::util::now_iso;

pub fn new_event_id() -> String {
    format!("evt-{}", uuid::Uuid::new_v4())
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

impl SystemEvent {
    /// Journal discovery excludes arbitrary payload/correlation and host project paths.
    /// Open the stable event id when the event body is needed.
    pub fn summary(&self) -> serde_json::Value {
        let mut row = serde_json::json!({"id": self.id, "kind": self.kind,
            "occurred_at": self.occurred_at, "producer": self.producer});
        if let Some(subject) = &self.subject {
            row["subject"] = serde_json::json!(subject);
        }
        if self.project.is_some() {
            row["project_scoped"] = serde_json::json!(true);
        }
        row
    }
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

// Not `#[cfg(test)]` gated: consumer-crate tests build events with this
// helper while this crate compiles as a normal dependency (cfg(test) false).
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
        ];
        for kind in &kinds {
            let json = serde_json::to_string(kind).unwrap();
            let back: SystemEventKind = serde_json::from_str(&json).unwrap();
            assert_eq!(*kind, back, "roundtrip failed for {:?}", kind);
        }
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
