use std::collections::HashMap;
use std::sync::Arc;

use parking_lot::RwLock;
use rmcp::handler::server::wrapper::Parameters;
use serde_json::{Value, json};

use crate::artifacts::ArtifactInstallParams;
use crate::index::TranscriptIndex;
use crate::knowledge::Knowledge;
use crate::lsp;
use crate::notes::Notes;
use crate::orchestration;
use crate::orchestration::TaskStore;
use crate::orchestration::tail::TailEvent;
use crate::packets::Packets;
use crate::pins::Pins;
use crate::projects::ProjectRegistry;
use crate::roadmap::Roadmap;
use crate::server::dispatch::try_slack_proposal_signal_hook;
use crate::server::install_artifact_value;
use crate::server::state::{BlackboxServer, SIGNAL_LOG_CAP, SharedState, WEBHOOK_LOG_CAP};
use crate::threads::Threads;
use crate::tools::bro_runtime_params::*;
use crate::{
    artifacts, council, crons, edge_index, embed, embed_queue, entity_ref, knowledge, path_cache,
    pollers, slack_channel_bindings, slack_proposal_links, slack_thread_store, system_events, util,
    vectors, webhooks, whiteboards, workflow,
};
use tokio::sync::broadcast;

fn test_server(tmp: &tempfile::TempDir) -> BlackboxServer {
    let index = TranscriptIndex::open_or_create(
        &tmp.path().join("index"),
        Vec::new(),
        None,
        tmp.path().join("projects.json"),
        tmp.path().join("knowledge.json"),
        tmp.path().join("threads.json"),
        tmp.path().join("roadmap.json"),
    )
    .unwrap();
    let kb = Knowledge::open(&tmp.path().join("knowledge.json")).unwrap();
    let threads = Threads::open(&tmp.path().join("threads.json")).unwrap();
    let roadmap_store = Roadmap::open(&tmp.path().join("roadmap.json")).unwrap();
    let notes = Notes::open(&tmp.path().join("notes.json")).unwrap();
    let pins = Pins::open(&tmp.path().join("pins.json")).unwrap();
    let projects = ProjectRegistry::open(tmp.path().join("projects.json")).unwrap();
    let packets = Packets::open(tmp.path()).unwrap();
    let artifacts = artifacts::ArtifactCatalog::open(tmp.path().join("artifacts")).unwrap();
    let (tail_tx, _) = broadcast::channel::<TailEvent>(16);
    let state = Arc::new(SharedState {
        idx: RwLock::new(index),
        kb: RwLock::new(kb),
        roadmap: RwLock::new(roadmap_store),
        threads: RwLock::new(threads),
        notes: RwLock::new(notes),
        pins: RwLock::new(pins),
        projects: RwLock::new(projects),
        packets: RwLock::new(packets),
        artifacts: RwLock::new(artifacts),
        bbox_watcher: std::sync::Mutex::new(None),
        edge_index: RwLock::new(edge_index::EdgeIndex::default()),
        path_cache: RwLock::new(path_cache::PathCache::default()),
        task_store: Arc::new(RwLock::new(TaskStore::new())),
        tail_tx,
        store_dir: tmp.path().join("bro"),
        running_arcs: RwLock::new(HashMap::new()),
        wait_store: Arc::new(crate::workflow::wait::WaitStore::new()),
        webhooks: Arc::new(webhooks::WebhookRegistry::new()),
        pollers: Arc::new(pollers::PollerRegistry::new()),
        crons: Arc::new(crons::CronRegistry::new()),
        whiteboards: Arc::new(whiteboards::WhiteboardRegistry::new()),
        workflow_registry: Arc::new(RwLock::new(HashMap::new())),
        bind_is_loopback: true,
        signal_log: RwLock::new(std::collections::VecDeque::with_capacity(SIGNAL_LOG_CAP)),
        webhook_delivery_log: RwLock::new(std::collections::VecDeque::with_capacity(
            WEBHOOK_LOG_CAP,
        )),
        arc_cancel_tokens: RwLock::new(HashMap::new()),
        councils: Arc::new(council::CouncilRegistry::new()),
        resume_leases: Arc::new(orchestration::resume_lease::ResumeLeaseRegistry::new()),
        agent_adapter_registry: Arc::new(RwLock::new(
            orchestration::agents::adapter::AgentAdapterRegistry::new(),
        )),
        badgey_registry: Arc::new(orchestration::badgey::BadgeyRegistry::new()),
        badgey_proposals: Arc::new(
            orchestration::badgey::ProposalStore::new(tmp.path().join("bro")).unwrap(),
        ),
        badgey_journal: Arc::new(
            orchestration::badgey::ActionJournal::new(tmp.path().join("bro")).unwrap(),
        ),
        slack_thread_store: Arc::new(
            slack_thread_store::SlackThreadStore::open(&tmp.path().join("bro")).unwrap(),
        ),
        slack_channel_bindings: Arc::new(
            slack_channel_bindings::SlackChannelBindings::open(&tmp.path().join("bro")).unwrap(),
        ),
        slack_proposal_links: Arc::new(
            slack_proposal_links::SlackProposalLinks::open(&tmp.path().join("bro")).unwrap(),
        ),
        lsp_sessions: lsp::LspSessionManager::new(),
        config: Arc::new(RwLock::new(
            crate::config::load()
                .unwrap_or_else(|e| panic!("loading config for test SharedState: {e}")),
        )),
        atom_invocation_store: Arc::new(RwLock::new(
            orchestration::atoms::invocation::InvocationStore::new(
                tmp.path().join("atom-invocations.json"),
            ),
        )),
        vector_store: Arc::new(
            crate::vectors::VectorStore::open(tmp.path().join("vectors"))
                .expect("test vector store should open"),
        ),
        system_events: Arc::new(system_events::EventHub::new(
            system_events::EventStore::new_at(tmp.path().join("events").join("journal")),
            system_events::OutboxStore::new(tmp.path().join("events").join("outbox")).unwrap(),
            tmp.path().join("reactions"),
            tmp.path().join("identities"),
        )),
    });
    BlackboxServer::new(state)
}

#[tokio::test]
async fn proposal_approved_hook_bumps_link_version() {
    // Verifies the dispatch_verdict signal hook resolves a Slack
    // message back to its SlackProposalLink and bumps the version
    // on `proposal-approved`. The HTTP ack post is short-circuited
    // (no SLACK_BOT_TOKEN set in the test env) but the bump
    // happens before the token check.
    let tmp = tempfile::tempdir().unwrap();
    let server = test_server(&tmp);
    let link = slack_proposal_links::SlackProposalLink {
        team_id: "T01".into(),
        channel_id: "C01".into(),
        msg_ts: "ts1".into(),
        proposal_id: "triage-1".into(),
        instance_id: None,
        authoring_session_id: None,
        version: 1,
        project_dir: "/repo/x".into(),
        posted_at: util::now_iso(),
    };
    server.state.slack_proposal_links.record(link).unwrap();
    let mut correlate = serde_json::Map::new();
    correlate.insert("thread_ts".into(), Value::String("ts1".into()));
    let entity = json!({
        "team_id": "T01",
        "channel": "C01",
        "user": "Ualice",
        "bbox_user": "alice",
    });
    // Ensure no token leaks in from the surrounding env so the
    // hook short-circuits before HTTP. (Safety belt — the test
    // depends on the bump happening before the token check.)
    unsafe {
        std::env::remove_var("SLACK_BOT_TOKEN");
    }
    try_slack_proposal_signal_hook("proposal-approved", &server.state, &correlate, &entity).await;
    let bumped = server
        .state
        .slack_proposal_links
        .lookup_by_msg("T01", "C01", "ts1")
        .unwrap();
    assert_eq!(bumped.version, 2);
}

#[tokio::test]
async fn proposal_clarify_hook_does_not_bump_version() {
    // Clarify hook resolves the message back to its link and
    // (will eventually) post a stub reply, but does NOT bump the
    // link version — version is reserved for chat.update of the
    // original proposal post when a refined version lands.
    let tmp = tempfile::tempdir().unwrap();
    let server = test_server(&tmp);
    let link = slack_proposal_links::SlackProposalLink {
        team_id: "T01".into(),
        channel_id: "C01".into(),
        msg_ts: "ts2".into(),
        proposal_id: "triage-2".into(),
        instance_id: None,
        authoring_session_id: None,
        version: 1,
        project_dir: "/repo/x".into(),
        posted_at: util::now_iso(),
    };
    server.state.slack_proposal_links.record(link).unwrap();
    let mut correlate = serde_json::Map::new();
    correlate.insert("thread_ts".into(), Value::String("ts2".into()));
    let entity = json!({
        "team_id": "T01",
        "channel": "C01",
        "user": "Ualice",
        "text": "actually never mind, this one is fine as-is",
    });
    unsafe {
        std::env::remove_var("SLACK_BOT_TOKEN");
    }
    try_slack_proposal_signal_hook("proposal-clarify", &server.state, &correlate, &entity).await;
    let unchanged = server
        .state
        .slack_proposal_links
        .lookup_by_msg("T01", "C01", "ts2")
        .unwrap();
    assert_eq!(unchanged.version, 1);
}

#[tokio::test]
async fn proposal_signal_hook_no_op_for_unknown_thread_ts() {
    // No SlackProposalLink for the correlated thread_ts → hook is
    // a silent no-op. Any other in-thread reply or reaction in
    // the workspace should NOT cause stub acks.
    let tmp = tempfile::tempdir().unwrap();
    let server = test_server(&tmp);
    let mut correlate = serde_json::Map::new();
    correlate.insert("thread_ts".into(), Value::String("ts-unknown".into()));
    let entity = json!({
        "team_id": "T01",
        "channel": "C01",
        "user": "Ualice",
    });
    unsafe {
        std::env::remove_var("SLACK_BOT_TOKEN");
    }
    try_slack_proposal_signal_hook("proposal-approved", &server.state, &correlate, &entity).await;
    // Nothing to assert beyond "did not panic" — but make a
    // sanity probe on the link store size to confirm we didn't
    // accidentally insert anything.
    assert!(
        server
            .state
            .slack_proposal_links
            .lookup_by_msg("T01", "C01", "ts-unknown")
            .is_none()
    );
}

#[tokio::test]
async fn embed_compaction_arc_gates_against_vector_status_vars() {
    let tmp = tempfile::tempdir().unwrap();
    let vector_store = Arc::new(vectors::VectorStore::open(tmp.path().join("vectors")).unwrap());
    let _guard = vectors::install_test_global(vector_store.clone());
    let route = "test-compaction-route";
    for idx in 0..10 {
        let theta = idx as f32 * 0.01;
        vector_store
            .upsert(
                route,
                &format!("entity-{idx}"),
                &format!("hash-{idx}"),
                vec![theta.cos(), theta.sin(), 0.0, 0.0],
            )
            .unwrap();
    }
    for idx in 0..4 {
        vector_store
            .delete(route, &format!("entity-{idx}"))
            .unwrap();
    }
    let before = vector_store.metrics().remove(route).unwrap();
    assert_eq!(before.active_count, 6);
    assert_eq!(before.deleted_count, 4);
    assert!(before.deleted_ratio > 0.3);

    let server = test_server(&tmp);
    let packet_value: Value = serde_json::from_str(include_str!(
        "../system-defaults/agentic-corpus/packets/embed/compaction-policy.json"
    ))
    .unwrap();
    install_artifact_value(
        &server.state,
        ArtifactInstallParams {
            kind: artifacts::ArtifactKind::Packet,
            source: "system-defaults/agentic-corpus/packets/embed/compaction-policy.json".into(),
            name: None,
            version: None,
            supersedes: None,
        },
        packet_value,
    )
    .await
    .unwrap();

    let workflow_spec: workflow::Workflow = serde_json::from_str(include_str!(
        "../system-defaults/agentic-corpus/workflows/embed-compaction-arc.json"
    ))
    .unwrap();
    let compiled = workflow::compile(workflow_spec).unwrap();
    let result = workflow::run_workflow_with_initial_vars(
        &server,
        &compiled,
        Some(tmp.path().to_string_lossy().into_owned()),
        Some(20),
        serde_json::Map::new(),
    )
    .await;

    assert_eq!(result.status, "completed");
    assert_eq!(result.vars.get("rebuild_started"), Some(&Value::Bool(true)));
    assert_eq!(result.vars.get("swapped"), Some(&Value::Bool(true)));
    assert!(result.events.iter().any(|event| {
        event.get("kind").and_then(Value::as_str) == Some("gate_applied")
            && event
                .get("data")
                .and_then(|data| data.get("verdict"))
                .and_then(Value::as_str)
                == Some("compact")
    }));
    let after = vector_store.metrics().remove(route).unwrap();
    assert_eq!(after.active_count, 6);
    assert_eq!(after.deleted_count, 0);
}

#[tokio::test]
async fn write_semantic_edge_projects_describes_sidecar() {
    let tmp = tempfile::tempdir().unwrap();
    let server = test_server(&tmp);
    let edges_dir = tmp.path().join("edges");
    let source = "project_file:proj1234:relhash:chunkhash:0";
    let target = "symbol:proj1234:EntityRef:defnhash";
    let ctx = workflow::context::ArcContext::new(workflow::context::ArcMeta {
        arc_id: "arc-test".into(),
        workflow_name: "auto-edge-arc".into(),
        workflow_version: 1,
        project_dir: Some(tmp.path().to_string_lossy().into_owned()),
        ..Default::default()
    });
    let hook = workflow::ops::HookOp {
        op: workflow::ops::OpKind::WriteSemanticEdge,
        args: json!({
            "source": source,
            "target": target,
            "kind": "DESCRIBES",
            "edges_dir": edges_dir,
            "note": "synthetic doc-section describes EntityRef"
        }),
        when: None,
        on_failure: workflow::ops::OnFailure::Halt,
        into_var: Some("semantic_edge".into()),
    };
    workflow::ops::execute_op(&hook, &ctx, None).await.unwrap();
    let edge_index = edge_index::EdgeIndex::rebuild(&edge_index::EdgeStoreRefs {
        index: &server.state.idx.read(),
        knowledge: &server.state.kb.read(),
        threads: &server.state.threads.read(),
        notes: &server.state.notes.read(),
        task_store: &server.state.task_store.read(),
        roadmap: &server.state.roadmap.read(),
        edges_dir,
        registered_project_ids: None,
        include_tantivy_projection: true,
        include_observed: true,
    });
    let source_ref = entity_ref::EntityRef::parse(source).unwrap();
    let target_ref = entity_ref::EntityRef::parse(target).unwrap();
    assert!(
        edge_index
            .forward_edges(&source_ref)
            .iter()
            .any(|edge| edge.kind == "DESCRIBES" && edge.target == target_ref)
    );
}

#[tokio::test]
async fn tier0_contradiction_without_arc_surfaces_surprise_note() {
    let tmp = tempfile::tempdir().unwrap();
    let server = test_server(&tmp);
    embed_queue::install_contradiction_threshold(0.85);
    embed_queue::install_contradiction_state(server.state.clone());
    let vector_store = Arc::new(vectors::VectorStore::open(tmp.path().join("vectors")).unwrap());
    let _guard = vectors::install_test_global(vector_store.clone());
    let now = "2026-01-01T00:00:00Z".to_string();
    for (id, content) in [
        ("aaaabbbb", "use provider A for embeddings"),
        ("ccccdddd", "never use provider A for embeddings"),
    ] {
        server
            .state
            .kb
            .write()
            .upsert_generated(knowledge::KnowledgeEntry {
                id: id.into(),
                title: id.into(),
                content: content.into(),
                cluster: None,
                variants: Default::default(),
                category: knowledge::Category::Memory,
                scope: knowledge::Scope::Global,
                project: None,
                providers: Vec::new(),
                priority: knowledge::Priority::Standard,
                weight: 100,
                status: knowledge::Status::Active,
                approval: knowledge::Approval::UserConfirmed,
                render: false,
                decay: true,
                review_at: None,
                supersedes: None,
                links: Vec::new(),
                rationale: None,
                expires_at: None,
                source: "test".into(),
                created_at: now.clone(),
                updated_at: now.clone(),
                recall_count: 0,
                last_recalled: None,
            })
            .unwrap();
    }
    vector_store
        .upsert(
            "knowledge-test",
            "knowledge:ccccdddd",
            "h-old",
            vec![1.0, 0.0],
        )
        .unwrap();
    vector_store
        .upsert(
            "knowledge-test",
            "knowledge:aaaabbbb",
            "h-new",
            vec![0.99, 0.01],
        )
        .unwrap();
    let request = embed::queue::EmbedRequest {
        bucket: embed::Bucket::Knowledge,
        project_id: None,
        entity_id: "knowledge:aaaabbbb".into(),
        chunk_hash: "h-new".into(),
        text: "use provider A for embeddings".into(),
    };
    embed_queue::maybe_detect_knowledge_contradiction(&request, "knowledge-test", &[0.99, 0.01]);

    assert!(server.state.notes.read().all().iter().any(|note| {
        note.body.contains("Tier-0 contradiction detected")
            && note.body.contains("knowledge:aaaabbbb")
            && note.body.contains("knowledge:ccccdddd")
    }));

    embed_queue::install_contradiction_threshold(1.0);
    let note_count = server.state.notes.read().all().len();
    embed_queue::maybe_detect_knowledge_contradiction(&request, "knowledge-test", &[0.99, 0.01]);
    assert_eq!(server.state.notes.read().all().len(), note_count);
    embed_queue::install_contradiction_threshold(0.85);
}

// ── Phase 6 real emit site tests ─────────────────────────────────────────────
#[tokio::test]
async fn system_events_task_lifecycle_started_and_terminal() {
    let tmp = tempfile::tempdir().unwrap();
    let server = test_server(&tmp);
    let hub = server.state.system_events.clone();

    let (tail_tx, _) = tokio::sync::broadcast::channel::<TailEvent>(16);
    let task_id = "test-task-se-001".to_string();
    let task = orchestration::spawn_in_process_task(
        task_id.clone(),
        orchestration::providers::Provider::Workflow,
        "arc-test-001".to_string(),
        None,
        tmp.path().join("bro"),
        server.state.task_store.clone(),
        tail_tx.clone(),
        Some("test-bro".to_string()),
        None,
        Some(hub.clone()),
    );

    // Yield so the spawned emit task can run
    for _ in 0..20 {
        tokio::task::yield_now().await;
    }

    let started: Vec<_> = hub
        .list_events(None, Some("task.started"), None, None)
        .unwrap()
        .into_iter()
        .filter(|e| e.payload.get("task_id").and_then(|v| v.as_str()) == Some(&task_id))
        .collect();
    assert_eq!(started.len(), 1, "expected one task.started event");

    orchestration::finish_in_process_task(
        &task,
        orchestration::TaskStatus::Completed,
        Some("ok".to_string()),
        None,
        &server.state.task_store,
        &tmp.path().join("bro"),
        &tail_tx,
        Some(hub.clone()),
    );

    for _ in 0..20 {
        tokio::task::yield_now().await;
    }

    let completed: Vec<_> = hub
        .list_events(None, Some("task.completed"), None, None)
        .unwrap()
        .into_iter()
        .filter(|e| e.payload.get("task_id").and_then(|v| v.as_str()) == Some(&task_id))
        .collect();
    assert_eq!(completed.len(), 1, "expected one task.completed event");
}

#[tokio::test]
async fn system_events_workflow_arc_started_and_completed() {
    use crate::workflow::{compile, engine, load_workflow};
    let tmp = tempfile::tempdir().unwrap();
    let server = test_server(&tmp);
    let hub = server.state.system_events.clone();

    let json = r#"{
        "name": "se-smoke",
        "version": 1,
        "actors": {},
        "nodes": {
            "Only": {
                "actor": "",
                "next": {"type": "terminal"}
            }
        },
        "start": "Only"
    }"#;
    let compiled = compile(load_workflow(json).unwrap()).unwrap();
    let result = engine::run_workflow_with_initial_vars(
        &server,
        &compiled,
        None,
        Some(5),
        serde_json::Map::new(),
    )
    .await;
    assert_eq!(result.status, "completed");

    let started = hub
        .list_events(None, Some("workflow.arc.started"), None, None)
        .unwrap();
    assert!(!started.is_empty(), "expected workflow.arc.started event");

    let completed = hub
        .list_events(None, Some("workflow.arc.completed"), None, None)
        .unwrap();
    assert!(
        !completed.is_empty(),
        "expected workflow.arc.completed event"
    );

    let arc_id = result.arc_id;
    assert!(
        started
            .iter()
            .any(|e| e.payload.get("arc_id").and_then(|v| v.as_str()) == Some(&arc_id)),
        "started event should carry arc_id"
    );
}

#[tokio::test]
async fn system_events_workflow_wait_registered_and_signal_received() {
    use crate::workflow::{compile, engine, load_workflow};
    let tmp = tempfile::tempdir().unwrap();
    let server = test_server(&tmp);
    let hub = server.state.system_events.clone();

    // Arc parks on a Wait, we fire the matching signal, confirm both events.
    let json = r#"{
        "name": "se-wait-signal",
        "version": 1,
        "actors": {},
        "nodes": {
            "Park": {
                "actor": "",
                "wait": {
                    "any_of": [{"signal": "test-signal"}]
                },
                "next": {"type": "terminal"}
            }
        },
        "start": "Park"
    }"#;
    let compiled = compile(load_workflow(json).unwrap()).unwrap();

    let server_state = server.state.clone();
    let hub_clone = hub.clone();
    let run_handle = tokio::spawn(async move {
        let srv = BlackboxServer::new(server_state);
        engine::run_workflow_with_initial_vars(
            &srv,
            &compiled,
            None,
            Some(5),
            serde_json::Map::new(),
        )
        .await
    });

    // Wait until wait_registered event appears in the hub.
    let mut registered = false;
    for _ in 0..200 {
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        let evts = hub_clone
            .list_events(None, Some("workflow.arc.wait_registered"), None, None)
            .unwrap_or_default();
        if !evts.is_empty() {
            registered = true;
            break;
        }
    }
    assert!(
        registered,
        "wait_registered event should appear before signal"
    );

    // Fire signal via match_and_take to unblock the arc.
    let snaps = server.wait_store().snapshot();
    assert!(!snaps.is_empty(), "expected a pending wait in the store");
    let w = &snaps[0];
    if let Some((slot, notify, _, _)) = server
        .wait_store()
        .match_and_take(&w.signal, &serde_json::Map::new())
    {
        *slot.lock() = Some(workflow::context::SignalRef {
            name: w.signal.clone(),
            payload: serde_json::Value::Null,
            correlation: serde_json::Map::new(),
            received_at: crate::util::now_iso(),
        });
        notify.notify_waiters();
    }

    let result = tokio::time::timeout(std::time::Duration::from_secs(5), run_handle)
        .await
        .expect("arc did not complete within 5s")
        .expect("runner panicked");
    assert_eq!(result.status, "completed");

    let received = hub
        .list_events(None, Some("workflow.arc.signal_received"), None, None)
        .unwrap();
    assert!(
        !received.is_empty(),
        "expected workflow.arc.signal_received event"
    );
}

#[tokio::test]
async fn workflow_wait_catches_recent_correlated_system_event() {
    use crate::system_events::SystemEventDraft;
    use crate::system_events::types::SystemEventKind;
    use crate::workflow::{compile, engine, load_workflow};

    let tmp = tempfile::tempdir().unwrap();
    let server = test_server(&tmp);
    let mut correlation = serde_json::Map::new();
    correlation.insert("identity".to_string(), serde_json::json!("impl"));
    server
        .state
        .system_events
        .emit(SystemEventDraft {
            kind: SystemEventKind::Unknown("bro.identity.provisioned".to_string()),
            producer: "test".to_string(),
            project: None,
            principal: None,
            subject: None,
            correlation: correlation.clone(),
            causation_id: None,
            payload: serde_json::json!({"ok": true}),
        })
        .await
        .unwrap();

    let json = r#"{
        "name": "se-wait-catch-up",
        "version": 1,
        "actors": {},
        "nodes": {
            "Park": {
                "actor": "",
                "wait": {
                    "any_of": [{
                        "signal": "bro.identity.provisioned",
                        "correlate": {
                            "identity": {"kind": "const", "value": "impl"}
                        }
                    }]
                },
                "next": {"type": "terminal"}
            }
        },
        "start": "Park"
    }"#;
    let compiled = compile(load_workflow(json).unwrap()).unwrap();
    let result = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        engine::run_workflow_with_initial_vars(
            &server,
            &compiled,
            None,
            Some(5),
            serde_json::Map::new(),
        ),
    )
    .await
    .expect("arc did not catch up to system event");
    assert_eq!(result.status, "completed");
}

#[tokio::test]
async fn system_events_whiteboard_transition_emits_phase_changed() {
    // Exercises the real whiteboard_transition tool path end-to-end.
    // Registers a pending wait matching the board-transitioned signal so both
    // routing and event emission are proved with one call: the routing dispatch
    // must resolve the wait (SignalRef written to the resolved slot) AND the
    // system event must land in the hub.
    let tmp = tempfile::tempdir().unwrap();
    let server = test_server(&tmp);
    let hub = server.state.system_events.clone();

    let board_id = "board-se-01";

    server
        .state
        .whiteboards
        .open(
            board_id,
            "Phase 6 test board",
            "",
            None,
            "facilitator-agent",
        )
        .expect("open board");
    server
        .state
        .whiteboards
        .register(
            board_id,
            "facilitator-agent",
            whiteboards::Role::Facilitator,
            "test-domain",
        )
        .expect("register facilitator");

    // Register a pending wait for the board-transitioned signal so the routing
    // call inside the spawned task has something to resolve.
    let notify = Arc::new(tokio::sync::Notify::new());
    let resolved: Arc<parking_lot::Mutex<Option<workflow::context::SignalRef>>> =
        Arc::new(parking_lot::Mutex::new(None));
    {
        let mut corr = serde_json::Map::new();
        corr.insert("board".into(), serde_json::json!(board_id));
        corr.insert("phase".into(), serde_json::json!("read"));
        server.wait_store().register(workflow::wait::PendingWait {
            arc_id: "test-arc-wb-01".into(),
            wait_id: "w1".into(),
            signal: "board-transitioned".into(),
            correlation: corr,
            notify: notify.clone(),
            resolved: resolved.clone(),
        });
    }

    // Call the real tool handler — this is the path under test.
    let result = server
        .whiteboard_transition(Parameters(WhiteboardTransitionParams {
            board_id: board_id.into(),
            agent_name: "facilitator-agent".into(),
            target_phase: "read".into(),
            summary: None,
        }))
        .await;
    assert_ne!(
        result.is_error,
        Some(true),
        "transition tool returned error"
    );

    // Yield enough for the spawned task (routing + system event emit) to complete.
    for _ in 0..40 {
        tokio::task::yield_now().await;
    }

    // Routing must have resolved the registered wait — proves signal routing fired.
    let sig = resolved.lock().clone();
    assert!(
        sig.is_some(),
        "wait was not resolved — routing did not fire"
    );
    assert_eq!(sig.unwrap().name, "board-transitioned");

    // The system event must also have been emitted.
    let phase_events = hub
        .list_events(None, Some("whiteboard.phase_changed"), None, None)
        .unwrap();
    assert!(
        !phase_events.is_empty(),
        "expected whiteboard.phase_changed system event"
    );
    let ev = &phase_events[0];
    assert_eq!(
        ev.payload.get("board_id").and_then(|v| v.as_str()),
        Some(board_id)
    );
    assert_eq!(
        ev.payload.get("to_phase").and_then(|v| v.as_str()),
        Some("read")
    );
    assert_eq!(ev.producer, "whiteboard.transition");
}

#[tokio::test]
async fn system_events_task_progress() {
    // Exercises the production task.progress emission path by calling
    // orchestration::emit_task_progress_event — the same helper the streaming
    // loop uses on every new deduplicated snippet. This proves the contract
    // (kind, correlation, payload fields) through production code rather than
    // a duplicate manual hub.emit.
    let tmp = tempfile::tempdir().unwrap();
    let server = test_server(&tmp);
    let hub = server.state.system_events.clone();

    let task_id = "test-task-progress-001";
    let activity = "Analyzing the codebase for refactor candidates…";

    orchestration::emit_task_progress_event(&hub, task_id.to_string(), activity.to_string());

    // Yield for the spawned background task to complete.
    for _ in 0..20 {
        tokio::task::yield_now().await;
    }

    let evts = hub
        .list_events(None, Some("task.progress"), None, None)
        .unwrap();
    assert_eq!(evts.len(), 1, "expected exactly one task.progress event");
    let ev = &evts[0];
    assert_eq!(
        ev.correlation.get("task_id").and_then(|v| v.as_str()),
        Some(task_id),
        "task_id must be in correlation"
    );
    assert_eq!(
        ev.payload.get("task_id").and_then(|v| v.as_str()),
        Some(task_id),
        "task_id must be in payload"
    );
    assert_eq!(
        ev.payload.get("activity").and_then(|v| v.as_str()),
        Some(activity),
        "activity must be in payload"
    );
    assert_eq!(ev.producer, "orchestration.dispatch");
}
