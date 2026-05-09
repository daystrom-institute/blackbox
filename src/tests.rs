use super::*;

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
    });
    BlackboxServer::new(state)
}

fn save_test_brofile(tmp: &tempfile::TempDir, name: &str) {
    orchestration::brofile::save_brofile(
        &orchestration::brofile::Brofile {
            name: name.to_string(),
            provider: Provider::Gemini,
            account: None,
            lens: None,
            model: None,
            effort: None,
            filters: None,
        },
        "global",
        &tmp.path().join("bro"),
        None,
    );
}

fn save_badgey_test_brofile(tmp: &tempfile::TempDir) {
    orchestration::brofile::save_brofile(
        &orchestration::brofile::Brofile {
            name: "badgey-persona".to_string(),
            provider: Provider::Codex,
            account: None,
            lens: Some("Badgey test lens".to_string()),
            model: None,
            effort: None,
            filters: Some(orchestration::mcp::McpFilters {
                allow: Vec::new(),
                disallow: vec!["mcp__blackbox__bro_*".to_string()],
            }),
        },
        "global",
        &tmp.path().join("bro"),
        None,
    );
}

#[test]
fn embed_status_reports_thread_coverage_from_vector_store() {
    let tmp = tempfile::tempdir().unwrap();
    let server = test_server(&tmp);
    let vector_tmp = tempfile::tempdir().unwrap();
    let store = Arc::new(crate::vectors::VectorStore::open(vector_tmp.path()).unwrap());
    let _guard = crate::vectors::install_test_global(store.clone());
    let created = server
        .state
        .threads
        .write()
        .thread(&threads::ThreadParams {
            action: "open".into(),
            name: Some("coverage-thread".into()),
            id: None,
            topic: Some("status coverage thread".into()),
            project: Some("/repo".into()),
            session_id: None,
            provider: None,
            session_name: None,
            handoff_doc: Some("handoff marker".into()),
            note: Some("note marker".into()),
            target: None,
            target_type: None,
            edge: None,
            promoted_to: None,
            kind: Some("investigation".into()),
        })
        .unwrap();
    let thread_id = regex::Regex::new(r"thread-[0-9a-f]{8}")
        .unwrap()
        .find(&created)
        .unwrap()
        .as_str()
        .to_string();
    let thread = server
        .state
        .threads
        .read()
        .all()
        .iter()
        .find(|thread| thread.id == thread_id)
        .unwrap()
        .clone();
    let route = crate::embed::EmbeddingRouter::default()
        .route(crate::embed::Bucket::Threads, None)
        .unwrap()
        .vector_route_id();
    let entity_id = crate::entity_ref::EntityRef::Thread { thread_id }.to_string();
    store
        .upsert(
            &route,
            &entity_id,
            &crate::embed_queue::thread_chunk_hash(&thread),
            vec![1.0, 0.0],
        )
        .unwrap();

    let status = crate::embed_queue::status_response_for_buckets(
        &server.state,
        &[crate::embed::Bucket::Threads],
    )
    .unwrap();
    let threads = status.routes.get("threads").unwrap();
    assert_eq!(threads.source_count, Some(1));
    assert_eq!(threads.indexed_count, 1);
    assert_eq!(threads.coverage_ratio, Some(1.0));
}

fn fake_codex_bin(tmp: &tempfile::TempDir, session_id: &str) -> String {
    let path = tmp.path().join("fake-codex");
    std::fs::write(
        &path,
        format!(
            "#!/bin/sh\nprintf '%s\\n' '{}'\n",
            serde_json::json!({
                "type": "thread.started",
                "thread_id": session_id,
            })
        ),
    )
    .unwrap();
    let mut perms = std::fs::metadata(&path).unwrap().permissions();
    std::os::unix::fs::PermissionsExt::set_mode(&mut perms, 0o755);
    std::fs::set_permissions(&path, perms).unwrap();
    path.to_string_lossy().into_owned()
}

async fn codex_bin_test_guard() -> tokio::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::OnceLock<tokio::sync::Mutex<()>> = std::sync::OnceLock::new();
    LOCK.get_or_init(|| tokio::sync::Mutex::new(()))
        .lock()
        .await
}

#[tokio::test]
async fn artifact_install_wires_f3_workflow_and_packet() {
    let tmp = tempfile::tempdir().unwrap();
    let server = test_server(&tmp);
    let workflow_value: Value = serde_json::from_str(include_str!(
        "../examples/agentic-corpus/workflows/schema-migration-arc.json"
    ))
    .unwrap();
    let packet_value: Value = serde_json::from_str(include_str!(
        "../examples/agentic-corpus/packets/workflow-policy/arc-budget.json"
    ))
    .unwrap();

    install_artifact_value(
        &server.state,
        ArtifactInstallParams {
            kind: artifacts::ArtifactKind::Workflow,
            source: "examples/agentic-corpus/workflows/schema-migration-arc.json".into(),
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
            source: "examples/agentic-corpus/packets/workflow-policy/arc-budget.json".into(),
            name: None,
            version: None,
            supersedes: None,
        },
        packet_value,
    )
    .await
    .unwrap();

    assert!(server
        .state
        .workflow_registry
        .read()
        .contains_key("schema-migration-arc"));
    assert!(server
        .state
        .packets
        .read()
        .load("domain:workflow-policy/arc-budget")
        .is_ok());
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
    std::env::remove_var("SLACK_BOT_TOKEN");
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
    std::env::remove_var("SLACK_BOT_TOKEN");
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
    std::env::remove_var("SLACK_BOT_TOKEN");
    try_slack_proposal_signal_hook("proposal-approved", &server.state, &correlate, &entity).await;
    // Nothing to assert beyond "did not panic" — but make a
    // sanity probe on the link store size to confirm we didn't
    // accidentally insert anything.
    assert!(server
        .state
        .slack_proposal_links
        .lookup_by_msg("T01", "C01", "ts-unknown")
        .is_none());
}

#[tokio::test]
async fn artifact_install_wires_project_bootstrap_arc() {
    let tmp = tempfile::tempdir().unwrap();
    let server = test_server(&tmp);
    let workflow_value: Value = serde_json::from_str(include_str!(
        "../examples/agentic-corpus/workflows/project-bootstrap-arc.json"
    ))
    .unwrap();

    install_artifact_value(
        &server.state,
        ArtifactInstallParams {
            kind: artifacts::ArtifactKind::Workflow,
            source: "examples/agentic-corpus/workflows/project-bootstrap-arc.json".into(),
            name: None,
            version: None,
            supersedes: None,
        },
        workflow_value,
    )
    .await
    .unwrap();

    assert!(server
        .state
        .workflow_registry
        .read()
        .contains_key("project-bootstrap-arc"));
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
    assert!(snapshot
        .completed_nodes
        .iter()
        .any(|node| node == "Publish"));
}

#[tokio::test]
async fn artifact_install_wires_m2_compaction_arc_and_packets() {
    let tmp = tempfile::tempdir().unwrap();
    let server = test_server(&tmp);
    let workflow_value: Value = serde_json::from_str(include_str!(
        "../examples/agentic-corpus/workflows/embed-compaction-arc.json"
    ))
    .unwrap();
    let policy_value: Value = serde_json::from_str(include_str!(
        "../examples/agentic-corpus/packets/embed/compaction-policy.json"
    ))
    .unwrap();
    let cron_routing_value: Value = serde_json::from_str(include_str!(
        "../examples/agentic-corpus/packets/cron-routing/embed-compaction.json"
    ))
    .unwrap();

    install_artifact_value(
        &server.state,
        ArtifactInstallParams {
            kind: artifacts::ArtifactKind::Workflow,
            source: "examples/agentic-corpus/workflows/embed-compaction-arc.json".into(),
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
            source: "examples/agentic-corpus/packets/embed/compaction-policy.json".into(),
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
            source: "examples/agentic-corpus/packets/cron-routing/embed-compaction.json".into(),
            name: None,
            version: None,
            supersedes: None,
        },
        cron_routing_value,
    )
    .await
    .unwrap();

    assert!(server
        .state
        .workflow_registry
        .read()
        .contains_key("embed-compaction-arc"));
    assert!(server
        .state
        .packets
        .read()
        .load("domain:embed/compaction-policy")
        .is_ok());
    assert!(server
        .state
        .packets
        .read()
        .load("domain:cron-routing/embed-compaction")
        .is_ok());
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
        "../examples/agentic-corpus/packets/embed/compaction-policy.json"
    ))
    .unwrap();
    install_artifact_value(
        &server.state,
        ArtifactInstallParams {
            kind: artifacts::ArtifactKind::Packet,
            source: "examples/agentic-corpus/packets/embed/compaction-policy.json".into(),
            name: None,
            version: None,
            supersedes: None,
        },
        packet_value,
    )
    .await
    .unwrap();

    let workflow_spec: workflow::Workflow = serde_json::from_str(include_str!(
        "../examples/agentic-corpus/workflows/embed-compaction-arc.json"
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
        "../examples/agentic-corpus/brofiles/digest-extractor.json"
    ))
    .unwrap();
    assert_eq!(
        brofile_value["disallow_tools"],
        serde_json::json!(["Edit", "Write", "Bash"])
    );
    let trust_value: Value = serde_json::from_str(include_str!(
        "../examples/agentic-corpus/packets/bro-trust/per-brofile.json"
    ))
    .unwrap();
    let quality_value: Value = serde_json::from_str(include_str!(
        "../examples/agentic-corpus/packets/auto-digest/entry-quality.json"
    ))
    .unwrap();
    let routing_value: Value = serde_json::from_str(include_str!(
        "../examples/agentic-corpus/packets/auto-digest/task-completed-routing.json"
    ))
    .unwrap();
    let workflow_value: Value = serde_json::from_str(include_str!(
        "../examples/agentic-corpus/workflows/auto-digest-arc.json"
    ))
    .unwrap();

    install_artifact_value(
        &server.state,
        ArtifactInstallParams {
            kind: artifacts::ArtifactKind::Brofile,
            source: "examples/agentic-corpus/brofiles/digest-extractor.json".into(),
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
            source: "examples/agentic-corpus/packets/bro-trust/per-brofile.json".into(),
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
            source: "examples/agentic-corpus/packets/auto-digest/entry-quality.json".into(),
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
            source: "examples/agentic-corpus/packets/auto-digest/task-completed-routing.json"
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
            source: "examples/agentic-corpus/workflows/auto-digest-arc.json".into(),
            name: None,
            version: None,
            supersedes: None,
        },
        workflow_value,
    )
    .await
    .unwrap();

    assert!(server
        .state
        .workflow_registry
        .read()
        .contains_key("auto-digest-arc"));
    assert!(server
        .state
        .packets
        .read()
        .load("domain:auto-digest/entry-quality")
        .is_ok());
    assert!(server
        .state
        .packets
        .read()
        .load("domain:auto-digest/task-completed-routing")
        .is_ok());
    assert!(orchestration::brofile::resolve_brofile(
        "digest-extractor",
        &server.state.store_dir,
        None
    )
    .is_some());

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
        "../examples/agentic-corpus/workflows/contradiction-review-arc.json"
    ))
    .unwrap();
    let packet_value: Value = serde_json::from_str(include_str!(
        "../examples/agentic-corpus/packets/contradiction/review-synthesis.json"
    ))
    .unwrap();
    let brofiles: [(&str, Value); 4] = [
        (
            "contradiction-provenance",
            serde_json::from_str(include_str!(
                "../examples/agentic-corpus/brofiles/contradiction-provenance.json"
            ))
            .unwrap(),
        ),
        (
            "contradiction-lifecycle",
            serde_json::from_str(include_str!(
                "../examples/agentic-corpus/brofiles/contradiction-lifecycle.json"
            ))
            .unwrap(),
        ),
        (
            "contradiction-coherence",
            serde_json::from_str(include_str!(
                "../examples/agentic-corpus/brofiles/contradiction-coherence.json"
            ))
            .unwrap(),
        ),
        (
            "contradiction-facilitator",
            serde_json::from_str(include_str!(
                "../examples/agentic-corpus/brofiles/contradiction-facilitator.json"
            ))
            .unwrap(),
        ),
    ];

    install_artifact_value(
        &server.state,
        ArtifactInstallParams {
            kind: artifacts::ArtifactKind::Packet,
            source: "examples/agentic-corpus/packets/contradiction/review-synthesis.json".into(),
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
                source: format!("examples/agentic-corpus/brofiles/{name}.json"),
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
            source: "examples/agentic-corpus/workflows/contradiction-review-arc.json".into(),
            name: None,
            version: None,
            supersedes: None,
        },
        workflow_value,
    )
    .await
    .unwrap();

    assert!(server
        .state
        .workflow_registry
        .read()
        .contains_key("contradiction-review-arc"));
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
    assert!(orchestration::brofile::resolve_brofile(
        "contradiction-facilitator",
        &server.state.store_dir,
        None
    )
    .is_some());
}

#[tokio::test]
async fn artifact_install_wires_m5_auto_edge_artifacts_and_audit() {
    let tmp = tempfile::tempdir().unwrap();
    let server = test_server(&tmp);
    let packet_value: Value = serde_json::from_str(include_str!(
        "../examples/agentic-corpus/packets/auto-edge/vote-aggregate.json"
    ))
    .unwrap();
    let workflow_value: Value = serde_json::from_str(include_str!(
        "../examples/agentic-corpus/workflows/auto-edge-arc.json"
    ))
    .unwrap();
    let brofiles: [(&str, Value); 6] = [
        (
            "describe-prose-signal",
            serde_json::from_str(include_str!(
                "../examples/agentic-corpus/brofiles/describe-prose-signal.json"
            ))
            .unwrap(),
        ),
        (
            "describe-symbol-fit",
            serde_json::from_str(include_str!(
                "../examples/agentic-corpus/brofiles/describe-symbol-fit.json"
            ))
            .unwrap(),
        ),
        (
            "describe-narrative-cohesion",
            serde_json::from_str(include_str!(
                "../examples/agentic-corpus/brofiles/describe-narrative-cohesion.json"
            ))
            .unwrap(),
        ),
        (
            "reference-citation-precision",
            serde_json::from_str(include_str!(
                "../examples/agentic-corpus/brofiles/reference-citation-precision.json"
            ))
            .unwrap(),
        ),
        (
            "reference-target-existence",
            serde_json::from_str(include_str!(
                "../examples/agentic-corpus/brofiles/reference-target-existence.json"
            ))
            .unwrap(),
        ),
        (
            "reference-context-fit",
            serde_json::from_str(include_str!(
                "../examples/agentic-corpus/brofiles/reference-context-fit.json"
            ))
            .unwrap(),
        ),
    ];

    install_artifact_value(
        &server.state,
        ArtifactInstallParams {
            kind: artifacts::ArtifactKind::Packet,
            source: "examples/agentic-corpus/packets/auto-edge/vote-aggregate.json".into(),
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
                source: format!("examples/agentic-corpus/brofiles/{name}.json"),
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
            source: "examples/agentic-corpus/workflows/auto-edge-arc.json".into(),
            name: None,
            version: None,
            supersedes: None,
        },
        workflow_value,
    )
    .await
    .unwrap();
    assert!(server
        .state
        .workflow_registry
        .read()
        .contains_key("auto-edge-arc"));

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
        include_tantivy_projection: true,
    });
    let source_ref = entity_ref::EntityRef::parse(source).unwrap();
    let target_ref = entity_ref::EntityRef::parse(target).unwrap();
    assert!(edge_index
        .forward_edges(&source_ref)
        .iter()
        .any(|edge| edge.kind == "DESCRIBES" && edge.target == target_ref));
}

#[tokio::test]
async fn shipped_packet_audit_examples_pass() {
    let tmp = tempfile::tempdir().unwrap();
    let server = test_server(&tmp);
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let packets = [
        "examples/agentic-corpus/packets/workflow-policy/arc-budget.json",
        "examples/agentic-corpus/packets/embed/compaction-policy.json",
        "examples/agentic-corpus/packets/cron-routing/embed-compaction.json",
        "examples/agentic-corpus/packets/bro-trust/per-brofile.json",
        "examples/agentic-corpus/packets/auto-digest/task-completed-routing.json",
        "examples/agentic-corpus/packets/auto-digest/entry-quality.json",
        "examples/agentic-corpus/packets/contradiction/review-synthesis.json",
        "examples/agentic-corpus/packets/auto-edge/vote-aggregate.json",
        "examples/agentic-corpus/packets/eval/drift-policy.json",
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
        "examples/agentic-corpus/packets/workflow-policy/arc-budget.audit_examples.json",
        "examples/agentic-corpus/packets/embed/compaction-policy.audit_examples.json",
        "examples/agentic-corpus/packets/cron-routing/embed-compaction.audit_examples.json",
        "examples/agentic-corpus/packets/bro-trust/per-brofile.audit_examples.json",
        "examples/agentic-corpus/packets/auto-digest/task-completed-routing.audit_examples.json",
        "examples/agentic-corpus/packets/auto-digest/entry-quality.audit_examples.json",
        "examples/agentic-corpus/packets/contradiction/review-synthesis.audit_examples.json",
        "examples/agentic-corpus/packets/auto-edge/vote-aggregate.audit_examples.json",
        "examples/agentic-corpus/packets/eval/drift-policy.audit_examples.json",
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
    assert!(server
        .state
        .workflow_registry
        .read()
        .contains_key("workflow-a"));

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

    assert!(!server
        .state
        .workflow_registry
        .read()
        .contains_key("workflow-a"));
    assert!(server
        .state
        .workflow_registry
        .read()
        .contains_key("workflow-a2"));
    assert!(!server
        .state
        .store_dir
        .join("workflows")
        .join("workflow-a.json")
        .exists());
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

#[tokio::test]
async fn read_artifact_source_rejects_oversized_http_response() {
    use tokio::io::AsyncWriteExt;
    use tokio::net::TcpListener;

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let response = concat!(
            "HTTP/1.1 200 OK\r\n",
            "Content-Type: application/json\r\n",
            "Content-Length: 1048577\r\n",
            "\r\n",
            "{}"
        );
        socket.write_all(response.as_bytes()).await.unwrap();
    });

    let err = read_artifact_source(&format!("http://{addr}/artifact.json"))
        .await
        .unwrap_err()
        .to_string();

    assert!(err.contains("too large"), "got: {err}");
}

#[test]
fn bbox_project_list_round_trips_through_tool_serialization() {
    let tmp = tempfile::tempdir().unwrap();
    let project = tmp.path().join("project");
    std::fs::create_dir_all(&project).unwrap();
    let server = test_server(&tmp);

    let register = server.bbox_project_register(Parameters(ProjectRegisterParams {
        path: project.to_string_lossy().into_owned(),
    }));
    assert_ne!(register.is_error, Some(true));

    let listed = server.bbox_project_list();
    assert_ne!(listed.is_error, Some(true));
    let wire = serde_json::to_value(&listed).unwrap();
    let text = wire["content"][0]["text"].as_str().unwrap();
    let response: ProjectListResponse = serde_json::from_str(text).unwrap();

    assert_eq!(response.projects.len(), 1);
    assert_eq!(
        response.projects[0].project_id,
        entity_ref::project_id_for_path(&project).unwrap()
    );
}

#[test]
fn bbox_project_rename_migrates_project_scoped_state() {
    let tmp = tempfile::tempdir().unwrap();
    let old_project = tmp.path().join("old-project");
    let new_project = tmp.path().join("new-project");
    std::fs::create_dir_all(&old_project).unwrap();
    std::fs::create_dir_all(&new_project).unwrap();
    let old_project = std::fs::canonicalize(&old_project)
        .unwrap()
        .to_string_lossy()
        .into_owned();
    let new_project = std::fs::canonicalize(&new_project)
        .unwrap()
        .to_string_lossy()
        .into_owned();
    let server = test_server(&tmp);

    let register = server.bbox_project_register(Parameters(ProjectRegisterParams {
        path: old_project.clone(),
    }));
    assert_ne!(register.is_error, Some(true));
    let text = serde_json::to_value(&register).unwrap()["content"][0]["text"]
        .as_str()
        .unwrap()
        .to_string();
    let registered: ProjectRecord = serde_json::from_str(&text).unwrap();

    server
        .state
        .kb
        .write()
        .remember(
            &knowledge::RememberParams {
                content: "project fact".into(),
                category: None,
                title: Some("project fact".into()),
                scope: Some("project".into()),
                project: Some(old_project.clone()),
                decay: None,
                review_at: None,
                expires_at: None,
            },
            false,
        )
        .unwrap();
    server
        .state
        .threads
        .write()
        .thread(&threads::ThreadParams {
            action: "open".into(),
            name: None,
            id: None,
            topic: Some("project thread".into()),
            project: Some(old_project.clone()),
            session_id: None,
            provider: None,
            session_name: None,
            handoff_doc: None,
            note: None,
            target: None,
            target_type: None,
            edge: None,
            promoted_to: None,
            kind: None,
        })
        .unwrap();
    server
        .state
        .notes
        .write()
        .create(&notes::NoteParams {
            kind: "learned".into(),
            body: "project note".into(),
            task_id: None,
            session_id: None,
            project: Some(old_project.clone()),
            thread_id: None,
            provider: None,
            bro: None,
        })
        .unwrap();
    server
        .state
        .pins
        .write()
        .pin(&pins::PinParams {
            action: "set".into(),
            id: None,
            content: Some("project pin".into()),
            title: Some("project pin".into()),
            scope: Some("session".into()),
            target: Some("sid".into()),
            project: Some(old_project.clone()),
            expires_at: None,
        })
        .unwrap();

    let renamed = server.bbox_project_rename(Parameters(ProjectRenameParams {
        project: registered.project_id.clone(),
        new_path: new_project.clone(),
        move_on_disk: None,
        dry_run: None,
    }));
    assert_ne!(renamed.is_error, Some(true));
    let text = serde_json::to_value(&renamed).unwrap()["content"][0]["text"]
        .as_str()
        .unwrap()
        .to_string();
    let payload: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(
        payload["record"]["project_id"].as_str(),
        Some(registered.project_id.as_str())
    );
    assert_eq!(
        payload["record"]["canonical_path"].as_str(),
        Some(new_project.as_str())
    );
    assert_eq!(payload["migrated_refs"]["knowledge"], 1);
    assert_eq!(payload["migrated_refs"]["threads"], 1);
    assert_eq!(payload["migrated_refs"]["notes"], 1);
    assert_eq!(payload["migrated_refs"]["pins"], 1);

    assert_eq!(
        server.state.kb.read().all_entries()[0].project.as_deref(),
        Some(new_project.as_str())
    );
    assert_eq!(
        server.state.threads.read().all()[0].project.as_str(),
        new_project.as_str()
    );
    assert_eq!(
        server.state.notes.read().all()[0].project.as_deref(),
        Some(new_project.as_str())
    );
    assert_eq!(server.state.pins.read().project_ref_count(&new_project), 1);
}

#[test]
fn resolve_resume_target_rejects_ambiguous_bro_names_across_live_teams() {
    let tmp = tempfile::tempdir().unwrap();
    let server = test_server(&tmp);
    save_test_brofile(&tmp, "reviewer");

    for (team_name, session_id) in [("red", "sid-red"), ("blue", "sid-blue")] {
        orchestration::team::save_team(
            &orchestration::team::Team {
                name: team_name.to_string(),
                teamplate: "review".into(),
                members: vec![orchestration::team::TeamMember {
                    name: "reviewer".into(),
                    brofile: "reviewer".into(),
                    session_id: Some(session_id.into()),
                    task_history: vec![],
                }],
                advisor: None,
                project_dir: None,
                created_at: 0,
            },
            &tmp.path().join("bro"),
        );
    }

    let err = server
        .resolve_resume_target(Some("reviewer"), None, None, None)
        .unwrap_err();
    assert!(err.contains("Ambiguous bro name: reviewer"));
    assert!(err.contains("red"));
    assert!(err.contains("blue"));
}

#[test]
fn resolve_resume_target_accepts_scoped_team_bro_selector() {
    let tmp = tempfile::tempdir().unwrap();
    let server = test_server(&tmp);
    save_test_brofile(&tmp, "reviewer");

    for (team_name, session_id) in [("red", "sid-red"), ("blue", "sid-blue")] {
        orchestration::team::save_team(
            &orchestration::team::Team {
                name: team_name.to_string(),
                teamplate: "review".into(),
                members: vec![orchestration::team::TeamMember {
                    name: "reviewer".into(),
                    brofile: "reviewer".into(),
                    session_id: Some(session_id.into()),
                    task_history: vec![],
                }],
                advisor: None,
                project_dir: Some(format!("/tmp/{team_name}")),
                created_at: 0,
            },
            &tmp.path().join("bro"),
        );
    }

    let (provider, session_id, _lens, _opts, _env, cwd, _filters) = server
        .resolve_resume_target(Some("blue::reviewer"), None, None, None)
        .unwrap();
    assert_eq!(provider, Provider::Gemini);
    assert_eq!(session_id, "sid-blue");
    assert_eq!(cwd.as_deref(), Some("/tmp/blue"));
}

#[test]
fn build_advisor_checkpoint_flattens_note_counts_for_packets() {
    let tmp = tempfile::tempdir().unwrap();
    let server = test_server(&tmp);
    {
        let mut notes = server.state.notes.write();
        notes
            .create(&NoteParams {
                kind: "blocked".into(),
                body: "worker is blocked".into(),
                task_id: Some("task-1".into()),
                session_id: None,
                project: None,
                thread_id: None,
                provider: None,
                bro: Some("worker".into()),
            })
            .unwrap();
        notes
            .create(&NoteParams {
                kind: "dispute".into(),
                body: "worker disputes premise".into(),
                task_id: Some("task-1".into()),
                session_id: None,
                project: None,
                thread_id: None,
                provider: None,
                bro: Some("worker".into()),
            })
            .unwrap();
    }
    let team = orchestration::team::Team {
        name: "demo".into(),
        teamplate: "tp".into(),
        members: vec![],
        advisor: Some(orchestration::team::TeamAdvisor {
            name: "advisor".into(),
            config: orchestration::team::TeamAdvisorConfig {
                brofile: "advisor".into(),
                alias: Some("advisor".into()),
                charter: "demo".into(),
                context: None,
                halt_conditions: vec![],
                exit_conditions: vec![],
                packet_id: Some("packet-demo".into()),
                timeout_seconds: None,
                mode: orchestration::team::AdvisorMode::Blocking,
            },
            session_id: None,
            task_history: vec![],
        }),
        project_dir: None,
        created_at: 0,
    };
    let checkpoint = server.build_advisor_checkpoint(
        &team,
        "when_all",
        &[json!({
            "taskId": "task-1",
            "status": "running",
            "timed_out": true
        })],
    );
    assert_eq!(checkpoint.blocked_count, 1);
    assert_eq!(checkpoint.dispute_count, 1);
    assert_eq!(checkpoint.notes.blocked_count, 1);
    assert_eq!(checkpoint.notes.dispute_count, 1);
}

#[test]
fn mcp_response_cap_limits_large_text() {
    let huge = "x".repeat(BlackboxServer::MCP_RESPONSE_CAP_BYTES + 1024);
    let capped = BlackboxServer::cap_response_text(&huge);
    assert!(capped.len() <= BlackboxServer::MCP_RESPONSE_CAP_BYTES);
    assert!(capped.contains("response truncated"));
}

#[tokio::test]
async fn run_workflow_at_depth_rejects_past_ceiling() {
    // A direct smoke test for the fix driven by the self-audit
    // live validation: the subworkflow depth counter used to live
    // in a per-runner HashMap, so nested runners silently reset
    // it. Now it's threaded through run_workflow_at_depth so the
    // ceiling is enforced globally across the composition chain.
    use crate::workflow::{compile, engine, load_workflow};
    let tmp = tempfile::tempdir().unwrap();
    let server = test_server(&tmp);

    // Minimal valid workflow — doesn't actually matter since the
    // depth check short-circuits before any dispatch.
    let json = r#"{
        "name": "depth-test",
        "version": 1,
        "actors": {"a": {"kind": "executor", "brofile": "b"}},
        "nodes": {"N": {"actor": "a", "next": {"type": "terminal"}}},
        "start": "N"
    }"#;
    let compiled = compile(load_workflow(json).unwrap()).unwrap();

    // At exactly MAX_COMPOSITION_DEPTH: should proceed (no error
    // from depth check). We don't actually dispatch because there's
    // no brofile "b" on this test server — but we confirm the
    // depth check isn't the thing that errors it out.
    let at_ceiling = engine::run_workflow_at_depth(
        &server,
        &compiled,
        None,
        Some(1),
        engine::MAX_COMPOSITION_DEPTH,
        std::collections::HashMap::new(),
        serde_json::Map::new(),
        None,
    )
    .await;
    assert!(
        !at_ceiling
            .status
            .starts_with("error: subworkflow composition depth"),
        "at-ceiling depth should not be rejected by the depth guard; got: {}",
        at_ceiling.status
    );

    // Past ceiling: short-circuit with a depth-error status.
    let past_ceiling = engine::run_workflow_at_depth(
        &server,
        &compiled,
        None,
        Some(1),
        engine::MAX_COMPOSITION_DEPTH + 1,
        std::collections::HashMap::new(),
        serde_json::Map::new(),
        None,
    )
    .await;
    assert!(
        past_ceiling
            .status
            .starts_with("error: subworkflow composition depth"),
        "past-ceiling should error on depth; got: {}",
        past_ceiling.status
    );
    assert!(past_ceiling.events.is_empty());
    assert!(past_ceiling.arc_thread_id.is_none());
}

#[tokio::test]
async fn workflow_foreach_runtime_collects_child_exports() {
    use crate::workflow::{compile, engine, load_workflow};
    let tmp = tempfile::tempdir().unwrap();
    let server = test_server(&tmp);
    let json = r#"{
        "name": "foreach-runtime",
        "version": 1,
        "actors": {},
        "vars_schema": {
            "parent": {"kind": "string"},
            "results": {"kind": "array"}
        },
        "nodes": {
            "Each": {
                "actor": "",
                "foreach": {
                    "items": ["a", "b"],
                    "as_var": "item",
                    "index_as": "idx",
                    "key": "${vars.item}-${vars.idx}",
                    "imports": ["parent"],
                    "exports": ["summary"],
                    "collect": {"into_var": "results"},
                    "subworkflow": {
                        "name": "foreach-child",
                        "version": 1,
                        "actors": {},
                        "vars_schema": {
                            "item": {"kind": "string"},
                            "idx": {"kind": "int"},
                            "parent": {"kind": "string"},
                            "summary": {"kind": "object"}
                        },
                        "nodes": {
                            "Make": {
                                "actor": "",
                                "on_enter": [{
                                    "op": "set_var",
                                    "args": {
                                        "key": "summary",
                                        "value": {
                                            "item": "${vars.item}",
                                            "idx": "${vars.idx}",
                                            "parent": "${vars.parent}"
                                        }
                                    }
                                }],
                                "next": {"type": "terminal"}
                            }
                        },
                        "start": "Make"
                    }
                },
                "next": {"type": "terminal"}
            }
        },
        "start": "Each"
    }"#;
    let compiled = compile(load_workflow(json).unwrap()).unwrap();
    let mut vars = serde_json::Map::new();
    vars.insert("parent".into(), Value::String("p0".into()));
    let result =
        engine::run_workflow_with_initial_vars(&server, &compiled, None, Some(20), vars).await;

    assert_eq!(result.status, "completed", "events: {:?}", result.events);
    let rows = result
        .vars
        .get("results")
        .and_then(Value::as_array)
        .expect("results array");
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0]["status"], "completed");
    assert_eq!(rows[0]["key"], "a-0");
    assert_eq!(rows[0]["exports"]["summary"]["item"], "a");
    assert_eq!(rows[1]["exports"]["summary"]["idx"], 1);
}

#[tokio::test]
async fn workflow_matrix_runtime_expands_axes_through_fanout() {
    use crate::workflow::{compile, engine, load_workflow};
    let tmp = tempfile::tempdir().unwrap();
    let server = test_server(&tmp);
    let json = r#"{
        "name": "matrix-runtime",
        "version": 1,
        "actors": {},
        "vars_schema": {
            "queries": {"kind": "array"},
            "results": {"kind": "array"}
        },
        "nodes": {
            "Grid": {
                "actor": "",
                "matrix": {
                    "axes": [
                        {"name": "query", "values": "${vars.queries}"},
                        {"name": "strategy", "values": ["search", "agentic"]}
                    ],
                    "as_var": "case",
                    "index_as": "idx",
                    "key": "${vars.case.query}/${vars.case.strategy}",
                    "exports": ["summary"],
                    "parallelism": 2,
                    "collect": {"into_var": "results"},
                    "subworkflow": {
                        "name": "matrix-child",
                        "version": 1,
                        "actors": {},
                        "vars_schema": {
                            "case": {"kind": "object"},
                            "idx": {"kind": "int"},
                            "summary": {"kind": "object"}
                        },
                        "nodes": {
                            "Make": {
                                "actor": "",
                                "on_enter": [{
                                    "op": "set_var",
                                    "args": {
                                        "key": "summary",
                                        "value": {
                                            "query": "${vars.case.query}",
                                            "strategy": "${vars.case.strategy}",
                                            "idx": "${vars.idx}"
                                        }
                                    }
                                }],
                                "next": {"type": "terminal"}
                            }
                        },
                        "start": "Make"
                    }
                },
                "next": {"type": "terminal"}
            }
        },
        "start": "Grid"
    }"#;
    let compiled = compile(load_workflow(json).unwrap()).unwrap();
    let mut vars = serde_json::Map::new();
    vars.insert("queries".into(), serde_json::json!(["q1", "q2"]));
    let result =
        engine::run_workflow_with_initial_vars(&server, &compiled, None, Some(20), vars).await;

    assert_eq!(result.status, "completed", "events: {:?}", result.events);
    let rows = result
        .vars
        .get("results")
        .and_then(Value::as_array)
        .expect("results array");
    assert_eq!(rows.len(), 4);
    assert_eq!(rows[0]["key"], "q1/search");
    assert_eq!(rows[1]["key"], "q1/agentic");
    assert_eq!(rows[2]["key"], "q2/search");
    assert_eq!(rows[3]["exports"]["summary"]["strategy"], "agentic");
}

#[tokio::test]
async fn workflow_foreach_continue_collects_item_failures() {
    use crate::workflow::{compile, engine, load_workflow};
    let tmp = tempfile::tempdir().unwrap();
    let server = test_server(&tmp);
    let json = r#"{
        "name": "foreach-continue",
        "version": 1,
        "actors": {},
        "vars_schema": {
            "results": {"kind": "array"}
        },
        "nodes": {
            "Each": {
                "actor": "",
                "foreach": {
                    "items": ["a", "b"],
                    "as_var": "item",
                    "exports": ["missing"],
                    "on_item_failure": "continue",
                    "collect": {"into_var": "results"},
                    "subworkflow": {
                        "name": "bad-child",
                        "version": 1,
                        "actors": {},
                        "vars_schema": {
                            "item": {"kind": "string"},
                            "missing": {"kind": "string"}
                        },
                        "nodes": {
                            "NoExport": {"actor": "", "next": {"type": "terminal"}}
                        },
                        "start": "NoExport"
                    }
                },
                "next": {"type": "terminal"}
            }
        },
        "start": "Each"
    }"#;
    let compiled = compile(load_workflow(json).unwrap()).unwrap();
    let result = engine::run_workflow(&server, &compiled, None, Some(20)).await;

    assert_eq!(result.status, "completed", "events: {:?}", result.events);
    let rows = result
        .vars
        .get("results")
        .and_then(Value::as_array)
        .expect("results array");
    assert_eq!(rows.len(), 2);
    assert!(rows.iter().all(|row| row["status"] == "error"));
    assert!(rows[0]["error"]
        .as_str()
        .unwrap()
        .contains("did not export declared key"));
}

#[tokio::test]
async fn workflow_spawn_returns_pollable_task() {
    use crate::workflow::{compile, load_workflow};
    let tmp = tempfile::tempdir().unwrap();
    let server = test_server(&tmp);
    let json = r#"{
        "name": "pollable-workflow",
        "version": 1,
        "actors": {},
        "nodes": {
            "Only": {
                "actor": "",
                "prompt": "done",
                "next": {"type": "terminal"}
            }
        },
        "start": "Only"
    }"#;
    let compiled = compile(load_workflow(json).unwrap()).unwrap();
    let (task, arc_id) =
        server.spawn_workflow_task(compiled, None, Some(5), serde_json::Map::new());
    {
        let inner = task.inner.lock();
        assert_eq!(inner.provider, Provider::Workflow);
        assert_eq!(inner.session_id, arc_id);
        assert_eq!(inner.status, orch::TaskStatus::Running);
    }
    assert!(orch::wait_for_task_with_timeout(&task, Some(5.0)).await);
    let status = orch::task_status_json(&task, 5);
    assert_eq!(status["status"], "completed");
    assert_eq!(status["provider"], "workflow");
    assert!(status["eventCount"].as_u64().unwrap_or_default() > 1);
    let result: Value = serde_json::from_str(status["result"].as_str().unwrap()).unwrap();
    assert_eq!(result["status"], "completed");
    assert_eq!(result["arc_id"], arc_id);
}

#[tokio::test]
async fn bro_arc_cancel_trips_a_parked_wait_arc() {
    // End-to-end cancel: spawn an arc that immediately parks on a
    // long-timeout Wait, cancel it via the SharedState, observe
    // that run() returns with status=cancelled. No LLM dispatch
    // needed — the arc is hook-only and immediately blocks on the
    // wait.
    use crate::workflow::{compile, engine, load_workflow};
    let tmp = tempfile::tempdir().unwrap();
    let server = test_server(&tmp);

    let json = r#"{
        "name": "cancel-smoke",
        "version": 1,
        "actors": {},
        "nodes": {
            "WaitFor": {
                "actor": "",
                "wait": {
                    "any_of": [{"signal": "never-arrives"}],
                    "timeout": "30s"
                },
                "next": {"type": "terminal"}
            }
        },
        "start": "WaitFor"
    }"#;
    let compiled = compile(load_workflow(json).unwrap()).unwrap();

    // Spawn the arc on a background task — it'll park inside the
    // Wait until either the timeout fires or our cancel trips.
    let server_state = server.state.clone();
    let run_handle = tokio::spawn(async move {
        let server2 = BlackboxServer::new(server_state);
        engine::run_workflow_with_initial_vars(
            &server2,
            &compiled,
            None,
            Some(50),
            serde_json::Map::new(),
        )
        .await
    });

    // Give the runner a moment to register the wait + cancel
    // token, then observe the registered token and trip it. Yield
    // a few times to let the task progress past wait registration
    // without hard-coding a timing assumption.
    for _ in 0..50 {
        tokio::task::yield_now().await;
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        let token_count = server.state.arc_cancel_tokens.read().len();
        if token_count > 0 {
            break;
        }
    }

    // Cancel every registered arc (test fixture only spawns one).
    let arc_ids: Vec<String> = server
        .state
        .arc_cancel_tokens
        .read()
        .keys()
        .cloned()
        .collect();
    assert!(
        !arc_ids.is_empty(),
        "expected an arc cancel token to be registered after dispatch"
    );
    for arc_id in &arc_ids {
        let cancelled = server.state.cancel_arc(arc_id);
        assert!(cancelled, "cancel_arc returned false for live arc {arc_id}");
    }

    // The runner should release the wait and return with
    // status=cancelled.
    let result = tokio::time::timeout(std::time::Duration::from_secs(5), run_handle)
        .await
        .expect("runner did not exit within 5s of cancel")
        .expect("runner panicked");
    assert_eq!(result.status, "cancelled", "got: {}", result.status);

    // Token should have been unregistered at terminus.
    assert!(
        server.state.arc_cancel_tokens.read().is_empty(),
        "cancel token still registered after arc terminated"
    );
}

#[test]
fn build_team_advisor_init_prompt_includes_charter_halt_exit_and_status_schema() {
    let tmp = tempfile::tempdir().unwrap();
    let server = test_server(&tmp);
    let team = orchestration::team::Team {
        name: "migration-team".into(),
        teamplate: "tp".into(),
        members: vec![
            orchestration::team::TeamMember {
                name: "executor".into(),
                brofile: "codex-exec".into(),
                session_id: None,
                task_history: vec![],
            },
            orchestration::team::TeamMember {
                name: "reviewer".into(),
                brofile: "claude-review".into(),
                session_id: None,
                task_history: vec![],
            },
        ],
        advisor: None,
        project_dir: None,
        created_at: 0,
    };
    let advisor = orchestration::team::TeamAdvisor {
        name: "lead-advisor".into(),
        config: orchestration::team::TeamAdvisorConfig {
            brofile: "advisor-brofile".into(),
            alias: Some("lead-advisor".into()),
            charter: "keep the migration honest; reject fake phase boundaries".into(),
            context: Some("phase 2 of 3".into()),
            halt_conditions: vec![
                "executor invents a phase boundary that masks coupling".into(),
                "reviewer rubber-stamps a phase without adversarial read".into(),
            ],
            exit_conditions: vec!["all three phases land and are reviewed".into()],
            packet_id: Some("packet-abcdef12".into()),
            timeout_seconds: None,
            mode: orchestration::team::AdvisorMode::Blocking,
        },
        session_id: None,
        task_history: vec![],
    };

    let prompt = server.build_team_advisor_init_prompt(&team, &advisor);

    // Status schema — load-bearing for orchestrator parsing of advisor output.
    assert!(
        prompt.contains("Status: CONTINUE | ESCALATE | CHARTER_DRIFT | EXIT_MET | REPLACE_BRO"),
        "advisor init prompt missing canonical status schema: {prompt}"
    );
    assert!(prompt.contains("Rationale:"), "missing Rationale line");
    assert!(prompt.contains("Next step:"), "missing Next step line");

    // Charter, context, packet_id round-tripped verbatim.
    assert!(prompt.contains("keep the migration honest"));
    assert!(prompt.contains("phase 2 of 3"));
    assert!(prompt.contains("packet-abcdef12"));

    // Every halt and exit condition must survive as its own bullet.
    assert!(prompt.contains("- executor invents a phase boundary that masks coupling"));
    assert!(prompt.contains("- reviewer rubber-stamps a phase without adversarial read"));
    assert!(prompt.contains("- all three phases land and are reviewed"));

    // Team roster surfaces so the advisor knows who it is steering.
    assert!(prompt.contains("executor (codex-exec)"));
    assert!(prompt.contains("reviewer (claude-review)"));
    assert!(prompt.contains("migration-team"));
}

#[test]
fn advisor_checkpoint_serializes_with_packet_entity_shape() {
    let tmp = tempfile::tempdir().unwrap();
    let server = test_server(&tmp);
    let team = orchestration::team::Team {
        name: "demo".into(),
        teamplate: "tp".into(),
        members: vec![],
        advisor: None,
        project_dir: None,
        created_at: 0,
    };
    let checkpoint = server.build_advisor_checkpoint(
        &team,
        "wait",
        &[
            json!({
                "taskId": "task-a",
                "status": "completed",
                "bro": "exec",
                "result": "ok"
            }),
            json!({
                "taskId": "task-b",
                "status": "running",
                "bro": "reviewer",
                "timed_out": true
            }),
        ],
    );
    let serialized = serde_json::to_value(&checkpoint).unwrap();

    // Fields the packet evaluator uses as predicate operands. If any of
    // these drift, every advisor packet in the wild breaks silently.
    for key in [
        "wait_kind",
        "team_name",
        "total_count",
        "completed_count",
        "failed_count",
        "running_count",
        "timed_out_count",
        "blocked_count",
        "dispute_count",
        "done_count",
        "members",
        "notes",
    ] {
        assert!(
            serialized.get(key).is_some(),
            "advisor checkpoint missing packet-facing field '{key}': {serialized}"
        );
    }

    assert_eq!(serialized["total_count"], 2);
    assert_eq!(serialized["completed_count"], 1);
    assert_eq!(serialized["running_count"], 1);
    assert_eq!(serialized["timed_out_count"], 1);
}

#[test]
fn apply_advisor_packet_returns_rule_hit_for_checkpoint_entity() {
    let tmp = tempfile::tempdir().unwrap();
    let server = test_server(&tmp);

    let packet_id = {
        let store = server.state.packets.read();
        let result = store
            .compile(&CompileParams {
                domain: "advisor/demo-escalate".into(),
                classification_lattice: Some(vec!["escalate".into(), "continue".into()]),
                prefix_inference: Some(
                    [
                        ("escalate_".into(), "escalate".into()),
                        ("continue_".into(), "continue".into()),
                    ]
                    .into(),
                ),
                rules: json!([
                    {
                        "id": "escalate_any_blocked",
                        "antecedent": {"op": "Gt", "field": "blocked_count", "value": 0},
                        "consequent": "ESCALATE"
                    },
                    {
                        "id": "continue_default",
                        "classification": "continue",
                        "emit": "fallback",
                        "antecedent": {"op": "True"},
                        "consequent": "CONTINUE"
                    }
                ]),
                scope: Some("global".into()),
                project: None,
                source_ids: None,
                rank_table: None,
                rank_lookup_key: None,
                threshold_table: None,
                threshold_lookup_key: None,
            })
            .unwrap();
        // compile() returns "Packet packet-<id> compiled (...)" — extract id.
        result
            .split_whitespace()
            .find(|tok| tok.starts_with("packet-"))
            .unwrap()
            .to_string()
    };

    let team = orchestration::team::Team {
        name: "t".into(),
        teamplate: "tp".into(),
        members: vec![],
        advisor: None,
        project_dir: None,
        created_at: 0,
    };
    {
        let mut notes = server.state.notes.write();
        notes
            .create(&NoteParams {
                kind: "blocked".into(),
                body: "exec is stuck".into(),
                task_id: Some("task-x".into()),
                session_id: None,
                project: None,
                thread_id: None,
                provider: None,
                bro: Some("exec".into()),
            })
            .unwrap();
    }
    let checkpoint = server.build_advisor_checkpoint(
        &team,
        "wait",
        &[json!({"taskId": "task-x", "status": "running"})],
    );

    let verdict = server
        .apply_advisor_packet(&packet_id, &checkpoint)
        .unwrap();
    assert_eq!(verdict["match"], true);
    assert_eq!(verdict["ruleId"], "escalate_any_blocked");
    assert_eq!(verdict["classification"], "escalate");
}

#[test]
fn arc_bound_warning_fires_on_residue_and_skips_system_ids() {
    let tmp = tempfile::tempdir().unwrap();
    let server = test_server(&tmp);

    {
        let store = server.state.packets.read();
        store
            .compile(&CompileParams {
                domain: "content-classification/arc-bound".into(),
                classification_lattice: Some(vec!["arc_bound".into(), "standing".into()]),
                prefix_inference: Some(
                    [
                        ("arc_".into(), "arc_bound".into()),
                        ("standing_".into(), "standing".into()),
                    ]
                    .into(),
                ),
                rules: json!([
                    {
                        "id": "arc_named_migration",
                        "antecedent": {
                            "op": "StringContains",
                            "field": "content",
                            "needle": "3-tier migration",
                            "case_insensitive": true
                        },
                        "consequent": "ARC_BOUND"
                    },
                    {
                        "id": "standing_catchall",
                        "classification": "standing",
                        "emit": "fallback",
                        "antecedent": {"op": "True"},
                        "consequent": "STANDING"
                    }
                ]),
                scope: Some("global".into()),
                project: None,
                source_ids: None,
                rank_table: None,
                rank_lookup_key: None,
                threshold_table: None,
                threshold_lookup_key: None,
            })
            .unwrap();
    }

    let nag_arc = server.arc_bound_warning(None, "For the 3-tier migration, avoid touching X");
    assert!(
        nag_arc
            .as_deref()
            .is_some_and(|s| s.contains("arc-bound") && s.contains("bbox_pin")),
        "arc-bound content should produce a pin-steering nag: {nag_arc:?}"
    );

    let nag_standing = server.arc_bound_warning(None, "Prefer rustls over openssl");
    assert!(
        nag_standing.is_none(),
        "standing content should not trigger a nag: {nag_standing:?}"
    );

    let nag_system = server.arc_bound_warning(
        Some("bb-tool-reference"),
        "For the 3-tier migration, avoid touching X",
    );
    assert!(
        nag_system.is_none(),
        "system-generated entries must be exempt from the nag: {nag_system:?}"
    );
}

fn seed_test_agent(
    catalog: &artifacts::ArtifactCatalog,
    file_name: &str,
    name: &str,
    version: u64,
    cost_class: Option<&str>,
) {
    let mut manifest = serde_json::json!({
        "description": format!("Test agent {name}."),
        "when_to_use": ["when testing"],
        "brofile_inline": {"provider": "claude"},
    });
    if let Some(cc) = cost_class {
        manifest["cost_class"] = serde_json::json!(cc);
    }
    catalog
        .install_value(
            artifacts::ArtifactKind::Agent,
            file_name.into(),
            &serde_json::json!({
                "kind": "agent",
                "name": name,
                "version": version,
                "manifest": manifest,
            }),
            None,
            None,
            None,
        )
        .unwrap();
}

fn extract_text(result: &CallToolResult) -> String {
    let wire = serde_json::to_value(result).unwrap();
    wire["content"][0]["text"].as_str().unwrap().to_string()
}

#[tokio::test]
async fn badgey_lifecycle_tools_write_thread_events() {
    let _env_guard = codex_bin_test_guard().await;
    let prior_bin = std::env::var("CODEX_BIN").ok();
    let tmp = tempfile::tempdir().unwrap();
    std::env::set_var("CODEX_BIN", fake_codex_bin(&tmp, "codex-session-test"));
    save_badgey_test_brofile(&tmp);
    let server = test_server(&tmp);

    let exec = server
        .badgey_exec(Parameters(BadgeyExecParams {
            project_dir: Some(tmp.path().to_string_lossy().into_owned()),
            brief: Some("answer graph questions".to_string()),
        }))
        .await;
    assert_ne!(exec.is_error, Some(true), "{}", extract_text(&exec));
    let exec_body: Value = serde_json::from_str(&extract_text(&exec)).unwrap();
    let badgey_id = exec_body["badgey_id"].as_str().unwrap().to_string();
    let task_id = exec_body["task_id"].as_str().unwrap().to_string();
    let thread_id = exec_body["thread_id"].as_str().unwrap().to_string();
    assert_eq!(exec_body["session_id"], "codex-session-test");
    assert!(server.state.task_store.read().get(&task_id).is_some());

    let resume = server
        .badgey_resume(Parameters(BadgeyResumeParams {
            badgey_id: badgey_id.clone(),
            prompt: "ping".to_string(),
            timeout_seconds: Some(2.0),
        }))
        .await;
    assert_ne!(resume.is_error, Some(true), "{}", extract_text(&resume));

    let dismiss = server.badgey_dismiss(Parameters(BadgeyDismissParams {
        badgey_id: badgey_id.clone(),
        reason: Some("done".to_string()),
    }));
    assert_ne!(dismiss.is_error, Some(true), "{}", extract_text(&dismiss));

    let notes = server.state.notes.read();
    let bodies: Vec<_> = notes
        .all()
        .iter()
        .filter(|note| note.thread_id.as_deref() == Some(thread_id.as_str()))
        .map(|note| note.body.as_str())
        .collect();
    assert!(bodies.iter().any(|body| body.contains(r#""event":"exec""#)
        && body.contains(r#""provider_session_id":"codex-session-test""#)));
    assert!(bodies.iter().any(|body| body.contains(r#""event":"turn""#)));
    assert!(bodies
        .iter()
        .any(|body| body.contains(r#""event":"dismiss""#)));

    match prior_bin {
        Some(value) => std::env::set_var("CODEX_BIN", value),
        None => std::env::remove_var("CODEX_BIN"),
    }
}

#[tokio::test]
async fn badgey_agent_dispatch_routes_through_wrapper_adapter() {
    let _env_guard = codex_bin_test_guard().await;
    let prior_bin = std::env::var("CODEX_BIN").ok();
    let tmp = tempfile::tempdir().unwrap();
    std::env::set_var("CODEX_BIN", fake_codex_bin(&tmp, "codex-session-test"));
    save_badgey_test_brofile(&tmp);
    let server = test_server(&tmp);
    server
        .state
        .agent_adapter_registry
        .write()
        .register(std::sync::Arc::new(BadgeyAgentAdapter {
            state: server.state.clone(),
        }));
    server
        .state
        .artifacts
        .read()
        .install_value(
            artifacts::ArtifactKind::Agent,
            "badgey.json".into(),
            &serde_json::json!({
                "kind": "agent",
                "name": "badgey",
                "version": 1,
                "manifest": {
                    "description": "Badgey test agent.",
                    "dispatch_adapter": "badgey",
                    "brofile_ref": "badgey-persona"
                }
            }),
            None,
            None,
            None,
        )
        .unwrap();

    let result = server
        .bro_agent_dispatch(Parameters(AgentDispatchParams {
            agent: "badgey".into(),
            args: serde_json::json!({"prompt": "advise"}),
            project_dir: Some(tmp.path().to_string_lossy().into_owned()),
            bro: None,
            ambient: None,
            caller_provider: None,
            caller_session_id: None,
        }))
        .await;
    assert_ne!(result.is_error, Some(true), "{}", extract_text(&result));
    let body: Value = serde_json::from_str(&extract_text(&result)).unwrap();
    assert_eq!(body["session"]["provider"], "codex");
    assert_eq!(body["session"]["session_id"], "codex-session-test");
    assert_eq!(body["resolved_brofile"], "badgey-persona");
    assert_eq!(
        body["merged_filters"]["disallow"][0],
        "mcp__blackbox__bro_*"
    );
    assert_eq!(server.state.badgey_registry.list().len(), 1);

    match prior_bin {
        Some(value) => std::env::set_var("CODEX_BIN", value),
        None => std::env::remove_var("CODEX_BIN"),
    }
}

#[tokio::test]
async fn badgey_post_processor_records_emit_proposal_actions_once() {
    let tmp = tempfile::tempdir().unwrap();
    let server = test_server(&tmp);
    let id: orchestration::badgey::types::BadgeyId = "bg-0123abcd-4567ef89".parse().unwrap();
    let instance = orchestration::badgey::registry::BadgeyInstance::new(
        id.clone(),
        orchestration::badgey::types::BadgeyScope {
            project_id: tmp.path().to_string_lossy().into_owned(),
            initial_brief: None,
        },
        Provider::Codex,
        "codex-session-3".to_string(),
        "thread-00000001".to_string(),
    );
    server
        .state
        .badgey_registry
        .register(instance.clone())
        .unwrap();
    let action_id = uuid::Uuid::new_v4().to_string();
    server
        .state
        .notes
        .write()
        .create(&notes::NoteParams {
            kind: "followup".to_string(),
            body: serde_json::json!({
                "event": "bg-action-emit-proposal",
                "action_id": action_id,
                "kind": "agent",
                "draft": {"source": "draft-agent.json", "name": "draft-agent"}
            })
            .to_string(),
            task_id: None,
            session_id: Some("codex-session-3".to_string()),
            project: Some(tmp.path().to_string_lossy().into_owned()),
            thread_id: Some("thread-00000001".to_string()),
            provider: Some("codex".to_string()),
            bro: Some("badgey".to_string()),
        })
        .unwrap();

    let results = server
        .badgey_post_process_turn(&instance, "1970-01-01T00:00:00Z")
        .await
        .unwrap();
    assert_eq!(results.len(), 1);
    assert_eq!(results[0]["proposal_id"], "P-1");
    let proposals = server.state.badgey_proposals.list_by_instance(&id).unwrap();
    assert_eq!(proposals.len(), 1);
    assert_eq!(
        proposals[0].kind,
        orchestration::badgey::types::ProposalKind::Agent
    );
    assert!(matches!(
        server
            .state
            .badgey_journal
            .list_non_terminal()
            .unwrap()
            .as_slice(),
        []
    ));

    let replay = server
        .badgey_post_process_turn(&instance, "1970-01-01T00:00:00Z")
        .await
        .unwrap();
    assert_eq!(replay.len(), 1);
    assert_eq!(
        server
            .state
            .badgey_proposals
            .list_by_instance(&id)
            .unwrap()
            .len(),
        1
    );
}

#[tokio::test]
async fn badgey_post_processor_marks_bad_actions_failed() {
    let tmp = tempfile::tempdir().unwrap();
    let server = test_server(&tmp);
    let id: orchestration::badgey::types::BadgeyId = "bg-0123abcd-4567ef89".parse().unwrap();
    let instance = orchestration::badgey::registry::BadgeyInstance::new(
        id.clone(),
        orchestration::badgey::types::BadgeyScope {
            project_id: tmp.path().to_string_lossy().into_owned(),
            initial_brief: None,
        },
        Provider::Codex,
        "codex-session-4".to_string(),
        "thread-00000001".to_string(),
    );
    server
        .state
        .badgey_registry
        .register(instance.clone())
        .unwrap();
    let action_id = uuid::Uuid::new_v4().to_string();
    server
        .state
        .notes
        .write()
        .create(&notes::NoteParams {
            kind: "followup".to_string(),
            body: serde_json::json!({
                "event": "bg-action-emit-proposal",
                "action_id": action_id,
                "draft": {"source": "missing-kind.json"}
            })
            .to_string(),
            task_id: None,
            session_id: Some("codex-session-4".to_string()),
            project: Some(tmp.path().to_string_lossy().into_owned()),
            thread_id: Some("thread-00000001".to_string()),
            provider: Some("codex".to_string()),
            bro: Some("badgey".to_string()),
        })
        .unwrap();

    let results = server
        .badgey_post_process_turn(&instance, "1970-01-01T00:00:00Z")
        .await
        .unwrap();
    assert_eq!(results[0]["status"], "failed");
    let failure_notes: Vec<_> = server
        .state
        .notes
        .read()
        .all()
        .iter()
        .filter(|note| note.body.contains("bg-action-failed"))
        .cloned()
        .collect();
    assert_eq!(failure_notes.len(), 1);
}

#[tokio::test]
async fn badgey_apply_and_reject_commands_update_proposal_store() {
    let tmp = tempfile::tempdir().unwrap();
    let server = test_server(&tmp);
    let id: orchestration::badgey::types::BadgeyId = "bg-0123abcd-4567ef89".parse().unwrap();
    let instance = orchestration::badgey::registry::BadgeyInstance::new(
        id.clone(),
        orchestration::badgey::types::BadgeyScope {
            project_id: tmp.path().to_string_lossy().into_owned(),
            initial_brief: None,
        },
        Provider::Codex,
        "codex-session-5".to_string(),
        "thread-00000001".to_string(),
    );
    server.state.badgey_registry.register(instance).unwrap();
    let draft_path = tmp.path().join("new-bro.json");
    std::fs::write(
        &draft_path,
        serde_json::json!({
            "name": "new-bro",
            "version": 1,
            "provider": "codex"
        })
        .to_string(),
    )
    .unwrap();
    let apply_proposal = server
        .state
        .badgey_proposals
        .create(
            &id,
            orchestration::badgey::types::ProposalKind::Brofile,
            serde_json::json!({"source": draft_path.to_string_lossy()}),
            None,
        )
        .unwrap();
    let reject_proposal = server
        .state
        .badgey_proposals
        .create(
            &id,
            orchestration::badgey::types::ProposalKind::Agent,
            serde_json::json!({"source": "not-used.json"}),
            None,
        )
        .unwrap();

    let apply = server
        .badgey_resume(Parameters(BadgeyResumeParams {
            badgey_id: id.to_string(),
            prompt: format!("apply {}", apply_proposal.id),
            timeout_seconds: None,
        }))
        .await;
    assert_ne!(apply.is_error, Some(true), "{}", extract_text(&apply));
    assert!(
        orchestration::brofile::resolve_brofile("new-bro", &tmp.path().join("bro"), None).is_some()
    );
    assert_eq!(
        server
            .state
            .badgey_proposals
            .get(&id, &apply_proposal.id)
            .unwrap()
            .unwrap()
            .state,
        orchestration::badgey::types::ProposalState::Applied
    );

    let reject = server
        .badgey_resume(Parameters(BadgeyResumeParams {
            badgey_id: id.to_string(),
            prompt: format!("reject {}", reject_proposal.id),
            timeout_seconds: None,
        }))
        .await;
    assert_ne!(reject.is_error, Some(true), "{}", extract_text(&reject));
    assert_eq!(
        server
            .state
            .badgey_proposals
            .get(&id, &reject_proposal.id)
            .unwrap()
            .unwrap()
            .state,
        orchestration::badgey::types::ProposalState::Failed
    );
}

#[test]
fn badgey_restart_replay_restores_registry_from_thread_notes() {
    let tmp = tempfile::tempdir().unwrap();
    let server = test_server(&tmp);
    let id: orchestration::badgey::types::BadgeyId = "bg-0123abcd-4567ef89".parse().unwrap();
    let thread_result = server
        .state
        .threads
        .write()
        .thread(&threads::ThreadParams {
            action: "open".to_string(),
            name: Some(format!("badgey:{id}")),
            id: None,
            topic: Some("Badgey replay".to_string()),
            project: Some(tmp.path().to_string_lossy().into_owned()),
            session_id: None,
            provider: None,
            session_name: None,
            handoff_doc: None,
            note: None,
            target: None,
            target_type: None,
            edge: None,
            promoted_to: None,
            kind: Some("work_item".to_string()),
        })
        .unwrap();
    let thread_id = server
        .badgey_thread_id_from_open_result(&thread_result)
        .unwrap();
    let scope = orchestration::badgey::types::BadgeyScope {
        project_id: tmp.path().to_string_lossy().into_owned(),
        initial_brief: Some("replay".to_string()),
    };
    server
        .state
        .notes
        .write()
        .create(&notes::NoteParams {
            kind: "learned".to_string(),
            body: serde_json::to_string(&orchestration::badgey::events::ThreadEvent::Exec {
                brofile_version: "badgey-persona".to_string(),
                scope: scope.clone(),
                charter: "replay".to_string(),
                provider: Provider::Codex,
                provider_session_id: "codex-session-6".to_string(),
            })
            .unwrap(),
            task_id: None,
            session_id: Some("codex-session-6".to_string()),
            project: Some(scope.project_id.clone()),
            thread_id: Some(thread_id.clone()),
            provider: Some("codex".to_string()),
            bro: Some("badgey".to_string()),
        })
        .unwrap();

    restore_badgey_registry_from_notes(&server.state);
    let restored = server.state.badgey_registry.get(&id).unwrap();
    assert_eq!(restored.thread_of_record_id, thread_id);
    assert_eq!(restored.provider_session_id, "codex-session-6");
}

#[test]
fn badgey_restart_replay_skips_unobserved_pending_session() {
    let tmp = tempfile::tempdir().unwrap();
    let server = test_server(&tmp);
    let id: orchestration::badgey::types::BadgeyId = "bg-0123abcd-4567ef89".parse().unwrap();
    let thread_result = server
        .state
        .threads
        .write()
        .thread(&threads::ThreadParams {
            action: "open".to_string(),
            name: Some(format!("badgey:{id}")),
            id: None,
            topic: Some("Badgey pending replay".to_string()),
            project: Some(tmp.path().to_string_lossy().into_owned()),
            session_id: None,
            provider: None,
            session_name: None,
            handoff_doc: None,
            note: None,
            target: None,
            target_type: None,
            edge: None,
            promoted_to: None,
            kind: Some("work_item".to_string()),
        })
        .unwrap();
    let thread_id = server
        .badgey_thread_id_from_open_result(&thread_result)
        .unwrap();
    let scope = orchestration::badgey::types::BadgeyScope {
        project_id: tmp.path().to_string_lossy().into_owned(),
        initial_brief: Some("pending replay".to_string()),
    };
    server
        .state
        .notes
        .write()
        .create(&notes::NoteParams {
            kind: "learned".to_string(),
            body: serde_json::to_string(&orchestration::badgey::events::ThreadEvent::Exec {
                brofile_version: "badgey-persona".to_string(),
                scope: scope.clone(),
                charter: "pending replay".to_string(),
                provider: Provider::Codex,
                provider_session_id: "pending".to_string(),
            })
            .unwrap(),
            task_id: None,
            session_id: None,
            project: Some(scope.project_id.clone()),
            thread_id: Some(thread_id),
            provider: Some("codex".to_string()),
            bro: Some("badgey".to_string()),
        })
        .unwrap();

    restore_badgey_registry_from_notes(&server.state);
    assert!(server.state.badgey_registry.get(&id).is_err());
    assert!(server.state.notes.read().all().iter().any(|note| note.kind
        == notes::NoteKind::Surprise
        && note
            .body
            .contains("badgey_restore_skipped_unobserved_session")));
}

#[test]
fn badgey_collect_waits_for_done_note_after_spawn() {
    let tmp = tempfile::tempdir().unwrap();
    let server = test_server(&tmp);
    let id: orchestration::badgey::types::BadgeyId = "bg-0123abcd-4567ef89".parse().unwrap();
    let instance = orchestration::badgey::registry::BadgeyInstance::new(
        id.clone(),
        orchestration::badgey::types::BadgeyScope {
            project_id: tmp.path().to_string_lossy().into_owned(),
            initial_brief: None,
        },
        Provider::Codex,
        "codex-session-7".to_string(),
        "thread-00000001".to_string(),
    );
    server
        .state
        .badgey_registry
        .register(instance.clone())
        .unwrap();
    server
        .badgey_write_event(
            &instance,
            orchestration::badgey::events::ThreadEvent::SubbroSpawned {
                task_id: "task-1".to_string(),
                scout_id: "scout-1".to_string(),
                charter: "look".to_string(),
            },
            Some("task-1".to_string()),
        )
        .unwrap();
    server
        .badgey_write_event(
            &instance,
            orchestration::badgey::events::ThreadEvent::SubbroSpawned {
                task_id: "task-2".to_string(),
                scout_id: "scout-1".to_string(),
                charter: "compare".to_string(),
            },
            Some("task-2".to_string()),
        )
        .unwrap();

    let walking = server
        .badgey_collect_internal(Some("scout-1"), Some(id.as_str()))
        .unwrap();
    assert_eq!(walking["status"], "still_walking");

    server
        .state
        .notes
        .write()
        .create(&notes::NoteParams {
            kind: "done".to_string(),
            body: serde_json::json!({
                "scout_id": "scout-1",
                "verdict": "found",
                "summary": "done"
            })
            .to_string(),
            task_id: Some("task-1".to_string()),
            session_id: Some("codex-session-7".to_string()),
            project: Some(tmp.path().to_string_lossy().into_owned()),
            thread_id: Some("thread-00000001".to_string()),
            provider: Some("codex".to_string()),
            bro: Some("badgey-scout".to_string()),
        })
        .unwrap();
    let done = server
        .badgey_collect_internal(Some("scout-1"), Some(id.as_str()))
        .unwrap();
    assert_eq!(done["status"], "still_walking");

    server
        .state
        .notes
        .write()
        .create(&notes::NoteParams {
            kind: "done".to_string(),
            body: serde_json::json!({
                "scout_id": "scout-1",
                "verdict": "found",
                "summary": "done 2"
            })
            .to_string(),
            task_id: Some("task-2".to_string()),
            session_id: Some("codex-session-7".to_string()),
            project: Some(tmp.path().to_string_lossy().into_owned()),
            thread_id: Some("thread-00000001".to_string()),
            provider: Some("codex".to_string()),
            bro: Some("badgey-scout".to_string()),
        })
        .unwrap();
    let done = server
        .badgey_collect_internal(Some("scout-1"), Some(id.as_str()))
        .unwrap();
    assert_eq!(done["status"], "done");
}

#[test]
fn badgey_triage_attached_to_instance_stores_redispatch_proposals() {
    let tmp = tempfile::tempdir().unwrap();
    let server = test_server(&tmp);
    let id: orchestration::badgey::types::BadgeyId = "bg-0123abcd-4567ef89".parse().unwrap();
    let instance = orchestration::badgey::registry::BadgeyInstance::new(
        id.clone(),
        orchestration::badgey::types::BadgeyScope {
            project_id: tmp.path().to_string_lossy().into_owned(),
            initial_brief: None,
        },
        Provider::Codex,
        "codex-session-8".to_string(),
        "thread-00000001".to_string(),
    );
    server.state.badgey_registry.register(instance).unwrap();
    let thread_result = server
        .state
        .threads
        .write()
        .thread(&threads::ThreadParams {
            action: "open".to_string(),
            name: Some("stale-work".to_string()),
            id: None,
            topic: Some("stale work".to_string()),
            project: Some(tmp.path().to_string_lossy().into_owned()),
            session_id: None,
            provider: None,
            session_name: None,
            handoff_doc: None,
            note: None,
            target: None,
            target_type: None,
            edge: None,
            promoted_to: None,
            kind: Some("work_item".to_string()),
        })
        .unwrap();
    let thread_id = server
        .badgey_thread_id_from_open_result(&thread_result)
        .unwrap();

    let triage = server
        .badgey_triage_inbox_internal(
            Some(tmp.path().to_string_lossy().into_owned()),
            None,
            Some(id.to_string()),
        )
        .unwrap();
    assert_eq!(triage["proposal_sheet"]["proposals"][0]["stored"], true);
    let proposals = server.state.badgey_proposals.list_by_instance(&id).unwrap();
    assert_eq!(proposals.len(), 1);
    assert_eq!(
        proposals[0].kind,
        orchestration::badgey::types::ProposalKind::RedispatchTask
    );
    assert_eq!(
        proposals[0].idempotency_key.as_deref(),
        Some(format!("triage:{thread_id}").as_str())
    );
}

#[test]
fn badgey_startup_recovery_fails_orphaned_non_terminal_state() {
    let tmp = tempfile::tempdir().unwrap();
    let server = test_server(&tmp);
    let action_id = orchestration::badgey::types::ActionId::new_v4();
    server
        .state
        .badgey_journal
        .record_seen(
            action_id.clone(),
            "bg-action-spawn-subbro".to_string(),
            serde_json::json!({"event": "bg-action-spawn-subbro"}),
        )
        .unwrap();
    let id: orchestration::badgey::types::BadgeyId = "bg-0123abcd-4567ef89".parse().unwrap();
    let proposal = server
        .state
        .badgey_proposals
        .create(
            &id,
            orchestration::badgey::types::ProposalKind::RedispatchTask,
            serde_json::json!({"prompt": "retry"}),
            Some("redispatch-task-1".to_string()),
        )
        .unwrap();
    server
        .state
        .badgey_proposals
        .transition(
            &id,
            &proposal.id,
            orchestration::badgey::types::ProposalState::Pending,
            orchestration::badgey::types::ProposalState::Applying,
            None,
        )
        .unwrap();

    recover_badgey_non_terminal_state(&server.state);
    assert!(matches!(
        server
            .state
            .badgey_journal
            .get(&action_id)
            .unwrap()
            .unwrap()
            .state,
        orchestration::badgey::types::ActionJournalState::Failed { .. }
    ));
    assert_eq!(
        server
            .state
            .badgey_proposals
            .get(&id, &proposal.id)
            .unwrap()
            .unwrap()
            .state,
        orchestration::badgey::types::ProposalState::Failed
    );
}

#[test]
fn bro_agent_list_output_shape() {
    let tmp = tempfile::tempdir().unwrap();
    let server = test_server(&tmp);
    seed_test_agent(
        &server.state.artifacts.read(),
        "reviewer.json",
        "reviewer",
        1,
        Some("expensive"),
    );
    seed_test_agent(
        &server.state.artifacts.read(),
        "writer.json",
        "writer",
        2,
        Some("cheap"),
    );

    let result = server.bro_agent_list(Parameters(AgentListParams {
        include_superseded: None,
        cost_class: None,
        provenance_kind: None,
        limit: None,
    }));
    assert_ne!(result.is_error, Some(true));

    let body: serde_json::Value = serde_json::from_str(&extract_text(&result)).unwrap();
    let agents = body["agents"].as_array().unwrap();
    assert_eq!(agents.len(), 2);

    let reviewer = agents.iter().find(|a| a["name"] == "reviewer").unwrap();
    assert_eq!(reviewer["version"], "1");
    assert_eq!(reviewer["active"], true);
    assert_eq!(reviewer["cost_class"], "expensive");
    assert_eq!(reviewer["embedding_pending"], true);

    let writer = agents.iter().find(|a| a["name"] == "writer").unwrap();
    assert_eq!(writer["version"], "2");
    assert_eq!(writer["cost_class"], "cheap");
}

#[test]
fn bro_agent_list_invalid_cost_class_is_error() {
    let tmp = tempfile::tempdir().unwrap();
    let server = test_server(&tmp);

    let result = server.bro_agent_list(Parameters(AgentListParams {
        include_superseded: None,
        cost_class: Some("notavalidclass".into()),
        provenance_kind: None,
        limit: None,
    }));
    assert_eq!(result.is_error, Some(true));
    let text = extract_text(&result);
    assert!(text.contains("unknown cost_class"), "got: {text}");
}

#[test]
fn bro_agent_get_found() {
    let tmp = tempfile::tempdir().unwrap();
    let server = test_server(&tmp);
    seed_test_agent(
        &server.state.artifacts.read(),
        "reviewer.json",
        "reviewer",
        3,
        None,
    );

    let result = server.bro_agent_get(Parameters(AgentGetParams {
        name: "reviewer".into(),
    }));
    assert_ne!(result.is_error, Some(true));

    let body: serde_json::Value = serde_json::from_str(&extract_text(&result)).unwrap();
    assert_eq!(body["name"], "reviewer");
    assert_eq!(body["version"], "3");
    assert_eq!(body["active"], true);
    assert!(body["manifest"].is_object());
}

#[test]
fn bro_agent_get_missing_is_error() {
    let tmp = tempfile::tempdir().unwrap();
    let server = test_server(&tmp);

    let result = server.bro_agent_get(Parameters(AgentGetParams {
        name: "nonexistent".into(),
    }));
    assert_eq!(result.is_error, Some(true));
    let text = extract_text(&result);
    assert!(text.contains("agent not found"), "got: {text}");
}

#[test]
fn bro_agent_get_pinned_ref() {
    let tmp = tempfile::tempdir().unwrap();
    let server = test_server(&tmp);
    seed_test_agent(
        &server.state.artifacts.read(),
        "reviewer.json",
        "reviewer",
        5,
        None,
    );

    let result = server.bro_agent_get(Parameters(AgentGetParams {
        name: "reviewer@v5".into(),
    }));
    assert_ne!(result.is_error, Some(true));

    let body: serde_json::Value = serde_json::from_str(&extract_text(&result)).unwrap();
    assert_eq!(body["version"], "5");
}

#[test]
fn bro_agent_get_invalid_ref_is_error() {
    let tmp = tempfile::tempdir().unwrap();
    let server = test_server(&tmp);

    let result = server.bro_agent_get(Parameters(AgentGetParams { name: "@v2".into() }));
    assert_eq!(result.is_error, Some(true));
    let text = extract_text(&result);
    assert!(text.contains("requires a name"), "got: {text}");
}

fn seed_test_agent_with_provenance(
    catalog: &artifacts::ArtifactCatalog,
    file_name: &str,
    name: &str,
    version: u64,
    provenance: serde_json::Value,
) {
    let manifest = serde_json::json!({
        "description": format!("Agent {name} with provenance."),
        "when_to_use": ["when testing"],
        "brofile_inline": {"provider": "claude"},
        "provenance": provenance,
    });
    catalog
        .install_value(
            artifacts::ArtifactKind::Agent,
            file_name.into(),
            &serde_json::json!({
                "kind": "agent",
                "name": name,
                "version": version,
                "manifest": manifest,
            }),
            None,
            None,
            None,
        )
        .unwrap();
}

#[test]
fn bro_agent_list_limit_caps_output() {
    let tmp = tempfile::tempdir().unwrap();
    let server = test_server(&tmp);
    let cat = &server.state.artifacts.read();
    seed_test_agent(cat, "a1.json", "alpha", 1, None);
    seed_test_agent(cat, "a2.json", "beta", 1, None);
    seed_test_agent(cat, "a3.json", "gamma", 1, None);

    let result = server.bro_agent_list(Parameters(AgentListParams {
        include_superseded: None,
        cost_class: None,
        provenance_kind: None,
        limit: Some(2),
    }));
    assert_ne!(result.is_error, Some(true));
    let body: serde_json::Value = serde_json::from_str(&extract_text(&result)).unwrap();
    assert_eq!(body["agents"].as_array().unwrap().len(), 2);
}

#[test]
fn bro_agent_list_provenance_kind_filter() {
    let tmp = tempfile::tempdir().unwrap();
    let server = test_server(&tmp);
    let cat = &server.state.artifacts.read();
    seed_test_agent(cat, "hand.json", "handmade", 1, None);
    seed_test_agent_with_provenance(
        cat,
        "distilled.json",
        "distilled",
        1,
        serde_json::json!({"kind": "distilled", "distilled_by": "badgey-01", "evidence_session_ids": [], "created_from_threads": [], "accept_count": 5, "reject_count": 0}),
    );

    let result = server.bro_agent_list(Parameters(AgentListParams {
        include_superseded: None,
        cost_class: None,
        provenance_kind: Some("distilled".into()),
        limit: None,
    }));
    assert_ne!(result.is_error, Some(true));
    let body: serde_json::Value = serde_json::from_str(&extract_text(&result)).unwrap();
    let agents = body["agents"].as_array().unwrap();
    assert_eq!(agents.len(), 1);
    assert_eq!(agents[0]["name"], "distilled");
}

#[test]
fn bro_agent_get_pinned_version_mismatch() {
    let tmp = tempfile::tempdir().unwrap();
    let server = test_server(&tmp);
    seed_test_agent(
        &server.state.artifacts.read(),
        "reviewer.json",
        "reviewer",
        5,
        None,
    );

    let result = server.bro_agent_get(Parameters(AgentGetParams {
        name: "reviewer@v4".into(),
    }));
    assert_eq!(result.is_error, Some(true));
    let text = extract_text(&result);
    assert!(text.contains("agent not found"), "got: {text}");
}

#[test]
fn bro_agent_list_include_superseded() {
    let tmp = tempfile::tempdir().unwrap();
    let server = test_server(&tmp);
    let cat = &server.state.artifacts.read();
    seed_test_agent(cat, "old-agent.json", "old-agent", 1, None);
    cat.install_value(
        artifacts::ArtifactKind::Agent,
        "new-agent.json".into(),
        &serde_json::json!({
            "kind": "agent",
            "name": "new-agent",
            "version": 1,
            "supersedes": "old-agent",
            "manifest": {
                "description": "Replacement for old-agent.",
                "when_to_use": ["when testing"],
                "brofile_inline": {"provider": "claude"},
            },
        }),
        None,
        None,
        Some("old-agent".into()),
    )
    .unwrap();

    let default_result = server.bro_agent_list(Parameters(AgentListParams {
        include_superseded: None,
        cost_class: None,
        provenance_kind: None,
        limit: None,
    }));
    assert_ne!(default_result.is_error, Some(true));
    let default_body: serde_json::Value =
        serde_json::from_str(&extract_text(&default_result)).unwrap();
    let default_agents = default_body["agents"].as_array().unwrap();
    let default_names: Vec<&str> = default_agents
        .iter()
        .map(|a| a["name"].as_str().unwrap())
        .collect();
    assert!(
        !default_names.contains(&"old-agent"),
        "superseded agent should be excluded by default: {default_names:?}"
    );
    assert!(
        default_names.contains(&"new-agent"),
        "active agent should be included: {default_names:?}"
    );

    let with_superseded = server.bro_agent_list(Parameters(AgentListParams {
        include_superseded: Some(true),
        cost_class: None,
        provenance_kind: None,
        limit: None,
    }));
    assert_ne!(with_superseded.is_error, Some(true));
    let all_body: serde_json::Value =
        serde_json::from_str(&extract_text(&with_superseded)).unwrap();
    let all_agents = all_body["agents"].as_array().unwrap();
    let all_names: Vec<&str> = all_agents
        .iter()
        .map(|a| a["name"].as_str().unwrap())
        .collect();
    assert!(
        all_names.contains(&"old-agent"),
        "superseded agent should appear with include_superseded=true: {all_names:?}"
    );
    let old = all_agents
        .iter()
        .find(|a| a["name"] == "old-agent")
        .unwrap();
    assert_eq!(old["active"], false);
}

#[test]
fn bro_agent_describe_inline_brofile() {
    let tmp = tempfile::tempdir().unwrap();
    let server = test_server(&tmp);
    seed_test_agent(
        &server.state.artifacts.read(),
        "reviewer.json",
        "reviewer",
        1,
        Some("normal"),
    );

    let result = server.bro_agent_describe(Parameters(AgentDescribeParams {
        agent: "reviewer".into(),
    }));
    assert_ne!(result.is_error, Some(true));
    let body: serde_json::Value = serde_json::from_str(&extract_text(&result)).unwrap();
    assert_eq!(body["name"], "reviewer");
    assert_eq!(body["version"], "1");
    assert_eq!(body["active"], true);
    assert_eq!(body["brofile_kind"], "inline");
    assert_eq!(body["brofile_provider"], "claude");
    assert_eq!(body["embedding_status"], "pending");
    assert!(body["manifest"].is_object());
    assert!(body["merged_filters"].is_object());
    assert!(body["brofile"].is_object());
    assert_eq!(body["brofile"]["provider"], "claude");
    assert!(body["install_warnings"].as_array().unwrap().is_empty());
}

#[test]
fn bro_agent_describe_inline_brofile_filters_in_merge() {
    let tmp = tempfile::tempdir().unwrap();
    let server = test_server(&tmp);
    let cat = &server.state.artifacts.read();
    let manifest = serde_json::json!({
        "description": "Agent with inline brofile filters.",
        "when_to_use": ["when testing"],
        "brofile_inline": {
            "provider": "claude",
            "filters": {
                "allow": ["mcp__blackbox__bbox_search"],
                "disallow": ["mcp__blackbox__bro_exec"]
            }
        },
    });
    cat.install_value(
        artifacts::ArtifactKind::Agent,
        "inline-filtered.json".into(),
        &serde_json::json!({
            "kind": "agent",
            "name": "inline-filtered",
            "version": 1,
            "manifest": manifest,
        }),
        None,
        None,
        None,
    )
    .unwrap();

    let result = server.bro_agent_describe(Parameters(AgentDescribeParams {
        agent: "inline-filtered".into(),
    }));
    assert_ne!(result.is_error, Some(true));
    let body: serde_json::Value = serde_json::from_str(&extract_text(&result)).unwrap();
    let allow = body["merged_filters"]["allow"].as_array().unwrap();
    let disallow = body["merged_filters"]["disallow"].as_array().unwrap();
    assert!(
        allow
            .iter()
            .any(|p| p.as_str() == Some("mcp__blackbox__bbox_search")),
        "inline allow should appear in merged: {allow:?}"
    );
    assert!(
        disallow
            .iter()
            .any(|p| p.as_str() == Some("mcp__blackbox__bro_exec")),
        "inline disallow should appear in merged: {disallow:?}"
    );
}

#[test]
fn bro_agent_describe_missing_is_error() {
    let tmp = tempfile::tempdir().unwrap();
    let server = test_server(&tmp);

    let result = server.bro_agent_describe(Parameters(AgentDescribeParams {
        agent: "nonexistent".into(),
    }));
    assert_eq!(result.is_error, Some(true));
    let text = extract_text(&result);
    assert!(text.contains("agent not found"), "got: {text}");
}

#[test]
fn bro_agent_describe_deny_wins_conflict() {
    let tmp = tempfile::tempdir().unwrap();
    let server = test_server(&tmp);
    let cat = &server.state.artifacts.read();
    let manifest = serde_json::json!({
        "description": "Agent where overlay allows what brofile denies.",
        "when_to_use": ["when testing"],
        "brofile_inline": {
            "provider": "claude",
            "filters": {
                "allow": ["mcp__blackbox__bbox_search"],
                "disallow": ["mcp__blackbox__bro_exec", "mcp__blackbox__bbox_cite"]
            }
        },
        "filter_overlay": {
            "allow": ["mcp__blackbox__bro_exec", "mcp__blackbox__bbox_cite"],
            "disallow": []
        },
    });
    cat.install_value(
        artifacts::ArtifactKind::Agent,
        "conflict.json".into(),
        &serde_json::json!({
            "kind": "agent",
            "name": "conflict",
            "version": 1,
            "manifest": manifest,
        }),
        None,
        None,
        None,
    )
    .unwrap();

    let result = server.bro_agent_describe(Parameters(AgentDescribeParams {
        agent: "conflict".into(),
    }));
    assert_ne!(result.is_error, Some(true));
    let body: serde_json::Value = serde_json::from_str(&extract_text(&result)).unwrap();
    let allow = body["merged_filters"]["allow"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|v| v.as_str())
        .collect::<Vec<_>>();
    let disallow = body["merged_filters"]["disallow"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|v| v.as_str())
        .collect::<Vec<_>>();
    assert!(
        !allow.contains(&"mcp__blackbox__bro_exec"),
        "overlay allow should be stripped by deny-wins: {allow:?}"
    );
    assert!(
        !allow.contains(&"mcp__blackbox__bbox_cite"),
        "overlay allow for cite should be stripped by deny-wins: {allow:?}"
    );
    assert!(
        allow.contains(&"mcp__blackbox__bbox_search"),
        "non-conflicting allow should survive: {allow:?}"
    );
    assert!(
        disallow.contains(&"mcp__blackbox__bro_exec"),
        "disallow should win: {disallow:?}"
    );
    assert!(
        disallow.contains(&"mcp__blackbox__bbox_cite"),
        "disallow should win for cite too: {disallow:?}"
    );
}

#[tokio::test]
async fn agent_install_records_filter_conflict_warning() {
    let tmp = tempfile::tempdir().unwrap();
    let server = test_server(&tmp);
    install_artifact_value(
        &server.state,
        artifacts::ArtifactInstallParams {
            kind: artifacts::ArtifactKind::Agent,
            source: "inline".into(),
            name: None,
            version: None,
            supersedes: None,
        },
        serde_json::json!({
            "kind": "agent",
            "name": "warning-agent",
            "version": 1,
            "manifest": {
                "description": "Agent where overlay conflicts are recorded.",
                "when_to_use": ["when testing filter warnings"],
                "brofile_inline": {
                    "provider": "claude",
                    "filters": {
                        "allow": ["Read"],
                        "disallow": ["Bash"]
                    }
                },
                "filter_overlay": {
                    "allow": ["Bash"],
                    "disallow": ["Read"]
                }
            }
        }),
    )
    .await
    .unwrap();

    let result = server.bro_agent_describe(Parameters(AgentDescribeParams {
        agent: "warning-agent".into(),
    }));
    assert_ne!(result.is_error, Some(true));
    let body: serde_json::Value = serde_json::from_str(&extract_text(&result)).unwrap();
    let warnings = body["install_warnings"].as_array().unwrap();
    assert_eq!(warnings.len(), 2, "warnings: {warnings:?}");
    assert!(
        warnings
            .iter()
            .any(|w| w.as_str().is_some_and(|s| s.contains("Bash"))),
        "allow/disallow conflict warning missing: {warnings:?}"
    );
    assert!(
        warnings
            .iter()
            .any(|w| w.as_str().is_some_and(|s| s.contains("Read"))),
        "disallow/allow conflict warning missing: {warnings:?}"
    );
}

#[test]
fn bro_agent_describe_brofile_ref_resolved() {
    let tmp = tempfile::tempdir().unwrap();
    let server = test_server(&tmp);
    let cat = &server.state.artifacts.read();
    use orchestration::brofile::Brofile;
    use orchestration::mcp::McpFilters;
    use orchestration::providers::Provider;
    let bf = Brofile {
        name: "auditor".into(),
        provider: Provider::Claude,
        account: None,
        lens: None,
        model: None,
        effort: None,
        filters: Some(McpFilters {
            allow: vec![
                "mcp__blackbox__bbox_search".into(),
                "mcp__blackbox__bbox_cite".into(),
            ],
            disallow: vec!["mcp__blackbox__bro_exec".into()],
        }),
    };
    orchestration::brofile::save_brofile(&bf, "global", &server.state.store_dir, None);
    let manifest = serde_json::json!({
        "description": "Agent referencing a saved brofile.",
        "when_to_use": ["when auditing"],
        "brofile_ref": "auditor",
    });
    cat.install_value(
        artifacts::ArtifactKind::Agent,
        "auditor-agent.json".into(),
        &serde_json::json!({
            "kind": "agent",
            "name": "auditor-agent",
            "version": 1,
            "manifest": manifest,
        }),
        None,
        None,
        None,
    )
    .unwrap();

    let result = server.bro_agent_describe(Parameters(AgentDescribeParams {
        agent: "auditor-agent".into(),
    }));
    assert_ne!(result.is_error, Some(true));
    let body: serde_json::Value = serde_json::from_str(&extract_text(&result)).unwrap();
    assert_eq!(body["brofile_kind"], "ref");
    assert_eq!(body["brofile_name"], "auditor");
    assert_eq!(body["brofile_provider"], "claude");
    assert!(body["brofile"].is_object());
    assert_eq!(body["brofile"]["name"], "auditor");
    assert_eq!(body["brofile"]["provider"], "claude");
    let allow = body["merged_filters"]["allow"].as_array().unwrap();
    let disallow = body["merged_filters"]["disallow"].as_array().unwrap();
    assert!(
        allow
            .iter()
            .any(|p| p.as_str() == Some("mcp__blackbox__bbox_search")),
        "brofile allow in merged: {allow:?}"
    );
    assert!(
        disallow
            .iter()
            .any(|p| p.as_str() == Some("mcp__blackbox__bro_exec")),
        "brofile disallow in merged: {disallow:?}"
    );
}

#[tokio::test]
async fn bro_agent_describe_flags_stale_brofile_ref() {
    let tmp = tempfile::tempdir().unwrap();
    let server = test_server(&tmp);
    install_artifact_value(
        &server.state,
        artifacts::ArtifactInstallParams {
            kind: artifacts::ArtifactKind::Brofile,
            source: "inline".into(),
            name: None,
            version: None,
            supersedes: None,
        },
        serde_json::json!({
            "name": "review-persona",
            "version": 1,
            "provider": "claude",
            "lens": "Review code."
        }),
    )
    .await
    .unwrap();
    install_artifact_value(
        &server.state,
        artifacts::ArtifactInstallParams {
            kind: artifacts::ArtifactKind::Agent,
            source: "inline".into(),
            name: None,
            version: None,
            supersedes: None,
        },
        serde_json::json!({
            "kind": "agent",
            "name": "stale-agent",
            "version": 1,
            "manifest": {
                "description": "Agent with a brofile ref that will become stale.",
                "when_to_use": ["when testing stale refs"],
                "brofile_ref": "review-persona"
            }
        }),
    )
    .await
    .unwrap();
    install_artifact_value(
        &server.state,
        artifacts::ArtifactInstallParams {
            kind: artifacts::ArtifactKind::Brofile,
            source: "inline".into(),
            name: None,
            version: None,
            supersedes: None,
        },
        serde_json::json!({
            "name": "review-persona-v2",
            "version": 2,
            "supersedes": "review-persona",
            "provider": "claude",
            "lens": "Review code better."
        }),
    )
    .await
    .unwrap();

    let result = server.bro_agent_describe(Parameters(AgentDescribeParams {
        agent: "stale-agent".into(),
    }));
    assert_ne!(result.is_error, Some(true));
    let body: serde_json::Value = serde_json::from_str(&extract_text(&result)).unwrap();
    assert_eq!(body["degraded"]["manifest_stale"].as_bool(), Some(true));
    assert!(
        body["warnings"]
            .as_array()
            .unwrap()
            .iter()
            .any(|warning| warning
                .as_str()
                .is_some_and(|text| text.contains("review-persona-v2"))),
        "expected stale warning: {body}"
    );
}

#[test]
fn bro_agent_describe_degraded_manifest() {
    let tmp = tempfile::tempdir().unwrap();
    let server = test_server(&tmp);
    let cat = &server.state.artifacts.read();
    cat.install_value(
        artifacts::ArtifactKind::Agent,
        "broken.json".into(),
        &serde_json::json!({
            "kind": "agent",
            "name": "broken",
            "version": 1,
            "manifest": 42,
        }),
        None,
        None,
        None,
    )
    .unwrap();

    let result = server.bro_agent_describe(Parameters(AgentDescribeParams {
        agent: "broken".into(),
    }));
    assert_ne!(result.is_error, Some(true));
    let body: serde_json::Value = serde_json::from_str(&extract_text(&result)).unwrap();
    assert_eq!(body["name"], "broken");
    assert!(body["error"]
        .as_str()
        .unwrap()
        .contains("manifest parse failed"));
}

#[test]
fn bro_agent_search_result_shape() {
    let tmp = tempfile::tempdir().unwrap();
    let server = test_server(&tmp);
    let cat = &server.state.artifacts.read();
    cat.install_value(
        artifacts::ArtifactKind::Agent,
        "reviewer.json".into(),
        &serde_json::json!({
            "kind": "agent",
            "name": "code-reviewer",
            "version": 1,
            "manifest": {
                "description": "Reviews pull requests for code quality and security.",
                "when_to_use": ["when you need a PR reviewed"],
                "anti_patterns": ["reviewing your own code"],
                "brofile_inline": {"provider": "claude"},
            },
        }),
        None,
        None,
        None,
    )
    .unwrap();

    let result = server.bro_agent_search(Parameters(AgentSearchParams {
        query: "review pull request security".into(),
        limit: None,
        cost_class: None,
        provenance_kind: None,
        exclude_anti_pattern_matches: None,
        include_vectors: None,
        query_vector: None,
    }));
    assert_ne!(result.is_error, Some(true));
    let body: serde_json::Value = serde_json::from_str(&extract_text(&result)).unwrap();
    assert_eq!(body["search_mode"], "keyword");
    assert!(body["results"].is_array());
    assert!(body["total_matched"].as_u64().unwrap() > 0);
    assert!(body["active_agents"].as_u64().unwrap() > 0);
    assert!(body["degraded"]["embedding_pending"].as_bool().unwrap());
    assert!(body["vector_status"]["coverage_ratio"].is_number());
    let first = &body["results"][0];
    assert_eq!(first["name"], "code-reviewer");
    assert!(first["score"].as_f64().unwrap() > 0.0);
    assert!(first["description"].is_string());
    assert!(first["when_to_use"].is_array());
    assert!(first["anti_patterns"].is_array());
}

#[test]
fn bro_agent_search_uses_agent_manifest_vectors_when_available() {
    let tmp = tempfile::tempdir().unwrap();
    let server = test_server(&tmp);
    let vector_store =
        std::sync::Arc::new(vectors::VectorStore::open(tmp.path().join("vectors")).unwrap());
    let _guard = vectors::install_test_global(vector_store.clone());
    let route = embed::EmbeddingRouter::default()
        .route(embed::Bucket::AgentManifest, None)
        .unwrap()
        .vector_route_id();
    let cat = &server.state.artifacts.read();
    cat.install_value(
        artifacts::ArtifactKind::Agent,
        "semantic-reviewer.json".into(),
        &serde_json::json!({
            "kind": "agent",
            "name": "semantic-reviewer",
            "version": 1,
            "manifest": {
                "description": "Reviews change sets with semantic ranking.",
                "when_to_use": ["when a vector query should find this agent"],
                "brofile_inline": {"provider": "claude"},
            },
        }),
        None,
        None,
        None,
    )
    .unwrap();
    let agent = orchestration::agents::types::AgentRef {
        name: "semantic-reviewer".into(),
        version: 1,
    };
    vector_store
        .upsert(
            &route,
            &embed_queue::agent_component_entity_id(
                &agent,
                embed_queue::AgentManifestComponent::Primary,
            ),
            "h1",
            vec![1.0, 0.0, 0.0],
        )
        .unwrap();

    let result = server.bro_agent_search(Parameters(AgentSearchParams {
        query: "orthogonal words".into(),
        limit: Some(5),
        cost_class: None,
        provenance_kind: None,
        exclude_anti_pattern_matches: None,
        include_vectors: Some(true),
        query_vector: Some(vec![1.0, 0.0, 0.0]),
    }));
    assert_ne!(result.is_error, Some(true));
    let body: serde_json::Value = serde_json::from_str(&extract_text(&result)).unwrap();
    assert_eq!(body["search_mode"], "hybrid");
    let first = &body["results"][0];
    assert_eq!(first["name"], "semantic-reviewer");
    assert!(first["sources"]["vector_primary"].as_f64().unwrap() > 0.0);
}

#[test]
fn agent_install_stamps_embeddings_and_distilled_edges() {
    let tmp = tempfile::tempdir().unwrap();
    let server = test_server(&tmp);
    let rt = tokio::runtime::Runtime::new().unwrap();
    let artifact = serde_json::json!({
        "kind": "agent",
        "name": "distilled-reviewer",
        "version": 1,
        "manifest": {
            "description": "Reviews recurring code patterns.",
            "when_to_use": ["when recurring review work appears"],
            "anti_patterns": ["one-off typo fixes"],
            "brofile_inline": {"provider": "claude"},
            "provenance": {
                "kind": "distilled",
                "distilled_by": "badgey-01",
                "evidence_session_ids": ["session:claude:sess-1"],
                "created_from_threads": ["thread:thread-abc"],
                "accept_count": 1,
                "reject_count": 0
            }
        }
    });
    rt.block_on(install_artifact_value(
        &server.state,
        ArtifactInstallParams {
            kind: artifacts::ArtifactKind::Agent,
            source: "distilled-reviewer.json".into(),
            name: None,
            version: None,
            supersedes: None,
        },
        artifact,
    ))
    .unwrap();

    let stored = server
        .state
        .artifacts
        .read()
        .load_artifact_value(artifacts::ArtifactKind::Agent, "distilled-reviewer")
        .unwrap()
        .unwrap();
    let embedding = &stored["manifest"]["embedding"];
    assert_eq!(
        embedding["components"]["primary"],
        "agent_embed:distilled-reviewer:v1:primary"
    );
    assert_eq!(
        embedding["components"]["when_to_use"],
        "agent_embed:distilled-reviewer:v1:when_to_use"
    );
    assert_eq!(
        embedding["components"]["anti_patterns"],
        "agent_embed:distilled-reviewer:v1:anti_patterns"
    );

    let agent_ref = entity_ref::EntityRef::Agent {
        name: "distilled-reviewer".into(),
        version: 1,
    };
    let edge_index = server.state.edge_index.read();
    let edges = edge_index.forward_edges_filtered(&agent_ref, &["DERIVED_FROM"]);
    assert_eq!(edges.len(), 2);
    assert!(edges.iter().any(|edge| {
        edge.target
            == entity_ref::EntityRef::Session {
                provider: "claude".into(),
                session_id: "sess-1".into(),
            }
    }));
    assert!(edges.iter().any(|edge| {
        edge.target
            == entity_ref::EntityRef::Thread {
                thread_id: "thread-abc".into(),
            }
    }));
}

#[test]
fn bro_agent_search_limit_caps_output() {
    let tmp = tempfile::tempdir().unwrap();
    let server = test_server(&tmp);
    let cat = &server.state.artifacts.read();
    for i in 0..5 {
        cat.install_value(
            artifacts::ArtifactKind::Agent,
            format!("agent{i}.json"),
            &serde_json::json!({
                "kind": "agent",
                "name": format!("search-agent-{i}"),
                "version": 1,
                "manifest": {
                    "description": format!("Agent {i} for testing search functionality."),
                    "when_to_use": ["when testing search"],
                    "brofile_inline": {"provider": "claude"},
                },
            }),
            None,
            None,
            None,
        )
        .unwrap();
    }

    let result = server.bro_agent_search(Parameters(AgentSearchParams {
        query: "testing search".into(),
        limit: Some(2),
        cost_class: None,
        provenance_kind: None,
        exclude_anti_pattern_matches: None,
        include_vectors: None,
        query_vector: None,
    }));
    assert_ne!(result.is_error, Some(true));
    let body: serde_json::Value = serde_json::from_str(&extract_text(&result)).unwrap();
    assert!(body["results"].as_array().unwrap().len() <= 2);
}

#[test]
fn bro_agent_search_empty_query_is_error() {
    let tmp = tempfile::tempdir().unwrap();
    let server = test_server(&tmp);
    let result = server.bro_agent_search(Parameters(AgentSearchParams {
        query: "  ".into(),
        limit: None,
        cost_class: None,
        provenance_kind: None,
        exclude_anti_pattern_matches: None,
        include_vectors: None,
        query_vector: None,
    }));
    assert_eq!(result.is_error, Some(true));
    let text = extract_text(&result);
    assert!(text.contains("query is required"), "got: {text}");
}

#[test]
fn bro_agent_search_default_excludes_anti_pattern_matches() {
    let tmp = tempfile::tempdir().unwrap();
    let server = test_server(&tmp);
    let cat = &server.state.artifacts.read();
    cat.install_value(
        artifacts::ArtifactKind::Agent,
        "ap-agent.json".into(),
        &serde_json::json!({
            "kind": "agent",
            "name": "deploy-bot",
            "version": 1,
            "manifest": {
                "description": "Deploys code to production.",
                "when_to_use": ["when deploying to production"],
                "anti_patterns": ["deploying untested code"],
                "brofile_inline": {"provider": "claude"},
            },
        }),
        None,
        None,
        None,
    )
    .unwrap();

    let result = server.bro_agent_search(Parameters(AgentSearchParams {
        query: "deploy untested code".into(),
        limit: None,
        cost_class: None,
        provenance_kind: None,
        exclude_anti_pattern_matches: None,
        include_vectors: None,
        query_vector: None,
    }));
    assert_ne!(result.is_error, Some(true));
    let body: serde_json::Value = serde_json::from_str(&extract_text(&result)).unwrap();
    let results = body["results"].as_array().unwrap();
    assert!(
        results.is_empty(),
        "anti-pattern match should be excluded by default: {results:?}"
    );
}

#[test]
fn bro_agent_search_include_anti_pattern_returns_matched() {
    let tmp = tempfile::tempdir().unwrap();
    let server = test_server(&tmp);
    let cat = &server.state.artifacts.read();
    cat.install_value(
        artifacts::ArtifactKind::Agent,
        "ap-agent2.json".into(),
        &serde_json::json!({
            "kind": "agent",
            "name": "deploy-bot-2",
            "version": 1,
            "manifest": {
                "description": "Deploys code to production safely.",
                "when_to_use": ["when deploying to production"],
                "anti_patterns": ["deploying untested code"],
                "brofile_inline": {"provider": "claude"},
            },
        }),
        None,
        None,
        None,
    )
    .unwrap();

    let result = server.bro_agent_search(Parameters(AgentSearchParams {
        query: "deploy untested code".into(),
        limit: None,
        cost_class: None,
        provenance_kind: None,
        exclude_anti_pattern_matches: Some(false),
        include_vectors: None,
        query_vector: None,
    }));
    assert_ne!(result.is_error, Some(true));
    let body: serde_json::Value = serde_json::from_str(&extract_text(&result)).unwrap();
    let results = body["results"].as_array().unwrap();
    assert!(
        !results.is_empty(),
        "should return result when exclude=false"
    );
    let matched_ap = results[0]["matched_anti_patterns"].as_array().unwrap();
    assert!(
        matched_ap
            .iter()
            .any(|v| v.as_str().unwrap().contains("untested")),
        "matched_anti_patterns should include the matching anti-pattern: {matched_ap:?}"
    );
}

#[test]
fn bro_agent_search_inactive_agents_excluded() {
    let tmp = tempfile::tempdir().unwrap();
    let server = test_server(&tmp);
    let cat = &server.state.artifacts.read();
    cat.install_value(
        artifacts::ArtifactKind::Agent,
        "active.json".into(),
        &serde_json::json!({
            "kind": "agent",
            "name": "active-agent",
            "version": 1,
            "manifest": {
                "description": "An active search test agent.",
                "when_to_use": ["when testing"],
                "brofile_inline": {"provider": "claude"},
            },
        }),
        None,
        None,
        None,
    )
    .unwrap();
    cat.install_value(
        artifacts::ArtifactKind::Agent,
        "retired-v1.json".into(),
        &serde_json::json!({
            "kind": "agent",
            "name": "retired-agent",
            "version": 1,
            "manifest": {
                "description": "A retired search test agent.",
                "when_to_use": ["when testing"],
                "brofile_inline": {"provider": "claude"},
            },
        }),
        None,
        None,
        None,
    )
    .unwrap();
    cat.install_value(
        artifacts::ArtifactKind::Agent,
        "replacement-v2.json".into(),
        &serde_json::json!({
            "kind": "agent",
            "name": "retired-agent",
            "version": 2,
            "manifest": {
                "description": "An active replacement search test agent.",
                "when_to_use": ["when testing"],
                "brofile_inline": {"provider": "claude"},
            },
        }),
        None,
        None,
        Some("retired-agent".to_string()),
    )
    .unwrap();

    let result = server.bro_agent_search(Parameters(AgentSearchParams {
        query: "search test agent".into(),
        limit: None,
        cost_class: None,
        provenance_kind: None,
        exclude_anti_pattern_matches: None,
        include_vectors: None,
        query_vector: None,
    }));
    assert_ne!(result.is_error, Some(true));
    let body: serde_json::Value = serde_json::from_str(&extract_text(&result)).unwrap();
    let results = body["results"].as_array().unwrap();
    let retired_entries: Vec<_> = results
        .iter()
        .filter(|r| r["name"].as_str() == Some("retired-agent"))
        .collect();
    assert_eq!(
        retired_entries.len(),
        1,
        "only v2 of retired-agent should appear"
    );
    assert_eq!(retired_entries[0]["version"], "2");
    assert!(
        results
            .iter()
            .any(|r| r["name"].as_str() == Some("active-agent")),
        "active-agent should appear: {results:?}"
    );
}

#[test]
fn bro_agent_search_invalid_cost_class_is_error() {
    let tmp = tempfile::tempdir().unwrap();
    let server = test_server(&tmp);
    let result = server.bro_agent_search(Parameters(AgentSearchParams {
        query: "test".into(),
        limit: None,
        cost_class: Some("invalid".into()),
        provenance_kind: None,
        exclude_anti_pattern_matches: None,
        include_vectors: None,
        query_vector: None,
    }));
    assert_eq!(result.is_error, Some(true));
    let text = extract_text(&result);
    assert!(text.contains("invalid cost_class"), "got: {text}");
}

#[test]
fn bro_agent_dispatch_agent_not_found() {
    let tmp = tempfile::tempdir().unwrap();
    let server = test_server(&tmp);
    let rt = tokio::runtime::Runtime::new().unwrap();
    let result = rt.block_on(server.bro_agent_dispatch(Parameters(AgentDispatchParams {
        agent: "nonexistent".into(),
        args: serde_json::Value::Null,
        project_dir: None,
        bro: None,
        ambient: None,
        caller_provider: None,
        caller_session_id: None,
    })));
    assert_eq!(result.is_error, Some(true));
    let text = extract_text(&result);
    assert!(text.contains("agent not found"), "got: {text}");
}

#[test]
fn bro_agent_dispatch_inactive_agent_error() {
    let tmp = tempfile::tempdir().unwrap();
    let server = test_server(&tmp);
    let cat = &server.state.artifacts.read();
    cat.install_value(
        artifacts::ArtifactKind::Agent,
        "v1.json".into(),
        &serde_json::json!({
            "kind": "agent",
            "name": "inactive-dispatch",
            "version": 1,
            "manifest": {
                "description": "Test agent.",
                "brofile_inline": {"provider": "claude"},
            },
        }),
        None,
        None,
        None,
    )
    .unwrap();
    cat.install_value(
        artifacts::ArtifactKind::Agent,
        "v2.json".into(),
        &serde_json::json!({
            "kind": "agent",
            "name": "inactive-dispatch",
            "version": 2,
            "manifest": {
                "description": "Replacement.",
                "brofile_inline": {"provider": "claude"},
            },
        }),
        None,
        None,
        Some("inactive-dispatch".to_string()),
    )
    .unwrap();

    let rt = tokio::runtime::Runtime::new().unwrap();
    let result = rt.block_on(server.bro_agent_dispatch(Parameters(AgentDispatchParams {
        agent: "inactive-dispatch@v1".into(),
        args: serde_json::Value::Null,
        project_dir: None,
        bro: None,
        ambient: None,
        caller_provider: None,
        caller_session_id: None,
    })));
    assert_eq!(result.is_error, Some(true));
    let text = extract_text(&result);
    assert!(
        text.contains("not active") || text.contains("agent not found"),
        "got: {text}"
    );
}

#[test]
fn bro_agent_dispatch_adapter_unavailable_hard_fails() {
    let tmp = tempfile::tempdir().unwrap();
    let server = test_server(&tmp);
    let cat = &server.state.artifacts.read();
    cat.install_value(
        artifacts::ArtifactKind::Agent,
        "badgey.json".into(),
        &serde_json::json!({
            "kind": "agent",
            "name": "badgey-agent",
            "version": 1,
            "manifest": {
                "description": "Badgey agent.",
                "dispatch_adapter": "badgey",
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
        agent: "badgey-agent".into(),
        args: serde_json::Value::Null,
        project_dir: None,
        bro: None,
        ambient: None,
        caller_provider: None,
        caller_session_id: None,
    })));
    assert_eq!(result.is_error, Some(true));
    let text = extract_text(&result);
    assert!(
        text.contains("adapter_unavailable"),
        "should hard-fail with adapter_unavailable: {text}"
    );
}

#[tokio::test]
async fn bro_agent_dispatch_noop_adapter_succeeds() {
    let tmp = tempfile::tempdir().unwrap();
    let server = test_server(&tmp);
    let cat = &server.state.artifacts.read();
    cat.install_value(
        artifacts::ArtifactKind::Agent,
        "noop-dispatch.json".into(),
        &serde_json::json!({
            "kind": "agent",
            "name": "noop-agent",
            "version": 1,
            "manifest": {
                "description": "Noop test agent.",
                "dispatch_adapter": "noop",
                "brofile_inline": {"provider": "claude"},
            },
        }),
        None,
        None,
        None,
    )
    .unwrap();

    struct NoopAdapter;
    impl orchestration::agents::adapter::AgentDispatchAdapter for NoopAdapter {
        fn name(&self) -> &'static str {
            "noop"
        }
        fn dispatch(
            &self,
            _manifest: &orchestration::agents::types::AgentManifest,
            _args: serde_json::Value,
            ctx: orchestration::agents::adapter::DispatchContext,
        ) -> std::pin::Pin<
            Box<
                dyn std::future::Future<
                        Output = Result<
                            orchestration::agents::adapter::AgentDispatchResult,
                            orchestration::agents::adapter::AgentDispatchError,
                        >,
                    > + Send
                    + '_,
            >,
        > {
            let caller_provider = ctx.caller_provider.clone().unwrap_or_default();
            let caller_session_id = ctx.caller_session_id.clone().unwrap_or_default();
            Box::pin(async move {
                Ok(orchestration::agents::adapter::AgentDispatchResult {
                    session: orchestration::agents::types::AgentSession {
                        session_id: format!("{caller_provider}/{caller_session_id}"),
                        provider: "test".into(),
                        project_dir: None,
                        agent: orchestration::agents::types::AgentRef {
                            name: "noop-agent".into(),
                            version: 1,
                        },
                        task_id: Some("test-task-456".into()),
                    },
                    resolved_brofile: None,
                    merged_filters: orchestration::agents::types::MergedFilters::default(),
                    degraded: None,
                })
            })
        }
    }
    server
        .state
        .agent_adapter_registry
        .write()
        .register(std::sync::Arc::new(NoopAdapter));

    let result = server
        .bro_agent_dispatch(Parameters(AgentDispatchParams {
            agent: "noop-agent".into(),
            args: serde_json::json!({"prompt": "hello"}),
            project_dir: None,
            bro: None,
            ambient: None,
            caller_provider: Some("claude".into()),
            caller_session_id: Some("caller-session-789".into()),
        }))
        .await;
    assert_ne!(result.is_error, Some(true));
    let body: serde_json::Value = serde_json::from_str(&extract_text(&result)).unwrap();
    assert_eq!(body["session"]["session_id"], "claude/caller-session-789");
    assert_eq!(body["task_id"], "test-task-456");
}

#[tokio::test]
async fn bro_agent_dispatch_handle_shape_reference_agent_noop_adapter() {
    let tmp = tempfile::tempdir().unwrap();
    let server = test_server(&tmp);
    let mut diff_narrator: serde_json::Value =
        serde_json::from_str(include_str!("../examples/agents/diff-narrator.json")).unwrap();
    diff_narrator["manifest"]["dispatch_adapter"] = serde_json::json!("noop-ref");
    let cat = &server.state.artifacts.read();
    cat.install_value(
        artifacts::ArtifactKind::Agent,
        "diff-narrator-ref.json".into(),
        &diff_narrator,
        None,
        None,
        None,
    )
    .unwrap();

    struct RefNoopAdapter;
    impl orchestration::agents::adapter::AgentDispatchAdapter for RefNoopAdapter {
        fn name(&self) -> &'static str {
            "noop-ref"
        }
        fn dispatch(
            &self,
            manifest: &orchestration::agents::types::AgentManifest,
            _args: serde_json::Value,
            ctx: orchestration::agents::adapter::DispatchContext,
        ) -> std::pin::Pin<
            Box<
                dyn std::future::Future<
                        Output = Result<
                            orchestration::agents::adapter::AgentDispatchResult,
                            orchestration::agents::adapter::AgentDispatchError,
                        >,
                    > + Send,
            >,
        > {
            let description = manifest.description.clone();
            Box::pin(async move {
                Ok(orchestration::agents::adapter::AgentDispatchResult {
                    session: orchestration::agents::types::AgentSession {
                        session_id: format!("ref-session-{description}"),
                        provider: "stub-provider".into(),
                        project_dir: ctx.project_dir.clone(),
                        agent: orchestration::agents::types::AgentRef {
                            name: "diff-narrator".into(),
                            version: 1,
                        },
                        task_id: Some(format!("ref-task-{description}")),
                    },
                    resolved_brofile: Some("diff-narrator-inline-brofile".into()),
                    merged_filters: orchestration::agents::types::MergedFilters {
                        allow: vec!["mcp__blackbox__bbox_*".into()],
                        disallow: vec![],
                    },
                    degraded: None,
                })
            })
        }
    }
    server
        .state
        .agent_adapter_registry
        .write()
        .register(std::sync::Arc::new(RefNoopAdapter));

    let result = server
        .bro_agent_dispatch(Parameters(AgentDispatchParams {
            agent: "diff-narrator".into(),
            args: serde_json::json!({"diff": "--- a/x\n+++ b/x\n@@ -1 +1 @@\n-old\n+new"}),
            project_dir: Some("/tmp/test".into()),
            bro: None,
            ambient: None,
            caller_provider: None,
            caller_session_id: None,
        }))
        .await;
    assert!(
        !result.is_error.unwrap_or(false),
        "bro_agent_dispatch should succeed: {}",
        extract_text(&result)
    );
    let body: serde_json::Value = serde_json::from_str(&extract_text(&result)).unwrap();

    // Verify handle shape: every field the agent-system.md 5.3 contract requires
    assert!(body["session"].is_object(), "should have session object");
    assert!(
        body["session"]["session_id"].is_string(),
        "session.session_id must be a string"
    );
    assert!(
        !body["session"]["session_id"].as_str().unwrap().is_empty(),
        "session_id must be non-empty"
    );
    assert_eq!(
        body["session"]["provider"], "stub-provider",
        "session.provider should match adapter output"
    );
    assert_eq!(
        body["session"]["project_dir"], "/tmp/test",
        "session.project_dir should echo DispatchContext"
    );
    assert!(
        body["session"]["agent"].is_object(),
        "session.agent must be an object"
    );
    assert_eq!(
        body["session"]["agent"]["name"], "diff-narrator",
        "agent.name should be diff-narrator"
    );
    assert_eq!(
        body["session"]["agent"]["version"], 1,
        "agent.version should be 1"
    );
    assert!(
        body["session"]["task_id"].is_string(),
        "session.task_id must be a string"
    );
    assert!(
        !body["session"]["task_id"].as_str().unwrap().is_empty(),
        "session.task_id must be non-empty"
    );
    assert!(
        body["task_id"].is_string(),
        "top-level task_id must be a string"
    );
    assert!(
        !body["task_id"].as_str().unwrap().is_empty(),
        "top-level task_id must be non-empty"
    );
    assert_eq!(
        body["task_id"], body["session"]["task_id"],
        "top-level task_id should match session.task_id"
    );
    assert!(
        body["resolved_brofile"].is_string(),
        "resolved_brofile should be present"
    );
    assert!(
        body["merged_filters"].is_object(),
        "merged_filters should be an object"
    );
    assert!(
        body["degraded"].is_null(),
        "degraded should be null for successful dispatch"
    );
}

#[test]
fn bro_agent_dispatch_unparseable_manifest_error() {
    let tmp = tempfile::tempdir().unwrap();
    let server = test_server(&tmp);
    let cat = &server.state.artifacts.read();
    cat.install_value(
        artifacts::ArtifactKind::Agent,
        "broken-dispatch.json".into(),
        &serde_json::json!({
            "kind": "agent",
            "name": "broken-dispatch",
            "version": 1,
            "manifest": 42,
        }),
        None,
        None,
        None,
    )
    .unwrap();

    let rt = tokio::runtime::Runtime::new().unwrap();
    let result = rt.block_on(server.bro_agent_dispatch(Parameters(AgentDispatchParams {
        agent: "broken-dispatch".into(),
        args: serde_json::Value::Null,
        project_dir: None,
        bro: None,
        ambient: None,
        caller_provider: None,
        caller_session_id: None,
    })));
    assert_eq!(result.is_error, Some(true));
    let text = extract_text(&result);
    assert!(text.contains("unparseable manifest"), "got: {text}");
}

#[test]
fn bro_agent_dispatch_no_brofile_error() {
    let tmp = tempfile::tempdir().unwrap();
    let server = test_server(&tmp);
    let cat = &server.state.artifacts.read();
    cat.install_value(
        artifacts::ArtifactKind::Agent,
        "no-brofile.json".into(),
        &serde_json::json!({
            "kind": "agent",
            "name": "no-brofile-agent",
            "version": 1,
            "manifest": {
                "description": "Agent without brofile.",
            },
        }),
        None,
        None,
        None,
    )
    .unwrap();

    let rt = tokio::runtime::Runtime::new().unwrap();
    let result = rt.block_on(server.bro_agent_dispatch(Parameters(AgentDispatchParams {
        agent: "no-brofile-agent".into(),
        args: serde_json::Value::Null,
        project_dir: None,
        bro: None,
        ambient: None,
        caller_provider: None,
        caller_session_id: None,
    })));
    assert_eq!(result.is_error, Some(true));
    let text = extract_text(&result);
    assert!(
        text.contains("neither brofile_ref nor brofile_inline"),
        "got: {text}"
    );
}

#[test]
fn expand_template_substitutes_args() {
    let tmpl = "Review {{diff}} for {{focus}} issues.";
    let args = serde_json::json!({"diff": "abc123", "focus": "security"});
    let result = BlackboxServer::expand_template(tmpl, &args);
    assert_eq!(result, "Review abc123 for security issues.");
}

#[test]
fn bro_agent_dispatch_invalid_inline_provider_error() {
    let tmp = tempfile::tempdir().unwrap();
    let server = test_server(&tmp);
    let cat = &server.state.artifacts.read();
    cat.install_value(
        artifacts::ArtifactKind::Agent,
        "bad-prov.json".into(),
        &serde_json::json!({
            "kind": "agent",
            "name": "bad-provider-agent",
            "version": 1,
            "manifest": {
                "description": "Agent with bad provider.",
                "brofile_inline": {"provider": "nonexistent_provider_xyz"},
            },
        }),
        None,
        None,
        None,
    )
    .unwrap();

    let rt = tokio::runtime::Runtime::new().unwrap();
    let result = rt.block_on(server.bro_agent_dispatch(Parameters(AgentDispatchParams {
        agent: "bad-provider-agent".into(),
        args: serde_json::Value::Null,
        project_dir: None,
        bro: None,
        ambient: None,
        caller_provider: None,
        caller_session_id: None,
    })));
    assert_eq!(result.is_error, Some(true));
    let text = extract_text(&result);
    assert!(
        text.contains("unknown_provider") && text.contains("nonexistent_provider_xyz"),
        "should report unknown provider: {text}"
    );
}

#[test]
fn bro_agent_dispatch_args_validation_missing_required() {
    let tmp = tempfile::tempdir().unwrap();
    let server = test_server(&tmp);
    let cat = &server.state.artifacts.read();
    cat.install_value(
        artifacts::ArtifactKind::Agent,
        "schema-agent.json".into(),
        &serde_json::json!({
            "kind": "agent",
            "name": "schema-agent",
            "version": 1,
            "manifest": {
                "description": "Agent with input schema.",
                "brofile_inline": {"provider": "claude"},
                "inputs": {
                    "schema": {
                        "type": "object",
                        "required": ["diff", "focus"],
                    },
                    "prompt_template": "Review {{diff}} for {{focus}} issues."
                },
            },
        }),
        None,
        None,
        None,
    )
    .unwrap();

    let rt = tokio::runtime::Runtime::new().unwrap();
    let result = rt.block_on(server.bro_agent_dispatch(Parameters(AgentDispatchParams {
        agent: "schema-agent".into(),
        args: serde_json::json!({"diff": "abc123"}),
        project_dir: None,
        bro: None,
        ambient: None,
        caller_provider: None,
        caller_session_id: None,
    })));
    assert_eq!(result.is_error, Some(true));
    let text = extract_text(&result);
    assert!(
        text.contains("schema_validation_failed") && text.contains("focus"),
        "should report schema validation failure for 'focus': {text}"
    );
}

#[test]
fn bro_agent_dispatch_args_type_mismatch_rejected() {
    let tmp = tempfile::tempdir().unwrap();
    let server = test_server(&tmp);
    let cat = &server.state.artifacts.read();
    cat.install_value(
        artifacts::ArtifactKind::Agent,
        "typed-agent.json".into(),
        &serde_json::json!({
            "kind": "agent",
            "name": "typed-agent",
            "version": 1,
            "manifest": {
                "description": "Agent with typed schema.",
                "brofile_inline": {"provider": "claude"},
                "inputs": {
                    "schema": {
                        "type": "object",
                        "properties": {
                            "diff": {"type": "string"}
                        },
                        "required": ["diff"],
                    },
                    "prompt_template": "Review {{diff}}."
                },
            },
        }),
        None,
        None,
        None,
    )
    .unwrap();

    let rt = tokio::runtime::Runtime::new().unwrap();
    let result = rt.block_on(server.bro_agent_dispatch(Parameters(AgentDispatchParams {
        agent: "typed-agent".into(),
        args: serde_json::json!({"diff": 123}),
        project_dir: None,
        bro: None,
        ambient: None,
        caller_provider: None,
        caller_session_id: None,
    })));
    assert_eq!(result.is_error, Some(true));
    let text = extract_text(&result);
    assert!(
        text.contains("schema_validation_failed"),
        "should reject wrong type: {text}"
    );
}

#[test]
fn bro_agent_dispatch_schema_202012_prefix_items_rejected() {
    let tmp = tempfile::tempdir().unwrap();
    let server = test_server(&tmp);
    let cat = &server.state.artifacts.read();
    cat.install_value(
        artifacts::ArtifactKind::Agent,
        "tuple-agent.json".into(),
        &serde_json::json!({
            "kind": "agent",
            "name": "tuple-agent",
            "version": 1,
            "manifest": {
                "description": "Agent with 2020-12 prefixItems schema.",
                "brofile_inline": {"provider": "claude"},
                "inputs": {
                    "schema": {
                        "type": "object",
                        "properties": {
                            "coords": {
                                "type": "array",
                                "prefixItems": [
                                    {"type": "number"},
                                    {"type": "number"}
                                ]
                            }
                        },
                        "required": ["coords"],
                    },
                    "prompt_template": "Plot {{coords}}."
                },
            },
        }),
        None,
        None,
        None,
    )
    .unwrap();

    let rt = tokio::runtime::Runtime::new().unwrap();
    let result = rt.block_on(server.bro_agent_dispatch(Parameters(AgentDispatchParams {
        agent: "tuple-agent".into(),
        args: serde_json::json!({"coords": ["not-a-number", 2]}),
        project_dir: None,
        bro: None,
        ambient: None,
        caller_provider: None,
        caller_session_id: None,
    })));
    assert_eq!(result.is_error, Some(true));
    let text = extract_text(&result);
    assert!(
        text.contains("schema_validation_failed"),
        "should reject via prefixItems: {text}"
    );
}

#[test]
fn bbox_describe_schema_includes_installed_agents() {
    let tmp = tempfile::tempdir().unwrap();
    let server = test_server(&tmp);
    let cat = &server.state.artifacts.read();
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
fn bro_agent_dispatch_bro_label_stamped_on_task() {
    let tmp = tempfile::tempdir().unwrap();
    let server = test_server(&tmp);
    let cat = &server.state.artifacts.read();
    cat.install_value(
        artifacts::ArtifactKind::Agent,
        "labeled.json".into(),
        &serde_json::json!({
            "kind": "agent",
            "name": "labeled-agent",
            "version": 3,
            "manifest": {
                "description": "Agent whose bro_label should be stamped.",
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
        agent: "labeled-agent".into(),
        args: serde_json::Value::Null,
        project_dir: Some(tmp.path().to_str().unwrap().to_string()),
        bro: None,
        ambient: None,
        caller_provider: None,
        caller_session_id: None,
    })));
    assert_ne!(result.is_error, Some(true));
    let body: serde_json::Value = serde_json::from_str(&extract_text(&result)).unwrap();
    assert_eq!(
        body["agentLabel"].as_str(),
        Some("agent:labeled-agent@v3"),
        "response should carry agentLabel: {body}"
    );
    let task_id = body["task_id"].as_str().unwrap();
    let task = server.state.task_store.read().get(task_id).unwrap();
    let label = task.inner.lock().bro_label.clone();
    assert_eq!(
        label.as_deref(),
        Some("agent:labeled-agent@v3"),
        "bro_label should be stamped: {label:?}"
    );
    let agent_lbl = task.inner.lock().agent_label.clone();
    assert_eq!(
        agent_lbl.as_deref(),
        Some("agent:labeled-agent@v3"),
        "agent_label should be stamped: {agent_lbl:?}"
    );
}

#[test]
fn bro_agent_dispatch_with_bro_preserves_agent_label() {
    let tmp = tempfile::tempdir().unwrap();
    let server = test_server(&tmp);
    let cat = &server.state.artifacts.read();
    cat.install_value(
        artifacts::ArtifactKind::Agent,
        "bro-dispatch.json".into(),
        &serde_json::json!({
            "kind": "agent",
            "name": "bro-dispatch-agent",
            "version": 2,
            "manifest": {
                "description": "Agent dispatched with bro=.",
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
        agent: "bro-dispatch-agent".into(),
        args: serde_json::Value::Null,
        project_dir: Some(tmp.path().to_str().unwrap().to_string()),
        bro: Some("some-bro".into()),
        ambient: None,
        caller_provider: None,
        caller_session_id: None,
    })));
    assert_ne!(result.is_error, Some(true));
    let body: serde_json::Value = serde_json::from_str(&extract_text(&result)).unwrap();
    let task_id = body["task_id"].as_str().unwrap();
    let task = server.state.task_store.read().get(task_id).unwrap();
    let inner = task.inner.lock();
    let bro_lbl = inner.bro_label.clone();
    assert_eq!(
        bro_lbl.as_deref(),
        Some("some-bro"),
        "bro_label should be the named bro: {bro_lbl:?}"
    );
    let agent_lbl = inner.agent_label.clone();
    assert_eq!(
        agent_lbl.as_deref(),
        Some("agent:bro-dispatch-agent@v2"),
        "agent_label should preserve agent attribution: {agent_lbl:?}"
    );
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
