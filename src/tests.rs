use std::collections::HashMap;
use std::sync::Arc;

use parking_lot::RwLock;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::CallToolResult;
use serde_json::{Value, json};

use crate::artifacts::{ArtifactInstallParams, ArtifactListParams};
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
use crate::server::state::{BlackboxServer, SIGNAL_LOG_CAP, SharedState, WEBHOOK_LOG_CAP};
use crate::server::workflow_capabilities::validate_workflow_capabilities;
use crate::server::{install_artifact_value, restore_runtime_artifacts_from_catalog};
use crate::threads::Threads;
use crate::tools::bro_params::*;
use crate::tools::bro_runtime_params::*;
use crate::{
    artifacts, council, crons, edge_index, embed, embed_queue, entity_ref, knowledge, packets,
    path_cache, pollers, slack_channel_bindings, slack_proposal_links, slack_thread_store,
    system_events, util, vectors, webhooks, whiteboards, workflow,
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
async fn artifact_install_wires_f3_workflow_and_packet() {
    let tmp = tempfile::tempdir().unwrap();
    let server = test_server(&tmp);
    let workflow_value: Value = serde_json::from_str(include_str!(
        "../system-defaults/agentic-corpus/workflows/schema-migration-arc.json"
    ))
    .unwrap();
    let packet_value: Value = serde_json::from_str(include_str!(
        "../system-defaults/agentic-corpus/packets/workflow-policy/arc-budget.json"
    ))
    .unwrap();

    install_artifact_value(
        &server.state,
        ArtifactInstallParams {
            kind: artifacts::ArtifactKind::Workflow,
            source: "system-defaults/agentic-corpus/workflows/schema-migration-arc.json".into(),
            name: None,
            version: None,
            supersedes: None,
        },
        workflow_value,
    )
    .await
    .unwrap();
    install_artifact_value(
        &server.state,
        ArtifactInstallParams {
            kind: artifacts::ArtifactKind::Packet,
            source: "system-defaults/agentic-corpus/packets/workflow-policy/arc-budget.json".into(),
            name: None,
            version: None,
            supersedes: None,
        },
        packet_value,
    )
    .await
    .unwrap();

    assert!(
        server
            .state
            .workflow_registry
            .read()
            .contains_key("schema-migration-arc")
    );
    assert!(
        server
            .state
            .packets
            .read()
            .load("domain:workflow-policy/arc-budget")
            .is_ok()
    );
    let rows = server
        .state
        .artifacts
        .read()
        .list(&ArtifactListParams {
            kind: None,
            name: None,
            include_superseded: false,
        })
        .unwrap();
    assert_eq!(rows.len(), 2);
}

#[tokio::test]
async fn active_workflow_artifact_restores_runtime_registry_on_boot() {
    let tmp = tempfile::tempdir().unwrap();
    let server = test_server(&tmp);
    let workflow_value: Value = serde_json::from_str(include_str!(
        "../system-defaults/agentic-corpus/workflows/schema-migration-arc.json"
    ))
    .unwrap();

    server
        .state
        .artifacts
        .write()
        .install_value(
            artifacts::ArtifactKind::Workflow,
            "system-defaults/agentic-corpus/workflows/schema-migration-arc.json".into(),
            &workflow_value,
            None,
            None,
            None,
        )
        .unwrap();

    assert!(
        !server
            .state
            .workflow_registry
            .read()
            .contains_key("schema-migration-arc"),
        "catalog-only install should not pre-populate the runtime registry"
    );
    assert!(
        !server
            .state
            .store_dir
            .join("workflows/schema-migration-arc.json")
            .exists(),
        "catalog-only install should not pre-populate the runtime workflow store"
    );

    let restored = restore_runtime_artifacts_from_catalog(&server.state).unwrap();
    assert_eq!(restored, 1);
    assert!(
        server
            .state
            .workflow_registry
            .read()
            .contains_key("schema-migration-arc"),
        "active workflow artifact must be available to orchestration after restart"
    );
    assert!(
        server
            .state
            .store_dir
            .join("workflows/schema-migration-arc.json")
            .exists(),
        "active workflow artifact must be persisted into the runtime workflow store"
    );
}

#[tokio::test]
async fn active_brofile_artifact_restores_runtime_registry_on_boot() {
    let tmp = tempfile::tempdir().unwrap();
    let server = test_server(&tmp);
    let brofile_value = serde_json::json!({
        "name": "catalog-only-reviewer",
        "version": 1,
        "provider": "claude",
        "model": "claude-opus-4-7",
        "effort": "xhigh",
        "lens": "Review without editing."
    });

    server
        .state
        .artifacts
        .write()
        .install_value(
            artifacts::ArtifactKind::Brofile,
            "inline".into(),
            &brofile_value,
            None,
            None,
            None,
        )
        .unwrap();

    assert!(
        orchestration::brofile::resolve_brofile(
            "catalog-only-reviewer",
            &server.state.store_dir,
            None,
        )
        .is_none(),
        "catalog-only install should not pre-populate the runtime brofile store"
    );

    let restored = restore_runtime_artifacts_from_catalog(&server.state).unwrap();
    assert_eq!(restored, 1);
    assert!(
        orchestration::brofile::resolve_brofile(
            "catalog-only-reviewer",
            &server.state.store_dir,
            None,
        )
        .is_some(),
        "active brofile artifact must resolve after restart reconciliation"
    );
}

#[tokio::test]
async fn active_packet_artifact_restores_runtime_registry_on_boot() {
    let tmp = tempfile::tempdir().unwrap();
    let server = test_server(&tmp);
    let packet_value: Value = serde_json::from_str(include_str!(
        "../system-defaults/agentic-corpus/packets/phase-decompose/dag-structure.json"
    ))
    .unwrap();

    server
        .state
        .artifacts
        .write()
        .install_value(
            artifacts::ArtifactKind::Packet,
            "system-defaults/agentic-corpus/packets/phase-decompose/dag-structure.json".into(),
            &packet_value,
            None,
            None,
            None,
        )
        .unwrap();

    assert!(
        server
            .state
            .packets
            .read()
            .load("domain:phase-decompose/dag-structure")
            .is_err(),
        "catalog-only install should not pre-populate the runtime packet registry"
    );

    let restored = restore_runtime_artifacts_from_catalog(&server.state).unwrap();
    assert_eq!(restored, 1);
    assert!(
        server
            .state
            .packets
            .read()
            .load("domain:phase-decompose/dag-structure")
            .is_ok(),
        "active packet artifact must compile into the runtime packet registry"
    );
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
async fn artifact_install_wires_project_bootstrap_arc() {
    let tmp = tempfile::tempdir().unwrap();
    let server = test_server(&tmp);
    let workflow_value: Value = serde_json::from_str(include_str!(
        "../system-defaults/agentic-corpus/workflows/project-bootstrap-arc.json"
    ))
    .unwrap();

    install_artifact_value(
        &server.state,
        ArtifactInstallParams {
            kind: artifacts::ArtifactKind::Workflow,
            source: "system-defaults/agentic-corpus/workflows/project-bootstrap-arc.json".into(),
            name: None,
            version: None,
            supersedes: None,
        },
        workflow_value,
    )
    .await
    .unwrap();

    assert!(
        server
            .state
            .workflow_registry
            .read()
            .contains_key("project-bootstrap-arc")
    );
    let rows = server
        .state
        .artifacts
        .read()
        .list(&ArtifactListParams {
            kind: Some(artifacts::ArtifactKind::Workflow),
            name: Some("project-bootstrap-arc".into()),
            include_superseded: false,
        })
        .unwrap();
    assert_eq!(rows.len(), 1);

    let compiled = {
        let workflow = server
            .state
            .workflow_registry
            .read()
            .get("project-bootstrap-arc")
            .cloned()
            .unwrap();
        workflow::compile(workflow).unwrap()
    };
    let mut vars = serde_json::Map::new();
    vars.insert("project_id".into(), Value::String("proj1234".into()));
    vars.insert(
        "project_path".into(),
        Value::String(tmp.path().to_string_lossy().into_owned()),
    );
    let result = workflow::run_workflow_with_initial_vars(
        &server,
        &compiled,
        Some(tmp.path().to_string_lossy().into_owned()),
        Some(50),
        vars,
    )
    .await;
    assert_eq!(result.status, "completed");
    assert_eq!(result.vars.get("published"), Some(&Value::Bool(true)));
    let arc_id = result.arc_thread_id.as_deref().unwrap_or_default();
    let snapshot = server
        .state
        .running_arcs
        .read()
        .get(arc_id)
        .cloned()
        .unwrap();
    assert_eq!(snapshot.status, "completed");
    assert!(
        snapshot
            .completed_nodes
            .iter()
            .any(|node| node == "Publish")
    );
}

#[tokio::test]
async fn artifact_install_wires_m2_compaction_arc_and_packets() {
    let tmp = tempfile::tempdir().unwrap();
    let server = test_server(&tmp);
    let workflow_value: Value = serde_json::from_str(include_str!(
        "../system-defaults/agentic-corpus/workflows/embed-compaction-arc.json"
    ))
    .unwrap();
    let policy_value: Value = serde_json::from_str(include_str!(
        "../system-defaults/agentic-corpus/packets/embed/compaction-policy.json"
    ))
    .unwrap();
    let cron_routing_value: Value = serde_json::from_str(include_str!(
        "../system-defaults/agentic-corpus/packets/cron-routing/embed-compaction.json"
    ))
    .unwrap();

    install_artifact_value(
        &server.state,
        ArtifactInstallParams {
            kind: artifacts::ArtifactKind::Workflow,
            source: "system-defaults/agentic-corpus/workflows/embed-compaction-arc.json".into(),
            name: None,
            version: None,
            supersedes: None,
        },
        workflow_value,
    )
    .await
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
        policy_value,
    )
    .await
    .unwrap();
    install_artifact_value(
        &server.state,
        ArtifactInstallParams {
            kind: artifacts::ArtifactKind::Packet,
            source: "system-defaults/agentic-corpus/packets/cron-routing/embed-compaction.json"
                .into(),
            name: None,
            version: None,
            supersedes: None,
        },
        cron_routing_value,
    )
    .await
    .unwrap();

    assert!(
        server
            .state
            .workflow_registry
            .read()
            .contains_key("embed-compaction-arc")
    );
    assert!(
        server
            .state
            .packets
            .read()
            .load("domain:embed/compaction-policy")
            .is_ok()
    );
    assert!(
        server
            .state
            .packets
            .read()
            .load("domain:cron-routing/embed-compaction")
            .is_ok()
    );
}

#[tokio::test]
async fn artifact_install_wires_daily_compaction_flow() {
    let tmp = tempfile::tempdir().unwrap();
    let server = test_server(&tmp);
    let workflow_value: Value = serde_json::from_str(include_str!(
        "../system-defaults/maintenance/workflows/daily-compaction-arc.json"
    ))
    .unwrap();
    let arc_budget_value: Value = serde_json::from_str(include_str!(
        "../system-defaults/agentic-corpus/packets/workflow-policy/arc-budget.json"
    ))
    .unwrap();
    let embed_policy_value: Value = serde_json::from_str(include_str!(
        "../system-defaults/agentic-corpus/packets/embed/compaction-policy.json"
    ))
    .unwrap();
    let cron_routing_value: Value = serde_json::from_str(include_str!(
        "../system-defaults/maintenance/packets/cron-routing/daily-compaction.json"
    ))
    .unwrap();
    let cron_value: Value = serde_json::from_str(include_str!(
        "../system-defaults/maintenance/crons/daily-compaction.json"
    ))
    .unwrap();

    install_artifact_value(
        &server.state,
        ArtifactInstallParams {
            kind: artifacts::ArtifactKind::Workflow,
            source: "system-defaults/maintenance/workflows/daily-compaction-arc.json".into(),
            name: None,
            version: None,
            supersedes: None,
        },
        workflow_value,
    )
    .await
    .unwrap();
    install_artifact_value(
        &server.state,
        ArtifactInstallParams {
            kind: artifacts::ArtifactKind::Packet,
            source: "system-defaults/agentic-corpus/packets/workflow-policy/arc-budget.json".into(),
            name: None,
            version: None,
            supersedes: None,
        },
        arc_budget_value,
    )
    .await
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
        embed_policy_value,
    )
    .await
    .unwrap();
    install_artifact_value(
        &server.state,
        ArtifactInstallParams {
            kind: artifacts::ArtifactKind::Packet,
            source: "system-defaults/maintenance/packets/cron-routing/daily-compaction.json".into(),
            name: None,
            version: None,
            supersedes: None,
        },
        cron_routing_value,
    )
    .await
    .unwrap();
    install_artifact_value(
        &server.state,
        ArtifactInstallParams {
            kind: artifacts::ArtifactKind::Cron,
            source: "system-defaults/maintenance/crons/daily-compaction.json".into(),
            name: None,
            version: None,
            supersedes: None,
        },
        cron_value,
    )
    .await
    .unwrap();

    assert!(
        server
            .state
            .workflow_registry
            .read()
            .contains_key("daily-compaction-arc")
    );
    assert!(
        server
            .state
            .packets
            .read()
            .load("domain:cron-routing/daily-compaction")
            .is_ok()
    );
    assert!(
        server
            .state
            .crons
            .list()
            .iter()
            .any(|spec| spec.name == "daily-compaction")
    );
    let rows = server
        .state
        .artifacts
        .read()
        .list(&ArtifactListParams {
            kind: Some(artifacts::ArtifactKind::Cron),
            name: Some("daily-compaction".into()),
            include_superseded: false,
        })
        .unwrap();
    assert_eq!(rows.len(), 1);
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
async fn artifact_install_wires_m3_auto_digest_artifacts_and_audit() {
    let tmp = tempfile::tempdir().unwrap();
    let server = test_server(&tmp);
    let brofile_value: Value = serde_json::from_str(include_str!(
        "../system-defaults/agentic-corpus/brofiles/digest-extractor.json"
    ))
    .unwrap();
    assert_eq!(
        brofile_value["disallow_tools"],
        serde_json::json!(["Edit", "Write", "Bash"])
    );
    let trust_value: Value = serde_json::from_str(include_str!(
        "../system-defaults/agentic-corpus/packets/bro-trust/per-brofile.json"
    ))
    .unwrap();
    let quality_value: Value = serde_json::from_str(include_str!(
        "../system-defaults/agentic-corpus/packets/auto-digest/entry-quality.json"
    ))
    .unwrap();
    let routing_value: Value = serde_json::from_str(include_str!(
        "../system-defaults/agentic-corpus/packets/auto-digest/task-completed-routing.json"
    ))
    .unwrap();
    let workflow_value: Value = serde_json::from_str(include_str!(
        "../system-defaults/agentic-corpus/workflows/auto-digest-arc.json"
    ))
    .unwrap();

    install_artifact_value(
        &server.state,
        ArtifactInstallParams {
            kind: artifacts::ArtifactKind::Brofile,
            source: "system-defaults/agentic-corpus/brofiles/digest-extractor.json".into(),
            name: None,
            version: None,
            supersedes: None,
        },
        brofile_value,
    )
    .await
    .unwrap();
    install_artifact_value(
        &server.state,
        ArtifactInstallParams {
            kind: artifacts::ArtifactKind::Packet,
            source: "system-defaults/agentic-corpus/packets/bro-trust/per-brofile.json".into(),
            name: None,
            version: None,
            supersedes: None,
        },
        trust_value,
    )
    .await
    .unwrap();
    install_artifact_value(
        &server.state,
        ArtifactInstallParams {
            kind: artifacts::ArtifactKind::Packet,
            source: "system-defaults/agentic-corpus/packets/auto-digest/entry-quality.json".into(),
            name: None,
            version: None,
            supersedes: None,
        },
        quality_value,
    )
    .await
    .unwrap();
    install_artifact_value(
        &server.state,
        ArtifactInstallParams {
            kind: artifacts::ArtifactKind::Packet,
            source:
                "system-defaults/agentic-corpus/packets/auto-digest/task-completed-routing.json"
                    .into(),
            name: None,
            version: None,
            supersedes: None,
        },
        routing_value,
    )
    .await
    .unwrap();
    install_artifact_value(
        &server.state,
        ArtifactInstallParams {
            kind: artifacts::ArtifactKind::Workflow,
            source: "system-defaults/agentic-corpus/workflows/auto-digest-arc.json".into(),
            name: None,
            version: None,
            supersedes: None,
        },
        workflow_value,
    )
    .await
    .unwrap();

    assert!(
        server
            .state
            .workflow_registry
            .read()
            .contains_key("auto-digest-arc")
    );
    assert!(
        server
            .state
            .packets
            .read()
            .load("domain:auto-digest/entry-quality")
            .is_ok()
    );
    assert!(
        server
            .state
            .packets
            .read()
            .load("domain:auto-digest/task-completed-routing")
            .is_ok()
    );
    assert!(
        orchestration::brofile::resolve_brofile("digest-extractor", &server.state.store_dir, None)
            .is_some()
    );

    let cases: Value =
        serde_json::from_str(include_str!("../eval/audit/auto-digest/cases.json")).unwrap();
    let cases = cases.as_array().unwrap();
    let packet_store = server.state.packets.read();
    let packet = packet_store
        .load("domain:auto-digest/entry-quality")
        .unwrap();
    let mut matched = 0usize;
    for case in cases {
        let entity = serde_json::json!({
            "vars": {
                "candidate": case["proposal"].clone()
            }
        });
        let prediction = packets::apply_with(&packet, &entity, &*packet_store)
            .unwrap_or_else(|| panic!("case {} produced no verdict", case["id"]));
        if prediction.classification == case["expected_verdict"].as_str().unwrap() {
            matched += 1;
        }
    }
    assert!(
        matched >= 18,
        "auto-digest audit fidelity {matched}/{} below gate",
        cases.len()
    );
    assert_eq!(matched, cases.len());
}

#[tokio::test]
async fn artifact_install_wires_m4_contradiction_review_artifacts() {
    let tmp = tempfile::tempdir().unwrap();
    let server = test_server(&tmp);
    let workflow_value: Value = serde_json::from_str(include_str!(
        "../system-defaults/agentic-corpus/workflows/contradiction-review-arc.json"
    ))
    .unwrap();
    let packet_value: Value = serde_json::from_str(include_str!(
        "../system-defaults/agentic-corpus/packets/contradiction/review-synthesis.json"
    ))
    .unwrap();
    let brofiles: [(&str, Value); 4] = [
        (
            "contradiction-provenance",
            serde_json::from_str(include_str!(
                "../system-defaults/agentic-corpus/brofiles/contradiction-provenance.json"
            ))
            .unwrap(),
        ),
        (
            "contradiction-lifecycle",
            serde_json::from_str(include_str!(
                "../system-defaults/agentic-corpus/brofiles/contradiction-lifecycle.json"
            ))
            .unwrap(),
        ),
        (
            "contradiction-coherence",
            serde_json::from_str(include_str!(
                "../system-defaults/agentic-corpus/brofiles/contradiction-coherence.json"
            ))
            .unwrap(),
        ),
        (
            "contradiction-facilitator",
            serde_json::from_str(include_str!(
                "../system-defaults/agentic-corpus/brofiles/contradiction-facilitator.json"
            ))
            .unwrap(),
        ),
    ];

    install_artifact_value(
        &server.state,
        ArtifactInstallParams {
            kind: artifacts::ArtifactKind::Packet,
            source: "system-defaults/agentic-corpus/packets/contradiction/review-synthesis.json"
                .into(),
            name: None,
            version: None,
            supersedes: None,
        },
        packet_value,
    )
    .await
    .unwrap();
    for (name, value) in brofiles {
        install_artifact_value(
            &server.state,
            ArtifactInstallParams {
                kind: artifacts::ArtifactKind::Brofile,
                source: format!("system-defaults/agentic-corpus/brofiles/{name}.json"),
                name: None,
                version: None,
                supersedes: None,
            },
            value,
        )
        .await
        .unwrap();
    }
    install_artifact_value(
        &server.state,
        ArtifactInstallParams {
            kind: artifacts::ArtifactKind::Workflow,
            source: "system-defaults/agentic-corpus/workflows/contradiction-review-arc.json".into(),
            name: None,
            version: None,
            supersedes: None,
        },
        workflow_value,
    )
    .await
    .unwrap();

    assert!(
        server
            .state
            .workflow_registry
            .read()
            .contains_key("contradiction-review-arc")
    );
    let packet_store = server.state.packets.read();
    let packet = packet_store
        .load("domain:contradiction/review-synthesis")
        .unwrap();
    let prediction = packets::apply_with(
        &packet,
        &json!({"vars": {"verdict": {"verdict": "contradicts"}}}),
        &*packet_store,
    )
    .unwrap();
    assert_eq!(prediction.classification, "contradicts");
    assert!(
        orchestration::brofile::resolve_brofile(
            "contradiction-facilitator",
            &server.state.store_dir,
            None
        )
        .is_some()
    );
}

#[tokio::test]
async fn artifact_install_wires_m5_auto_edge_artifacts_and_audit() {
    let tmp = tempfile::tempdir().unwrap();
    let server = test_server(&tmp);
    let packet_value: Value = serde_json::from_str(include_str!(
        "../system-defaults/agentic-corpus/packets/auto-edge/vote-aggregate.json"
    ))
    .unwrap();
    let workflow_value: Value = serde_json::from_str(include_str!(
        "../system-defaults/agentic-corpus/workflows/auto-edge-arc.json"
    ))
    .unwrap();
    let brofiles: [(&str, Value); 6] = [
        (
            "describe-prose-signal",
            serde_json::from_str(include_str!(
                "../system-defaults/agentic-corpus/brofiles/describe-prose-signal.json"
            ))
            .unwrap(),
        ),
        (
            "describe-symbol-fit",
            serde_json::from_str(include_str!(
                "../system-defaults/agentic-corpus/brofiles/describe-symbol-fit.json"
            ))
            .unwrap(),
        ),
        (
            "describe-narrative-cohesion",
            serde_json::from_str(include_str!(
                "../system-defaults/agentic-corpus/brofiles/describe-narrative-cohesion.json"
            ))
            .unwrap(),
        ),
        (
            "reference-citation-precision",
            serde_json::from_str(include_str!(
                "../system-defaults/agentic-corpus/brofiles/reference-citation-precision.json"
            ))
            .unwrap(),
        ),
        (
            "reference-target-existence",
            serde_json::from_str(include_str!(
                "../system-defaults/agentic-corpus/brofiles/reference-target-existence.json"
            ))
            .unwrap(),
        ),
        (
            "reference-context-fit",
            serde_json::from_str(include_str!(
                "../system-defaults/agentic-corpus/brofiles/reference-context-fit.json"
            ))
            .unwrap(),
        ),
    ];

    install_artifact_value(
        &server.state,
        ArtifactInstallParams {
            kind: artifacts::ArtifactKind::Packet,
            source: "system-defaults/agentic-corpus/packets/auto-edge/vote-aggregate.json".into(),
            name: None,
            version: None,
            supersedes: None,
        },
        packet_value,
    )
    .await
    .unwrap();
    for (name, value) in brofiles {
        install_artifact_value(
            &server.state,
            ArtifactInstallParams {
                kind: artifacts::ArtifactKind::Brofile,
                source: format!("system-defaults/agentic-corpus/brofiles/{name}.json"),
                name: None,
                version: None,
                supersedes: None,
            },
            value,
        )
        .await
        .unwrap();
        assert!(
            orchestration::brofile::resolve_brofile(name, &server.state.store_dir, None).is_some()
        );
    }
    install_artifact_value(
        &server.state,
        ArtifactInstallParams {
            kind: artifacts::ArtifactKind::Workflow,
            source: "system-defaults/agentic-corpus/workflows/auto-edge-arc.json".into(),
            name: None,
            version: None,
            supersedes: None,
        },
        workflow_value,
    )
    .await
    .unwrap();
    assert!(
        server
            .state
            .workflow_registry
            .read()
            .contains_key("auto-edge-arc")
    );

    let packet_store = server.state.packets.read();
    let packet = packet_store
        .load("domain:auto-edge/vote-aggregate")
        .unwrap();
    for cases in [
        serde_json::from_str::<Value>(include_str!("../eval/audit/auto-edge/describes.json"))
            .unwrap(),
        serde_json::from_str::<Value>(include_str!("../eval/audit/auto-edge/references.json"))
            .unwrap(),
    ] {
        let rows = cases.as_array().unwrap();
        let mut matched = 0usize;
        for case in rows {
            let prediction = packets::apply_with(&packet, &case["entity"], &*packet_store)
                .unwrap_or_else(|| panic!("case {} produced no verdict", case["id"]));
            if prediction.classification == case["expected"].as_str().unwrap() {
                matched += 1;
            }
        }
        assert!(
            matched >= 12,
            "auto-edge audit fidelity {matched}/{} below gate",
            rows.len()
        );
        assert_eq!(matched, rows.len());
    }
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
async fn shipped_packet_audit_examples_pass() {
    let tmp = tempfile::tempdir().unwrap();
    let server = test_server(&tmp);
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let packets = [
        "system-defaults/agentic-corpus/packets/workflow-policy/arc-budget.json",
        "system-defaults/agentic-corpus/packets/embed/compaction-policy.json",
        "system-defaults/agentic-corpus/packets/cron-routing/embed-compaction.json",
        "system-defaults/agentic-corpus/packets/bro-trust/per-brofile.json",
        "system-defaults/agentic-corpus/packets/auto-digest/task-completed-routing.json",
        "system-defaults/agentic-corpus/packets/auto-digest/entry-quality.json",
        "system-defaults/agentic-corpus/packets/contradiction/review-synthesis.json",
        "system-defaults/agentic-corpus/packets/auto-edge/vote-aggregate.json",
        "system-defaults/agentic-corpus/packets/eval/drift-policy.json",
    ];
    for rel in packets {
        let path = root.join(rel);
        let value: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        install_artifact_value(
            &server.state,
            ArtifactInstallParams {
                kind: artifacts::ArtifactKind::Packet,
                source: rel.into(),
                name: None,
                version: None,
                supersedes: None,
            },
            value,
        )
        .await
        .unwrap();
    }

    let audits = [
        "system-defaults/agentic-corpus/packets/workflow-policy/arc-budget.audit_examples.json",
        "system-defaults/agentic-corpus/packets/embed/compaction-policy.audit_examples.json",
        "system-defaults/agentic-corpus/packets/cron-routing/embed-compaction.audit_examples.json",
        "system-defaults/agentic-corpus/packets/bro-trust/per-brofile.audit_examples.json",
        "system-defaults/agentic-corpus/packets/auto-digest/task-completed-routing.audit_examples.json",
        "system-defaults/agentic-corpus/packets/auto-digest/entry-quality.audit_examples.json",
        "system-defaults/agentic-corpus/packets/contradiction/review-synthesis.audit_examples.json",
        "system-defaults/agentic-corpus/packets/auto-edge/vote-aggregate.audit_examples.json",
        "system-defaults/agentic-corpus/packets/eval/drift-policy.audit_examples.json",
    ];
    let packet_store = server.state.packets.read();
    for rel in audits {
        let spec: Value =
            serde_json::from_str(&std::fs::read_to_string(root.join(rel)).unwrap()).unwrap();
        let rendered = packet_store
            .audit_tool(&packets::AuditParams {
                packet_id: spec["packet_id"].as_str().unwrap().into(),
                dataset: spec["dataset"].clone(),
                mode: None,
            })
            .unwrap();
        let report: Value = serde_json::from_str(&rendered).unwrap();
        assert_eq!(
            report["fidelity"].as_f64().unwrap(),
            1.0,
            "audit examples failed for {rel}: {rendered}"
        );
    }
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

#[tokio::test]
async fn artifact_supersession_deactivates_workflow_registry_entry() {
    let tmp = tempfile::tempdir().unwrap();
    let server = test_server(&tmp);
    let workflow_a = serde_json::json!({
        "name": "workflow-a",
        "version": 1,
        "actors": {},
        "start": "Done",
        "nodes": {"Done": {"actor": "", "next": {"type": "terminal"}}}
    });
    let workflow_a2 = serde_json::json!({
        "name": "workflow-a2",
        "version": 2,
        "supersedes": "workflow-a",
        "actors": {},
        "start": "Done",
        "nodes": {"Done": {"actor": "", "next": {"type": "terminal"}}}
    });

    install_artifact_value(
        &server.state,
        ArtifactInstallParams {
            kind: artifacts::ArtifactKind::Workflow,
            source: "workflow-a.json".into(),
            name: None,
            version: None,
            supersedes: None,
        },
        workflow_a,
    )
    .await
    .unwrap();
    assert!(
        server
            .state
            .workflow_registry
            .read()
            .contains_key("workflow-a")
    );

    install_artifact_value(
        &server.state,
        ArtifactInstallParams {
            kind: artifacts::ArtifactKind::Workflow,
            source: "workflow-a2.json".into(),
            name: None,
            version: None,
            supersedes: None,
        },
        workflow_a2,
    )
    .await
    .unwrap();

    assert!(
        !server
            .state
            .workflow_registry
            .read()
            .contains_key("workflow-a")
    );
    assert!(
        server
            .state
            .workflow_registry
            .read()
            .contains_key("workflow-a2")
    );
    assert!(
        !server
            .state
            .store_dir
            .join("workflows")
            .join("workflow-a.json")
            .exists()
    );
}

#[tokio::test]
async fn artifact_same_name_workflow_upgrade_keeps_runtime_registry_entry() {
    let tmp = tempfile::tempdir().unwrap();
    let server = test_server(&tmp);
    let workflow_v1 = serde_json::json!({
        "name": "workflow-a",
        "version": 1,
        "actors": {},
        "start": "Done",
        "nodes": {"Done": {"actor": "", "next": {"type": "terminal"}}}
    });
    let workflow_v2 = serde_json::json!({
        "name": "workflow-a",
        "version": 2,
        "actors": {},
        "start": "Done",
        "nodes": {"Done": {"actor": "", "next": {"type": "terminal"}}}
    });

    install_artifact_value(
        &server.state,
        ArtifactInstallParams {
            kind: artifacts::ArtifactKind::Workflow,
            source: "workflow-a.json".into(),
            name: Some("workflow-a".into()),
            version: Some("1".into()),
            supersedes: None,
        },
        workflow_v1,
    )
    .await
    .unwrap();

    install_artifact_value(
        &server.state,
        ArtifactInstallParams {
            kind: artifacts::ArtifactKind::Workflow,
            source: "workflow-a.json".into(),
            name: Some("workflow-a".into()),
            version: Some("2".into()),
            supersedes: Some("workflow-a".into()),
        },
        workflow_v2,
    )
    .await
    .unwrap();

    assert!(
        server
            .state
            .workflow_registry
            .read()
            .contains_key("workflow-a")
    );
    assert!(
        server
            .state
            .store_dir
            .join("workflows")
            .join("workflow-a.json")
            .exists()
    );
}

#[tokio::test]
async fn agent_artifact_install_list_supersede_round_trip() {
    let tmp = tempfile::tempdir().unwrap();
    let server = test_server(&tmp);
    let agent_v1 = serde_json::json!({
        "kind": "agent",
        "name": "test-reviewer",
        "version": 1,
        "manifest": {
            "description": "Reviews code for correctness.",
            "when_to_use": ["after writing code"],
            "brofile_inline": {"provider": "claude", "lens": "reviewer"}
        }
    });
    let agent_v2 = serde_json::json!({
        "kind": "agent",
        "name": "test-reviewer-v2",
        "version": 2,
        "supersedes": "test-reviewer",
        "manifest": {
            "description": "Reviews code with style checks.",
            "when_to_use": ["after writing code"],
            "brofile_inline": {"provider": "claude", "lens": "reviewer"}
        }
    });

    let meta1 = install_artifact_value(
        &server.state,
        ArtifactInstallParams {
            kind: artifacts::ArtifactKind::Agent,
            source: "agent-v1.json".into(),
            name: None,
            version: None,
            supersedes: None,
        },
        agent_v1,
    )
    .await
    .unwrap();
    assert!(meta1.active);

    let meta2 = install_artifact_value(
        &server.state,
        ArtifactInstallParams {
            kind: artifacts::ArtifactKind::Agent,
            source: "agent-v2.json".into(),
            name: None,
            version: None,
            supersedes: None,
        },
        agent_v2,
    )
    .await
    .unwrap();
    assert!(meta2.active);
    assert_eq!(meta2.supersedes_chain, vec!["test-reviewer"]);

    let rows = server
        .state
        .artifacts
        .read()
        .list(&ArtifactListParams {
            kind: Some(artifacts::ArtifactKind::Agent),
            name: None,
            include_superseded: false,
        })
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].name, "test-reviewer-v2");

    let all_rows = server
        .state
        .artifacts
        .read()
        .list(&ArtifactListParams {
            kind: Some(artifacts::ArtifactKind::Agent),
            name: None,
            include_superseded: true,
        })
        .unwrap();
    assert_eq!(all_rows.len(), 2);
    let old = all_rows.iter().find(|r| r.name == "test-reviewer").unwrap();
    assert!(!old.active);
    assert_eq!(old.superseded_by.as_deref(), Some("test-reviewer-v2"));

    let rows_all = server
        .state
        .artifacts
        .read()
        .list(&ArtifactListParams {
            kind: None,
            name: None,
            include_superseded: true,
        })
        .unwrap();
    assert_eq!(rows_all.len(), 2);
}

fn deterministic_echo_atom(name: &str) -> serde_json::Value {
    serde_json::json!({
        "_contract": "atom/v1",
        "kind": "atom",
        "name": name,
        "version": 1,
        "manifest": {
            "description": "Echo deterministic atom for runtime tests.",
            "when_to_use": ["when testing deterministic atom invocation"],
            "inputs": {
                "schema": {
                    "type": "object",
                    "additionalProperties": true
                }
            },
            "outputs": {
                "schema": {
                    "type": "object",
                    "required": ["echo"],
                    "properties": {
                        "echo": {}
                    }
                }
            },
            "effects": {
                "writes_files": false,
                "dispatches_runs": 0,
                "max_depth": 0,
                "uses_network": false
            },
            "composition": {
                "may_invoke_atoms": {"kind": "none"}
            },
            "implementation": {
                "kind": "deterministic",
                "runner": "echo"
            }
        }
    })
}

fn badgey_adapter_atom(name: &str) -> serde_json::Value {
    serde_json::json!({
        "_contract": "atom/v1",
        "kind": "atom",
        "name": name,
        "version": 1,
        "manifest": {
            "description": "Badgey adapter atom for runtime tests.",
            "when_to_use": ["when testing adapter atom invocation"],
            "inputs": {
                "schema": {"type": "object", "additionalProperties": true}
            },
            "outputs": {
                "schema": {
                    "type": "object",
                    "required": ["adapter", "accepted"],
                    "properties": {
                        "adapter": {"const": "badgey"},
                        "accepted": {"const": true}
                    }
                }
            },
            "effects": {
                "writes_files": false,
                "dispatches_runs": 0,
                "max_depth": 0,
                "uses_network": false
            },
            "composition": {
                "may_invoke_atoms": {"kind": "none"}
            },
            "implementation": {
                "kind": "adapter",
                "adapter_name": "badgey"
            }
        }
    })
}

fn workflow_wrapper_atom(name: &str, workflow_ref: &str) -> serde_json::Value {
    serde_json::json!({
        "_contract": "atom/v1",
        "kind": "atom",
        "name": name,
        "version": 1,
        "manifest": {
            "description": "Workflow-backed atom for runtime tests.",
            "when_to_use": ["when testing workflow atom invocation"],
            "inputs": {
                "schema": {"type": "object", "additionalProperties": true}
            },
            "effects": {
                "writes_files": false,
                "dispatches_runs": 1,
                "max_depth": 0,
                "uses_network": false
            },
            "composition": {
                "may_invoke_atoms": {"kind": "none"}
            },
            "implementation": {
                "kind": "workflow",
                "workflow_ref": workflow_ref
            }
        }
    })
}

#[tokio::test]
async fn atom_invoke_deterministic_runner_returns_terminal_trace() {
    let tmp = tempfile::tempdir().unwrap();
    let server = test_server(&tmp);
    install_artifact_value(
        &server.state,
        ArtifactInstallParams {
            kind: artifacts::ArtifactKind::Atom,
            source: "echo-atom.json".into(),
            name: None,
            version: None,
            supersedes: None,
        },
        deterministic_echo_atom("echo-atom"),
    )
    .await
    .unwrap();

    let invoke = server
        .atom_invoke(Parameters(AtomInvokeParams {
            atom: "atom:echo-atom@v1".into(),
            args: serde_json::json!({"message": "hello"}),
            project_dir: None,
            owner: Some("operator:test".into()),
            parent_invocation_id: None,
            runtime: None,
            supervision_override: None,
            suppress_auto_supervision: false,
        }))
        .await;
    assert_ne!(invoke.is_error, Some(true), "{}", extract_text(&invoke));
    let body: serde_json::Value = serde_json::from_str(&extract_text(&invoke)).unwrap();
    assert_eq!(body["status"], "succeeded");
    assert_eq!(body["data"]["echo"]["message"], "hello");
    assert_eq!(body["output_shape"]["valid"], true);

    let status = server.atom_status(Parameters(AtomStatusParams {
        invocation_id: body["invocation_id"].as_str().unwrap().to_string(),
        owner: Some("operator:test".into()),
    }));
    assert_ne!(status.is_error, Some(true), "{}", extract_text(&status));
    let trace: serde_json::Value = serde_json::from_str(&extract_text(&status)).unwrap();
    assert_eq!(trace["implementation_kind"], "deterministic");
    assert_eq!(trace["state"], "succeeded");
    assert_eq!(trace["effects_observed"]["dispatches_runs"], 0);
    assert_eq!(trace["output_shape"]["valid"], true);
}

#[tokio::test]
async fn atom_invoke_adapter_runner_returns_terminal_trace() {
    let tmp = tempfile::tempdir().unwrap();
    let server = test_server(&tmp);
    install_artifact_value(
        &server.state,
        ArtifactInstallParams {
            kind: artifacts::ArtifactKind::Atom,
            source: "badgey-adapter.json".into(),
            name: None,
            version: None,
            supersedes: None,
        },
        badgey_adapter_atom("badgey-adapter"),
    )
    .await
    .unwrap();

    let invoke = server
        .atom_invoke(Parameters(AtomInvokeParams {
            atom: "atom:badgey-adapter@v1".into(),
            args: serde_json::json!({"brief": "hello badgey"}),
            project_dir: None,
            owner: Some("operator:test".into()),
            parent_invocation_id: None,
            runtime: None,
            supervision_override: None,
            suppress_auto_supervision: false,
        }))
        .await;
    assert_ne!(invoke.is_error, Some(true), "{}", extract_text(&invoke));
    let body: serde_json::Value = serde_json::from_str(&extract_text(&invoke)).unwrap();
    assert_eq!(body["status"], "succeeded");
    assert_eq!(body["data"]["adapter"], "badgey");
    assert_eq!(body["data"]["accepted"], true);
    assert_eq!(body["output_shape"]["valid"], true);
}

#[tokio::test]
async fn shipped_refactor_atom_installs_after_persona_brofile() {
    let tmp = tempfile::tempdir().unwrap();
    let server = test_server(&tmp);
    let brofile: serde_json::Value = serde_json::from_str(include_str!(
        "../system-defaults/brofiles/refactor/rust-refactor-persona.json"
    ))
    .unwrap();
    let atom: serde_json::Value = serde_json::from_str(include_str!(
        "../system-defaults/atoms/refactor/rust-test-island-extract.json"
    ))
    .unwrap();

    install_artifact_value(
        &server.state,
        ArtifactInstallParams {
            kind: artifacts::ArtifactKind::Brofile,
            source: "system-defaults/brofiles/refactor/rust-refactor-persona.json".into(),
            name: None,
            version: None,
            supersedes: None,
        },
        brofile,
    )
    .await
    .unwrap();
    let meta = install_artifact_value(
        &server.state,
        ArtifactInstallParams {
            kind: artifacts::ArtifactKind::Atom,
            source: "system-defaults/atoms/refactor/rust-test-island-extract.json".into(),
            name: None,
            version: None,
            supersedes: None,
        },
        atom,
    )
    .await
    .unwrap();

    assert_eq!(meta.kind, artifacts::ArtifactKind::Atom);
    assert_eq!(meta.name, "rust-test-island-extract");
    assert!(meta.active);
}

#[tokio::test]
async fn shipped_rust_batch2_atoms_install_after_persona_brofile() {
    let tmp = tempfile::tempdir().unwrap();
    let server = test_server(&tmp);
    let brofile: serde_json::Value = serde_json::from_str(include_str!(
        "../system-defaults/brofiles/refactor/rust-refactor-persona.json"
    ))
    .unwrap();

    install_artifact_value(
        &server.state,
        ArtifactInstallParams {
            kind: artifacts::ArtifactKind::Brofile,
            source: "system-defaults/brofiles/refactor/rust-refactor-persona.json".into(),
            name: None,
            version: None,
            supersedes: None,
        },
        brofile,
    )
    .await
    .unwrap();

    let atoms = [
        (
            "system-defaults/atoms/refactor/rust-rename-symbol.json",
            "rust-rename-symbol",
            include_str!("../system-defaults/atoms/refactor/rust-rename-symbol.json"),
        ),
        (
            "system-defaults/atoms/refactor/rust-extract-to-submodule.json",
            "rust-extract-to-submodule",
            include_str!("../system-defaults/atoms/refactor/rust-extract-to-submodule.json"),
        ),
        (
            "system-defaults/atoms/refactor/rust-organize-imports.json",
            "rust-organize-imports",
            include_str!("../system-defaults/atoms/refactor/rust-organize-imports.json"),
        ),
        (
            "system-defaults/atoms/refactor/rust-cargo-add-dep.json",
            "rust-cargo-add-dep",
            include_str!("../system-defaults/atoms/refactor/rust-cargo-add-dep.json"),
        ),
    ];

    for (source, expected_name, body) in atoms {
        let atom: serde_json::Value = serde_json::from_str(body).unwrap();
        let meta = install_artifact_value(
            &server.state,
            ArtifactInstallParams {
                kind: artifacts::ArtifactKind::Atom,
                source: source.into(),
                name: None,
                version: None,
                supersedes: None,
            },
            atom,
        )
        .await
        .unwrap();

        assert_eq!(meta.kind, artifacts::ArtifactKind::Atom);
        assert_eq!(meta.name, expected_name);
        assert!(meta.active);
    }
}

#[tokio::test]
async fn atom_invoke_workflow_wrapper_returns_workflow_handle() {
    let tmp = tempfile::tempdir().unwrap();
    let server = test_server(&tmp);
    let workflow_json = r#"{
        "name": "hook-workflow",
        "version": 1,
        "actors": {},
        "nodes": {
            "Done": {
                "prompt": "workflow complete",
                "next": {"type": "terminal"}
            }
        },
        "start": "Done"
    }"#;
    let workflow_spec = workflow::load_workflow(workflow_json).unwrap();
    server
        .state
        .workflow_registry
        .write()
        .insert("hook-workflow".into(), workflow_spec);
    install_artifact_value(
        &server.state,
        ArtifactInstallParams {
            kind: artifacts::ArtifactKind::Atom,
            source: "workflow-wrapper.json".into(),
            name: None,
            version: None,
            supersedes: None,
        },
        workflow_wrapper_atom("workflow-wrapper", "workflow:hook-workflow@v1"),
    )
    .await
    .unwrap();

    let invoke = server
        .atom_invoke(Parameters(AtomInvokeParams {
            atom: "atom:workflow-wrapper@v1".into(),
            args: serde_json::json!({}),
            project_dir: None,
            owner: Some("operator:test".into()),
            parent_invocation_id: None,
            runtime: None,
            supervision_override: None,
            suppress_auto_supervision: false,
        }))
        .await;
    assert_ne!(invoke.is_error, Some(true), "{}", extract_text(&invoke));
    let body: serde_json::Value = serde_json::from_str(&extract_text(&invoke)).unwrap();
    let task_id = body["task_id"].as_str().unwrap().to_string();
    let task = server.state.task_store.read().get(&task_id).unwrap();
    assert!(orchestration::wait_for_task_with_timeout(&task, Some(2.0)).await);

    let status = server.atom_status(Parameters(AtomStatusParams {
        invocation_id: body["invocation_id"].as_str().unwrap().to_string(),
        owner: Some("operator:test".into()),
    }));
    assert_ne!(status.is_error, Some(true), "{}", extract_text(&status));
    let trace: serde_json::Value = serde_json::from_str(&extract_text(&status)).unwrap();
    assert_eq!(trace["implementation_kind"], "workflow");
    assert_eq!(trace["state"], "succeeded");
    assert_eq!(trace["cost"]["dispatched_runs"], 1);
}

#[tokio::test]
async fn workflow_atom_rejects_underdeclared_raw_actor_dispatch_budget() {
    let tmp = tempfile::tempdir().unwrap();
    let server = test_server(&tmp);
    let workflow_json = r#"{
        "name": "actor-workflow",
        "version": 1,
        "actors": {
            "worker": {"kind": "executor", "brofile": "missing-worker"}
        },
        "nodes": {
            "Work": {
                "actor": "worker",
                "next": {"type": "terminal"}
            }
        },
        "start": "Work"
    }"#;
    server.state.workflow_registry.write().insert(
        "actor-workflow".into(),
        workflow::load_workflow(workflow_json).unwrap(),
    );
    install_artifact_value(
        &server.state,
        ArtifactInstallParams {
            kind: artifacts::ArtifactKind::Atom,
            source: "underdeclared-workflow.json".into(),
            name: None,
            version: None,
            supersedes: None,
        },
        workflow_wrapper_atom("underdeclared-workflow", "workflow:actor-workflow@v1"),
    )
    .await
    .unwrap();

    let invoke = server
        .atom_invoke(Parameters(AtomInvokeParams {
            atom: "atom:underdeclared-workflow@v1".into(),
            args: serde_json::json!({}),
            project_dir: None,
            owner: Some("operator:test".into()),
            parent_invocation_id: None,
            runtime: None,
            supervision_override: None,
            suppress_auto_supervision: false,
        }))
        .await;
    assert_eq!(invoke.is_error, Some(true));
    assert!(extract_text(&invoke).contains("dispatches_runs_exhausted"));
}

#[tokio::test]
async fn atom_binding_workflow_invokes_deterministic_atom() {
    let tmp = tempfile::tempdir().unwrap();
    let server = test_server(&tmp);
    install_artifact_value(
        &server.state,
        ArtifactInstallParams {
            kind: artifacts::ArtifactKind::Atom,
            source: "echo-atom.json".into(),
            name: None,
            version: None,
            supersedes: None,
        },
        deterministic_echo_atom("workflow-echo"),
    )
    .await
    .unwrap();

    let workflow_json = r#"{
        "name": "workflow-atom-binding-runtime",
        "version": 1,
        "actors": {},
        "vars_schema": {
            "message": {"kind": "string"}
        },
        "atom_bindings": {
            "echo": {
                "atom_ref": "atom:workflow-echo@v1",
                "limits": {"dispatches_runs": 0}
            }
        },
        "nodes": {
            "Echo": {
                "atom": "echo",
                "atom_args": {"message": "${vars.message}"},
                "next": {"type": "terminal"}
            }
        },
        "start": "Echo"
    }"#;
    let spec = workflow::load_workflow(workflow_json).unwrap();
    let compiled = workflow::compile(spec).unwrap();
    validate_workflow_capabilities(&compiled, &server.state).unwrap();
    let result = workflow::run_workflow_with_initial_vars(
        &server,
        &compiled,
        None,
        Some(10),
        serde_json::Map::from_iter([(
            "message".to_string(),
            serde_json::Value::String("from workflow".into()),
        )]),
    )
    .await;
    assert_eq!(result.status, "completed");
    let output: serde_json::Value = serde_json::from_str(&result.node_outputs["Echo"]).unwrap();
    assert_eq!(output["implementation_kind"], "deterministic");
    assert_eq!(output["state"], "succeeded");
}

#[tokio::test]
async fn atom_install_rejects_unknown_deterministic_runner() {
    let tmp = tempfile::tempdir().unwrap();
    let server = test_server(&tmp);
    let mut atom = deterministic_echo_atom("bad-runner");
    atom["manifest"]["implementation"]["runner"] = serde_json::json!("missing-runner");
    let result = install_artifact_value(
        &server.state,
        ArtifactInstallParams {
            kind: artifacts::ArtifactKind::Atom,
            source: "bad-runner.json".into(),
            name: None,
            version: None,
            supersedes: None,
        },
        atom,
    )
    .await;
    assert!(result.is_err());
    assert!(
        result
            .unwrap_err()
            .to_string()
            .contains("unknown deterministic")
    );
}

#[tokio::test]
async fn agent_artifact_rejects_non_object() {
    let tmp = tempfile::tempdir().unwrap();
    let server = test_server(&tmp);
    let result = install_artifact_value(
        &server.state,
        ArtifactInstallParams {
            kind: artifacts::ArtifactKind::Agent,
            source: "bad.json".into(),
            name: None,
            version: None,
            supersedes: None,
        },
        serde_json::json!("not an object"),
    )
    .await;
    assert!(result.is_err());
    let err = result.unwrap_err().to_string();
    assert!(
        err.contains("JSON object"),
        "expected 'JSON object' in error, got: {err}"
    );
}

fn extract_text(result: &CallToolResult) -> String {
    let wire = serde_json::to_value(result).unwrap();
    wire["content"][0]["text"].as_str().unwrap().to_string()
}

#[test]
fn bbox_describe_schema_includes_installed_agents() {
    let tmp = tempfile::tempdir().unwrap();
    let server = test_server(&tmp);
    let cat = server.state.artifacts.read();
    cat.install_value(
        artifacts::ArtifactKind::Agent,
        "schema-agent.json".into(),
        &serde_json::json!({
            "kind": "agent",
            "name": "schema-tester",
            "version": 1,
            "manifest": {
                "description": "Agent for schema test.",
                "when_to_use": ["use when testing schema"],
                "anti_patterns": ["do not use in prod"],
                "brofile_inline": {"provider": "claude"},
                "cost_class": "normal",
            },
        }),
        None,
        None,
        None,
    )
    .unwrap();
    cat.install_value(
        artifacts::ArtifactKind::Agent,
        "badgey-agent.json".into(),
        &serde_json::json!({
            "kind": "agent",
            "name": "badgey-agent",
            "version": 3,
            "manifest": {
                "description": "Badgey-backed agent.",
                "brofile_inline": {"provider": "claude"},
                "cost_class": "cheap",
                "dispatch_adapter": "badgey",
            },
        }),
        None,
        None,
        None,
    )
    .unwrap();
    drop(cat);

    let result = server.bbox_describe_schema();
    assert_ne!(result.is_error, Some(true));
    let body: serde_json::Value = serde_json::from_str(&extract_text(&result)).unwrap();
    let agents = body["agents"].as_array().expect("agents array");
    assert_eq!(agents.len(), 2);
    let schema_tester = agents
        .iter()
        .find(|a| a["name"] == "schema-tester")
        .unwrap();
    assert_eq!(schema_tester["version"].as_str(), Some("1"));
    assert_eq!(schema_tester["cost_class"].as_str(), Some("normal"));
    assert_eq!(schema_tester["when_to_use"].as_array().unwrap().len(), 1);
    assert_eq!(schema_tester["anti_patterns"].as_array().unwrap().len(), 1);
    assert!(schema_tester["dispatch_adapter"].is_null());

    let badgey = agents.iter().find(|a| a["name"] == "badgey-agent").unwrap();
    assert_eq!(badgey["dispatch_adapter"].as_str(), Some("badgey"));
    assert_eq!(
        badgey["when_to_use"]
            .as_array()
            .expect("when_to_use always present"),
        &Vec::<serde_json::Value>::new(),
        "badgey-agent has empty when_to_use but field must be present"
    );
    assert_eq!(
        badgey["anti_patterns"]
            .as_array()
            .expect("anti_patterns always present"),
        &Vec::<serde_json::Value>::new(),
        "badgey-agent has empty anti_patterns but field must be present"
    );

    let by_adapter = body["agents_by_dispatch_adapter"]
        .as_object()
        .expect("agents_by_dispatch_adapter object");
    assert_eq!(by_adapter["direct"].as_array().unwrap().len(), 1);
    assert_eq!(by_adapter["badgey"].as_array().unwrap().len(), 1);
}

#[test]
fn bro_dashboard_emits_agent_label() {
    let tmp = tempfile::tempdir().unwrap();
    let server = test_server(&tmp);
    let cat = &server.state.artifacts.read();
    cat.install_value(
        artifacts::ArtifactKind::Agent,
        "dash-agent.json".into(),
        &serde_json::json!({
            "kind": "agent",
            "name": "dash-agent",
            "version": 1,
            "manifest": {
                "description": "Agent for dashboard test.",
                "brofile_inline": {"provider": "claude"},
            },
        }),
        None,
        None,
        None,
    )
    .unwrap();

    let rt = tokio::runtime::Runtime::new().unwrap();
    let result = rt.block_on(server.bro_agent_dispatch(Parameters(AgentDispatchParams {
        agent: "dash-agent".into(),
        args: serde_json::Value::Null,
        project_dir: Some(tmp.path().to_str().unwrap().to_string()),
        bro: None,
        ambient: None,
        caller_provider: None,
        caller_session_id: None,
        runtime: None,
    })));
    assert_ne!(result.is_error, Some(true));
    let body: serde_json::Value = serde_json::from_str(&extract_text(&result)).unwrap();
    let task_id = body["task_id"].as_str().unwrap();

    let dash = server.bro_dashboard(Parameters(DashboardParams {
        limit: Some(20),
        provider: None,
        status: None,
        team: None,
    }));
    let dash_body: serde_json::Value = serde_json::from_str(&extract_text(&dash)).unwrap();
    let tasks = dash_body["tasks"].as_array().unwrap();
    let found = tasks.iter().find(|t| t["taskId"].as_str() == Some(task_id));
    assert!(found.is_some(), "task should appear in dashboard");
    let entry = found.unwrap();
    assert_eq!(
        entry["agentLabel"].as_str(),
        Some("agent:dash-agent@v1"),
        "dashboard entry should carry agentLabel: {entry}"
    );
    assert_eq!(
        entry["broLabel"].as_str(),
        Some("agent:dash-agent@v1"),
        "dashboard entry should carry broLabel: {entry}"
    );
    let agent_metrics = &dash_body["agents"]["agent:dash-agent@v1"];
    assert_eq!(agent_metrics["dispatch_count"].as_u64(), Some(1));
    assert_eq!(agent_metrics["success_count"].as_u64(), Some(0));
    assert_eq!(agent_metrics["failure_count"].as_u64(), Some(0));
}

#[test]
fn bro_report_surfaces_latest_task_report() {
    let tmp = tempfile::tempdir().unwrap();
    let server = test_server(&tmp);
    let cat = &server.state.artifacts.read();
    cat.install_value(
        artifacts::ArtifactKind::Agent,
        "report-agent.json".into(),
        &serde_json::json!({
            "kind": "agent",
            "name": "report-agent",
            "version": 1,
            "manifest": {
                "description": "Agent for report test.",
                "brofile_inline": {"provider": "claude"},
            },
        }),
        None,
        None,
        None,
    )
    .unwrap();

    let rt = tokio::runtime::Runtime::new().unwrap();
    let result = rt.block_on(server.bro_agent_dispatch(Parameters(AgentDispatchParams {
        agent: "report-agent".into(),
        args: serde_json::Value::Null,
        project_dir: Some(tmp.path().to_str().unwrap().to_string()),
        bro: None,
        ambient: None,
        caller_provider: None,
        caller_session_id: None,
        runtime: None,
    })));
    assert_ne!(result.is_error, Some(true));
    let body: serde_json::Value = serde_json::from_str(&extract_text(&result)).unwrap();
    let task_id = body["task_id"].as_str().unwrap().to_string();

    let report = server.bro_report(Parameters(ReportParams {
        task_id: task_id.clone(),
        message: "writing focused tests".into(),
        needs: Some("review API naming".into()),
        data: Some(serde_json::json!({"phase": "test"})),
    }));
    assert_ne!(report.is_error, Some(true));
    let report_body: serde_json::Value = serde_json::from_str(&extract_text(&report)).unwrap();
    assert_eq!(
        report_body["report"]["message"].as_str(),
        Some("writing focused tests")
    );
    assert_eq!(
        report_body["report"]["needs"].as_str(),
        Some("review API naming")
    );
    assert_eq!(
        report_body["report"]["data"]["phase"].as_str(),
        Some("test")
    );
    assert!(report_body["report"]["reportedAt"].as_u64().is_some());
    assert!(report_body["report"]["reportedAgo"].as_str().is_some());

    let dash = server.bro_dashboard(Parameters(DashboardParams {
        limit: Some(20),
        provider: None,
        status: None,
        team: None,
    }));
    let dash_body: serde_json::Value = serde_json::from_str(&extract_text(&dash)).unwrap();
    let entry = dash_body["tasks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["taskId"].as_str() == Some(task_id.as_str()))
        .expect("task should appear in dashboard");
    assert_eq!(
        entry["report"]["message"].as_str(),
        Some("writing focused tests")
    );
    assert_eq!(entry["report"]["needs"].as_str(), Some("review API naming"));

    let status = server.bro_status(Parameters(StatusParams {
        task_id: task_id.clone(),
        tail: None,
    }));
    let status_body: serde_json::Value = serde_json::from_str(&extract_text(&status)).unwrap();
    assert_eq!(
        status_body["report"]["message"].as_str(),
        Some("writing focused tests")
    );
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
