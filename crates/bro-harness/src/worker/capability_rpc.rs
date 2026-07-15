//! Session-scoped `bro-capabilities` clients over the worker connection.
//!
//! Codec names and payload types live here so the worker and fleet router can
//! share one contract rather than duplicating string literals. The RPC client
//! is permanently bound to one worker session. Connection generations may be
//! replaced after reconnect, but callers cannot select another session or
//! authority scope in a payload.

use std::collections::BTreeSet;
use std::sync::{Arc, RwLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use bro_capabilities::{
    AgentCapability, AgentIdentity, AgentMessageRequest, AgentSpawnRequest, AgentStatus,
    AgentSummary, AgentTarget, AgentWaitRequest, AgentWake, AtomCapability, AtomInvocation,
    AtomOutput, AttemptOutcome, CorpusCapability, CorpusHit, CorpusLookup, ExecutionAccepted,
    ExecutionCapability, ExecutionRequest, RefactorCapability, RefactorPlanHandle, RefactorRequest,
    ToolCallOutput, ToolCapability, ToolInvocation,
};
use bro_core::{AttemptId, BroError, SessionId};
use bro_protocol::{
    CapabilityError, CapabilityErrorCode, CapabilityRequest, CapabilityResponse,
    ServiceAvailability, SessionPolicy, WorkerMessage,
};
use bro_rpc::{MessagePriority, PeerHandle};
use serde::Deserialize;
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::Value;

use crate::capabilities::HarnessSessionServices;

pub const CAPABILITY_TOOL: &str = "tool";
pub const OPERATION_CALL_TOOL: &str = "call_tool";
pub const CAPABILITY_CORPUS: &str = "corpus";
pub const OPERATION_SEARCH_CORPUS: &str = "search_corpus";
pub const CAPABILITY_ATOM: &str = "atom";
pub const OPERATION_INVOKE_ATOM: &str = "invoke_atom";
pub const CAPABILITY_REFACTOR: &str = "refactor";
pub const OPERATION_PLAN_REFACTOR: &str = "plan_refactor";
pub const OPERATION_MATERIALIZE_PLAN: &str = "materialize_plan";
pub const CAPABILITY_EXECUTION: &str = "execution";
pub const OPERATION_REQUEST_EXECUTION: &str = "request_execution";
pub const OPERATION_ATTEMPT_OUTCOME: &str = "attempt_outcome";
pub const CAPABILITY_AGENT: &str = "blackops.agent";
pub const OPERATION_AGENT_SPAWN: &str = "spawn";
pub const OPERATION_AGENT_SEND: &str = "send_message";
pub const OPERATION_AGENT_FOLLOWUP: &str = "followup";
pub const OPERATION_AGENT_INTERRUPT: &str = "interrupt";
pub const OPERATION_AGENT_STATUS: &str = "status";
pub const OPERATION_AGENT_LIST: &str = "list";
pub const OPERATION_AGENT_WAIT: &str = "wait";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RefactorMaterializeRequest {
    pub id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AttemptOutcomeRequest {
    pub attempt_id: AttemptId,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentListRequest {
    pub prefix: Option<String>,
}

#[derive(Clone)]
struct ConnectionSlot {
    generation: u64,
    handle: PeerHandle,
    allowed_capabilities: BTreeSet<String>,
    downstream_availability: bro_protocol::DownstreamServiceAvailability,
}

#[derive(Default)]
struct RpcState {
    connection: RwLock<Option<ConnectionSlot>>,
}

/// Generation-independent capability client retained by the live session.
#[derive(Clone)]
pub struct RpcCapabilityClient {
    session_id: SessionId,
    state: Arc<RpcState>,
    request_timeout: Duration,
}

/// Supervisor-side authority for replacing the connection generation.
#[derive(Clone)]
pub struct RpcCapabilityConnection {
    state: Arc<RpcState>,
}

impl RpcCapabilityClient {
    pub fn new(session_id: SessionId) -> (Self, RpcCapabilityConnection) {
        let state = Arc::new(RpcState::default());
        (
            Self {
                session_id,
                state: state.clone(),
                request_timeout: Duration::from_secs(30),
            },
            RpcCapabilityConnection { state },
        )
    }

    /// Build only remote-authority services admitted by fleet policy.
    ///
    /// `ToolCapability` is deliberately not installed here. File, shell, Git,
    /// V8, and working-copy tools remain worker-local and are injected by the
    /// agent loop from its filtered local registry.
    pub fn session_services(&self, policy: &SessionPolicy) -> HarnessSessionServices {
        let allowed = policy
            .allowed_capabilities
            .iter()
            .map(String::as_str)
            .collect::<BTreeSet<_>>();
        let shared = Arc::new(self.clone());
        let mut services = HarnessSessionServices::standalone();
        if allowed.contains(CAPABILITY_CORPUS) {
            services = services.with_corpus(shared.clone());
        }
        if allowed.contains(CAPABILITY_ATOM) {
            services = services.with_atoms(shared.clone());
        }
        if allowed.contains(CAPABILITY_REFACTOR) {
            services = services.with_refactor(shared.clone());
        }
        if allowed.contains(CAPABILITY_EXECUTION) {
            services = services.with_execution(shared.clone());
        }
        if allowed.contains(CAPABILITY_AGENT) {
            services = services.with_agent(shared);
        }
        services
    }

    /// Build generation-stable proxies for every remote capability while
    /// retaining the current welcome policy as the model-visible gate. A
    /// reconnect may revoke or restore a capability without rebuilding the
    /// provider session; the live connection still authorizes every call.
    pub fn reconnectable_session_services(&self, policy: &SessionPolicy) -> HarnessSessionServices {
        let revision = policy
            .feature_policy()
            .ok()
            .flatten()
            .map(|features| features.policy)
            .unwrap_or_default();
        let downstream_availability = policy
            .downstream_service_availability()
            .ok()
            .flatten()
            .unwrap_or_default();
        let shared = Arc::new(self.clone());
        HarnessSessionServices::standalone()
            .with_corpus(shared.clone())
            .with_atoms(shared.clone())
            .with_refactor(shared.clone())
            .with_execution(shared.clone())
            .with_agent(shared)
            // The session starts disconnected. The supervisor enqueues the
            // first connected policy before generation activation and before
            // replayed normal-priority input is drained.
            .with_remote_policy(
                policy.allowed_capabilities.clone(),
                downstream_availability,
                false,
                revision,
            )
    }

    async fn call<Request, Response>(
        &self,
        capability: &'static str,
        operation: &'static str,
        request: &Request,
    ) -> Result<Response, BroError>
    where
        Request: Serialize + ?Sized,
        Response: DeserializeOwned,
    {
        self.call_for_invocation(capability, operation, None, request)
            .await
    }

    async fn call_for_invocation<Request, Response>(
        &self,
        capability: &'static str,
        operation: &'static str,
        invocation_id: Option<&str>,
        request: &Request,
    ) -> Result<Response, BroError>
    where
        Request: Serialize + ?Sized,
        Response: DeserializeOwned,
    {
        let connection = self
            .state
            .connection
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
            .ok_or_else(|| unavailable(&self.session_id, capability, "worker is disconnected"))?;
        if !connection.allowed_capabilities.contains(capability) {
            return Err(BroError::new(
                "capability.unauthorized",
                format!("capability {capability} is not authorized for this session"),
            ));
        }
        let availability = match capability {
            CAPABILITY_CORPUS | CAPABILITY_REFACTOR => connection.downstream_availability.corpus,
            CAPABILITY_ATOM | CAPABILITY_AGENT => connection.downstream_availability.blackops,
            _ => ServiceAvailability::Unavailable,
        };
        if availability != ServiceAvailability::Available {
            return Err(unavailable(
                &self.session_id,
                capability,
                format!("downstream service is {availability:?}").to_ascii_lowercase(),
            ));
        }

        let payload = serde_json::to_value(request).map_err(|error| {
            BroError::new(
                "capability.invalid_request",
                format!("failed to encode {capability}/{operation}: {error}"),
            )
        })?;
        let call_id = connection.handle.next_message_id();
        let deadline_unix_ms = now_ms()
            .saturating_add(u64::try_from(self.request_timeout.as_millis()).unwrap_or(u64::MAX));
        let response = connection
            .handle
            .request_with_id(
                call_id.clone(),
                WorkerMessage::CapabilityRequest(CapabilityRequest {
                    call_id: call_id.clone(),
                    invocation_id: invocation_id.map(str::to_string),
                    capability: capability.to_string(),
                    operation: operation.to_string(),
                    bounded_payload: payload,
                    deadline_unix_ms: Some(deadline_unix_ms),
                }),
                MessagePriority::Normal,
                self.request_timeout,
            )
            .await
            .map_err(|error| unavailable(&self.session_id, capability, error.to_string()))?;
        let WorkerMessage::CapabilityResponse(response) = response.body else {
            return Err(BroError::new(
                "capability.invalid_response",
                format!("fleet returned a non-capability response for {capability}/{operation}"),
            ));
        };
        decode_response(response, capability, operation)
    }
}

impl RpcCapabilityConnection {
    pub fn activate(&self, handle: PeerHandle, policy: &SessionPolicy) {
        let generation = handle.binding().connection_generation;
        let allowed_capabilities = policy.allowed_capabilities.iter().cloned().collect();
        let downstream_availability = policy
            .downstream_service_availability()
            .ok()
            .flatten()
            .unwrap_or_default();
        *self
            .state
            .connection
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(ConnectionSlot {
            generation,
            handle,
            allowed_capabilities,
            downstream_availability,
        });
    }

    pub fn update_policy(
        &self,
        generation: u64,
        policy: &SessionPolicy,
    ) -> serde_json::Result<bool> {
        let downstream_availability = policy
            .downstream_service_availability()?
            .unwrap_or_default();
        let mut connection = self
            .state
            .connection
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let Some(slot) = connection
            .as_mut()
            .filter(|slot| slot.generation == generation)
        else {
            return Ok(false);
        };
        slot.allowed_capabilities = policy.allowed_capabilities.iter().cloned().collect();
        slot.downstream_availability = downstream_availability;
        Ok(true)
    }

    /// Detach only the named generation. A late disconnect from an old socket
    /// must never clear a newer reconnected handle.
    pub fn disconnect_generation(&self, generation: u64) {
        let mut connection = self
            .state
            .connection
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if connection
            .as_ref()
            .is_some_and(|slot| slot.generation == generation)
        {
            *connection = None;
        }
    }

    pub fn disconnect(&self) {
        *self
            .state
            .connection
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = None;
    }
}

#[async_trait]
impl ToolCapability for RpcCapabilityClient {
    async fn call_tool(&self, invocation: ToolInvocation) -> Result<ToolCallOutput, BroError> {
        self.call(CAPABILITY_TOOL, OPERATION_CALL_TOOL, &invocation)
            .await
    }
}

#[async_trait]
impl CorpusCapability for RpcCapabilityClient {
    async fn search_corpus(&self, lookup: CorpusLookup) -> Result<Vec<CorpusHit>, BroError> {
        self.call(CAPABILITY_CORPUS, OPERATION_SEARCH_CORPUS, &lookup)
            .await
    }
}

#[async_trait]
impl AtomCapability for RpcCapabilityClient {
    async fn invoke_atom(&self, invocation: AtomInvocation) -> Result<AtomOutput, BroError> {
        self.call(CAPABILITY_ATOM, OPERATION_INVOKE_ATOM, &invocation)
            .await
    }

    async fn invoke_atom_for_invocation(
        &self,
        invocation_id: &str,
        invocation: AtomInvocation,
    ) -> Result<AtomOutput, BroError> {
        self.call_for_invocation(
            CAPABILITY_ATOM,
            OPERATION_INVOKE_ATOM,
            Some(invocation_id),
            &invocation,
        )
        .await
    }
}

#[async_trait]
impl RefactorCapability for RpcCapabilityClient {
    async fn plan_refactor(
        &self,
        request: RefactorRequest,
    ) -> Result<RefactorPlanHandle, BroError> {
        self.call(CAPABILITY_REFACTOR, OPERATION_PLAN_REFACTOR, &request)
            .await
    }

    async fn materialize_plan(&self, id: String) -> Result<Value, BroError> {
        self.call(
            CAPABILITY_REFACTOR,
            OPERATION_MATERIALIZE_PLAN,
            &RefactorMaterializeRequest { id },
        )
        .await
    }
}

#[async_trait]
impl ExecutionCapability for RpcCapabilityClient {
    async fn request_execution(
        &self,
        request: ExecutionRequest,
    ) -> Result<ExecutionAccepted, BroError> {
        self.call(CAPABILITY_EXECUTION, OPERATION_REQUEST_EXECUTION, &request)
            .await
    }

    async fn attempt_outcome(&self, attempt_id: AttemptId) -> Result<AttemptOutcome, BroError> {
        self.call(
            CAPABILITY_EXECUTION,
            OPERATION_ATTEMPT_OUTCOME,
            &AttemptOutcomeRequest { attempt_id },
        )
        .await
    }
}

#[async_trait]
impl AgentCapability for RpcCapabilityClient {
    async fn spawn(&self, request: AgentSpawnRequest) -> Result<AgentIdentity, BroError> {
        self.call(CAPABILITY_AGENT, OPERATION_AGENT_SPAWN, &request)
            .await
    }

    async fn spawn_for_invocation(
        &self,
        invocation_id: &str,
        request: AgentSpawnRequest,
    ) -> Result<AgentIdentity, BroError> {
        self.call_for_invocation(
            CAPABILITY_AGENT,
            OPERATION_AGENT_SPAWN,
            Some(invocation_id),
            &request,
        )
        .await
    }

    async fn send_message(&self, request: AgentMessageRequest) -> Result<(), BroError> {
        self.call(CAPABILITY_AGENT, OPERATION_AGENT_SEND, &request)
            .await
    }

    async fn send_message_for_invocation(
        &self,
        invocation_id: &str,
        request: AgentMessageRequest,
    ) -> Result<(), BroError> {
        self.call_for_invocation(
            CAPABILITY_AGENT,
            OPERATION_AGENT_SEND,
            Some(invocation_id),
            &request,
        )
        .await
    }

    async fn followup(&self, request: AgentMessageRequest) -> Result<(), BroError> {
        self.call(CAPABILITY_AGENT, OPERATION_AGENT_FOLLOWUP, &request)
            .await
    }

    async fn followup_for_invocation(
        &self,
        invocation_id: &str,
        request: AgentMessageRequest,
    ) -> Result<(), BroError> {
        self.call_for_invocation(
            CAPABILITY_AGENT,
            OPERATION_AGENT_FOLLOWUP,
            Some(invocation_id),
            &request,
        )
        .await
    }

    async fn interrupt(&self, target: AgentTarget) -> Result<AgentStatus, BroError> {
        self.call(CAPABILITY_AGENT, OPERATION_AGENT_INTERRUPT, &target)
            .await
    }

    async fn interrupt_for_invocation(
        &self,
        invocation_id: &str,
        target: AgentTarget,
    ) -> Result<AgentStatus, BroError> {
        self.call_for_invocation(
            CAPABILITY_AGENT,
            OPERATION_AGENT_INTERRUPT,
            Some(invocation_id),
            &target,
        )
        .await
    }

    async fn status(&self, target: AgentTarget) -> Result<AgentSummary, BroError> {
        self.call(CAPABILITY_AGENT, OPERATION_AGENT_STATUS, &target)
            .await
    }

    async fn list(&self, prefix: Option<String>) -> Result<Vec<AgentSummary>, BroError> {
        self.call(
            CAPABILITY_AGENT,
            OPERATION_AGENT_LIST,
            &AgentListRequest { prefix },
        )
        .await
    }

    async fn wait(&self, request: AgentWaitRequest) -> Result<AgentWake, BroError> {
        self.call(CAPABILITY_AGENT, OPERATION_AGENT_WAIT, &request)
            .await
    }
}

/// Fleet-side helper for preserving a backend `BroError` inside the protocol's
/// structured error envelope.
pub fn response_from_bro_error(
    call_id: impl Into<String>,
    error: BroError,
) -> serde_json::Result<CapabilityResponse> {
    CapabilityResponse::error(
        call_id,
        CapabilityError {
            code: CapabilityErrorCode::Internal,
            message: error.message.clone(),
            retryable: false,
            details: Some(serde_json::json!({"bro_error": error})),
        },
    )
}

fn decode_response<T: DeserializeOwned>(
    response: CapabilityResponse,
    capability: &str,
    operation: &str,
) -> Result<T, BroError> {
    if response.is_error {
        let error = response.structured_error().map_err(|decode_error| {
            BroError::new(
                "capability.invalid_response",
                format!("failed to decode {capability}/{operation} error: {decode_error}"),
            )
        })?;
        let Some(error) = error else {
            return Err(BroError::new(
                "capability.invalid_response",
                format!("{capability}/{operation} returned an empty error"),
            ));
        };
        if let Some(bro_error) = error
            .details
            .as_ref()
            .and_then(|details| details.get("bro_error"))
            .and_then(|value| serde_json::from_value::<BroError>(value.clone()).ok())
        {
            return Err(bro_error);
        }
        return Err(BroError::new(error_code(error.code), error.message));
    }
    serde_json::from_value(response.result_or_error).map_err(|error| {
        BroError::new(
            "capability.invalid_response",
            format!("failed to decode {capability}/{operation} response: {error}"),
        )
    })
}

fn error_code(code: CapabilityErrorCode) -> &'static str {
    match code {
        CapabilityErrorCode::Unavailable => "capability.unavailable",
        CapabilityErrorCode::Unauthorized => "capability.unauthorized",
        CapabilityErrorCode::InvalidRequest => "capability.invalid_request",
        CapabilityErrorCode::DeadlineExceeded => "capability.deadline_exceeded",
        CapabilityErrorCode::Conflict => "capability.conflict",
        CapabilityErrorCode::Internal => "capability.internal",
        CapabilityErrorCode::Unknown => "capability.unknown",
    }
}

fn unavailable(
    session_id: &SessionId,
    capability: &str,
    cause: impl std::fmt::Display,
) -> BroError {
    BroError::new(
        "capability.unavailable",
        format!(
            "capability {capability} is unavailable for session {}: {cause}",
            session_id.as_str()
        ),
    )
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use bro_core::{AgentId, TaskId, WorkerId};
    use bro_protocol::{
        AuthenticationProof, BuildIdentity, DownstreamServiceAvailability, FeaturePolicy,
        FleetWelcome, LeaseGrant, PolicyIdentity, WORKER_PROTOCOL_V1, WorkerFeature, WorkerHello,
    };
    use bro_rpc::{
        FleetHandshakeGrant, HandshakeAuthorityReject, HandshakeOptions, PeerConfig, RpcPeer,
    };
    use serde_json::json;

    use super::*;

    fn policy(allowed: &[&str]) -> SessionPolicy {
        let mut policy = SessionPolicy {
            allowed_capabilities: allowed.iter().map(|value| (*value).to_string()).collect(),
            attributes: BTreeMap::new(),
        };
        policy
            .set_downstream_service_availability(DownstreamServiceAvailability {
                blackops: ServiceAvailability::Available,
                corpus: ServiceAvailability::Available,
            })
            .unwrap();
        policy
            .set_feature_policy(FeaturePolicy {
                enabled_features: [WorkerFeature::new(WorkerFeature::CAPABILITY_RPC)]
                    .into_iter()
                    .collect(),
                policy: PolicyIdentity {
                    version: 1,
                    digest: "sha256:test".to_string(),
                },
            })
            .unwrap();
        policy
    }

    async fn peer_pair(generation: u64, policy: SessionPolicy) -> (RpcPeer, RpcPeer, FleetWelcome) {
        let (worker_io, fleet_io) = tokio::io::duplex(256 * 1024);
        let hello = WorkerHello {
            protocol_versions: vec![WORKER_PROTOCOL_V1],
            worker_build: BuildIdentity {
                version: "test".to_string(),
                build_id: "worker".to_string(),
            },
            worker_id: WorkerId::new("worker-1"),
            task_id: TaskId::new("task-1"),
            session_id: SessionId::new("session-1"),
            bootstrap_or_resume_proof: AuthenticationProof::new("bootstrap"),
            last_local_event_seq: 0,
            last_fleet_command_seq: 0,
            worker_capabilities: vec![WorkerFeature::CAPABILITY_RPC.to_string()],
        };
        let grant = FleetHandshakeGrant {
            connection_generation: generation,
            event_ack: 0,
            next_command_seq: 1,
            lease: LeaseGrant {
                lease_id: format!("lease-{generation}"),
                expires_at_unix_ms: now_ms() + 60_000,
                heartbeat_interval_ms: 10_000,
                reattach_grace_ms: 10_000,
            },
            reconnect_proof: AuthenticationProof::new(format!("reconnect-{generation}")),
            session_policy: policy,
            fleet_build: BuildIdentity {
                version: "test".to_string(),
                build_id: "fleet".to_string(),
            },
        };
        let worker =
            bro_rpc::connect_worker_with_options(worker_io, hello, HandshakeOptions::default());
        let fleet = bro_rpc::accept_worker_with_authority(
            fleet_io,
            vec![WORKER_PROTOCOL_V1],
            HandshakeOptions::default(),
            move |_, _| async move { Ok::<FleetHandshakeGrant, HandshakeAuthorityReject>(grant) },
        );
        let (worker, fleet) = tokio::join!(worker, fleet);
        let (worker_io, welcome) = worker.unwrap();
        let (fleet_io, _, _) = fleet.unwrap();
        (
            RpcPeer::spawn(worker_io, PeerConfig::default()).unwrap(),
            RpcPeer::spawn(fleet_io, PeerConfig::default()).unwrap(),
            welcome,
        )
    }

    #[tokio::test]
    async fn corpus_proxy_uses_shared_codec_and_typed_result() {
        let session_policy = policy(&[CAPABILITY_CORPUS]);
        let (worker_peer, mut fleet_peer, _) = peer_pair(1, session_policy.clone()).await;
        let (client, connection) = RpcCapabilityClient::new(SessionId::new("session-1"));
        connection.activate(worker_peer.handle(), &session_policy);

        let (done_tx, done_rx) = tokio::sync::oneshot::channel();
        let fleet = tokio::spawn(async move {
            let envelope = fleet_peer.recv().await.unwrap();
            let WorkerMessage::CapabilityRequest(request) = &envelope.body else {
                panic!("expected capability request")
            };
            assert_eq!(request.capability, CAPABILITY_CORPUS);
            assert_eq!(request.operation, OPERATION_SEARCH_CORPUS);
            assert_eq!(
                serde_json::from_value::<CorpusLookup>(request.bounded_payload.clone()).unwrap(),
                CorpusLookup {
                    query: "needle".to_string(),
                    limit: 3,
                }
            );
            fleet_peer
                .handle()
                .respond(
                    &envelope,
                    WorkerMessage::CapabilityResponse(CapabilityResponse::success(
                        request.call_id.clone(),
                        json!([{"id":"hit-1","text":"found"}]),
                    )),
                    MessagePriority::Control,
                )
                .unwrap();
            let _ = done_rx.await;
        });

        let hits = client
            .search_corpus(CorpusLookup {
                query: "needle".to_string(),
                limit: 3,
            })
            .await
            .unwrap();
        assert_eq!(
            hits,
            vec![CorpusHit {
                id: "hit-1".to_string(),
                text: "found".to_string(),
            }]
        );
        let _ = done_tx.send(());
        fleet.await.unwrap();
        drop(worker_peer);
    }

    #[tokio::test]
    async fn agent_proxy_preserves_provider_invocation_identity_separately_from_rpc_call_id() {
        let session_policy = policy(&[CAPABILITY_AGENT]);
        let (worker_peer, mut fleet_peer, _) = peer_pair(1, session_policy.clone()).await;
        let (client, connection) = RpcCapabilityClient::new(SessionId::new("session-1"));
        connection.activate(worker_peer.handle(), &session_policy);

        let (done_tx, done_rx) = tokio::sync::oneshot::channel();
        let fleet = tokio::spawn(async move {
            let envelope = fleet_peer.recv().await.unwrap();
            let WorkerMessage::CapabilityRequest(request) = &envelope.body else {
                panic!("expected capability request")
            };
            assert_eq!(request.capability, CAPABILITY_AGENT);
            assert_eq!(request.operation, OPERATION_AGENT_SPAWN);
            assert_eq!(
                request.invocation_id.as_deref(),
                Some("provider-tool-call-17")
            );
            assert_ne!(request.call_id, "provider-tool-call-17");
            fleet_peer
                .handle()
                .respond(
                    &envelope,
                    WorkerMessage::CapabilityResponse(CapabilityResponse::success(
                        request.call_id.clone(),
                        json!({
                            "agent_id": "agent-child",
                            "canonical_path": "/root/child"
                        }),
                    )),
                    MessagePriority::Control,
                )
                .unwrap();
            let _ = done_rx.await;
        });

        let identity = client
            .spawn_for_invocation(
                "provider-tool-call-17",
                AgentSpawnRequest {
                    task_name: "child".into(),
                    message: "work".into(),
                    fork_turns: bro_capabilities::AgentForkTurns::None,
                },
            )
            .await
            .unwrap();
        assert_eq!(identity.agent_id, AgentId::new("agent-child"));
        let _ = done_tx.send(());
        fleet.await.unwrap();
        drop(worker_peer);
    }

    #[tokio::test]
    async fn disconnected_or_unauthorized_capability_fails_closed() {
        let (client, connection) = RpcCapabilityClient::new(SessionId::new("session-1"));
        let error = client
            .search_corpus(CorpusLookup {
                query: "needle".to_string(),
                limit: 1,
            })
            .await
            .unwrap_err();
        assert_eq!(error.code, "capability.unavailable");

        let denied_policy = policy(&[]);
        let (worker_peer, _fleet_peer, _) = peer_pair(1, denied_policy.clone()).await;
        connection.activate(worker_peer.handle(), &denied_policy);
        let error = client
            .search_corpus(CorpusLookup {
                query: "needle".to_string(),
                limit: 1,
            })
            .await
            .unwrap_err();
        assert_eq!(error.code, "capability.unauthorized");
    }

    #[tokio::test]
    async fn old_generation_disconnect_cannot_clear_reconnected_capability_client() {
        let session_policy = policy(&[CAPABILITY_CORPUS]);
        let (old_worker, _old_fleet, _) = peer_pair(1, session_policy.clone()).await;
        let (new_worker, mut new_fleet, _) = peer_pair(2, session_policy.clone()).await;
        let (client, connection) = RpcCapabilityClient::new(SessionId::new("session-1"));
        connection.activate(old_worker.handle(), &session_policy);
        connection.activate(new_worker.handle(), &session_policy);
        connection.disconnect_generation(1);

        let (done_tx, done_rx) = tokio::sync::oneshot::channel();
        let fleet = tokio::spawn(async move {
            let envelope = new_fleet.recv().await.unwrap();
            let WorkerMessage::CapabilityRequest(request) = &envelope.body else {
                panic!("expected capability request")
            };
            new_fleet
                .handle()
                .respond(
                    &envelope,
                    WorkerMessage::CapabilityResponse(CapabilityResponse::success(
                        request.call_id.clone(),
                        json!([]),
                    )),
                    MessagePriority::Control,
                )
                .unwrap();
            let _ = done_rx.await;
        });
        assert!(
            client
                .search_corpus(CorpusLookup {
                    query: "still connected".to_string(),
                    limit: 1,
                })
                .await
                .unwrap()
                .is_empty()
        );
        let _ = done_tx.send(());
        fleet.await.unwrap();
    }

    #[test]
    fn session_services_install_only_policy_admitted_remote_authority() {
        let (client, _) = RpcCapabilityClient::new(SessionId::new("session-1"));
        let services = client.session_services(&policy(&[
            CAPABILITY_CORPUS,
            CAPABILITY_ATOM,
            CAPABILITY_REFACTOR,
            CAPABILITY_EXECUTION,
            CAPABILITY_TOOL,
        ]));
        assert!(services.corpus().is_some());
        assert!(services.atoms().is_some());
        assert!(services.refactor().is_some());
        assert!(services.execution().is_some());
        assert!(
            services.tool().is_none(),
            "worker-local tools must not route through fleet capability RPC"
        );
    }

    #[test]
    fn reconnectable_services_start_disconnected_until_supervisor_activation() {
        let (client, _) = RpcCapabilityClient::new(SessionId::new("session-1"));
        let services = client.reconnectable_session_services(&policy(&[CAPABILITY_CORPUS]));
        assert!(services.corpus().is_some());
        assert!(services.agent().is_some());
        let initial = services.remote_policy().unwrap();
        assert!(!initial.connected);
        assert!(initial.allowed_capabilities.contains(CAPABILITY_CORPUS));
        assert!(!initial.allowed_capabilities.contains(CAPABILITY_AGENT));
    }
}
