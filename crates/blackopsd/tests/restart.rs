use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use async_trait::async_trait;
use blackops_core::{
    DefinitionInstallRequest, DefinitionKind, IntegrationIntentKind, IntegrationIntentTemplate,
    InvocationId, InvocationRequest, InvocationTemplate, POLL_SOURCE_CONTRACT_VERSION,
    PollSourceSpec, ScheduleId, ScheduleIntent, ScheduleTrigger, ScheduledExecution,
    WORKFLOW_NODE_CONTRACT_VERSION, WORKFLOW_SCHEMA_VERSION, WaitResolveRequest, WaitStatus,
    WorkflowDefinition, WorkflowNodeDefinition, WorkflowNodeKind, WorkflowRetryPolicy,
    WorkflowRunStatus, WorkflowTransition,
};
use blackopsd::{
    BlackopsRuntime, ExecutionProfile, FleetControlCapability, ReconcileReport,
    RoutedCapabilityRequest, import_catalog, router,
};
use bro_capabilities::{
    AgentCapability, AgentForkTurns, AgentMessageRequest, AgentSpawnRequest, AgentTarget,
    AgentWaitRequest, AgentWake, AtomCapability, AtomInvocation, AtomOutput, AttemptOutcome,
    AttemptState, ExecutionAccepted, ExecutionCapability, ExecutionKind, ExecutionRequest,
    ExecutionServiceTier, ExecutionToolPolicy, RecordEnvelope, RecordIngestCapability,
    RecordIngestReceipt, RecordIngestRequest, WorkingSetIntent,
};
use bro_core::{AtomRef, AttemptId, BroError, CommandId, Provider, SessionId, TaskId, WorkerId};
use bro_protocol::{
    AgentMailboxDelivery, AgentMailboxDeliveryReceipt, AgentMailboxDeliveryState,
    CapabilityAuthorization, CapabilityError, CapabilityErrorCode, CapabilityRequest,
    CapabilityResponse, PolicyIdentity, SessionCapabilityPolicy,
};
use serde_json::{Value, json};
use tokio::sync::Mutex;
use tower::ServiceExt;

const SERVICE_TOKEN_SECRET: &str =
    "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

fn service_token() -> Arc<bro_rpc::ServiceToken> {
    Arc::new(bro_rpc::ServiceToken::parse(SERVICE_TOKEN_SECRET).unwrap())
}

fn authorization(
    capability: &str,
    operation: &str,
    atom_refs: impl IntoIterator<Item = AtomRef>,
) -> CapabilityAuthorization {
    CapabilityAuthorization {
        worker_id: WorkerId::new("worker-root"),
        session_id: SessionId::new("session-root"),
        task_id: TaskId::new("task-root"),
        attempt_id: AttemptId::new("attempt-root"),
        session_attempt_generation: 1,
        policy: PolicyIdentity {
            version: 1,
            digest: "sha256:test-policy".into(),
        },
        capability_policy: SessionCapabilityPolicy {
            allowed_operations: BTreeMap::from([(
                capability.to_string(),
                BTreeSet::from([operation.to_string()]),
            )]),
            allowed_atom_refs: atom_refs.into_iter().collect(),
        },
    }
}

#[derive(Default)]
struct FakeFleet {
    requests: Mutex<BTreeMap<String, (ExecutionRequest, ExecutionAccepted)>>,
    outcomes: Mutex<HashMap<AttemptId, AttemptOutcome>>,
    mailbox_deliveries: Mutex<BTreeMap<String, AgentMailboxDelivery>>,
    request_calls: AtomicUsize,
    fail_after_accept_once: AtomicBool,
    complete_immediately: AtomicBool,
}

impl FakeFleet {
    fn ambiguous_once() -> Self {
        Self {
            fail_after_accept_once: AtomicBool::new(true),
            ..Self::default()
        }
    }

    fn completing() -> Self {
        Self {
            complete_immediately: AtomicBool::new(true),
            ..Self::default()
        }
    }

    async fn unique_attempts(&self) -> usize {
        self.requests.lock().await.len()
    }

    async fn set_outcome(&self, attempt_id: AttemptId, state: AttemptState) {
        self.outcomes.lock().await.insert(
            attempt_id.clone(),
            AttemptOutcome {
                attempt_id,
                state,
                result: json!({"state": format!("{state:?}")}),
            },
        );
    }
}

#[async_trait]
impl ExecutionCapability for FakeFleet {
    async fn request_execution(
        &self,
        request: ExecutionRequest,
    ) -> Result<ExecutionAccepted, BroError> {
        self.request_calls.fetch_add(1, Ordering::SeqCst);
        let mut requests = self.requests.lock().await;
        if let Some((prior_request, prior)) = requests.get(&request.idempotency_key) {
            if prior_request != &request {
                return Err(BroError::new(
                    "execution.idempotency_conflict",
                    "idempotency key was reused with a different request",
                ));
            }
            let mut prior = prior.clone();
            prior.deduplicated = true;
            return Ok(prior);
        }
        let ordinal = requests.len() + 1;
        let session_id = match &request.kind {
            ExecutionKind::Fresh { .. } => SessionId::new(format!("child-session-{ordinal}")),
            ExecutionKind::Resume { session_id, .. }
            | ExecutionKind::MailboxResume { session_id } => session_id.clone(),
        };
        let accepted = ExecutionAccepted {
            operation_id: request.operation_id.clone(),
            attempt_id: AttemptId::new(format!("attempt-{ordinal}")),
            task_id: TaskId::new(format!("task-{ordinal}")),
            session_id,
            deduplicated: false,
        };
        requests.insert(request.idempotency_key.clone(), (request, accepted.clone()));
        let complete = self.complete_immediately.load(Ordering::SeqCst);
        self.outcomes.lock().await.insert(
            accepted.attempt_id.clone(),
            AttemptOutcome {
                attempt_id: accepted.attempt_id.clone(),
                state: if complete {
                    AttemptState::Completed
                } else {
                    AttemptState::Accepted
                },
                result: if complete {
                    json!({"result": "reviewed"})
                } else {
                    Value::Null
                },
            },
        );
        if self.fail_after_accept_once.swap(false, Ordering::SeqCst) {
            return Err(BroError::new(
                "fleet.ambiguous",
                "connection ended after fleet durably accepted the request",
            ));
        }
        Ok(accepted)
    }

    async fn attempt_outcome(&self, attempt_id: AttemptId) -> Result<AttemptOutcome, BroError> {
        self.outcomes
            .lock()
            .await
            .get(&attempt_id)
            .cloned()
            .ok_or_else(|| BroError::new("attempt.not_found", attempt_id.to_string()))
    }
}

#[async_trait]
impl FleetControlCapability for FakeFleet {
    async fn interrupt_task(&self, task_id: TaskId) -> Result<Value, BroError> {
        Ok(json!({"accepted": true, "task_id": task_id}))
    }

    async fn deliver_agent_mailbox(
        &self,
        delivery: AgentMailboxDelivery,
    ) -> Result<AgentMailboxDeliveryReceipt, BroError> {
        let requests = self.requests.lock().await;
        let outcomes = self.outcomes.lock().await;
        let has_live_attempt = requests.values().any(|(_, accepted)| {
            accepted.session_id == delivery.session_id
                && outcomes.get(&accepted.attempt_id).is_some_and(|outcome| {
                    matches!(
                        outcome.state,
                        AttemptState::Accepted | AttemptState::Running
                    )
                })
        });
        drop(outcomes);
        drop(requests);
        if !has_live_attempt {
            return Ok(AgentMailboxDeliveryReceipt {
                delivery_id: delivery.delivery_id,
                target_agent_id: delivery.target_agent_id,
                canonical_target: delivery.canonical_target,
                session_id: delivery.session_id,
                through_sequence: delivery.through_sequence,
                state: AgentMailboxDeliveryState::Pending,
                command_id: None,
                error: None,
            });
        }
        let mut deliveries = self.mailbox_deliveries.lock().await;
        if let Some(prior) = deliveries.get(&delivery.delivery_id) {
            if prior != &delivery {
                return Err(BroError::new(
                    "fleet.mailbox_conflict",
                    "delivery identity was reused with different mailbox data",
                ));
            }
        } else {
            deliveries.insert(delivery.delivery_id.clone(), delivery.clone());
        }
        Ok(AgentMailboxDeliveryReceipt {
            delivery_id: delivery.delivery_id,
            target_agent_id: delivery.target_agent_id,
            canonical_target: delivery.canonical_target,
            session_id: delivery.session_id,
            through_sequence: delivery.through_sequence,
            state: AgentMailboxDeliveryState::Admitted,
            command_id: Some(CommandId::new("mailbox-command")),
            error: None,
        })
    }
}

#[derive(Default)]
struct FakeRecords {
    available: AtomicBool,
    fail_after_store_once: AtomicBool,
    seen: Mutex<BTreeMap<String, RecordEnvelope>>,
}

impl FakeRecords {
    fn available() -> Self {
        Self {
            available: AtomicBool::new(true),
            ..Self::default()
        }
    }

    fn ambiguous_once() -> Self {
        Self {
            available: AtomicBool::new(true),
            fail_after_store_once: AtomicBool::new(true),
            ..Self::default()
        }
    }
}

#[async_trait]
impl RecordIngestCapability for FakeRecords {
    async fn ingest_records(
        &self,
        request: RecordIngestRequest,
    ) -> Result<RecordIngestReceipt, BroError> {
        if !self.available.load(Ordering::SeqCst) {
            return Err(BroError::new("records.unavailable", "blackboxd is offline"));
        }
        let mut seen = self.seen.lock().await;
        let mut accepted = 0;
        let mut deduplicated = 0;
        let mut cursors = BTreeMap::new();
        for record in request.records {
            cursors.insert(record.producer.clone(), record.cursor.clone());
            if let Some(prior) = seen.get(&record.record_id) {
                assert_eq!(prior, &record);
                deduplicated += 1;
            } else {
                seen.insert(record.record_id.clone(), record);
                accepted += 1;
            }
        }
        if self.fail_after_store_once.swap(false, Ordering::SeqCst) {
            return Err(BroError::new(
                "records.ambiguous",
                "connection ended after records were durably accepted",
            ));
        }
        Ok(RecordIngestReceipt {
            accepted,
            deduplicated,
            producer_cursors: cursors,
            transcript_cursors: BTreeMap::new(),
        })
    }
}

async fn runtime(
    root: &std::path::Path,
    fleet: Arc<FakeFleet>,
    records: Arc<FakeRecords>,
) -> BlackopsRuntime {
    runtime_with_build(root, fleet, records, "blackopsd-test-build").await
}

async fn runtime_with_build(
    root: &std::path::Path,
    fleet: Arc<FakeFleet>,
    records: Arc<FakeRecords>,
    build_id: &str,
) -> BlackopsRuntime {
    BlackopsRuntime::open(
        root,
        fleet.clone(),
        fleet,
        records,
        ExecutionProfile {
            provider: Provider::Glm,
            model: "glm-test".into(),
        },
        build_id,
    )
    .await
    .unwrap()
}

async fn spawn(runtime: &BlackopsRuntime, call: &str) -> bro_capabilities::AgentIdentity {
    runtime
        .session_agents("worker-root", SessionId::new("session-root"), call)
        .spawn(AgentSpawnRequest {
            task_name: "reviewer".into(),
            message: "review the implementation".into(),
            fork_turns: AgentForkTurns::All,
        })
        .await
        .unwrap()
}

fn scheduled_execution() -> ScheduledExecution {
    ScheduledExecution {
        provider: Provider::Glm,
        model: "glm-test".into(),
        prompt: "process one polled event".into(),
        effort: None,
        service_tier: ExecutionServiceTier::Default,
        code_mode: None,
        dispatch_context: None,
        working_set: WorkingSetIntent::Scratch,
        shell_env: BTreeMap::new(),
        tool_policy: ExecutionToolPolicy::default(),
        system_prompt: None,
        output_schema: None,
        labels: BTreeMap::new(),
    }
}

fn workflow_body() -> Value {
    serde_json::to_value(WorkflowDefinition {
        schema_version: WORKFLOW_SCHEMA_VERSION,
        start: "execute".into(),
        nodes: BTreeMap::from([
            (
                "execute".into(),
                WorkflowNodeDefinition {
                    contract_version: WORKFLOW_NODE_CONTRACT_VERSION,
                    kind: WorkflowNodeKind::Execution {
                        execution: Box::new(scheduled_execution()),
                        input: json!({"phase": "execute"}),
                    },
                    transition: WorkflowTransition::Goto { to: "wait".into() },
                    retry: WorkflowRetryPolicy {
                        max_retries: 1,
                        backoff_ms: 0,
                    },
                },
            ),
            (
                "wait".into(),
                WorkflowNodeDefinition {
                    contract_version: WORKFLOW_NODE_CONTRACT_VERSION,
                    kind: WorkflowNodeKind::Wait {
                        topic: "operator.review".into(),
                        selector: json!({"change": 7}),
                        timeout_ms: Some(60_000),
                    },
                    transition: WorkflowTransition::Goto {
                        to: "integrate".into(),
                    },
                    retry: WorkflowRetryPolicy::default(),
                },
            ),
            (
                "integrate".into(),
                WorkflowNodeDefinition {
                    contract_version: WORKFLOW_NODE_CONTRACT_VERSION,
                    kind: WorkflowNodeKind::Integration {
                        intent: IntegrationIntentTemplate {
                            kind: IntegrationIntentKind::Integrate,
                            target: "change/7".into(),
                            payload: json!({"strategy": "verified"}),
                        },
                    },
                    transition: WorkflowTransition::Terminal,
                    retry: WorkflowRetryPolicy::default(),
                },
            ),
        ]),
        terminal_intents: vec![IntegrationIntentTemplate {
            kind: IntegrationIntentKind::Publish,
            target: "campaign/events".into(),
            payload: json!({"event": "workflow_finished"}),
        }],
    })
    .unwrap()
}

async fn start_poll_server() -> (String, tokio::task::JoinHandle<()>) {
    let app = axum::Router::new().route(
        "/events",
        axum::routing::get(|| async {
            axum::Json(json!({
                "items": [{"id": "durable-event-1", "value": 7}]
            }))
        }),
    );
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (format!("http://{address}/events"), server)
}

async fn mcp_request(app: &axum::Router, payload: Value) -> axum::response::Response {
    app.clone()
        .oneshot(
            axum::http::Request::builder()
                .method("POST")
                .uri("/mcp")
                .header("content-type", "application/json")
                .header("accept", "application/json, text/event-stream")
                .header("authorization", format!("Bearer {SERVICE_TOKEN_SECRET}"))
                .body(axum::body::Body::from(
                    serde_json::to_vec(&payload).unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap()
}

async fn internal_capability_request(
    app: &axum::Router,
    payload: RoutedCapabilityRequest,
) -> CapabilityResponse {
    let response = app
        .clone()
        .oneshot(
            axum::http::Request::builder()
                .method("POST")
                .uri("/internal/capability")
                .header("content-type", "application/json")
                .header("authorization", format!("Bearer {SERVICE_TOKEN_SECRET}"))
                .body(axum::body::Body::from(
                    serde_json::to_vec(&payload).unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), axum::http::StatusCode::OK);
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

#[tokio::test]
async fn restart_retries_ambiguous_submission_without_duplicate_execution() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap().join("blackops");
    let fleet = Arc::new(FakeFleet::ambiguous_once());
    let records = Arc::new(FakeRecords::available());
    let first = runtime(&root, fleet.clone(), records.clone()).await;
    let identity = spawn(&first, "spawn-call").await;
    let first_report = first.drive_once().await;
    assert_eq!(first_report.accepted, 0);
    assert_eq!(fleet.unique_attempts().await, 1);
    first.authority().shutdown().await;
    drop(first);

    let restarted = runtime(&root, fleet.clone(), records).await;
    let report = restarted.drive_once().await;
    assert_eq!(report.accepted, 1);
    assert_eq!(fleet.unique_attempts().await, 1);
    assert_eq!(fleet.request_calls.load(Ordering::SeqCst), 2);
    let path = identity.canonical_path.clone();
    let restored = restarted
        .authority()
        .call(move |authority| authority.agent_by_path(&path))
        .await
        .unwrap();
    assert_eq!(restored.agent_id, identity.agent_id);
    assert!(restored.current_attempt_id.is_some());
}

#[tokio::test]
async fn poll_cursor_survives_ambiguous_effect_delivery_and_restart() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap().join("blackops");
    let fleet = Arc::new(FakeFleet::ambiguous_once());
    let records = Arc::new(FakeRecords::available());
    let (poll_url, server) = start_poll_server().await;
    let first = runtime(&root, fleet.clone(), records.clone()).await;
    first
        .authority()
        .call(move |authority| {
            let definition = authority.install_definition(DefinitionInstallRequest {
                kind: DefinitionKind::Atom,
                name: "poll-handler".into(),
                version: "v1".into(),
                input_contract: json!({"type": "object"}),
                body: json!({}),
                activate: true,
                created_at_unix_ms: 1,
            })?;
            authority.put_schedule(ScheduleIntent {
                schedule_id: ScheduleId::new("durable-poll"),
                name: "durable local poll".into(),
                invocation: InvocationTemplate {
                    definition: definition.key,
                    input: json!({"configured": true}),
                    execution: Some(scheduled_execution()),
                },
                trigger: ScheduleTrigger::Poll {
                    every_ms: 60_000,
                    source: PollSourceSpec {
                        contract_version: POLL_SOURCE_CONTRACT_VERSION,
                        url: poll_url,
                        method: "GET".into(),
                        headers: BTreeMap::new(),
                        body: None,
                        timeout_ms: 2_000,
                        items_pointer: Some("/items".into()),
                        delivery_id_pointer: Some("/id".into()),
                        max_items: 4,
                    },
                },
                enabled: true,
                next_due_unix_ms: Some(0),
                generation: 1,
                created_at_unix_ms: 1,
                updated_at_unix_ms: 1,
            })?;
            Ok(())
        })
        .await
        .unwrap();

    let ambiguous = first.drive_once().await;
    assert_eq!(ambiguous.poll_sources_fetched, 1);
    assert_eq!(ambiguous.poll_deliveries_admitted, 1);
    assert_eq!(ambiguous.accepted, 0);
    assert!(!ambiguous.errors.is_empty());
    assert_eq!(fleet.unique_attempts().await, 1);
    let before_restart = first.authority().snapshot().await.unwrap();
    let cursor = &before_restart.poll_cursors[&ScheduleId::new("durable-poll")];
    assert_eq!(cursor.fetch_sequence, 1);
    assert_eq!(cursor.deliveries.len(), 1);
    assert_eq!(before_restart.invocations.len(), 1);
    assert_eq!(before_restart.fleet_outbox.len(), 1);
    first.authority().shutdown().await;
    drop(first);

    let restarted = runtime(&root, fleet.clone(), records).await;
    let recovered = restarted.drive_once().await;
    assert_eq!(recovered.poll_sources_fetched, 0);
    assert_eq!(recovered.accepted, 1);
    assert_eq!(fleet.unique_attempts().await, 1);
    assert_eq!(fleet.request_calls.load(Ordering::SeqCst), 2);
    let after_restart = restarted.authority().snapshot().await.unwrap();
    let cursor = &after_restart.poll_cursors[&ScheduleId::new("durable-poll")];
    assert_eq!(cursor.fetch_sequence, 1);
    assert_eq!(cursor.deliveries.len(), 1);
    assert!(after_restart.fleet_outbox.is_empty());
    server.abort();
    let _ = server.await;
}

#[tokio::test]
async fn workflow_retries_and_each_suspension_boundary_survive_restart() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap().join("blackops");
    let fleet = Arc::new(FakeFleet::default());
    let records = Arc::new(FakeRecords::available());
    let invocation_id = InvocationId::new("restart-workflow");
    let first = runtime(&root, fleet.clone(), records.clone()).await;
    let invocation_for_install = invocation_id.clone();
    first
        .authority()
        .call(move |authority| {
            let definition = authority.install_definition(DefinitionInstallRequest {
                kind: DefinitionKind::Workflow,
                name: "restart-review".into(),
                version: "v1".into(),
                input_contract: json!({"type": "object"}),
                body: workflow_body(),
                activate: true,
                created_at_unix_ms: 1,
            })?;
            authority.request_invocation(InvocationRequest {
                invocation_id: invocation_for_install,
                definition: definition.key,
                input: json!({"change": 7}),
                execution: None,
                requested_at_unix_ms: 2,
            })?;
            Ok(())
        })
        .await
        .unwrap();
    let requested = first.authority().snapshot().await.unwrap();
    let stable_operation_id = requested.workflow_runs[&invocation_id].nodes["execute"]
        .operation_id
        .clone()
        .unwrap();
    assert_eq!(requested.fleet_outbox.len(), 1);
    first.authority().shutdown().await;
    drop(first);

    let accepted_runtime = runtime(&root, fleet.clone(), records.clone()).await;
    let accepted = accepted_runtime.drive_once().await;
    assert_eq!(accepted.accepted, 1);
    let accepted_snapshot = accepted_runtime.authority().snapshot().await.unwrap();
    let first_attempt = accepted_snapshot.operations[&stable_operation_id]
        .accepted
        .as_ref()
        .unwrap()
        .attempt_id
        .clone();
    fleet.set_outcome(first_attempt, AttemptState::Failed).await;
    let failed = accepted_runtime.drive_once().await;
    assert_eq!(failed.terminal, 1);
    assert_eq!(
        accepted_runtime
            .authority()
            .snapshot()
            .await
            .unwrap()
            .workflow_runs[&invocation_id]
            .status,
        WorkflowRunStatus::RetryScheduled
    );
    accepted_runtime.authority().shutdown().await;
    drop(accepted_runtime);

    let retry_runtime = runtime(&root, fleet.clone(), records.clone()).await;
    let retried = retry_runtime.drive_once().await;
    assert_eq!(retried.workflow_retries_started, 1);
    assert_eq!(retried.accepted, 1);
    assert_eq!(fleet.unique_attempts().await, 2);
    let retry_snapshot = retry_runtime.authority().snapshot().await.unwrap();
    assert_eq!(
        retry_snapshot.workflow_runs[&invocation_id].nodes["execute"]
            .operation_id
            .as_ref(),
        Some(&stable_operation_id)
    );
    let retry_attempt = retry_snapshot.operations[&stable_operation_id]
        .accepted
        .as_ref()
        .unwrap()
        .attempt_id
        .clone();
    fleet
        .set_outcome(retry_attempt, AttemptState::Completed)
        .await;
    let completed_node = retry_runtime.drive_once().await;
    assert_eq!(completed_node.terminal, 1);
    let waiting = retry_runtime.authority().snapshot().await.unwrap();
    let wait_id = waiting.workflow_runs[&invocation_id]
        .waiting_on
        .clone()
        .unwrap();
    assert_eq!(
        waiting.workflow_runs[&invocation_id].status,
        WorkflowRunStatus::Waiting
    );
    retry_runtime.authority().shutdown().await;
    drop(retry_runtime);

    let waiting_runtime = runtime(&root, fleet.clone(), records.clone()).await;
    waiting_runtime
        .authority()
        .call(move |authority| {
            authority.resolve_wait(
                &wait_id,
                WaitResolveRequest {
                    status: WaitStatus::Satisfied,
                    resolution: json!({"approved": true}),
                    resolved_at_unix_ms: u64::MAX,
                },
            )
        })
        .await
        .unwrap();
    let terminal = waiting_runtime.authority().snapshot().await.unwrap();
    assert_eq!(
        terminal.workflow_runs[&invocation_id].status,
        WorkflowRunStatus::Completed
    );
    assert_eq!(
        terminal.workflow_runs[&invocation_id]
            .integration_intent_ids
            .len(),
        2
    );
    waiting_runtime.authority().shutdown().await;
    drop(waiting_runtime);

    let terminal_runtime = runtime(&root, fleet, records).await;
    let restored = terminal_runtime.authority().snapshot().await.unwrap();
    assert_eq!(
        restored.workflow_runs[&invocation_id].status,
        WorkflowRunStatus::Completed
    );
    assert_eq!(
        restored.workflow_runs[&invocation_id]
            .integration_intent_ids
            .len(),
        2
    );
}

#[tokio::test]
async fn identity_mailbox_cursor_and_terminal_reconciliation_survive_restart() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap().join("blackops");
    let fleet = Arc::new(FakeFleet::default());
    let records = Arc::new(FakeRecords::available());
    let first = runtime(&root, fleet.clone(), records.clone()).await;
    let identity = spawn(&first, "spawn-call").await;
    let accepted = first.drive_once().await;
    assert_eq!(accepted.accepted, 1);
    let path = identity.canonical_path.clone();
    let agent = first
        .authority()
        .call(move |authority| authority.agent_by_path(&path))
        .await
        .unwrap();
    let attempt_id = agent.current_attempt_id.clone().unwrap();
    fleet
        .set_outcome(attempt_id.clone(), AttemptState::Completed)
        .await;
    let terminal = first.drive_once().await;
    assert_eq!(terminal.terminal, 1);

    first
        .session_agents("worker-root", SessionId::new("session-root"), "send-call")
        .send_message(AgentMessageRequest {
            target: AgentTarget {
                canonical_path: identity.canonical_path.clone(),
            },
            message: "context only".into(),
        })
        .await
        .unwrap();
    let wake = first
        .session_agents("worker-root", SessionId::new("session-root"), "wait-call")
        .wait(AgentWaitRequest {
            timeout_ms: Some(1),
            path_prefix: Some(identity.canonical_path.clone()),
            after_mailbox_sequence: Some(0),
        })
        .await
        .unwrap();
    assert!(matches!(
        wake,
        AgentWake::DescendantStatus {
            agent: bro_capabilities::AgentSummary {
                status: bro_capabilities::AgentStatus::Completed,
                ..
            }
        }
    ));
    let wake = first
        .session_agents(
            "worker-root",
            SessionId::new("session-root"),
            "wait-mailbox-call",
        )
        .wait(AgentWaitRequest {
            timeout_ms: Some(1),
            path_prefix: Some(identity.canonical_path.clone()),
            after_mailbox_sequence: Some(0),
        })
        .await
        .unwrap();
    assert_eq!(wake, AgentWake::Timeout);

    let snapshot = first.authority().snapshot().await.unwrap();
    let caller_path = snapshot
        .agents
        .values()
        .find(|agent| agent.current_session_id == Some(SessionId::new("session-root")))
        .unwrap()
        .path
        .clone();
    first
        .session_agents(
            "worker-root",
            SessionId::new("session-root"),
            "send-caller-mailbox",
        )
        .send_message(AgentMessageRequest {
            target: AgentTarget {
                canonical_path: caller_path,
            },
            message: "caller context".into(),
        })
        .await
        .unwrap();
    let wake = first
        .session_agents(
            "worker-root",
            SessionId::new("session-root"),
            "wait-caller-mailbox",
        )
        .wait(AgentWaitRequest {
            timeout_ms: Some(1),
            path_prefix: Some(identity.canonical_path.clone()),
            after_mailbox_sequence: Some(0),
        })
        .await
        .unwrap();
    assert_eq!(
        wake,
        AgentWake::MailboxChanged {
            through_sequence: 1
        }
    );
    let wake = first
        .session_agents(
            "worker-root",
            SessionId::new("session-root"),
            "wait-no-repeat-call",
        )
        .wait(AgentWaitRequest {
            timeout_ms: Some(1),
            path_prefix: Some(identity.canonical_path.clone()),
            after_mailbox_sequence: Some(1),
        })
        .await
        .unwrap();
    assert_eq!(wake, AgentWake::Timeout);
    first.authority().shutdown().await;
    drop(first);

    let restarted = runtime(&root, fleet, records).await;
    let snapshot = restarted.authority().snapshot().await.unwrap();
    let agent = snapshot.agents.get(&identity.agent_id).unwrap();
    assert_eq!(agent.path, identity.canonical_path);
    let mailbox = snapshot.mailboxes.get(&identity.agent_id).unwrap();
    assert_eq!(mailbox.messages.len(), 1);
    assert!(!mailbox.cursors.contains_key("session:session-root"));
    let caller = snapshot
        .agents
        .values()
        .find(|agent| agent.current_session_id == Some(SessionId::new("session-root")))
        .unwrap();
    assert_eq!(
        snapshot.mailboxes[&caller.agent_id].cursors["session:session-root"].last_sequence,
        1
    );
    assert!(
        snapshot
            .operations
            .values()
            .any(|operation| operation.status == blackops_core::OperationStatus::Completed)
    );
}

#[tokio::test]
async fn completed_agent_followup_has_one_mailbox_input_and_attempt_owned_terminal_state() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap().join("blackops");
    let fleet = Arc::new(FakeFleet::default());
    let records = Arc::new(FakeRecords::available());
    let runtime = runtime(&root, fleet.clone(), records).await;
    let identity = spawn(&runtime, "spawn-followup-target").await;
    assert_eq!(runtime.drive_once().await.accepted, 1);
    let snapshot = runtime.authority().snapshot().await.unwrap();
    let first_attempt = snapshot.agents[&identity.agent_id]
        .current_attempt_id
        .clone()
        .unwrap();
    fleet
        .set_outcome(first_attempt, AttemptState::Completed)
        .await;
    assert_eq!(runtime.drive_once().await.terminal, 1);

    runtime
        .session_agents(
            "worker-root",
            SessionId::new("session-root"),
            "followup-completed-target",
        )
        .followup(AgentMessageRequest {
            target: AgentTarget {
                canonical_path: identity.canonical_path.clone(),
            },
            message: "inspect the final regression".into(),
        })
        .await
        .unwrap();
    let snapshot = runtime.authority().snapshot().await.unwrap();
    let followup = snapshot
        .operations
        .values()
        .find(|operation| {
            matches!(
                operation.kind,
                blackops_core::OperationKind::Followup { .. }
            )
        })
        .unwrap();
    assert_eq!(followup.status, blackops_core::OperationStatus::Requested);
    assert!(matches!(
        followup.execution_request.as_ref().unwrap().kind,
        ExecutionKind::MailboxResume { .. }
    ));

    let accepted = runtime.drive_once().await;
    assert_eq!(accepted.accepted, 1);
    assert!(fleet.mailbox_deliveries.lock().await.is_empty());
    let snapshot = runtime.authority().snapshot().await.unwrap();
    let followup = snapshot
        .operations
        .values()
        .find(|operation| {
            matches!(
                operation.kind,
                blackops_core::OperationKind::Followup { .. }
            )
        })
        .unwrap();
    assert_eq!(followup.status, blackops_core::OperationStatus::Accepted);
    assert_eq!(
        snapshot.agents[&identity.agent_id].status,
        blackops_core::LogicalAgentStatus::Running
    );
    let followup_attempt = followup.accepted.as_ref().unwrap().attempt_id.clone();

    runtime.drive_once().await;
    let deliveries = fleet.mailbox_deliveries.lock().await;
    assert_eq!(deliveries.len(), 1);
    let delivery = deliveries.values().next().unwrap();
    assert!(delivery.wake);
    assert_eq!(delivery.messages.len(), 1);
    assert_eq!(delivery.messages[0].body, "inspect the final regression");
    drop(deliveries);
    let snapshot = runtime.authority().snapshot().await.unwrap();
    let followup = snapshot
        .operations
        .values()
        .find(|operation| {
            matches!(
                operation.kind,
                blackops_core::OperationKind::Followup { .. }
            )
        })
        .unwrap();
    assert_eq!(
        followup.status,
        blackops_core::OperationStatus::Accepted,
        "worker admission is not execution completion"
    );

    fleet
        .set_outcome(followup_attempt, AttemptState::Completed)
        .await;
    assert_eq!(runtime.drive_once().await.terminal, 1);
    let snapshot = runtime.authority().snapshot().await.unwrap();
    let agent = &snapshot.agents[&identity.agent_id];
    assert_eq!(agent.status, blackops_core::LogicalAgentStatus::Completed);
    let terminal_updated_at = agent.updated_at_unix_ms;
    assert_eq!(runtime.drive_once().await.terminal, 0);
    let snapshot = runtime.authority().snapshot().await.unwrap();
    assert_eq!(
        snapshot.agents[&identity.agent_id].updated_at_unix_ms, terminal_updated_at,
        "terminal reconciliation is idempotent"
    );
}

#[tokio::test]
async fn record_outbox_catches_up_after_blackboxd_outage() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap().join("blackops");
    let fleet = Arc::new(FakeFleet::default());
    let records = Arc::new(FakeRecords::default());
    let runtime = runtime(&root, fleet, records.clone()).await;
    spawn(&runtime, "spawn-call").await;
    let degraded: ReconcileReport = runtime.drive_once().await;
    assert!(!degraded.errors.is_empty());
    assert!(
        !runtime
            .authority()
            .snapshot()
            .await
            .unwrap()
            .record_outbox
            .is_empty()
    );

    records.available.store(true, Ordering::SeqCst);
    let recovered = runtime.drive_once().await;
    assert!(recovered.records_published > 0);
    assert!(
        runtime
            .authority()
            .snapshot()
            .await
            .unwrap()
            .record_outbox
            .is_empty()
    );
    assert!(!records.seen.lock().await.is_empty());
    assert!(
        records
            .seen
            .lock()
            .await
            .values()
            .all(|record| !record.attributes.contains_key("blackopsd_build_id"))
    );
}

#[tokio::test]
async fn ambiguous_record_delivery_replays_identical_content_across_build_change() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap().join("blackops");
    let fleet = Arc::new(FakeFleet::default());
    let records = Arc::new(FakeRecords::ambiguous_once());
    let first = runtime_with_build(&root, fleet.clone(), records.clone(), "build-a").await;
    spawn(&first, "spawn-call").await;
    let ambiguous = first.drive_once().await;
    assert!(!ambiguous.errors.is_empty());
    assert!(
        !first
            .authority()
            .snapshot()
            .await
            .unwrap()
            .record_outbox
            .is_empty()
    );
    first.authority().shutdown().await;
    drop(first);

    let restarted = runtime_with_build(&root, fleet, records.clone(), "build-b").await;
    let recovered = restarted.drive_once().await;
    assert!(recovered.records_published > 0);
    assert!(
        restarted
            .authority()
            .snapshot()
            .await
            .unwrap()
            .record_outbox
            .is_empty()
    );
    assert!(
        records
            .seen
            .lock()
            .await
            .values()
            .all(|record| !record.attributes.contains_key("blackopsd_build_id"))
    );
}

#[tokio::test]
async fn service_auth_protects_every_non_health_route() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap().join("blackops");
    let runtime = runtime(
        &root,
        Arc::new(FakeFleet::default()),
        Arc::new(FakeRecords::available()),
    )
    .await;
    let app = router(runtime, service_token());

    for path in ["/healthz", "/readyz"] {
        let response = app
            .clone()
            .oneshot(
                axum::http::Request::builder()
                    .uri(path)
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), axum::http::StatusCode::OK);
    }
    for authorization in [None, Some("Bearer wrong")] {
        let mut request = axum::http::Request::builder().uri("/v1/status");
        if let Some(authorization) = authorization {
            request = request.header("authorization", authorization);
        }
        let response = app
            .clone()
            .oneshot(request.body(axum::body::Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), axum::http::StatusCode::UNAUTHORIZED);
    }
    let response = app
        .oneshot(
            axum::http::Request::builder()
                .uri("/v1/status")
                .header("authorization", format!("Bearer {SERVICE_TOKEN_SECRET}"))
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), axum::http::StatusCode::OK);
}

#[tokio::test]
async fn internal_capability_uses_exact_blackops_agent_contract() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap().join("blackops");
    let fleet = Arc::new(FakeFleet::default());
    let records = Arc::new(FakeRecords::available());
    let runtime = runtime(&root, fleet.clone(), records).await;
    let payload = RoutedCapabilityRequest {
        worker_id: "worker-root".into(),
        session_id: "session-root".into(),
        authorization: authorization("blackops.agent", "spawn", []),
        request: CapabilityRequest {
            call_id: "call-spawn".into(),
            invocation_id: Some("provider-call-spawn".into()),
            capability: "blackops.agent".into(),
            operation: "spawn".into(),
            bounded_payload: serde_json::to_value(AgentSpawnRequest {
                task_name: "implementer".into(),
                message: "implement the next slice".into(),
                fork_turns: AgentForkTurns::Recent(3),
            })
            .unwrap(),
            deadline_unix_ms: None,
        },
    };
    let response = router(runtime.clone(), service_token())
        .oneshot(
            axum::http::Request::builder()
                .method("POST")
                .uri("/internal/capability")
                .header("content-type", "application/json")
                .header("authorization", format!("Bearer {SERVICE_TOKEN_SECRET}"))
                .body(axum::body::Body::from(
                    serde_json::to_vec(&payload).unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), axum::http::StatusCode::OK);
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let response: CapabilityResponse = serde_json::from_slice(&bytes).unwrap();
    assert!(!response.is_error);
    let identity: bro_capabilities::AgentIdentity =
        serde_json::from_value(response.result_or_error).unwrap();
    assert!(identity.canonical_path.starts_with("/sessions/"));
    assert!(identity.canonical_path.ends_with("/implementer"));
    let snapshot = runtime.authority().snapshot().await.unwrap();
    let request = snapshot
        .operations
        .values()
        .find_map(|operation| operation.execution_request.as_ref())
        .unwrap();
    assert_eq!(
        request.tool_policy.allowed_remote_operations,
        BTreeMap::from([("blackops.agent".into(), vec!["spawn".into()])])
    );
    assert!(request.tool_policy.allowed_atom_refs.is_empty());
}

#[tokio::test]
async fn internal_capability_fences_session_root_attempt_generations() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap().join("blackops");
    let runtime = runtime(
        &root,
        Arc::new(FakeFleet::default()),
        Arc::new(FakeRecords::available()),
    )
    .await;
    let app = router(runtime.clone(), service_token());
    let payload =
        |worker_id: &str, task_id: &str, attempt_id: &str, generation: u64, call_id: &str| {
            let mut authorization = authorization("blackops.agent", "list", []);
            authorization.worker_id = WorkerId::new(worker_id);
            authorization.task_id = TaskId::new(task_id);
            authorization.attempt_id = AttemptId::new(attempt_id);
            authorization.session_attempt_generation = generation;
            RoutedCapabilityRequest {
                worker_id: worker_id.into(),
                session_id: "session-root".into(),
                authorization,
                request: CapabilityRequest {
                    call_id: call_id.into(),
                    invocation_id: Some(format!("provider-{call_id}")),
                    capability: "blackops.agent".into(),
                    operation: "list".into(),
                    bounded_payload: Value::Null,
                    deadline_unix_ms: None,
                },
            }
        };

    let first = internal_capability_request(
        &app,
        payload("worker-1", "task-1", "attempt-1", 1, "bind-first"),
    )
    .await;
    assert!(!first.is_error);
    let retry = internal_capability_request(
        &app,
        payload("worker-1", "task-1", "attempt-1", 1, "bind-retry"),
    )
    .await;
    assert!(!retry.is_error);
    let same_generation_drift = internal_capability_request(
        &app,
        payload("worker-2", "task-2", "attempt-2", 1, "bind-drift"),
    )
    .await;
    assert!(same_generation_drift.is_error);
    let advanced = internal_capability_request(
        &app,
        payload("worker-2", "task-2", "attempt-2", 2, "bind-advance"),
    )
    .await;
    assert!(!advanced.is_error);
    let stale = internal_capability_request(
        &app,
        payload("worker-1", "task-1", "attempt-1", 1, "bind-stale"),
    )
    .await;
    assert!(stale.is_error);

    let snapshot = runtime.authority().snapshot().await.unwrap();
    let root = snapshot
        .agents
        .values()
        .find(|agent| agent.current_session_id == Some(SessionId::new("session-root")))
        .unwrap();
    assert_eq!(root.current_worker_id.as_deref(), Some("worker-2"));
    assert_eq!(root.current_task_id.as_ref(), Some(&TaskId::new("task-2")));
    assert_eq!(
        root.current_attempt_id.as_ref(),
        Some(&AttemptId::new("attempt-2"))
    );
    assert_eq!(root.current_session_attempt_generation, Some(2));
}

#[tokio::test]
async fn internal_capability_rechecks_exact_operation_and_atom_ref() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap().join("blackops");
    let fleet = Arc::new(FakeFleet::default());
    let records = Arc::new(FakeRecords::available());
    let runtime = runtime(&root, fleet.clone(), records).await;
    let app = router(runtime, service_token());
    let payloads = [
        RoutedCapabilityRequest {
            worker_id: "worker-root".into(),
            session_id: "session-root".into(),
            authorization: authorization("blackops.agent", "status", []),
            request: CapabilityRequest {
                call_id: "call-wrong-operation".into(),
                invocation_id: Some("provider-call-wrong-operation".into()),
                capability: "blackops.agent".into(),
                operation: "spawn".into(),
                bounded_payload: serde_json::to_value(AgentSpawnRequest {
                    task_name: "forbidden".into(),
                    message: "must not dispatch".into(),
                    fork_turns: AgentForkTurns::None,
                })
                .unwrap(),
                deadline_unix_ms: None,
            },
        },
        RoutedCapabilityRequest {
            worker_id: "worker-root".into(),
            session_id: "session-root".into(),
            authorization: authorization("atom", "invoke_atom", [AtomRef::new("atom:allowed@v1")]),
            request: CapabilityRequest {
                call_id: "call-wrong-atom".into(),
                invocation_id: Some("provider-call-wrong-atom".into()),
                capability: "atom".into(),
                operation: "invoke_atom".into(),
                bounded_payload: serde_json::to_value(AtomInvocation {
                    atom: AtomRef::new("atom:other@v1"),
                    input_json: Value::Null,
                })
                .unwrap(),
                deadline_unix_ms: None,
            },
        },
    ];

    for payload in payloads {
        let response = app
            .clone()
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri("/internal/capability")
                    .header("content-type", "application/json")
                    .header("authorization", format!("Bearer {SERVICE_TOKEN_SECRET}"))
                    .body(axum::body::Body::from(
                        serde_json::to_vec(&payload).unwrap(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), axum::http::StatusCode::OK);
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let response: CapabilityResponse = serde_json::from_slice(&bytes).unwrap();
        assert!(response.is_error);
        let error: CapabilityError = serde_json::from_value(response.result_or_error).unwrap();
        assert_eq!(error.code, CapabilityErrorCode::Unauthorized);
    }
    assert_eq!(fleet.request_calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn provider_invocation_identity_deduplicates_drop_after_commit_replay() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap().join("blackops");
    let fleet = Arc::new(FakeFleet::default());
    let records = Arc::new(FakeRecords::available());
    let runtime = runtime(&root, fleet, records).await;
    let app = router(runtime.clone(), service_token());

    let stable_invocation_id = "provider-tool-call-spawn-42";
    let mut replayed_identity = None;
    for (attempt, rpc_call_id) in ["rpc-before-response-drop", "rpc-after-reconnect"]
        .into_iter()
        .enumerate()
    {
        let payload = RoutedCapabilityRequest {
            worker_id: "worker-root".into(),
            session_id: "session-root".into(),
            authorization: authorization("blackops.agent", "spawn", []),
            request: CapabilityRequest {
                call_id: rpc_call_id.into(),
                invocation_id: Some(stable_invocation_id.into()),
                capability: "blackops.agent".into(),
                operation: "spawn".into(),
                bounded_payload: serde_json::to_value(AgentSpawnRequest {
                    task_name: "durable_child".into(),
                    message: "perform one durable unit of work".into(),
                    fork_turns: AgentForkTurns::None,
                })
                .unwrap(),
                deadline_unix_ms: None,
            },
        };
        let response = app
            .clone()
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri("/internal/capability")
                    .header("content-type", "application/json")
                    .header("authorization", format!("Bearer {SERVICE_TOKEN_SECRET}"))
                    .body(axum::body::Body::from(
                        serde_json::to_vec(&payload).unwrap(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), axum::http::StatusCode::OK);
        if attempt == 0 {
            // Model the worker losing its connection after blackopsd commits
            // the effect but before the response body is observed.
            drop(response);
            continue;
        }
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let response: CapabilityResponse = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(response.call_id, rpc_call_id);
        assert!(!response.is_error);
        let observed: bro_capabilities::AgentIdentity =
            serde_json::from_value(response.result_or_error).unwrap();
        replayed_identity = Some(observed);
    }

    let snapshot = runtime.authority().snapshot().await.unwrap();
    assert_eq!(
        replayed_identity.unwrap().agent_id,
        snapshot
            .agents
            .values()
            .find(|agent| agent.role != "session-root")
            .unwrap()
            .agent_id
    );
    assert_eq!(
        snapshot
            .agents
            .values()
            .filter(|agent| agent.role != "session-root")
            .count(),
        1
    );
    assert_eq!(snapshot.operations.len(), 1);
    assert_eq!(snapshot.fleet_outbox.len(), 1);
    let operation = snapshot.operations.values().next().unwrap();
    assert!(operation.idempotency_key.contains(stable_invocation_id));
}

#[tokio::test]
async fn mutating_internal_capabilities_reject_missing_or_blank_invocation_identity() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap().join("blackops");
    let fleet = Arc::new(FakeFleet::default());
    let records = Arc::new(FakeRecords::available());
    let runtime = runtime(&root, fleet.clone(), records).await;
    let app = router(runtime, service_token());

    for (index, invocation_id) in [None, Some("   ".to_string())].into_iter().enumerate() {
        let payload = RoutedCapabilityRequest {
            worker_id: "worker-root".into(),
            session_id: "session-root".into(),
            authorization: authorization("blackops.agent", "spawn", []),
            request: CapabilityRequest {
                call_id: format!("call-missing-invocation-{index}"),
                invocation_id,
                capability: "blackops.agent".into(),
                operation: "spawn".into(),
                bounded_payload: serde_json::to_value(AgentSpawnRequest {
                    task_name: format!("must-not-spawn-{index}"),
                    message: "must not dispatch".into(),
                    fork_turns: AgentForkTurns::None,
                })
                .unwrap(),
                deadline_unix_ms: None,
            },
        };
        let response = app
            .clone()
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri("/internal/capability")
                    .header("content-type", "application/json")
                    .header("authorization", format!("Bearer {SERVICE_TOKEN_SECRET}"))
                    .body(axum::body::Body::from(
                        serde_json::to_vec(&payload).unwrap(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let response: CapabilityResponse = serde_json::from_slice(&bytes).unwrap();
        let error: CapabilityError = serde_json::from_value(response.result_or_error).unwrap();
        assert_eq!(error.code, CapabilityErrorCode::InvalidRequest);
    }
    assert_eq!(fleet.request_calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn internal_capability_uses_exact_atom_contract_and_deduplicates_retries() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap().join("blackops");
    let fleet = Arc::new(FakeFleet::completing());
    let records = Arc::new(FakeRecords::available());
    let runtime = runtime(&root, fleet, records).await;
    runtime
        .authority()
        .call(|authority| {
            authority.install_definition(DefinitionInstallRequest {
                kind: DefinitionKind::Atom,
                name: "reviewer".into(),
                version: "v1".into(),
                input_contract: json!({"type": "object"}),
                body: json!({"prompt": "Review the supplied change"}),
                activate: true,
                created_at_unix_ms: 1,
            })
        })
        .await
        .unwrap();
    let mut payload = RoutedCapabilityRequest {
        worker_id: "worker-root".into(),
        session_id: "session-root".into(),
        authorization: authorization("atom", "invoke_atom", [AtomRef::new("atom:reviewer@v1")]),
        request: CapabilityRequest {
            call_id: "call-atom".into(),
            invocation_id: Some("provider-call-atom".into()),
            capability: "atom".into(),
            operation: "invoke_atom".into(),
            bounded_payload: serde_json::to_value(AtomInvocation {
                atom: AtomRef::new("atom:reviewer@v1"),
                input_json: json!({"change": 7}),
            })
            .unwrap(),
            deadline_unix_ms: None,
        },
    };
    let app = router(runtime.clone(), service_token());
    let mut invocation_id = None;
    for rpc_call_id in ["call-atom-before-drop", "call-atom-after-retry"] {
        payload.request.call_id = rpc_call_id.into();
        let response = app
            .clone()
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri("/internal/capability")
                    .header("content-type", "application/json")
                    .header("authorization", format!("Bearer {SERVICE_TOKEN_SECRET}"))
                    .body(axum::body::Body::from(
                        serde_json::to_vec(&payload).unwrap(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), axum::http::StatusCode::OK);
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let response: CapabilityResponse = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(response.call_id, rpc_call_id);
        assert!(!response.is_error);
        let output: AtomOutput = serde_json::from_value(response.result_or_error).unwrap();
        assert_eq!(output.output_json, json!({"result": "reviewed"}));
        let snapshot = runtime.authority().snapshot().await.unwrap();
        let observed = serde_json::to_value(snapshot.invocations.keys().next().unwrap()).unwrap();
        if let Some(prior) = &invocation_id {
            assert_eq!(prior, &observed);
        }
        invocation_id = Some(observed);
    }
    let snapshot = runtime.authority().snapshot().await.unwrap();
    assert_eq!(snapshot.invocations.len(), 1);
    assert_eq!(snapshot.operations.len(), 1);
    assert!(snapshot.fleet_outbox.is_empty());
    assert_eq!(
        snapshot.operations.values().next().unwrap().status,
        blackops_core::OperationStatus::Completed
    );
}

#[tokio::test]
async fn shipped_catalog_runs_deterministic_and_workflow_atoms_without_fleet_dispatch() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap();
    let state = root.join("blackops");
    let catalog = root.join("artifacts");
    let fleet = Arc::new(FakeFleet::default());
    let records = Arc::new(FakeRecords::available());
    let runtime = runtime(&state, fleet.clone(), records).await;
    let report = import_catalog(&runtime.authority(), &catalog)
        .await
        .unwrap();
    assert!(report.shipped_atoms > 100);
    assert_eq!(report.installed_atoms, 0);

    let echo = runtime.session_atoms(
        "worker-root",
        SessionId::new("session-root"),
        "provider-echo-call",
    );
    let output = echo
        .invoke_atom(AtomInvocation {
            atom: AtomRef::new("atom:echo@v1"),
            input_json: json!({"message": "hello"}),
        })
        .await
        .unwrap();
    assert_eq!(output.output_json, json!({"echo": {"message": "hello"}}));

    let workflow = runtime.session_atoms(
        "worker-root",
        SessionId::new("session-root"),
        "provider-workflow-call",
    );
    let output = workflow
        .invoke_atom(AtomInvocation {
            atom: AtomRef::new("atom:echo-review-workflow@v1"),
            input_json: json!({"message": "review me"}),
        })
        .await
        .unwrap();
    assert_eq!(
        output.output_json,
        json!({"echo": {"message": "review me"}})
    );
    assert_eq!(fleet.request_calls.load(Ordering::SeqCst), 0);
    let snapshot = runtime.authority().snapshot().await.unwrap();
    assert!(snapshot.definitions.len() > 100);
    assert_eq!(snapshot.operations.len(), 0);
    assert_eq!(snapshot.invocations.len(), 3);
    assert!(
        snapshot
            .invocations
            .values()
            .all(|invocation| invocation.output.is_some())
    );
}

#[tokio::test]
async fn shipped_consultant_atom_keeps_one_durable_logical_agent_across_turns() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap();
    let fleet = Arc::new(FakeFleet::completing());
    let records = Arc::new(FakeRecords::available());
    let runtime = runtime(&root.join("blackops"), fleet.clone(), records).await;
    import_catalog(&runtime.authority(), &root.join("artifacts"))
        .await
        .unwrap();

    let first = runtime.session_atoms(
        "worker-root",
        SessionId::new("session-root"),
        "provider-consultant-open",
    );
    let opened = first
        .invoke_atom(AtomInvocation {
            atom: AtomRef::new("atom:badgey-consult@v1"),
            input_json: json!({"brief": "review this boundary"}),
        })
        .await
        .unwrap()
        .output_json;
    let consultant_id = opened["consultant_id"].as_str().unwrap().to_owned();
    assert_eq!(opened["status"], "completed");

    let second = runtime.session_atoms(
        "worker-root",
        SessionId::new("session-root"),
        "provider-consultant-followup",
    );
    let resumed = second
        .invoke_atom(AtomInvocation {
            atom: AtomRef::new("atom:badgey-consult@v1"),
            input_json: json!({
                "consultant_id": consultant_id,
                "prompt": "now check restart behavior"
            }),
        })
        .await
        .unwrap()
        .output_json;
    assert_eq!(resumed["consultant_id"], opened["consultant_id"]);
    assert_eq!(resumed["status"], "completed");
    assert_eq!(fleet.request_calls.load(Ordering::SeqCst), 2);
    let snapshot = runtime.authority().snapshot().await.unwrap();
    assert_eq!(
        snapshot
            .agents
            .values()
            .filter(|agent| agent.role != "session-root")
            .count(),
        1
    );
    assert_eq!(snapshot.operations.len(), 2);
    assert_eq!(snapshot.invocations.len(), 2);
}

#[tokio::test]
async fn mcp_streamable_http_initialize_list_notification_and_call_conform() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap().join("blackops");
    let fleet = Arc::new(FakeFleet::default());
    let records = Arc::new(FakeRecords::available());
    let runtime = runtime(&root, fleet, records).await;
    let app = router(runtime, service_token());

    let response = mcp_request(
        &app,
        json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": "2025-06-18",
                "capabilities": {},
                "clientInfo": {"name": "test", "version": "1"}
            }
        }),
    )
    .await;
    assert_eq!(response.status(), axum::http::StatusCode::OK);
    assert_eq!(response.headers()["content-type"], "application/json");
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let initialized: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(initialized["result"]["serverInfo"]["name"], "blackopsd");
    assert_eq!(
        initialized["result"]["serverInfo"]["buildId"],
        "blackopsd-test-build"
    );

    let response = mcp_request(
        &app,
        json!({
            "jsonrpc": "2.0",
            "method": "notifications/initialized"
        }),
    )
    .await;
    assert_eq!(response.status(), axum::http::StatusCode::ACCEPTED);
    assert!(
        axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap()
            .is_empty()
    );

    let response = mcp_request(
        &app,
        json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list", "params": {}}),
    )
    .await;
    assert_eq!(response.status(), axum::http::StatusCode::OK);
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let listed: Value = serde_json::from_slice(&bytes).unwrap();
    let names: Vec<_> = listed["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|tool| tool["name"].as_str())
        .collect();
    assert!(names.contains(&"blackops_atom_invoke"));
    assert!(names.contains(&"blackops_wait_create"));
    assert!(names.contains(&"blackops_approval_request"));
    assert!(names.contains(&"blackops_whiteboard_put"));
    assert!(names.contains(&"blackops_workflow_status"));
    assert!(names.contains(&"blackops_integration_resolve"));

    let response = mcp_request(
        &app,
        json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "tools/call",
            "params": {
                "name": "blackops_whiteboard_create",
                "arguments": {
                    "whiteboard_id": "campaign",
                    "name": "Campaign",
                    "created_at_unix_ms": 1
                }
            }
        }),
    )
    .await;
    assert_eq!(response.status(), axum::http::StatusCode::OK);
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let called: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(called["result"]["isError"], false);
    assert!(
        called["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("campaign")
    );
}

#[tokio::test]
async fn wait_mailbox_cursor_is_caller_local_across_sibling_sequence_streams() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap().join("blackops");
    let fleet = Arc::new(FakeFleet::default());
    let records = Arc::new(FakeRecords::available());
    let runtime = runtime(&root, fleet, records).await;

    let first = runtime
        .session_agents(
            "worker-root",
            SessionId::new("session-root"),
            "spawn-first-sibling",
        )
        .spawn(AgentSpawnRequest {
            task_name: "first_sibling".into(),
            message: "first".into(),
            fork_turns: AgentForkTurns::None,
        })
        .await
        .unwrap();
    let second = runtime
        .session_agents(
            "worker-root",
            SessionId::new("session-root"),
            "spawn-second-sibling",
        )
        .spawn(AgentSpawnRequest {
            task_name: "second_sibling".into(),
            message: "second".into(),
            fork_turns: AgentForkTurns::None,
        })
        .await
        .unwrap();
    for (call_id, target) in [
        ("send-first-sibling", first.canonical_path.clone()),
        ("send-second-sibling", second.canonical_path.clone()),
    ] {
        runtime
            .session_agents("worker-root", SessionId::new("session-root"), call_id)
            .send_message(AgentMessageRequest {
                target: AgentTarget {
                    canonical_path: target,
                },
                message: "sibling context".into(),
            })
            .await
            .unwrap();
    }

    let wake = runtime
        .session_agents(
            "worker-root",
            SessionId::new("session-root"),
            "wait-siblings",
        )
        .wait(AgentWaitRequest {
            timeout_ms: Some(1),
            path_prefix: None,
            after_mailbox_sequence: Some(0),
        })
        .await
        .unwrap();
    assert_eq!(wake, AgentWake::Timeout);

    let snapshot = runtime.authority().snapshot().await.unwrap();
    for identity in [&first, &second] {
        let mailbox = &snapshot.mailboxes[&identity.agent_id];
        assert_eq!(mailbox.next_sequence, 2);
        assert!(
            !mailbox.cursors.contains_key("session:session-root"),
            "waiting from the caller must not consume a sibling mailbox"
        );
    }
}

#[tokio::test]
async fn session_agent_capability_cannot_escape_its_bound_tree() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap().join("blackops");
    let fleet = Arc::new(FakeFleet::default());
    let records = Arc::new(FakeRecords::available());
    let runtime = runtime(&root, fleet, records).await;

    let first = runtime.session_agents(
        "worker-first",
        SessionId::new("session-first"),
        "first-spawn",
    );
    let first_identity = first
        .spawn(AgentSpawnRequest {
            task_name: "first_child".into(),
            message: "first".into(),
            fork_turns: AgentForkTurns::None,
        })
        .await
        .unwrap();
    let snapshot = runtime.authority().snapshot().await.unwrap();
    let first_agent = snapshot.agents.get(&first_identity.agent_id).unwrap();
    let first_operation = snapshot
        .operations
        .get(first_agent.current_operation_id.as_ref().unwrap())
        .unwrap();
    assert_eq!(
        first_operation.execution_request.as_ref().unwrap().labels["prompt_cache_root"],
        "session-first"
    );
    let second = runtime.session_agents(
        "worker-second",
        SessionId::new("session-second"),
        "second-spawn",
    );
    let second_identity = second
        .spawn(AgentSpawnRequest {
            task_name: "second_child".into(),
            message: "second".into(),
            fork_turns: AgentForkTurns::None,
        })
        .await
        .unwrap();
    assert_ne!(
        first_identity.canonical_path.split('/').nth(2).unwrap(),
        second_identity.canonical_path.split('/').nth(2).unwrap()
    );

    let first = runtime.session_agents(
        "worker-first",
        SessionId::new("session-first"),
        "escape-attempt",
    );
    let target = AgentTarget {
        canonical_path: second_identity.canonical_path.clone(),
    };
    let status_error = first.status(target.clone()).await.unwrap_err();
    assert_eq!(status_error.code, "agent.unauthorized_target");
    let send_error = first
        .send_message(AgentMessageRequest {
            target,
            message: "cross-tree".into(),
        })
        .await
        .unwrap_err();
    assert_eq!(send_error.code, "agent.unauthorized_target");
    let list_error = first
        .list(Some(second_identity.canonical_path.clone()))
        .await
        .unwrap_err();
    assert_eq!(list_error.code, "agent.unauthorized_target");
    let wait_error = first
        .wait(AgentWaitRequest {
            timeout_ms: Some(1),
            path_prefix: Some(second_identity.canonical_path),
            after_mailbox_sequence: None,
        })
        .await
        .unwrap_err();
    assert_eq!(wait_error.code, "agent.unauthorized_target");
}
