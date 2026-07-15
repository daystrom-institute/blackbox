//! Versioned same-host worker protocol contracts.
//!
//! These are pure wire values. Socket framing, reconnect policy, and client or
//! server state machines live in `bro-rpc`, above the contract bottom.

use std::collections::BTreeMap;
use std::fmt;

use bro_core::{CommandId, SessionId, TaskId, WorkerId};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// First protocol generation described by `design/bro-harness/worker-protocol.md`.
pub const WORKER_PROTOCOL_V1: u16 = 1;

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct AuthenticationProof(String);

impl AuthenticationProof {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn expose_secret(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for AuthenticationProof {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("AuthenticationProof([REDACTED])")
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BuildIdentity {
    pub version: String,
    pub build_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkerHello {
    pub protocol_versions: Vec<u16>,
    pub worker_build: BuildIdentity,
    pub worker_id: WorkerId,
    pub task_id: TaskId,
    pub session_id: SessionId,
    /// Bootstrap material is sent only during the first connection. Reconnect
    /// implementations put their rotated proof in the same opaque field.
    pub bootstrap_or_resume_proof: AuthenticationProof,
    pub last_local_event_seq: u64,
    pub last_fleet_command_seq: u64,
    #[serde(default)]
    pub worker_capabilities: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LeaseGrant {
    pub lease_id: String,
    pub expires_at_unix_ms: u64,
    pub heartbeat_interval_ms: u64,
    pub reattach_grace_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionPolicy {
    #[serde(default)]
    pub allowed_capabilities: Vec<String>,
    #[serde(default)]
    pub attributes: BTreeMap<String, Value>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FleetWelcome {
    pub selected_protocol: u16,
    pub connection_generation: u64,
    pub event_ack: u64,
    pub next_command_seq: u64,
    pub lease: LeaseGrant,
    /// Rotated credential used for later reconnect handshakes. The bootstrap
    /// proof is one-shot and is never reused after this welcome.
    pub reconnect_proof: AuthenticationProof,
    pub session_policy: SessionPolicy,
    pub fleet_build: BuildIdentity,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HandshakeReject {
    pub code: String,
    pub message: String,
    #[serde(default)]
    pub supported_protocol_versions: Vec<u16>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "type", content = "payload")]
pub enum HandshakeMessage {
    WorkerHello(WorkerHello),
    FleetWelcome(FleetWelcome),
    Reject(HandshakeReject),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Envelope {
    pub protocol_version: u16,
    pub connection_generation: u64,
    pub message_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reply_to: Option<String>,
    pub body: WorkerMessage,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WorkerEvent {
    pub event_seq: u64,
    pub occurred_at_unix_ms: u64,
    pub event: Value,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EventAck {
    pub through_event_seq: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WorkerCommand {
    pub command_seq: u64,
    pub command_id: CommandId,
    pub command: WorkerCommandKind,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "type")]
pub enum WorkerCommandKind {
    UserTurn { text: String },
    Steer { text: String },
    Interrupt,
    SetModel { model: String },
    Compact,
    Drain,
    Shutdown,
    RequestStatus,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CommandOutcome {
    pub command_seq: u64,
    pub command_id: CommandId,
    pub accepted: bool,
    pub terminal: bool,
    pub result_or_error: Value,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CapabilityRequest {
    pub call_id: String,
    pub capability: String,
    pub operation: String,
    pub bounded_payload: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deadline_unix_ms: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CapabilityResponse {
    pub call_id: String,
    pub result_or_error: Value,
    pub is_error: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Heartbeat {
    pub lease_id: String,
    pub observed_at_unix_ms: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkerLifecycleState {
    Starting,
    Connecting,
    Active,
    Disconnected,
    Reconnecting,
    Draining,
    Terminal,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkerStatus {
    pub worker_id: WorkerId,
    pub task_id: TaskId,
    pub session_id: SessionId,
    pub worker_build: BuildIdentity,
    pub protocol_version: u16,
    pub connection_generation: u64,
    pub last_local_event_seq: u64,
    pub last_fleet_command_seq: u64,
    pub state: WorkerLifecycleState,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "type", content = "payload")]
pub enum WorkerMessage {
    Event(WorkerEvent),
    EventAck(EventAck),
    Command(WorkerCommand),
    CommandOutcome(CommandOutcome),
    CapabilityRequest(CapabilityRequest),
    CapabilityResponse(CapabilityResponse),
    Heartbeat(Heartbeat),
    DrainAck,
    Status(WorkerStatus),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn additive_unknown_fields_are_tolerated() {
        let value = serde_json::json!({
            "type": "worker_hello",
            "payload": {
                "protocol_versions": [1],
                "worker_build": {"version": "0.0.1", "build_id": "abc"},
                "worker_id": "worker-1",
                "task_id": "task-1",
                "session_id": "session-1",
                "bootstrap_or_resume_proof": "secret",
                "last_local_event_seq": 0,
                "last_fleet_command_seq": 0,
                "worker_capabilities": [],
                "future_field": true
            }
        });
        let decoded: HandshakeMessage = serde_json::from_value(value).unwrap();
        assert!(matches!(decoded, HandshakeMessage::WorkerHello(_)));
    }

    #[test]
    fn worker_command_round_trips() {
        let command = WorkerMessage::Command(WorkerCommand {
            command_seq: 7,
            command_id: CommandId::new("command-7"),
            command: WorkerCommandKind::Steer {
                text: "continue".to_string(),
            },
        });
        let value = serde_json::to_value(&command).unwrap();
        assert_eq!(
            serde_json::from_value::<WorkerMessage>(value).unwrap(),
            command
        );
    }

    #[test]
    fn authentication_proof_debug_is_redacted() {
        let proof = AuthenticationProof::new("do-not-log-me");
        let debug = format!("{proof:?}");
        assert!(!debug.contains("do-not-log-me"));
        assert!(debug.contains("REDACTED"));
    }
}
