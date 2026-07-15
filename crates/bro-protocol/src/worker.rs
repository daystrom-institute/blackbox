//! Versioned same-host worker protocol contracts.
//!
//! These are pure wire values. Socket framing, reconnect policy, and client or
//! server state machines live in `bro-rpc`, above the contract bottom.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use bro_core::{AgentId, AtomRef, AttemptId, CommandId, SessionId, TaskId, WorkerId};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// First protocol generation described by `design/bro-harness/worker-protocol.md`.
pub const WORKER_PROTOCOL_V1: u16 = 1;

/// Reserved `SessionPolicy.attributes` key for the negotiated worker feature
/// and policy identity contract. Keeping the extension in the existing policy
/// map makes the V1 handshake additive for already shipped struct literals.
pub const WORKER_FEATURE_POLICY_ATTRIBUTE: &str = "worker_protocol_feature_policy";

/// Reserved `SessionPolicy.attributes` key for typed downstream readiness.
/// Authorization remains in `allowed_capabilities`; this value reports only
/// whether the separately configured service can currently be reached.
pub const DOWNSTREAM_SERVICE_AVAILABILITY_ATTRIBUTE: &str = "downstream_service_availability";

/// Reserved `SessionPolicy.attributes` key for the exact remote operations
/// and atom refs admitted for one durable session.
pub const SESSION_CAPABILITY_POLICY_ATTRIBUTE: &str = "session_capability_policy";

/// Capability family for projected daemon `bbox_*` tools. The tool name travels
/// as the capability operation; the fine-grained grant lives in
/// `SessionCapabilityPolicy.allowed_operations["bbox"]`. See
/// design/bro-harness/bbox-tool-projection.md.
pub const CAPABILITY_BBOX: &str = "bbox";

/// A forward-compatible worker protocol feature name.
///
/// This is a newtype rather than a closed enum so a newer peer can advertise a
/// feature without making an older peer reject the whole handshake.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct WorkerFeature(String);

impl WorkerFeature {
    pub const PREFIX: &'static str = "protocol.";
    pub const ORDERED_REPLAY: &'static str = "protocol.ordered_replay";
    pub const COMMAND_IDEMPOTENCY: &'static str = "protocol.command_idempotency";
    pub const GENERATION_FENCING: &'static str = "protocol.generation_fencing";
    pub const CAPABILITY_RPC: &'static str = "protocol.capability_rpc";
    pub const LEASE_RENEWAL: &'static str = "protocol.lease_renewal";
    pub const DRAIN_SHUTDOWN: &'static str = "protocol.drain_shutdown";

    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Identity of the session policy used for a connection generation.
/// Reconnect must fail closed if either field changes unexpectedly.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PolicyIdentity {
    pub version: u64,
    pub digest: String,
}

/// Features selected from the worker offer together with the policy identity
/// under which they were authorized.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FeaturePolicy {
    #[serde(default)]
    pub enabled_features: BTreeSet<WorkerFeature>,
    #[serde(default)]
    pub policy: PolicyIdentity,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ServiceAvailability {
    #[default]
    Unconfigured,
    Unavailable,
    Available,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DownstreamServiceAvailability {
    #[serde(default)]
    pub blackops: ServiceAvailability,
    #[serde(default)]
    pub corpus: ServiceAvailability,
}

/// Fine-grained remote authority admitted for one session.
///
/// `allowed_capabilities` remains the coarse discovery and routing projection;
/// this value is the call-time authority. Both the capability and operation
/// must match, and atom invocation additionally requires an exact versioned
/// ref. There are deliberately no wildcard semantics.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionCapabilityPolicy {
    #[serde(default)]
    pub allowed_operations: BTreeMap<String, BTreeSet<String>>,
    #[serde(default)]
    pub allowed_atom_refs: BTreeSet<AtomRef>,
}

impl SessionCapabilityPolicy {
    pub fn is_empty(&self) -> bool {
        self.allowed_operations.is_empty() && self.allowed_atom_refs.is_empty()
    }

    pub fn allows_operation(&self, capability: &str, operation: &str) -> bool {
        self.allowed_operations
            .get(capability)
            .is_some_and(|operations| operations.contains(operation))
    }

    pub fn allows_atom_ref(&self, atom_ref: &AtomRef) -> bool {
        self.allowed_atom_refs.contains(atom_ref)
    }

    pub fn validate(&self) -> Result<(), String> {
        if self.allowed_operations.len() > 64 || self.allowed_atom_refs.len() > 256 {
            return Err("session capability policy exceeds its entry bounds".into());
        }
        let mut operation_count = 0usize;
        for (capability, operations) in &self.allowed_operations {
            if !valid_policy_name(capability) || operations.is_empty() || operations.len() > 64 {
                return Err(format!(
                    "capability {capability:?} has an invalid name or operation set"
                ));
            }
            operation_count = operation_count.saturating_add(operations.len());
            for operation in operations {
                if !valid_policy_name(operation) {
                    return Err(format!("operation {operation:?} has an invalid name"));
                }
            }
        }
        if operation_count > 256 {
            return Err("session capability policy exceeds its operation bound".into());
        }
        for atom_ref in &self.allowed_atom_refs {
            if !valid_exact_atom_ref(atom_ref.as_str()) {
                return Err(format!("atom ref {atom_ref:?} is not exact and versioned"));
            }
        }
        if !self.allowed_atom_refs.is_empty() && !self.allows_operation("atom", "invoke_atom") {
            return Err("atom refs require the atom/invoke_atom operation".into());
        }
        Ok(())
    }
}

fn valid_policy_name(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
}

fn valid_exact_atom_ref(value: &str) -> bool {
    if value.is_empty() || value.len() > 256 || value.contains('*') {
        return false;
    }
    let Some(reference) = value.strip_prefix("atom:") else {
        return false;
    };
    let Some((name, version)) = reference.rsplit_once('@') else {
        return false;
    };
    !name.is_empty()
        && !version.is_empty()
        && !name.chars().any(char::is_whitespace)
        && !version.chars().any(char::is_whitespace)
}

/// Fleet-issued authorization envelope forwarded only over authenticated
/// same-host service transport. Downstream services recheck it before acting,
/// so a coarse capability label or model-facing tool filter is never enough.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CapabilityAuthorization {
    pub worker_id: WorkerId,
    pub session_id: SessionId,
    pub task_id: TaskId,
    pub attempt_id: AttemptId,
    pub session_attempt_generation: u64,
    pub policy: PolicyIdentity,
    pub capability_policy: SessionCapabilityPolicy,
}

impl CapabilityAuthorization {
    pub fn authorizes(
        &self,
        worker_id: &WorkerId,
        session_id: &SessionId,
        capability: &str,
        operation: &str,
    ) -> bool {
        &self.worker_id == worker_id
            && &self.session_id == session_id
            && !self.task_id.as_str().trim().is_empty()
            && !self.attempt_id.as_str().trim().is_empty()
            && self.session_attempt_generation > 0
            && self.policy.version > 0
            && !self.policy.digest.trim().is_empty()
            && self.capability_policy.validate().is_ok()
            && self
                .capability_policy
                .allows_operation(capability, operation)
    }
}

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

impl WorkerHello {
    /// Return only reserved protocol feature names. Other capability labels
    /// remain available to legacy callers in `worker_capabilities`.
    pub fn offered_protocol_features(&self) -> BTreeSet<WorkerFeature> {
        self.worker_capabilities
            .iter()
            .filter(|name| name.starts_with(WorkerFeature::PREFIX))
            .cloned()
            .map(WorkerFeature::new)
            .collect()
    }

    /// Replace the protocol feature portion of `worker_capabilities` while
    /// preserving non-protocol capability labels.
    pub fn set_offered_protocol_features(
        &mut self,
        features: impl IntoIterator<Item = WorkerFeature>,
    ) {
        self.worker_capabilities
            .retain(|name| !name.starts_with(WorkerFeature::PREFIX));
        self.worker_capabilities
            .extend(features.into_iter().map(|feature| feature.0));
        self.worker_capabilities.sort();
        self.worker_capabilities.dedup();
    }
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

impl SessionPolicy {
    pub fn feature_policy(&self) -> serde_json::Result<Option<FeaturePolicy>> {
        self.attributes
            .get(WORKER_FEATURE_POLICY_ATTRIBUTE)
            .cloned()
            .map(serde_json::from_value)
            .transpose()
    }

    pub fn set_feature_policy(&mut self, policy: FeaturePolicy) -> serde_json::Result<()> {
        self.attributes.insert(
            WORKER_FEATURE_POLICY_ATTRIBUTE.to_string(),
            serde_json::to_value(policy)?,
        );
        Ok(())
    }

    pub fn downstream_service_availability(
        &self,
    ) -> serde_json::Result<Option<DownstreamServiceAvailability>> {
        self.attributes
            .get(DOWNSTREAM_SERVICE_AVAILABILITY_ATTRIBUTE)
            .cloned()
            .map(serde_json::from_value)
            .transpose()
    }

    pub fn set_downstream_service_availability(
        &mut self,
        availability: DownstreamServiceAvailability,
    ) -> serde_json::Result<()> {
        self.attributes.insert(
            DOWNSTREAM_SERVICE_AVAILABILITY_ATTRIBUTE.to_string(),
            serde_json::to_value(availability)?,
        );
        Ok(())
    }

    pub fn capability_policy(&self) -> serde_json::Result<Option<SessionCapabilityPolicy>> {
        self.attributes
            .get(SESSION_CAPABILITY_POLICY_ATTRIBUTE)
            .cloned()
            .map(serde_json::from_value)
            .transpose()
    }

    pub fn set_capability_policy(
        &mut self,
        policy: SessionCapabilityPolicy,
    ) -> serde_json::Result<()> {
        self.attributes.insert(
            SESSION_CAPABILITY_POLICY_ATTRIBUTE.to_string(),
            serde_json::to_value(policy)?,
        );
        Ok(())
    }
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

/// Fleet request for replay beginning at an exact event sequence.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReplayRequest {
    pub from_event_seq: u64,
}

/// Worker response when the requested replay prefix is no longer available.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReplayUnavailable {
    pub requested_from_event_seq: u64,
    pub earliest_available_event_seq: u64,
    pub latest_available_event_seq: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WorkerCommand {
    pub command_seq: u64,
    pub command_id: CommandId,
    pub command: WorkerCommandKind,
}

/// Provider-neutral mailbox kind carried from blackopsd to one bound worker.
/// This mirrors the durable blackops mailbox without making the worker
/// protocol depend on the operational-authority implementation crate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentMailboxMessageKind {
    Send,
    Followup,
    System,
}

/// One immutable logical-agent mailbox item.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentMailboxMessage {
    pub message_id: String,
    pub sequence: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sender: Option<AgentId>,
    pub recipient: AgentId,
    pub kind: AgentMailboxMessageKind,
    pub body: String,
    pub created_at_unix_ms: u64,
}

/// Cursor-bearing delivery request for exactly one bound agent session.
///
/// `delivery_id` is stable across every retry. Messages are a contiguous
/// sequence ending at `through_sequence`. `wake` is policy, not prose:
/// queue-only sends keep an idle loop idle while followups start a turn.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentMailboxDelivery {
    pub delivery_id: String,
    pub target_agent_id: AgentId,
    pub canonical_target: String,
    pub session_id: SessionId,
    pub through_sequence: u64,
    pub wake: bool,
    pub messages: Vec<AgentMailboxMessage>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentMailboxDeliveryState {
    Pending,
    Admitted,
    Rejected,
}

/// Durable fleet receipt for an idempotent mailbox delivery request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentMailboxDeliveryReceipt {
    pub delivery_id: String,
    pub target_agent_id: AgentId,
    pub canonical_target: String,
    pub session_id: SessionId,
    pub through_sequence: u64,
    pub state: AgentMailboxDeliveryState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command_id: Option<CommandId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "type")]
pub enum WorkerCommandKind {
    UserTurn {
        text: String,
    },
    Steer {
        text: String,
    },
    AgentMailbox {
        delivery: Box<AgentMailboxDelivery>,
    },
    Interrupt,
    SetModel {
        model: String,
    },
    Compact,
    Drain {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        deadline_unix_ms: Option<u64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
        #[serde(default)]
        safe_boundary: DrainBoundary,
    },
    Shutdown {
        #[serde(default)]
        mode: ShutdownMode,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        deadline_unix_ms: Option<u64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
    },
    RequestStatus,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DrainBoundary {
    #[default]
    TurnBoundary,
    ImmediateIfIdle,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ShutdownMode {
    #[default]
    Graceful,
    Force,
}

/// Typed payload stored in a successful drain command outcome.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DrainCompletion {
    pub through_event_seq: u64,
    pub completed_at_unix_ms: u64,
    pub forced_by_deadline: bool,
}

/// Typed payload stored in a successful shutdown command outcome.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShutdownCompletion {
    pub through_event_seq: u64,
    pub completed_at_unix_ms: u64,
    pub forced: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CommandOutcome {
    pub command_seq: u64,
    pub command_id: CommandId,
    pub accepted: bool,
    pub terminal: bool,
    pub result_or_error: Value,
}

/// Highest contiguous command outcome durably recorded by fleet.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommandOutcomeAck {
    pub through_command_seq: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CapabilityRequest {
    pub call_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub invocation_id: Option<String>,
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CapabilityErrorCode {
    Unavailable,
    Unauthorized,
    InvalidRequest,
    DeadlineExceeded,
    Conflict,
    Internal,
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CapabilityError {
    pub code: CapabilityErrorCode,
    pub message: String,
    #[serde(default)]
    pub retryable: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub details: Option<Value>,
}

impl CapabilityResponse {
    pub fn success(call_id: impl Into<String>, result: Value) -> Self {
        Self {
            call_id: call_id.into(),
            result_or_error: result,
            is_error: false,
        }
    }

    pub fn error(call_id: impl Into<String>, error: CapabilityError) -> serde_json::Result<Self> {
        Ok(Self {
            call_id: call_id.into(),
            result_or_error: serde_json::to_value(error)?,
            is_error: true,
        })
    }

    pub fn structured_error(&self) -> serde_json::Result<Option<CapabilityError>> {
        self.is_error
            .then(|| serde_json::from_value(self.result_or_error.clone()))
            .transpose()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Heartbeat {
    pub lease_id: String,
    pub observed_at_unix_ms: u64,
}

/// Fleet acknowledgement of a heartbeat and renewal of the named lease.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LeaseRenewal {
    pub lease_id: String,
    pub renewed_at_unix_ms: u64,
    pub expires_at_unix_ms: u64,
    pub next_heartbeat_due_unix_ms: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProtocolErrorCode {
    InvalidEnvelope,
    UnsupportedProtocol,
    StaleGeneration,
    DuplicateMessageId,
    SequenceGap,
    CorrelationMismatch,
    Unauthorized,
    PolicyMismatch,
    FrameTooLarge,
    InvalidPayload,
    Internal,
    #[serde(other)]
    Unknown,
}

/// Peer-visible post-handshake protocol failure.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProtocolError {
    pub code: ProtocolErrorCode,
    pub message: String,
    #[serde(default)]
    pub fatal: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub related_message_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_protocol_version: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_connection_generation: Option<u64>,
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
    ReplayRequest(ReplayRequest),
    ReplayUnavailable(ReplayUnavailable),
    Command(WorkerCommand),
    CommandOutcome(CommandOutcome),
    CommandOutcomeAck(CommandOutcomeAck),
    CapabilityRequest(CapabilityRequest),
    CapabilityResponse(CapabilityResponse),
    /// Complete monotonic service policy for this still-connected generation.
    ServicePolicy(SessionPolicy),
    Heartbeat(Heartbeat),
    LeaseRenewal(LeaseRenewal),
    ProtocolError(ProtocolError),
    /// Legacy V1 compatibility only. New drain flows use CommandOutcome with
    /// a serialized DrainCompletion payload.
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
    fn cursor_bearing_agent_mailbox_command_round_trips() {
        let delivery = AgentMailboxDelivery {
            delivery_id: "mailbox:agent-1:session-1:1".into(),
            target_agent_id: AgentId::new("agent-1"),
            canonical_target: "/root/child".into(),
            session_id: SessionId::new("session-1"),
            through_sequence: 1,
            wake: false,
            messages: vec![AgentMailboxMessage {
                message_id: "message-1".into(),
                sequence: 1,
                sender: Some(AgentId::new("agent-root")),
                recipient: AgentId::new("agent-1"),
                kind: AgentMailboxMessageKind::Send,
                body: "queued information".into(),
                created_at_unix_ms: 42,
            }],
        };
        let command = WorkerMessage::Command(WorkerCommand {
            command_seq: 3,
            command_id: CommandId::new("command-3"),
            command: WorkerCommandKind::AgentMailbox {
                delivery: Box::new(delivery),
            },
        });
        let encoded = serde_json::to_value(&command).unwrap();
        assert_eq!(
            serde_json::from_value::<WorkerMessage>(encoded).unwrap(),
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

    #[test]
    fn legacy_drain_and_shutdown_commands_receive_safe_defaults() {
        let drain: WorkerCommandKind = serde_json::from_value(serde_json::json!({
            "type": "drain"
        }))
        .unwrap();
        assert_eq!(
            drain,
            WorkerCommandKind::Drain {
                deadline_unix_ms: None,
                reason: None,
                safe_boundary: DrainBoundary::TurnBoundary,
            }
        );

        let shutdown: WorkerCommandKind = serde_json::from_value(serde_json::json!({
            "type": "shutdown"
        }))
        .unwrap();
        assert_eq!(
            shutdown,
            WorkerCommandKind::Shutdown {
                mode: ShutdownMode::Graceful,
                deadline_unix_ms: None,
                reason: None,
            }
        );
    }

    #[test]
    fn feature_policy_is_typed_without_breaking_legacy_capability_labels() {
        let mut hello = WorkerHello {
            protocol_versions: vec![WORKER_PROTOCOL_V1],
            worker_build: BuildIdentity {
                version: "0.0.1".to_string(),
                build_id: "worker".to_string(),
            },
            worker_id: WorkerId::new("worker-1"),
            task_id: TaskId::new("task-1"),
            session_id: SessionId::new("session-1"),
            bootstrap_or_resume_proof: AuthenticationProof::new("secret"),
            last_local_event_seq: 0,
            last_fleet_command_seq: 0,
            worker_capabilities: vec!["probe".to_string()],
        };
        hello.set_offered_protocol_features([
            WorkerFeature::new(WorkerFeature::ORDERED_REPLAY),
            WorkerFeature::new("protocol.future_feature"),
        ]);
        assert!(hello.worker_capabilities.contains(&"probe".to_string()));
        assert_eq!(hello.offered_protocol_features().len(), 2);

        let feature_policy = FeaturePolicy {
            enabled_features: hello.offered_protocol_features(),
            policy: PolicyIdentity {
                version: 7,
                digest: "sha256:abc".to_string(),
            },
        };
        let mut policy = SessionPolicy {
            allowed_capabilities: vec!["probe".to_string()],
            attributes: BTreeMap::new(),
        };
        policy.set_feature_policy(feature_policy.clone()).unwrap();
        let availability = DownstreamServiceAvailability {
            blackops: ServiceAvailability::Unavailable,
            corpus: ServiceAvailability::Available,
        };
        policy
            .set_downstream_service_availability(availability)
            .unwrap();
        assert_eq!(policy.feature_policy().unwrap(), Some(feature_policy));
        assert_eq!(
            policy.downstream_service_availability().unwrap(),
            Some(availability)
        );
    }

    #[test]
    fn m4_control_contracts_round_trip() {
        let messages = [
            WorkerMessage::ReplayRequest(ReplayRequest { from_event_seq: 8 }),
            WorkerMessage::ReplayUnavailable(ReplayUnavailable {
                requested_from_event_seq: 8,
                earliest_available_event_seq: 12,
                latest_available_event_seq: 18,
            }),
            WorkerMessage::CommandOutcomeAck(CommandOutcomeAck {
                through_command_seq: 4,
            }),
            WorkerMessage::ServicePolicy(SessionPolicy {
                allowed_capabilities: vec!["corpus".to_string()],
                attributes: BTreeMap::new(),
            }),
            WorkerMessage::LeaseRenewal(LeaseRenewal {
                lease_id: "lease-1".to_string(),
                renewed_at_unix_ms: 100,
                expires_at_unix_ms: 200,
                next_heartbeat_due_unix_ms: 150,
            }),
            WorkerMessage::ProtocolError(ProtocolError {
                code: ProtocolErrorCode::StaleGeneration,
                message: "stale connection generation".to_string(),
                fatal: true,
                related_message_id: Some("message-1".to_string()),
                expected_protocol_version: Some(1),
                expected_connection_generation: Some(9),
            }),
        ];

        for message in messages {
            let value = serde_json::to_value(&message).unwrap();
            assert_eq!(
                serde_json::from_value::<WorkerMessage>(value).unwrap(),
                message
            );
        }
    }

    #[test]
    fn capability_error_is_structured_and_forward_compatible() {
        let response = CapabilityResponse::error(
            "call-1",
            CapabilityError {
                code: CapabilityErrorCode::Unavailable,
                message: "temporarily unavailable".to_string(),
                retryable: true,
                details: None,
            },
        )
        .unwrap();
        assert!(response.is_error);
        assert_eq!(
            response.structured_error().unwrap().unwrap().code,
            CapabilityErrorCode::Unavailable
        );

        let unknown: CapabilityError = serde_json::from_value(serde_json::json!({
            "code": "future_error",
            "message": "newer peer",
            "retryable": false
        }))
        .unwrap();
        assert_eq!(unknown.code, CapabilityErrorCode::Unknown);
    }
}
