use std::collections::{BTreeMap, BTreeSet};
use std::convert::Infallible;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::RwLock;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use axum::Router;
use axum::body::{Body, Bytes};
use axum::http::HeaderMap;
use axum::response::Response;
use axum::routing::post;
use bro_capabilities::{
    ExecutionKind, ExecutionRequest, ExecutionServiceTier, ExecutionToolPolicy, WorkingSetIntent,
};
use bro_core::{OperationId, Provider, SessionId, TaskId, WorkerId};
use bro_protocol::{
    AuthenticationProof, BuildIdentity, CapabilityAuthorization, CapabilityErrorCode,
    CapabilityRequest, CapabilityResponse, CloseoutOutcome, CloseoutPhase, CommandOutcome,
    DownstreamServiceAvailability, Heartbeat, RosterSnapshotV1, ServiceAvailability, TaskStatus,
    WORKER_PROTOCOL_V1, WorkerFeature, WorkerHello, WorkerLifecycleState, WorkerMessage,
    WorkerStatus,
};
use bro_rpc::{MessagePriority, PeerConfig, RpcPeer, ServiceToken, connect_worker_with_options};
use fleet_core::CapabilityRoute;
use fleetd::{
    CapabilityBroker, FleetMode, Fleetd, FleetdConfig, FleetdError, FleetdResult,
    HttpCapabilityBroker, LaunchSpec, LaunchedWorker, UnavailableCapabilityBroker, WorkerLauncher,
};
use futures::{StreamExt as _, stream};
use serde_json::json;
use tokio::net::UnixStream;
use tokio::sync::{Semaphore, mpsc};

struct CapturingLauncher {
    sender: mpsc::Sender<LaunchSpec>,
    event_root: PathBuf,
}

struct BlockingCapabilityBroker {
    entered: AtomicUsize,
    release: Semaphore,
}

impl BlockingCapabilityBroker {
    fn new() -> Self {
        Self {
            entered: AtomicUsize::new(0),
            release: Semaphore::new(0),
        }
    }
}

#[async_trait]
impl CapabilityBroker for BlockingCapabilityBroker {
    async fn call(
        &self,
        _route: CapabilityRoute,
        _worker_id: &WorkerId,
        _session_id: &SessionId,
        _authorization: CapabilityAuthorization,
        request: CapabilityRequest,
    ) -> FleetdResult<CapabilityResponse> {
        self.entered.fetch_add(1, Ordering::SeqCst);
        let _release = self
            .release
            .acquire()
            .await
            .map_err(|error| FleetdError::CapabilityUnavailable(error.to_string()))?;
        Ok(CapabilityResponse::success(
            request.call_id,
            json!({"ok": true}),
        ))
    }
}

struct ControllableAvailabilityBroker {
    availability: RwLock<DownstreamServiceAvailability>,
}

impl ControllableAvailabilityBroker {
    fn new(availability: DownstreamServiceAvailability) -> Self {
        Self {
            availability: RwLock::new(availability),
        }
    }

    fn set(&self, availability: DownstreamServiceAvailability) {
        *self
            .availability
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = availability;
    }
}

#[async_trait]
impl CapabilityBroker for ControllableAvailabilityBroker {
    async fn call(
        &self,
        _route: CapabilityRoute,
        _worker_id: &WorkerId,
        _session_id: &SessionId,
        _authorization: CapabilityAuthorization,
        request: CapabilityRequest,
    ) -> FleetdResult<CapabilityResponse> {
        Ok(CapabilityResponse::success(
            request.call_id,
            json!({"ok": true}),
        ))
    }

    async fn downstream_availability(&self) -> DownstreamServiceAvailability {
        *self
            .availability
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn availability_poll_interval(&self) -> Duration {
        Duration::from_millis(20)
    }
}

#[async_trait]
impl WorkerLauncher for CapturingLauncher {
    async fn launch(&self, spec: LaunchSpec) -> FleetdResult<LaunchedWorker> {
        let event_log_path = self
            .event_root
            .join(spec.provisioned.worker_id.as_str())
            .join("events.jsonl");
        self.sender
            .send(spec)
            .await
            .map_err(|_| FleetdError::WorkerLaunch("test launcher receiver closed".into()))?;
        Ok(LaunchedWorker {
            process_id: Some(42),
            event_log_path,
        })
    }
}

fn config(root: &std::path::Path, mode: FleetMode) -> FleetdConfig {
    let mut config = FleetdConfig {
        mode,
        bind: "127.0.0.1:0".parse().unwrap(),
        state_dir: root.join("state"),
        service_token_file: Some(root.join("service.token")),
        worker_socket: Some(root.join("run/worker.sock")),
        managed_worktree_root: Some(root.join("worktrees")),
        lease_duration_ms: 2_000,
        heartbeat_interval_ms: 500,
        reattach_grace_ms: 2_000,
        allowed_capabilities: vec![
            "atom".into(),
            "blackops.agent".into(),
            "corpus".into(),
            "corpus.search".into(),
        ],
        ..FleetdConfig::default()
    };
    if mode == FleetMode::Shadow {
        config.shadow_source = Some("http://127.0.0.1:7264".into());
    }
    // Linux authority validation requires an external worker sandbox
    // launcher path (macOS uses the built-in Seatbelt and rejects one).
    // These tests inject a CapturingLauncher and never build the real
    // sandbox, so any root-owned executable satisfies the config invariant
    // without being probed or run.
    #[cfg(target_os = "linux")]
    if mode.is_authority() {
        config.worker_sandbox_launcher = Some("/bin/true".into());
    }
    config
}

fn authenticated_client(root: &std::path::Path) -> reqwest::Client {
    let token = ServiceToken::load_or_create(&root.join("service.token")).unwrap();
    let mut headers = HeaderMap::new();
    headers.insert(reqwest::header::AUTHORIZATION, token.authorization_header());
    reqwest::Client::builder()
        .default_headers(headers)
        .build()
        .unwrap()
}

fn request(key: &str) -> ExecutionRequest {
    let allowed_remote_operations = BTreeMap::from([
        ("corpus.search".into(), vec!["search".into()]),
        ("corpus".into(), vec!["search_corpus".into()]),
        (
            "blackops.agent".into(),
            vec![
                "spawn".into(),
                "send_message".into(),
                "followup".into(),
                "interrupt".into(),
                "status".into(),
                "list".into(),
                "wait".into(),
            ],
        ),
        ("atom".into(), vec!["invoke_atom".into()]),
    ]);
    ExecutionRequest {
        operation_id: OperationId::new(format!("operation-{key}")),
        idempotency_key: key.to_string(),
        kind: ExecutionKind::Fresh {
            prompt: "perform the first turn".into(),
        },
        provider: Provider::Glm,
        model: "glm-test".into(),
        effort: None,
        service_tier: ExecutionServiceTier::Default,
        code_mode: None,
        dispatch_context: None,
        working_set: WorkingSetIntent::Scratch,
        shell_env: BTreeMap::new(),
        tool_policy: ExecutionToolPolicy {
            allowed_remote_operations,
            allowed_atom_refs: vec![bro_core::AtomRef::new("atom:allowed@v1")],
            ..ExecutionToolPolicy::default()
        },
        system_prompt: None,
        output_schema: None,
        labels: BTreeMap::new(),
    }
}

fn worker_hello(spec: &LaunchSpec, proof: AuthenticationProof) -> WorkerHello {
    let mut hello = WorkerHello {
        protocol_versions: vec![WORKER_PROTOCOL_V1],
        worker_build: BuildIdentity {
            version: env!("CARGO_PKG_VERSION").into(),
            build_id: "integration-worker".into(),
        },
        worker_id: spec.provisioned.worker_id.clone(),
        task_id: spec.accepted.task_id.clone(),
        session_id: spec.accepted.session_id.clone(),
        bootstrap_or_resume_proof: proof,
        last_local_event_seq: 0,
        last_fleet_command_seq: 0,
        worker_capabilities: Vec::new(),
    };
    hello.set_offered_protocol_features(required_features());
    hello
}

fn required_features() -> BTreeSet<WorkerFeature> {
    [
        WorkerFeature::ORDERED_REPLAY,
        WorkerFeature::COMMAND_IDEMPOTENCY,
        WorkerFeature::GENERATION_FENCING,
        WorkerFeature::CAPABILITY_RPC,
        WorkerFeature::LEASE_RENEWAL,
        WorkerFeature::DRAIN_SHUTDOWN,
    ]
    .into_iter()
    .map(WorkerFeature::new)
    .collect()
}

async fn connect_worker(
    socket: &std::path::Path,
    hello: WorkerHello,
) -> (RpcPeer, bro_protocol::FleetWelcome) {
    let mut last_error = None;
    for _ in 0..100 {
        match UnixStream::connect(socket).await {
            Ok(stream) => {
                let (negotiated, welcome) =
                    connect_worker_with_options(stream, hello.clone(), Default::default())
                        .await
                        .unwrap();
                return (
                    RpcPeer::spawn(negotiated, PeerConfig::default()).unwrap(),
                    welcome,
                );
            }
            Err(error) => last_error = Some(error),
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("worker socket did not become ready: {last_error:?}");
}

async fn recv_matching(
    peer: &mut RpcPeer,
    label: &str,
    predicate: impl Fn(&WorkerMessage) -> bool,
) -> bro_protocol::Envelope {
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            let envelope = peer.recv().await.unwrap();
            if predicate(&envelope.body) {
                return envelope;
            }
        }
    })
    .await
    .unwrap_or_else(|_| panic!("timed out waiting for {label}"))
}

async fn post_mcp(
    client: &reqwest::Client,
    base: &str,
    request: serde_json::Value,
) -> reqwest::Response {
    client
        .post(format!("{base}/mcp"))
        .header(
            reqwest::header::ACCEPT,
            "application/json, text/event-stream",
        )
        .json(&request)
        .send()
        .await
        .unwrap()
}

async fn run_git(repo: &std::path::Path, args: &[&str]) {
    let output = tokio::process::Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .output()
        .await
        .unwrap();
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shadow_mode_replicates_views_and_rejects_mutation() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap();
    let service = Fleetd::open_with(
        config(&root, FleetMode::Shadow),
        None,
        Arc::new(UnavailableCapabilityBroker),
    )
    .await
    .unwrap();
    let handle = service.start().await.unwrap();
    let client = authenticated_client(&root);
    let base = format!("http://{}", handle.local_addr());
    let snapshot = RosterSnapshotV1 {
        version: 0,
        tasks: Vec::new(),
        daemon_version: Some("source".into()),
        daemon_build_id: Some("source-build".into()),
    };
    client
        .post(format!("{base}/internal/shadow/roster"))
        .json(&snapshot)
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    let observed: RosterSnapshotV1 = client
        .get(format!("{base}/control/roster"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(observed, snapshot);

    let mutation = client
        .post(format!("{base}/v1/executions"))
        .json(&request("shadow"))
        .send()
        .await
        .unwrap();
    assert_eq!(mutation.status(), reqwest::StatusCode::CONFLICT);

    let roster: serde_json::Value = post_mcp(
        &client,
        &base,
        json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/call",
            "params": {"name": "bro_roster", "arguments": {}}
        }),
    )
    .await
    .json()
    .await
    .unwrap();
    assert_eq!(roster["result"]["isError"], false);
    let rejected: serde_json::Value = post_mcp(
        &client,
        &base,
        json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "tools/call",
            "params": {
                "name": "bro_cancel",
                "arguments": {"task_id": "task-shadow"}
            }
        }),
    )
    .await
    .json()
    .await
    .unwrap();
    assert_eq!(rejected["result"]["isError"], true);
    assert!(!root.join("run/worker.sock").exists());
    assert!(!root.join("state/fleet-state.json").exists());
    handle.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shadow_replication_routes_require_explicit_loopback_source() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap();
    let mut isolated = config(&root, FleetMode::Shadow);
    isolated.shadow_source = None;
    let service = Fleetd::open_with(isolated, None, Arc::new(UnavailableCapabilityBroker))
        .await
        .unwrap();
    let handle = service.start().await.unwrap();
    let response = authenticated_client(&root)
        .post(format!(
            "http://{}/internal/shadow/roster",
            handle.local_addr()
        ))
        .json(&RosterSnapshotV1 {
            version: 0,
            tasks: Vec::new(),
            daemon_version: None,
            daemon_build_id: None,
        })
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), reqwest::StatusCode::NOT_FOUND);
    handle.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn readyz_reports_bound_worker_socket() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap();
    let (launch_tx, _launch_rx) = mpsc::channel(1);
    let service = Fleetd::open_with(
        config(&root, FleetMode::Authority),
        Some(Arc::new(CapturingLauncher {
            sender: launch_tx,
            event_root: root.join("attempts"),
        })),
        Arc::new(UnavailableCapabilityBroker),
    )
    .await
    .unwrap();
    let handle = service.start().await.unwrap();
    let ready: serde_json::Value = reqwest::get(format!("http://{}/readyz", handle.local_addr()))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(ready["ready"], true);
    assert_eq!(ready["worker_socket"], true);
    assert!(
        ready["build_id"]
            .as_str()
            .is_some_and(|value| !value.is_empty())
    );
    let health: serde_json::Value = reqwest::get(format!("http://{}/healthz", handle.local_addr()))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(health["build_id"], ready["build_id"]);
    assert!(root.join("run/worker.sock").exists());
    handle.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn service_routes_require_shared_bearer_while_health_remains_public() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap();
    let (launch_tx, mut launch_rx) = mpsc::channel(1);
    let service = Fleetd::open_with(
        config(&root, FleetMode::Authority),
        Some(Arc::new(CapturingLauncher {
            sender: launch_tx,
            event_root: root.join("attempts"),
        })),
        Arc::new(UnavailableCapabilityBroker),
    )
    .await
    .unwrap();
    let handle = service.start().await.unwrap();
    let base = format!("http://{}", handle.local_addr());
    let anonymous = reqwest::Client::new();
    for path in ["/healthz", "/readyz"] {
        anonymous
            .get(format!("{base}{path}"))
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap();
    }
    let unauthenticated = [
        anonymous
            .get(format!("{base}/control/roster"))
            .send()
            .await
            .unwrap(),
        anonymous
            .post(format!("{base}/v1/executions"))
            .json(&request("unauthenticated"))
            .send()
            .await
            .unwrap(),
        anonymous
            .post(format!("{base}/mcp"))
            .header(
                reqwest::header::ACCEPT,
                "application/json, text/event-stream",
            )
            .json(&json!({"jsonrpc": "2.0", "id": 1, "method": "ping"}))
            .send()
            .await
            .unwrap(),
    ];
    for response in unauthenticated {
        assert_eq!(response.status(), reqwest::StatusCode::UNAUTHORIZED);
    }
    assert!(launch_rx.try_recv().is_err());

    let client = authenticated_client(&root);
    client
        .get(format!("{base}/control/roster"))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    post_mcp(
        &client,
        &base,
        json!({"jsonrpc": "2.0", "id": 2, "method": "ping"}),
    )
    .await
    .error_for_status()
    .unwrap();
    client
        .post(format!("{base}/v1/executions"))
        .json(&request("authenticated"))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    launch_rx.recv().await.unwrap();

    handle.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fleet_mcp_conforms_and_uses_local_authority_without_upstreams() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap();
    let (launch_tx, _launch_rx) = mpsc::channel(1);
    let service = Fleetd::open_with(
        config(&root, FleetMode::Authority),
        Some(Arc::new(CapturingLauncher {
            sender: launch_tx,
            event_root: root.join("attempts"),
        })),
        Arc::new(UnavailableCapabilityBroker),
    )
    .await
    .unwrap();
    let handle = service.start().await.unwrap();
    let base = format!("http://{}", handle.local_addr());
    let client = authenticated_client(&root);

    let initialize: serde_json::Value = post_mcp(
        &client,
        &base,
        json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": "2025-06-18",
                "capabilities": {},
                "clientInfo": {"name": "fleet-test", "version": "1"}
            }
        }),
    )
    .await
    .error_for_status()
    .unwrap()
    .json()
    .await
    .unwrap();
    assert_eq!(initialize["result"]["serverInfo"]["name"], "fleetd");
    assert_eq!(initialize["result"]["protocolVersion"], "2025-06-18");

    let tools: serde_json::Value = post_mcp(
        &client,
        &base,
        json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list", "params": {}}),
    )
    .await
    .error_for_status()
    .unwrap()
    .json()
    .await
    .unwrap();
    let names = tools["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|tool| tool["name"].as_str().unwrap())
        .collect::<BTreeSet<_>>();
    assert_eq!(
        names,
        BTreeSet::from([
            "bro_cancel",
            "bro_closeout",
            "bro_dashboard",
            "bro_exec",
            "bro_interrupt",
            "bro_resume",
            "bro_roster",
            "bro_status",
            "bro_steer",
            "bro_wait",
        ])
    );
    assert!(
        tools["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .all(|tool| tool["inputSchema"]["additionalProperties"] == false)
    );

    let roster: serde_json::Value = post_mcp(
        &client,
        &base,
        json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "tools/call",
            "params": {"name": "bro_roster", "arguments": {}}
        }),
    )
    .await
    .error_for_status()
    .unwrap()
    .json()
    .await
    .unwrap();
    assert_eq!(roster["result"]["isError"], false);

    let missing_accept = client
        .post(format!("{base}/mcp"))
        .json(&json!({"jsonrpc": "2.0", "id": 4, "method": "ping"}))
        .send()
        .await
        .unwrap();
    assert_eq!(missing_accept.status(), reqwest::StatusCode::NOT_ACCEPTABLE);

    let oversized = client
        .post(format!("{base}/mcp"))
        .header(
            reqwest::header::ACCEPT,
            "application/json, text/event-stream",
        )
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .body(" ".repeat(129 * 1024))
        .send()
        .await
        .unwrap();
    assert_eq!(oversized.status(), reqwest::StatusCode::PAYLOAD_TOO_LARGE);

    handle.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn legacy_exec_matches_bro_fleet_client_wire_contract() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap();
    let (launch_tx, mut launch_rx) = mpsc::channel(1);
    let launcher = Arc::new(CapturingLauncher {
        sender: launch_tx,
        event_root: root.join("attempts"),
    });
    let service = Fleetd::open_with(
        config(&root, FleetMode::Authority),
        Some(launcher),
        Arc::new(UnavailableCapabilityBroker),
    )
    .await
    .unwrap();
    let handle = service.start().await.unwrap();
    let base = format!("http://{}", handle.local_addr());
    let cwd = root.to_string_lossy().into_owned();
    let outer: serde_json::Value = authenticated_client(&root)
        .post(format!("{base}/control/exec"))
        .json(&json!({
            "provider": "glm",
            "prompt": "legacy first turn",
            "cwd": cwd,
            "pin_model": "glm-test",
            "pin_effort": "high",
            "code_mode": "optional",
            "service_tier": "default",
            "display_name": "legacy row",
            "allow_recursion": true
        }))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(
        outer.get("isError").and_then(|value| value.as_bool()),
        Some(false)
    );
    let text = outer["content"][0]["text"].as_str().unwrap();
    let response: serde_json::Value = serde_json::from_str(text).unwrap();
    assert!(response["taskId"].as_str().is_some());
    assert!(response["sessionId"].as_str().is_some());
    assert!(response["attemptId"].as_str().is_some());

    let launched = launch_rx.recv().await.unwrap();
    assert_eq!(
        launched.request.labels.get("name").map(String::as_str),
        Some("legacy row")
    );
    assert!(matches!(
        launched.request.kind,
        ExecutionKind::Fresh { ref prompt } if prompt == "legacy first turn"
    ));
    assert!(matches!(
        launched.request.working_set,
        WorkingSetIntent::Existing {
            ref cwd,
            managed_worktree: false
        } if cwd == root.to_string_lossy().as_ref()
    ));
    handle.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn duplicate_admission_launches_exactly_one_worker() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap();
    let (launch_tx, mut launch_rx) = mpsc::channel(2);
    let service = Fleetd::open_with(
        config(&root, FleetMode::Authority),
        Some(Arc::new(CapturingLauncher {
            sender: launch_tx,
            event_root: root.join("attempts"),
        })),
        Arc::new(UnavailableCapabilityBroker),
    )
    .await
    .unwrap();
    let handle = service.start().await.unwrap();
    let client = authenticated_client(&root);
    let endpoint = format!("http://{}/v1/executions", handle.local_addr());
    let first: bro_capabilities::ExecutionAccepted = client
        .post(&endpoint)
        .json(&request("duplicate"))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    let second: bro_capabilities::ExecutionAccepted = client
        .post(&endpoint)
        .json(&request("duplicate"))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(first.task_id, second.task_id);
    assert!(second.deduplicated);
    launch_rx.recv().await.unwrap();
    assert!(launch_rx.try_recv().is_err());
    handle.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn closeout_runs_locally_without_corpus_and_releases_fleet_ownership_once() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap();
    let repo = root.join("repo");
    tokio::fs::create_dir(&repo).await.unwrap();
    run_git(&repo, &["init", "-q"]).await;
    tokio::fs::write(repo.join("fixture.txt"), b"fixture\n")
        .await
        .unwrap();
    run_git(&repo, &["add", "fixture.txt"]).await;
    run_git(
        &repo,
        &[
            "-c",
            "user.name=Fleet Test",
            "-c",
            "user.email=fleet@example.invalid",
            "commit",
            "-qm",
            "fixture",
        ],
    )
    .await;
    let repo = repo.canonicalize().unwrap();

    let fleet_config_path = root.join("fleet.json");
    let repo_key = repo.to_string_lossy().into_owned();
    tokio::fs::write(
        &fleet_config_path,
        serde_json::to_vec(&json!({
            "project_closeout": {
                (repo_key.clone()): {
                    "closeout_hooks": {
                        "pre_remove": [
                            "printf 'manager\\n' >> \"$BBOX_BASE_REPO/../manager-hook\""
                        ]
                    },
                    "hook_policy": {"on_fail": "block", "timeout_secs": 5}
                }
            }
        }))
        .unwrap(),
    )
    .await
    .unwrap();

    let (launch_tx, mut launch_rx) = mpsc::channel(1);
    let mut service_config = config(&root, FleetMode::Authority);
    service_config.fleet_config = Some(fleet_config_path);
    assert!(service_config.blackboxd_url.is_none());
    let service = Fleetd::open_with(
        service_config,
        Some(Arc::new(CapturingLauncher {
            sender: launch_tx,
            event_root: root.join("attempts"),
        })),
        Arc::new(UnavailableCapabilityBroker),
    )
    .await
    .unwrap();
    let handle = service.start().await.unwrap();
    let base = format!("http://{}", handle.local_addr());
    let client = authenticated_client(&root);

    let mut execution = request("closeout");
    execution.working_set = WorkingSetIntent::CreateManagedWorktree {
        base_repo: repo_key.clone(),
        requested_path: None,
        seed_dirs: Vec::new(),
    };
    let accepted: bro_capabilities::ExecutionAccepted = client
        .post(format!("{base}/v1/executions"))
        .json(&execution)
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    let launched = launch_rx.recv().await.unwrap();
    let WorkingSetIntent::CreateManagedWorktree {
        requested_path: Some(worktree),
        ..
    } = launched.request.working_set
    else {
        panic!("fleet launch did not carry its materialized worktree");
    };
    let worktree_path = PathBuf::from(&worktree);
    assert!(worktree_path.is_dir());
    run_git(&worktree_path, &["switch", "-c", "bro-fleet/closeout-test"]).await;

    let outcome: CloseoutOutcome = client
        .post(format!("{base}/control/closeout"))
        .json(&json!({
            "worktree": worktree,
            "disposition": "discard",
            "confirm": true,
            "closeout_hooks": {
                "hooks": {
                    "on_discard": [
                        "printf 'driver\\n' >> \"$BBOX_BASE_REPO/../driver-hook\""
                    ]
                },
                "on_fail": "block",
                "timeout_secs": 5
            }
        }))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    let CloseoutOutcome::Success { phases } = outcome else {
        panic!("discard closeout did not succeed");
    };
    assert!(
        phases
            .iter()
            .any(|phase| phase.phase == CloseoutPhase::Hook && phase.ok)
    );
    assert!(
        phases
            .iter()
            .any(|phase| phase.phase == CloseoutPhase::Remove && phase.ok)
    );
    assert!(!worktree_path.exists());
    assert_eq!(
        tokio::fs::read_to_string(root.join("driver-hook"))
            .await
            .unwrap(),
        "driver\n"
    );
    assert!(!root.join("manager-hook").exists());

    let task: serde_json::Value = client
        .get(format!("{base}/control/status/{}", accepted.task_id))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(task["managed_worktree"], serde_json::Value::Null);
    assert_eq!(task["cwd"], repo_key);

    handle.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn only_sunset_operational_routes_require_a_compatibility_owner() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap();
    let (launch_tx, _launch_rx) = mpsc::channel(1);
    let launcher = Arc::new(CapturingLauncher {
        sender: launch_tx,
        event_root: root.join("attempts"),
    });
    let service = Fleetd::open_with(
        config(&root, FleetMode::Authority),
        Some(launcher),
        Arc::new(UnavailableCapabilityBroker),
    )
    .await
    .unwrap();
    let handle = service.start().await.unwrap();
    let base = format!("http://{}", handle.local_addr());
    let client = authenticated_client(&root);
    let responses = [
        client
            .post(format!("{base}/control/broadcast"))
            .json(&json!({}))
            .send()
            .await
            .unwrap(),
        client
            .get(format!("{base}/control/team/example"))
            .send()
            .await
            .unwrap(),
    ];
    for response in responses {
        assert_eq!(response.status(), reqwest::StatusCode::SERVICE_UNAVAILABLE);
        let body: serde_json::Value = response.json().await.unwrap();
        assert_eq!(
            body.get("code").and_then(|value| value.as_str()),
            Some("fleet.compatibility_owner_unavailable")
        );
    }

    let dashboard: serde_json::Value = client
        .get(format!("{base}/control/dashboard?tail=5"))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(dashboard["count"], 0);

    let malformed_closeout = client
        .post(format!("{base}/control/closeout"))
        .json(&json!({}))
        .send()
        .await
        .unwrap();
    assert_eq!(
        malformed_closeout.status(),
        reqwest::StatusCode::UNPROCESSABLE_ENTITY
    );
    handle.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn authority_restarts_while_worker_reconnects_without_event_duplication() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter("fleetd=trace")
        .with_test_writer()
        .try_init();
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap();
    let socket = root.join("run/worker.sock");
    let (launch_tx, mut launch_rx) = mpsc::channel(1);
    let launcher = Arc::new(CapturingLauncher {
        sender: launch_tx,
        event_root: root.join("attempts"),
    });
    let service = Fleetd::open_with(
        config(&root, FleetMode::Authority),
        Some(launcher),
        Arc::new(UnavailableCapabilityBroker),
    )
    .await
    .unwrap();
    let handle = service.start().await.unwrap();
    let base = format!("http://{}", handle.local_addr());
    let client = authenticated_client(&root);
    let accepted: bro_capabilities::ExecutionAccepted = client
        .post(format!("{base}/v1/executions"))
        .json(&request("restart"))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    let spec = launch_rx.recv().await.unwrap();
    assert_eq!(spec.accepted, accepted);
    let initial_proof = spec.provisioned.bootstrap_proof.clone();
    let mut hello = worker_hello(&spec, initial_proof);
    let worker_id = hello.worker_id.clone();
    let task_id = hello.task_id.clone();
    let session_id = hello.session_id.clone();
    let worker_build = hello.worker_build.clone();
    let (mut peer, welcome) = connect_worker(&socket, hello.clone()).await;

    let command_envelope = recv_matching(&mut peer, "initial command", |message| {
        matches!(message, WorkerMessage::Command(_))
    })
    .await;
    let WorkerMessage::Command(command) = command_envelope.body else {
        unreachable!();
    };
    assert!(matches!(
        command.command,
        bro_protocol::WorkerCommandKind::UserTurn { ref text }
            if text == "perform the first turn"
    ));
    peer.handle()
        .send(
            WorkerMessage::CommandOutcome(CommandOutcome {
                command_seq: command.command_seq,
                command_id: command.command_id,
                accepted: true,
                terminal: true,
                result_or_error: json!({"accepted": true}),
            }),
            MessagePriority::Control,
        )
        .unwrap();
    recv_matching(&mut peer, "command outcome ack", |message| {
        matches!(message, WorkerMessage::CommandOutcomeAck(_))
    })
    .await;

    peer.handle()
        .send(
            WorkerMessage::Event(bro_protocol::WorkerEvent {
                event_seq: 1,
                occurred_at_unix_ms: 10,
                event: json!({
                    "type": "assistant",
                    "message": {"content": [{"type": "text", "text": "before restart"}]}
                }),
            }),
            MessagePriority::Normal,
        )
        .unwrap();
    recv_matching(
        &mut peer,
        "event ack before restart",
        |message| matches!(message, WorkerMessage::EventAck(ack) if ack.through_event_seq == 1),
    )
    .await;

    handle.shutdown().await.unwrap();
    drop(peer);

    let (unused_tx, mut unused_rx) = mpsc::channel(1);
    let restarted = Fleetd::open_with(
        config(&root, FleetMode::Authority),
        Some(Arc::new(CapturingLauncher {
            sender: unused_tx,
            event_root: root.join("attempts"),
        })),
        Arc::new(UnavailableCapabilityBroker),
    )
    .await
    .unwrap();
    let restarted = restarted.start().await.unwrap();
    hello.bootstrap_or_resume_proof = welcome.reconnect_proof;
    hello.last_local_event_seq = 1;
    hello.last_fleet_command_seq = 1;
    let (mut peer, second_welcome) = connect_worker(&socket, hello).await;
    assert_eq!(second_welcome.event_ack, 1);
    assert_eq!(second_welcome.connection_generation, 2);
    assert!(unused_rx.try_recv().is_err());

    peer.handle()
        .send(
            WorkerMessage::Event(bro_protocol::WorkerEvent {
                event_seq: 2,
                occurred_at_unix_ms: 20,
                event: json!({
                    "type": "result",
                    "result": "done after restart",
                    "num_turns": 2
                }),
            }),
            MessagePriority::Normal,
        )
        .unwrap();
    recv_matching(
        &mut peer,
        "staged result ack",
        |message| matches!(message, WorkerMessage::EventAck(ack) if ack.through_event_seq == 2),
    )
    .await;
    let before_marker: RosterSnapshotV1 = client
        .get(format!("http://{}/control/roster", restarted.local_addr()))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(before_marker.tasks[0].status, TaskStatus::Running);

    peer.handle()
        .send(
            WorkerMessage::Event(bro_protocol::WorkerEvent {
                event_seq: 3,
                occurred_at_unix_ms: 21,
                event: json!({
                    "type": "harness_milestone",
                    "milestone": "session_snapshot_committed"
                }),
            }),
            MessagePriority::Control,
        )
        .unwrap();
    recv_matching(
        &mut peer,
        "snapshot marker ack",
        |message| matches!(message, WorkerMessage::EventAck(ack) if ack.through_event_seq == 3),
    )
    .await;
    let completed: RosterSnapshotV1 = client
        .get(format!("http://{}/control/roster", restarted.local_addr()))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(completed.tasks.len(), 1);
    assert_eq!(
        completed.tasks[0].task_id,
        TaskId::new(accepted.task_id.as_str())
    );
    assert_eq!(completed.tasks[0].status, TaskStatus::Completed);
    assert_eq!(
        completed.tasks[0].last_message_snippet.as_deref(),
        Some("done after restart")
    );

    peer.handle()
        .send(
            WorkerMessage::Status(WorkerStatus {
                worker_id: WorkerId::new(worker_id.as_str()),
                task_id,
                session_id,
                worker_build,
                protocol_version: WORKER_PROTOCOL_V1,
                connection_generation: 2,
                last_local_event_seq: 3,
                last_fleet_command_seq: 1,
                state: WorkerLifecycleState::Terminal,
            }),
            MessagePriority::Control,
        )
        .unwrap();
    tokio::time::sleep(Duration::from_millis(20)).await;
    drop(peer);
    restarted.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn mailbox_resume_reuses_session_without_synthesizing_an_initial_turn() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap();
    let socket = root.join("run/worker.sock");
    let (launch_tx, mut launch_rx) = mpsc::channel(2);
    let service = Fleetd::open_with(
        config(&root, FleetMode::Authority),
        Some(Arc::new(CapturingLauncher {
            sender: launch_tx,
            event_root: root.join("attempts"),
        })),
        Arc::new(UnavailableCapabilityBroker),
    )
    .await
    .unwrap();
    let service = service.start().await.unwrap();
    let base = format!("http://{}", service.local_addr());
    let client = authenticated_client(&root);
    let fresh: bro_capabilities::ExecutionAccepted = client
        .post(format!("{base}/v1/executions"))
        .json(&request("mailbox-seed"))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    let _seed_spec = launch_rx.recv().await.unwrap();

    let mut mailbox_request = request("mailbox-resume");
    mailbox_request.kind = ExecutionKind::MailboxResume {
        session_id: fresh.session_id.clone(),
    };
    let resumed: bro_capabilities::ExecutionAccepted = client
        .post(format!("{base}/v1/executions"))
        .json(&mailbox_request)
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(resumed.session_id, fresh.session_id);
    let resumed_spec = launch_rx.recv().await.unwrap();
    assert!(matches!(
        resumed_spec.request.kind,
        ExecutionKind::MailboxResume { ref session_id } if session_id == &fresh.session_id
    ));
    let hello = worker_hello(
        &resumed_spec,
        resumed_spec.provisioned.bootstrap_proof.clone(),
    );
    let (mut peer, welcome) = connect_worker(&socket, hello).await;
    assert!(
        tokio::time::timeout(Duration::from_millis(100), peer.recv())
            .await
            .is_err(),
        "mailbox resume must not enqueue an initial user turn"
    );
    peer.handle()
        .send(
            WorkerMessage::Heartbeat(Heartbeat {
                lease_id: welcome.lease.lease_id.clone(),
                observed_at_unix_ms: 1,
            }),
            MessagePriority::Control,
        )
        .unwrap();
    recv_matching(&mut peer, "mailbox resume lease renewal", |message| {
        matches!(message, WorkerMessage::LeaseRenewal(renewal)
            if renewal.lease_id == welcome.lease.lease_id)
    })
    .await;

    drop(peer);
    service.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn restart_reconciles_versioned_session_policy_without_changing_worker_identity() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap();
    let socket = root.join("run/worker.sock");
    let (launch_tx, mut launch_rx) = mpsc::channel(1);
    let mut initial_config = config(&root, FleetMode::Authority);
    initial_config.allowed_capabilities = vec!["corpus.search".into()];
    let service = Fleetd::open_with(
        initial_config,
        Some(Arc::new(CapturingLauncher {
            sender: launch_tx,
            event_root: root.join("attempts"),
        })),
        Arc::new(UnavailableCapabilityBroker),
    )
    .await
    .unwrap();
    let handle = service.start().await.unwrap();
    let client = authenticated_client(&root);
    let accepted: bro_capabilities::ExecutionAccepted = client
        .post(format!("http://{}/v1/executions", handle.local_addr()))
        .json(&request("policy-restart"))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    let spec = launch_rx.recv().await.unwrap();
    let mut hello = worker_hello(&spec, spec.provisioned.bootstrap_proof.clone());
    let identity = (
        hello.worker_id.clone(),
        hello.task_id.clone(),
        hello.session_id.clone(),
    );
    let (first_peer, first_welcome) = connect_worker(&socket, hello.clone()).await;
    let first_identity = first_welcome
        .session_policy
        .feature_policy()
        .unwrap()
        .unwrap()
        .policy;
    assert_eq!(first_identity.version, 1);
    assert_eq!(
        first_welcome.session_policy.allowed_capabilities,
        vec!["corpus.search"]
    );
    handle.shutdown().await.unwrap();
    drop(first_peer);

    let (revoked_tx, _revoked_rx) = mpsc::channel(1);
    let mut revoked_config = config(&root, FleetMode::Authority);
    revoked_config.allowed_capabilities = Vec::new();
    let revoked_service = Fleetd::open_with(
        revoked_config,
        Some(Arc::new(CapturingLauncher {
            sender: revoked_tx,
            event_root: root.join("attempts"),
        })),
        Arc::new(UnavailableCapabilityBroker),
    )
    .await
    .unwrap();
    let revoked_handle = revoked_service.start().await.unwrap();
    hello.bootstrap_or_resume_proof = first_welcome.reconnect_proof;
    let (revoked_peer, revoked_welcome) = connect_worker(&socket, hello.clone()).await;
    let revoked_identity = revoked_welcome
        .session_policy
        .feature_policy()
        .unwrap()
        .unwrap()
        .policy;
    assert_eq!(revoked_identity.version, 2);
    assert_ne!(revoked_identity.digest, first_identity.digest);
    assert!(
        revoked_welcome
            .session_policy
            .allowed_capabilities
            .is_empty()
    );
    assert_eq!(
        (
            hello.worker_id.clone(),
            hello.task_id.clone(),
            hello.session_id.clone()
        ),
        identity
    );
    revoked_handle.shutdown().await.unwrap();
    drop(revoked_peer);

    let (restored_tx, _restored_rx) = mpsc::channel(1);
    let mut restored_config = config(&root, FleetMode::Authority);
    restored_config.allowed_capabilities = vec!["corpus.search".into()];
    let restored_service = Fleetd::open_with(
        restored_config,
        Some(Arc::new(CapturingLauncher {
            sender: restored_tx,
            event_root: root.join("attempts"),
        })),
        Arc::new(UnavailableCapabilityBroker),
    )
    .await
    .unwrap();
    let restored_handle = restored_service.start().await.unwrap();
    hello.bootstrap_or_resume_proof = revoked_welcome.reconnect_proof;
    let (restored_peer, restored_welcome) = connect_worker(&socket, hello).await;
    let restored_identity = restored_welcome
        .session_policy
        .feature_policy()
        .unwrap()
        .unwrap()
        .policy;
    assert_eq!(restored_identity.version, 3);
    assert_eq!(restored_identity.digest, first_identity.digest);
    assert_eq!(
        restored_welcome.session_policy.allowed_capabilities,
        vec!["corpus.search"]
    );
    assert_eq!(accepted.task_id, identity.1);

    drop(restored_peer);
    restored_handle.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn connected_worker_receives_monotonic_blackops_and_corpus_outage_recovery() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap();
    let socket = root.join("run/worker.sock");
    let available = DownstreamServiceAvailability {
        blackops: ServiceAvailability::Available,
        corpus: ServiceAvailability::Available,
    };
    let broker = Arc::new(ControllableAvailabilityBroker::new(available));
    let (launch_tx, mut launch_rx) = mpsc::channel(1);
    let mut service_config = config(&root, FleetMode::Authority);
    service_config.allowed_capabilities = vec!["corpus".into(), "blackops.agent".into()];
    let service = Fleetd::open_with(
        service_config,
        Some(Arc::new(CapturingLauncher {
            sender: launch_tx,
            event_root: root.join("attempts"),
        })),
        broker.clone(),
    )
    .await
    .unwrap();
    let service = service.start().await.unwrap();
    let base = format!("http://{}", service.local_addr());
    authenticated_client(&root)
        .post(format!("{base}/v1/executions"))
        .json(&request("live-service-policy"))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    let spec = launch_rx.recv().await.unwrap();
    let hello = worker_hello(&spec, spec.provisioned.bootstrap_proof.clone());
    let (mut peer, welcome) = connect_worker(&socket, hello).await;
    assert_eq!(
        welcome
            .session_policy
            .downstream_service_availability()
            .unwrap(),
        Some(available)
    );
    assert_eq!(
        welcome
            .session_policy
            .feature_policy()
            .unwrap()
            .unwrap()
            .policy
            .version,
        1
    );

    let transitions = [
        DownstreamServiceAvailability {
            blackops: ServiceAvailability::Unavailable,
            corpus: ServiceAvailability::Available,
        },
        available,
        DownstreamServiceAvailability {
            blackops: ServiceAvailability::Available,
            corpus: ServiceAvailability::Unavailable,
        },
        available,
    ];
    for (offset, expected) in transitions.into_iter().enumerate() {
        broker.set(expected);
        let envelope = recv_matching(&mut peer, "live service policy", |message| {
            matches!(message, WorkerMessage::ServicePolicy(policy)
                if policy.downstream_service_availability().ok().flatten() == Some(expected))
        })
        .await;
        assert_eq!(
            envelope.connection_generation, welcome.connection_generation,
            "availability changes must not reconnect the worker"
        );
        let WorkerMessage::ServicePolicy(policy) = envelope.body else {
            unreachable!();
        };
        assert_eq!(
            policy.allowed_capabilities,
            vec!["blackops.agent", "corpus"],
            "configured authorization must survive downstream outages"
        );
        assert_eq!(
            policy.feature_policy().unwrap().unwrap().policy.version,
            u64::try_from(offset).unwrap() + 2
        );
    }

    peer.handle()
        .send(
            WorkerMessage::Heartbeat(Heartbeat {
                lease_id: welcome.lease.lease_id.clone(),
                observed_at_unix_ms: 1,
            }),
            MessagePriority::Control,
        )
        .unwrap();
    let renewal = recv_matching(
        &mut peer,
        "lease renewal after service recovery",
        |message| {
            matches!(message, WorkerMessage::LeaseRenewal(renewal)
            if renewal.lease_id == welcome.lease.lease_id)
        },
    )
    .await;
    assert_eq!(renewal.connection_generation, welcome.connection_generation);

    drop(peer);
    service.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn capability_calls_are_bounded_per_worker_without_blocking_the_peer_loop() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap();
    let socket = root.join("run/worker.sock");
    let broker = Arc::new(BlockingCapabilityBroker::new());
    let (launch_tx, mut launch_rx) = mpsc::channel(1);
    let mut service_config = config(&root, FleetMode::Authority);
    service_config.allowed_capabilities = vec!["corpus.search".into()];
    let service = Fleetd::open_with(
        service_config,
        Some(Arc::new(CapturingLauncher {
            sender: launch_tx,
            event_root: root.join("attempts"),
        })),
        broker.clone(),
    )
    .await
    .unwrap();
    let service = service.start().await.unwrap();
    let base = format!("http://{}", service.local_addr());
    authenticated_client(&root)
        .post(format!("{base}/v1/executions"))
        .json(&request("capability-bound"))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    let spec = launch_rx.recv().await.unwrap();
    let hello = worker_hello(&spec, spec.provisioned.bootstrap_proof.clone());
    let (mut peer, _welcome) = connect_worker(&socket, hello).await;
    recv_matching(&mut peer, "initial command", |message| {
        matches!(message, WorkerMessage::Command(_))
    })
    .await;

    let mut calls = Vec::new();
    for ordinal in 0..8 {
        let request_handle = peer.handle();
        calls.push(tokio::spawn(async move {
            let call_id = format!("call-bounded-{ordinal}");
            request_handle
                .request_with_id(
                    call_id.clone(),
                    WorkerMessage::CapabilityRequest(CapabilityRequest {
                        call_id,
                        invocation_id: None,
                        capability: "corpus.search".into(),
                        operation: "search".into(),
                        bounded_payload: json!({"ordinal": ordinal}),
                        deadline_unix_ms: None,
                    }),
                    MessagePriority::Normal,
                    Duration::from_secs(10),
                )
                .await
                .unwrap()
        }));
    }
    tokio::time::timeout(Duration::from_secs(1), async {
        while broker.entered.load(Ordering::SeqCst) != 8 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("eight capability calls did not occupy the worker bound");

    let overflow_handle = peer.handle();
    let overflow = tokio::time::timeout(
        Duration::from_millis(300),
        overflow_handle.request_with_id(
            "call-overflow",
            WorkerMessage::CapabilityRequest(CapabilityRequest {
                call_id: "call-overflow".into(),
                invocation_id: None,
                capability: "corpus.search".into(),
                operation: "search".into(),
                bounded_payload: json!({}),
                deadline_unix_ms: None,
            }),
            MessagePriority::Normal,
            Duration::from_secs(10),
        ),
    )
    .await
    .expect("worker receive loop blocked behind its capability concurrency bound")
    .unwrap();
    let WorkerMessage::CapabilityResponse(overflow) = overflow.body else {
        panic!("overflow request received an unexpected response");
    };
    let overflow_error = overflow.structured_error().unwrap().unwrap();
    assert_eq!(overflow_error.code, CapabilityErrorCode::Unavailable);
    assert!(overflow_error.retryable);
    assert!(overflow_error.message.contains("concurrency limit"));

    broker.release.add_permits(8);
    for call in calls {
        let envelope = tokio::time::timeout(Duration::from_secs(1), call)
            .await
            .unwrap()
            .unwrap();
        let WorkerMessage::CapabilityResponse(response) = envelope.body else {
            panic!("bounded request received an unexpected response");
        };
        assert!(!response.is_error);
    }

    drop(peer);
    service.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn stalled_capability_body_does_not_starve_heartbeat_or_interrupt() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap();
    let socket = root.join("run/worker.sock");
    let token = Arc::new(ServiceToken::parse("b".repeat(64)).unwrap());
    let expected_authorization = token.authorization_header();
    let body_started = Arc::new(tokio::sync::Notify::new());
    let notify_body_started = body_started.clone();
    let app = Router::new().route(
        "/internal/capability",
        post(move |headers: HeaderMap| {
            let expected_authorization = expected_authorization.clone();
            let notify_body_started = notify_body_started.clone();
            async move {
                assert_eq!(
                    headers.get(axum::http::header::AUTHORIZATION),
                    Some(&expected_authorization)
                );
                notify_body_started.notify_one();
                let body = stream::iter([Ok::<Bytes, Infallible>(Bytes::from_static(b"{"))])
                    .chain(stream::pending());
                Response::new(Body::from_stream(body))
            }
        }),
    );
    let capability_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let capability_address = capability_listener.local_addr().unwrap();
    let capability_server = tokio::spawn(async move {
        axum::serve(capability_listener, app).await.unwrap();
    });
    let broker = HttpCapabilityBroker::new(
        None,
        Some(format!("http://{capability_address}")),
        Duration::from_millis(800),
        token,
    )
    .unwrap();

    let (launch_tx, mut launch_rx) = mpsc::channel(1);
    let mut service_config = config(&root, FleetMode::Authority);
    service_config.allowed_capabilities = vec!["corpus.search".into()];
    service_config.lease_duration_ms = 5_000;
    let service = Fleetd::open_with(
        service_config,
        Some(Arc::new(CapturingLauncher {
            sender: launch_tx,
            event_root: root.join("attempts"),
        })),
        Arc::new(broker),
    )
    .await
    .unwrap();
    let service = service.start().await.unwrap();
    let base = format!("http://{}", service.local_addr());
    let client = authenticated_client(&root);
    let accepted: bro_capabilities::ExecutionAccepted = client
        .post(format!("{base}/v1/executions"))
        .json(&request("capability-liveness"))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    let spec = launch_rx.recv().await.unwrap();
    let hello = worker_hello(&spec, spec.provisioned.bootstrap_proof.clone());
    let (mut peer, welcome) = connect_worker(&socket, hello).await;
    recv_matching(&mut peer, "initial command", |message| {
        matches!(message, WorkerMessage::Command(_))
    })
    .await;

    let request_handle = peer.handle();
    let capability_call = tokio::spawn(async move {
        request_handle
            .request_with_id(
                "call-liveness",
                WorkerMessage::CapabilityRequest(CapabilityRequest {
                    call_id: "call-liveness".into(),
                    invocation_id: None,
                    capability: "corpus.search".into(),
                    operation: "search".into(),
                    bounded_payload: json!({"query": "bounded"}),
                    deadline_unix_ms: None,
                }),
                MessagePriority::Normal,
                Duration::from_secs(10),
            )
            .await
            .unwrap()
    });
    tokio::time::timeout(Duration::from_secs(1), body_started.notified())
        .await
        .expect("capability service did not begin its stalled body");

    peer.handle()
        .send(
            WorkerMessage::Heartbeat(Heartbeat {
                lease_id: welcome.lease.lease_id.clone(),
                observed_at_unix_ms: 1,
            }),
            MessagePriority::Control,
        )
        .unwrap();
    tokio::time::timeout(
        Duration::from_millis(300),
        recv_matching(
            &mut peer,
            "lease renewal during capability call",
            |message| {
                matches!(message, WorkerMessage::LeaseRenewal(renewal)
                if renewal.lease_id == welcome.lease.lease_id)
            },
        ),
    )
    .await
    .expect("stalled capability body starved the heartbeat receive loop");
    assert!(!capability_call.is_finished());

    client
        .post(format!("{base}/control/interrupt"))
        .json(&json!({"task_id": accepted.task_id}))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    tokio::time::timeout(
        Duration::from_millis(300),
        recv_matching(&mut peer, "interrupt during capability call", |message| {
            matches!(
                message,
                WorkerMessage::Command(command)
                    if matches!(command.command, bro_protocol::WorkerCommandKind::Interrupt)
            )
        }),
    )
    .await
    .expect("stalled capability body starved interrupt delivery");
    assert!(!capability_call.is_finished());

    let envelope = tokio::time::timeout(Duration::from_secs(2), capability_call)
        .await
        .unwrap()
        .unwrap();
    let WorkerMessage::CapabilityResponse(response) = envelope.body else {
        panic!("capability request received an unexpected response");
    };
    assert_eq!(response.call_id, "call-liveness");
    assert_eq!(
        response.structured_error().unwrap().unwrap().code,
        CapabilityErrorCode::DeadlineExceeded
    );

    drop(peer);
    service.shutdown().await.unwrap();
    capability_server.abort();
}
