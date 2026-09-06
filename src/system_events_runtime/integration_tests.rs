use std::collections::HashMap;
use std::sync::Arc;

use parking_lot::RwLock;
use rmcp::handler::server::wrapper::Parameters;

use crate::index::TranscriptIndex;
use crate::knowledge::Knowledge;
use crate::notes::Notes;
use crate::orchestration;
use crate::orchestration::TaskStore;
use crate::orchestration::tail::TailEvent;
use crate::packets::Packets;
use crate::pins::Pins;
use crate::projects::ProjectRegistry;
use crate::roadmap::Roadmap;
use crate::server::state::{BlackboxServer, SIGNAL_LOG_CAP, SharedState, WEBHOOK_LOG_CAP};
use crate::store_persister::StorePersister;
use crate::threads::Threads;
use crate::tools::bro_runtime_params::*;
use crate::{
    artifacts, crons, edge_index, path_cache, pollers, slack_channel_bindings,
    slack_proposal_links, system_events, webhooks, whiteboards, workflow,
};
use tokio::sync::broadcast;

fn test_server(tmp: &tempfile::TempDir) -> BlackboxServer {
    let index = TranscriptIndex::open_or_create_with_records(
        &tmp.path().join("index"),
        Vec::new(),
        None,
        tmp.path().join("projects.json"),
        tmp.path().join("knowledge.json"),
        tmp.path().join("threads.json"),
        tmp.path().join("roadmap.json"),
        std::sync::Arc::new(bbox_corpus_index::index::StaticProjectRecordsProvider::empty()),
    )
    .unwrap();
    let kb_path = tmp.path().join("knowledge.json");
    let kb = Arc::new(RwLock::new(Knowledge::open(&kb_path).unwrap()));
    let kb_persister = StorePersister::spawn("knowledge-test", kb.clone(), kb_path);
    let gaps = crate::gaps::GapStore::open(&tmp.path().join("gaps.json")).unwrap();
    let threads_path = tmp.path().join("threads.json");
    let threads = Arc::new(RwLock::new(Threads::open(&threads_path).unwrap()));
    let threads_persister = StorePersister::spawn("threads-test", threads.clone(), threads_path);
    let roadmap_path = tmp.path().join("roadmap.json");
    let roadmap_store = Arc::new(RwLock::new(Roadmap::open(&roadmap_path).unwrap()));
    let roadmap_persister =
        StorePersister::spawn("roadmap-test", roadmap_store.clone(), roadmap_path);
    let notes_path = tmp.path().join("notes.json");
    let notes = Arc::new(RwLock::new(Notes::open(&notes_path).unwrap()));
    let notes_persister = StorePersister::spawn("notes-test", notes.clone(), notes_path);
    let pins_path = tmp.path().join("pins.json");
    let pins = Arc::new(RwLock::new(Pins::open(&pins_path).unwrap()));
    let pins_persister = StorePersister::spawn("pins-test", pins.clone(), pins_path);
    let checkout_mutations_path = tmp.path().join("checkout-mutations.json");
    let checkout_mutations = Arc::new(RwLock::new(
        crate::checkout_mutations::CheckoutMutations::open(&checkout_mutations_path).unwrap(),
    ));
    let checkout_mutations_persister = StorePersister::spawn(
        "checkout-mutations-test",
        checkout_mutations.clone(),
        checkout_mutations_path,
    );
    let projects_path = tmp.path().join("projects.json");
    let projects = Arc::new(RwLock::new(
        ProjectRegistry::open(projects_path.clone()).unwrap(),
    ));
    let projects_persister =
        StorePersister::spawn("projects-test", projects.clone(), projects_path);
    let checkout_registry = Arc::new(RwLock::new(
        bbox_indexing::checkout_registry::CheckoutRegistry::open(
            &tmp.path().join("checkout-registry.json"),
        )
        .unwrap(),
    ));
    let checkout_access_observations =
        bbox_indexing::checkout_access::CheckoutAccessObservations::open(
            tmp.path().join("checkout-access-observations.json"),
        )
        .unwrap();
    let checkout_access = Arc::new(bbox_indexing::checkout_access::CheckoutAccessBroker::new(
        Arc::new(
            bbox_indexing::checkout_access_v1::V1CheckoutAccessAuthority::new(
                projects.clone(),
                checkout_registry.clone(),
            ),
        ),
        checkout_access_observations.clone(),
    ));
    let records_provider: Arc<dyn bbox_corpus_core::project_record::ProjectRecordsProvider> =
        Arc::new(bbox_indexing::projects::BridgeProjectRecordsProvider::new(
            projects.clone(),
        ));
    let index_writer = crate::index::IndexWriterActor::spawn_for_with_checkout_access(
        &index,
        records_provider.clone(),
        checkout_access.clone(),
    );
    let packets = Packets::open(tmp.path()).unwrap();
    let artifacts = artifacts::ArtifactCatalog::open(tmp.path().join("artifacts")).unwrap();
    let (tail_tx, _) = broadcast::channel::<TailEvent>(16);
    let (roster_tx, _) = broadcast::channel::<bro_protocol::RosterDelta>(16);
    let active_code_selectors = index.active_code_selectors();
    let code_searcher = index.searcher();
    let state = Arc::new(SharedState {
        idx: RwLock::new(index),
        index_writer,
        kb,
        kb_persister,
        gaps: RwLock::new(gaps),
        roadmap: roadmap_store,
        roadmap_persister,
        threads,
        threads_persister,
        notes,
        notes_persister,
        pins,
        pins_persister,
        checkout_mutations,
        checkout_mutations_persister,
        project_authority: crate::server::state::ProjectAuthority::Bridge {
            registry: projects,
            persister: projects_persister,
        },
        accepted_publications: None,
        records_provider,
        checkout_registry,
        checkout_access_observations,
        resolver_compat: crate::server::resolver_compat::ResolverCompatObservations::in_memory(),
        checkout_access,
        knowledge_transport_observations:
            bbox_indexing::knowledge_transport_observations::KnowledgeTransportObservationsV1::in_memory(),
        blame_locality_observations:
            bbox_indexing::blame_locality_observations::BlameLocalityObservationsV1::in_memory(),
        render_locality_observations:
            bbox_indexing::render_locality_observations::RenderLocalityObservationsV1::in_memory(),
        publisher_refs: RwLock::new(
            bbox_indexing::publisher::PublisherRefStore::open(
                tmp.path().join("publisher-refs.json"),
            )
            .unwrap(),
        ),
        knowledge_overlays: RwLock::new(bbox_knowledge::overlay::KnowledgeOverlayStore::default()),
        gap_overlays: RwLock::new(bbox_gaps::overlay::GapOverlayStore::default()),
        knowledge_overlay_refresh: parking_lot::Mutex::new(()),
        gap_overlay_refresh: parking_lot::Mutex::new(()),
        path_fallback_cut: std::sync::atomic::AtomicBool::new(false),
        knowledge_published_cache: RwLock::new(Default::default()),
        gap_published_cache: RwLock::new(Default::default()),
        catalog_knowledge_published_cache: RwLock::new(Default::default()),
        catalog_gap_published_cache: RwLock::new(Default::default()),
        project_graph_views: RwLock::new(Default::default()),
        publisher_authorization_cache: RwLock::new(Default::default()),
        packets: RwLock::new(packets),
        surface_decisions: crate::server::surface::SurfaceDecisionCache::default(),
        artifacts: RwLock::new(artifacts),
        bbox_watcher: std::sync::Mutex::new(None),
        reindex_dirty: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
        code_read_view: RwLock::new(Arc::new(crate::server::CodeReadView {
            active_selectors: active_code_selectors,
            searcher: code_searcher,
            edge_index: Arc::new(edge_index::EdgeIndex::default()),
            catalog_epoch: 0,
            git_overlays: std::collections::BTreeMap::new(),
        })),
        edge_index_ready: std::sync::atomic::AtomicBool::new(true),
        code_sources: Arc::new(crate::server::code_source::CodeSourceRuntime::for_test(
            tmp.path(),
        )),
        file_sources: Arc::new(crate::server::file_source::FileSourceRuntime::for_test(
            tmp.path(),
        )),
        conversation_sources: Arc::new(
            crate::server::conversation_source::ConversationSourceRuntime::for_test(tmp.path()),
        ),
        git_sources: Arc::new(crate::server::git_source::GitSourceRuntime::for_test(
            tmp.path(),
        )),
        knowledge_sources: Arc::new(
            crate::server::knowledge_source::KnowledgeSourceRuntime::for_test(tmp.path()),
        ),
        git_transport_cutover: Arc::new(
            bbox_indexing::git_transport_cutover::GitTransportCutoverRuntimeV1::default(),
        ),
        knowledge_transport_cutover: Arc::new(
            bbox_indexing::knowledge_transport_cutover::KnowledgeTransportCutoverRuntimeV1::default(),
        ),
        blame_locality_cutover: Arc::new(
            bbox_indexing::blame_locality_cutover::BlameLocalityCutoverRuntimeV1::default(),
        ),
        render_locality_cutover: Arc::new(
            bbox_indexing::render_locality_cutover::RenderLocalityCutoverRuntimeV1::default(),
        ),
        code_source_locality_cutover: Arc::new(
            bbox_indexing::code_source_locality_cutover::CodeSourceLocalityCutoverRuntimeV1::default(),
        ),
        reconciler_shutdown: parking_lot::RwLock::new(Arc::new(
            std::sync::atomic::AtomicBool::new(false),
        )),
        edge_rebuild_nudge_tx: std::sync::mpsc::sync_channel(1).0,
        edge_rebuild_nudge_rx: std::sync::Mutex::new(None),
        path_cache: RwLock::new(path_cache::PathCache::default()),
        task_store: Arc::new(RwLock::new(TaskStore::new())),
        tail_tx,
        roster_version: Arc::new(std::sync::atomic::AtomicU64::new(0)),
        roster_tx,
        roster_view: Arc::new(orchestration::RosterView::new()),
        store_dir: tmp.path().join("bro"),
        running_arcs: RwLock::new(HashMap::new()),
        wait_store: Arc::new(crate::workflow::wait::WaitStore::new()),
        arc_store: Arc::new(crate::workflow::arc_store::ArcStore::new(
            tmp.path().join("bro").join("arcs"),
        )),
        arc_admissions: parking_lot::Mutex::new(HashMap::new()),
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
        resume_leases: Arc::new(orchestration::resume_lease::ResumeLeaseRegistry::new()),
        drain: crate::server::drain::DrainState::in_memory(tmp.path()),
        long_polls: Arc::new(crate::server::drain::LongPollRegistry::new()),
        agent_adapter_registry: Arc::new(RwLock::new(
            orchestration::agents::adapter::AgentAdapterRegistry::new(),
        )),
        slack_channel_bindings: Arc::new(
            slack_channel_bindings::SlackChannelBindings::open(&tmp.path().join("bro")).unwrap(),
        ),
        slack_proposal_links: Arc::new(
            slack_proposal_links::SlackProposalLinks::open(&tmp.path().join("bro")).unwrap(),
        ),
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
        None,
        Some("test-bro".to_string()),
        None,
        Some(hub.clone()),
        // Workflow origin — system_events lifecycle test exercises
        // the same workflow harness-task path as orchestrate.rs.
        bro_core::Origin::Workflow,
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
            source_event_id: None,
        });
        // notify_one (not notify_waiters): the real router uses it
        // because it stores a permit when the runner has not reached
        // its suspension select yet; notify_waiters would lose the
        // wake in that window and hang the arc.
        notify.notify_one();
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
