use super::*;
use crate::artifacts::{self, ArtifactInstallParams};
use crate::server::install_artifact_value;
use crate::server::state::SharedState;
use crate::server::workflow_capabilities::validate_workflow_capabilities;
use crate::tools::atoms::helpers::bounded_effect_u64;
use crate::workflow;
use std::sync::Arc;

fn test_server(tmp: &tempfile::TempDir) -> BlackboxServer {
    BlackboxServer::new(Arc::new(SharedState::for_test(&tmp.path().join("bro"))))
}

fn extract_text(result: &CallToolResult) -> String {
    let wire = serde_json::to_value(result).unwrap();
    wire["content"][0]["text"].as_str().unwrap().to_string()
}

fn make_task(
    server: &BlackboxServer,
    task_id: &str,
    events: Vec<serde_json::Value>,
    last_message: Option<&str>,
    report: Option<orchestration::BroReport>,
) -> Arc<orchestration::Task> {
    let task = orchestration::spawn_in_process_task(
        task_id.to_string(),
        crate::orchestration::providers::Provider::Codex,
        "session-primary".to_string(),
        None,
        server.state.store_dir.clone(),
        server.state.task_store.clone(),
        server.state.tail_tx.clone(),
        None,
        None,
        None,
    );
    {
        let mut inner = task.inner.lock();
        inner.events = events;
        inner.last_assistant_message = last_message.map(str::to_string);
        inner.report = report;
    }
    task
}

#[test]
fn atom_ref_allowed_accepts_exact_and_latest_refs() {
    assert!(atom_ref_allowed(
        &["atom:rust-review@v1".to_string()],
        "atom:rust-review@v1"
    ));
    assert!(atom_ref_allowed(
        &["atom:rust-review@latest".to_string()],
        "atom:rust-review@v7"
    ));
    assert!(!atom_ref_allowed(
        &["atom:rust-review@v1".to_string()],
        "atom:rust-review@v2"
    ));
}

#[test]
fn bounded_effect_u64_parses_bounded_and_unbounded() {
    assert_eq!(
        bounded_effect_u64(Some(&serde_json::json!(3))).unwrap(),
        Some(3)
    );
    assert_eq!(
        bounded_effect_u64(Some(&serde_json::json!("unbounded"))).unwrap(),
        None
    );
    assert!(bounded_effect_u64(Some(&serde_json::json!(-1))).is_err());
}

#[test]
fn default_atom_owner_is_stable_for_omitted_owner_tools() {
    assert_eq!(default_atom_owner(), "operator:local");
}

#[test]
fn attached_supervision_poll_value_authorizes_lineage_and_denies_unrelated() {
    let dir = tempfile::tempdir().unwrap();
    let server = BlackboxServer::new(Arc::new(crate::server::state::SharedState::for_test(
        dir.path(),
    )));

    let primary = orchestration::atoms::invocation::AtomInvocation::new_profile(
        "inv-primary".into(),
        "atom:test@v1".into(),
        None,
        "operator:primary".into(),
        "claude".into(),
        "session-primary".into(),
        None,
        "task-primary".into(),
    );
    let classifier = orchestration::atoms::invocation::AtomInvocation::new_profile(
        "inv-classifier".into(),
        "atom:test@v1".into(),
        None,
        "operator:classifier".into(),
        "claude".into(),
        "session-classifier".into(),
        None,
        "task-classifier".into(),
    );

    {
        let mut store = server.state.atom_invocation_store.write();
        store.insert(primary);
        store.insert(classifier);
        store.insert_attachment(orchestration::atoms::invocation::SupervisionAttachment {
            supervision_run_id: "run-1".into(),
            primary_invocation_id: "inv-primary".into(),
            primary_task_id: "task-primary".into(),
            classifier_invocation_id: Some("inv-classifier".into()),
            advisor_invocation_id: None,
            attempt: 1,
        });
    }

    make_task(&server, "task-primary", vec![], Some("still running"), None);

    assert!(
        server
            .attached_supervision_poll_value("inv-primary", "operator:classifier", Some(1))
            .is_ok()
    );
    let missing_attempt =
        server.attached_supervision_poll_value("inv-primary", "operator:classifier", Some(2));
    assert!(missing_attempt.is_err());
    let denied =
        server.attached_supervision_poll_value("inv-primary", "operator:stranger", Some(1));
    assert!(denied.is_err());
}

#[test]
fn attached_supervision_poll_value_bounds_note_and_tail_sizes() {
    let dir = tempfile::tempdir().unwrap();
    let server = BlackboxServer::new(Arc::new(crate::server::state::SharedState::for_test(
        dir.path(),
    )));

    let primary = orchestration::atoms::invocation::AtomInvocation::new_profile(
        "inv-primary".into(),
        "atom:test@v1".into(),
        None,
        "operator:primary".into(),
        "claude".into(),
        "session-primary".into(),
        None,
        "task-primary".into(),
    );

    {
        let mut store = server.state.atom_invocation_store.write();
        store.insert(primary);
        store.insert_attachment(orchestration::atoms::invocation::SupervisionAttachment {
            supervision_run_id: "run-1".into(),
            primary_invocation_id: "inv-primary".into(),
            primary_task_id: "task-primary".into(),
            classifier_invocation_id: None,
            advisor_invocation_id: None,
            attempt: 1,
        });
    }

    let mut events = Vec::new();
    for i in 0..40 {
        events.push(serde_json::json!({ "i": i }));
    }
    make_task(
        &server,
        "task-primary",
        events,
        Some(&"x".repeat(5000)),
        None,
    );

    for i in 0..25 {
        let _ = server
            .state
            .notes
            .write()
            .create(&crate::notes::NoteParams {
                kind: "assumption".into(),
                body: format!("note-{} {}", i, "y".repeat(5000)),
                task_id: Some("task-primary".into()),
                session_id: None,
                project: None,
                thread_id: None,
                provider: None,
                bro: None,
            })
            .unwrap();
    }

    let snapshot = server
        .attached_supervision_poll_value("inv-primary", "operator:primary", Some(1))
        .unwrap();
    assert_eq!(
        snapshot["attempt_metadata"]["attempt"],
        serde_json::json!(1)
    );
    assert_eq!(
        snapshot["recent_provider_events"].as_array().unwrap().len(),
        20
    );
    assert_eq!(snapshot["task_notes"].as_array().unwrap().len(), 20);
    assert!(snapshot["assistant_tail"].as_str().unwrap().len() <= 4000);
    for note in snapshot["task_notes"].as_array().unwrap() {
        assert!(note["body"].as_str().unwrap().len() <= 4000);
    }
}

#[tokio::test]
async fn execute_supervision_action_accept_records_without_mutation() {
    let dir = tempfile::tempdir().unwrap();
    let server = BlackboxServer::new(Arc::new(crate::server::state::SharedState::for_test(
        dir.path(),
    )));
    let primary = orchestration::atoms::invocation::AtomInvocation::new_profile(
        "inv-primary".into(),
        "atom:test@v1".into(),
        None,
        "operator:primary".into(),
        "claude".into(),
        "session-primary".into(),
        None,
        "task-primary".into(),
    );
    let advisor = orchestration::atoms::invocation::AtomInvocation::new_profile(
        "inv-advisor".into(),
        "atom:advisor@v1".into(),
        None,
        "operator:advisor".into(),
        "claude".into(),
        "session-advisor".into(),
        None,
        "task-advisor".into(),
    );
    {
        let mut store = server.state.atom_invocation_store.write();
        store.insert(primary);
        store.insert(advisor);
        store.insert_attachment(orchestration::atoms::invocation::SupervisionAttachment {
            supervision_run_id: "run-1".into(),
            primary_invocation_id: "inv-primary".into(),
            primary_task_id: "task-primary".into(),
            classifier_invocation_id: None,
            advisor_invocation_id: Some("inv-advisor".into()),
            attempt: 1,
        });
    }
    make_task(&server, "task-primary", vec![], Some("done"), None);

    let result = server
        .execute_supervision_action_value(
            "inv-primary",
            "operator:advisor",
            Some(1),
            serde_json::json!({"action": "accept", "reason": "meets criteria"}),
        )
        .await
        .unwrap();
    assert_eq!(result["result"]["status"], serde_json::json!("recorded"));
    assert_eq!(
        result["result"]["mutated_primary"],
        serde_json::json!(false)
    );
}

#[tokio::test]
async fn execute_supervision_action_cancel_scopes_to_primary_task() {
    let dir = tempfile::tempdir().unwrap();
    let server = BlackboxServer::new(Arc::new(crate::server::state::SharedState::for_test(
        dir.path(),
    )));
    let primary = orchestration::atoms::invocation::AtomInvocation::new_profile(
        "inv-primary".into(),
        "atom:test@v1".into(),
        None,
        "operator:primary".into(),
        "claude".into(),
        "session-primary".into(),
        None,
        "task-primary".into(),
    );
    let advisor = orchestration::atoms::invocation::AtomInvocation::new_profile(
        "inv-advisor".into(),
        "atom:advisor@v1".into(),
        None,
        "operator:advisor".into(),
        "claude".into(),
        "session-advisor".into(),
        None,
        "task-advisor".into(),
    );
    {
        let mut store = server.state.atom_invocation_store.write();
        store.insert(primary);
        store.insert(advisor);
        store.insert_attachment(orchestration::atoms::invocation::SupervisionAttachment {
            supervision_run_id: "run-1".into(),
            primary_invocation_id: "inv-primary".into(),
            primary_task_id: "task-primary".into(),
            classifier_invocation_id: None,
            advisor_invocation_id: Some("inv-advisor".into()),
            attempt: 1,
        });
    }
    make_task(&server, "task-primary", vec![], Some("running"), None);
    make_task(&server, "task-unrelated", vec![], Some("running"), None);

    let result = server
        .execute_supervision_action_value(
            "inv-primary",
            "operator:advisor",
            Some(1),
            serde_json::json!({"action": "cancel_and_retry", "reason": "retry"}),
        )
        .await
        .unwrap();
    assert_eq!(result["result"]["status"], serde_json::json!("cancelled"));

    let primary_task = server.state.task_store.read().get("task-primary").unwrap();
    assert!(matches!(
        primary_task.inner.lock().status,
        orchestration::TaskStatus::Cancelled
    ));
    let unrelated_task = server
        .state
        .task_store
        .read()
        .get("task-unrelated")
        .unwrap();
    assert!(matches!(
        unrelated_task.inner.lock().status,
        orchestration::TaskStatus::Running
    ));
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
        "../../../system-defaults/brofiles/refactor/rust-refactor-persona.json"
    ))
    .unwrap();
    let atom: serde_json::Value = serde_json::from_str(include_str!(
        "../../../system-defaults/atoms/refactor/rust-test-island-extract.json"
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
        "../../../system-defaults/brofiles/refactor/rust-refactor-persona.json"
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
            include_str!("../../../system-defaults/atoms/refactor/rust-rename-symbol.json"),
        ),
        (
            "system-defaults/atoms/refactor/rust-extract-to-submodule.json",
            "rust-extract-to-submodule",
            include_str!("../../../system-defaults/atoms/refactor/rust-extract-to-submodule.json"),
        ),
        (
            "system-defaults/atoms/refactor/rust-organize-imports.json",
            "rust-organize-imports",
            include_str!("../../../system-defaults/atoms/refactor/rust-organize-imports.json"),
        ),
        (
            "system-defaults/atoms/refactor/rust-cargo-add-dep.json",
            "rust-cargo-add-dep",
            include_str!("../../../system-defaults/atoms/refactor/rust-cargo-add-dep.json"),
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
async fn shipped_rust_architecture_pathology_artifacts_install_and_validate() {
    let tmp = tempfile::tempdir().unwrap();
    let server = test_server(&tmp);
    let brofile: serde_json::Value = serde_json::from_str(include_str!(
        "../../../system-defaults/brofiles/refactor/rust-architecture-pathologist.json"
    ))
    .unwrap();

    install_artifact_value(
        &server.state,
        ArtifactInstallParams {
            kind: artifacts::ArtifactKind::Brofile,
            source: "system-defaults/brofiles/refactor/rust-architecture-pathologist.json".into(),
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
            "system-defaults/atoms/refactor/rust-architecture-impl-role-coherence.json",
            "rust-architecture-impl-role-coherence",
            include_str!(
                "../../../system-defaults/atoms/refactor/rust-architecture-impl-role-coherence.json"
            ),
        ),
        (
            "system-defaults/atoms/refactor/rust-architecture-state-ownership-collapse.json",
            "rust-architecture-state-ownership-collapse",
            include_str!(
                "../../../system-defaults/atoms/refactor/rust-architecture-state-ownership-collapse.json"
            ),
        ),
        (
            "system-defaults/atoms/refactor/rust-architecture-construction-boundary-collapse.json",
            "rust-architecture-construction-boundary-collapse",
            include_str!(
                "../../../system-defaults/atoms/refactor/rust-architecture-construction-boundary-collapse.json"
            ),
        ),
        (
            "system-defaults/atoms/refactor/rust-architecture-trait-boundary-mismatch.json",
            "rust-architecture-trait-boundary-mismatch",
            include_str!(
                "../../../system-defaults/atoms/refactor/rust-architecture-trait-boundary-mismatch.json"
            ),
        ),
        (
            "system-defaults/atoms/refactor/rust-architecture-module-topology-drift.json",
            "rust-architecture-module-topology-drift",
            include_str!(
                "../../../system-defaults/atoms/refactor/rust-architecture-module-topology-drift.json"
            ),
        ),
        (
            "system-defaults/atoms/refactor/rust-architecture-error-contract-drift.json",
            "rust-architecture-error-contract-drift",
            include_str!(
                "../../../system-defaults/atoms/refactor/rust-architecture-error-contract-drift.json"
            ),
        ),
        (
            "system-defaults/atoms/refactor/rust-architecture-feature-cfg-matrix.json",
            "rust-architecture-feature-cfg-matrix",
            include_str!(
                "../../../system-defaults/atoms/refactor/rust-architecture-feature-cfg-matrix.json"
            ),
        ),
        (
            "system-defaults/atoms/refactor/rust-architecture-async-runtime-lifecycle.json",
            "rust-architecture-async-runtime-lifecycle",
            include_str!(
                "../../../system-defaults/atoms/refactor/rust-architecture-async-runtime-lifecycle.json"
            ),
        ),
        (
            "system-defaults/atoms/refactor/rust-architecture-test-implied-architecture.json",
            "rust-architecture-test-implied-architecture",
            include_str!(
                "../../../system-defaults/atoms/refactor/rust-architecture-test-implied-architecture.json"
            ),
        ),
        (
            "system-defaults/atoms/refactor/rust-architecture-unsafe-contract-opacity.json",
            "rust-architecture-unsafe-contract-opacity",
            include_str!(
                "../../../system-defaults/atoms/refactor/rust-architecture-unsafe-contract-opacity.json"
            ),
        ),
        (
            "system-defaults/atoms/refactor/rust-architecture-macro-generated-contract-opacity.json",
            "rust-architecture-macro-generated-contract-opacity",
            include_str!(
                "../../../system-defaults/atoms/refactor/rust-architecture-macro-generated-contract-opacity.json"
            ),
        ),
        (
            "system-defaults/atoms/refactor/rust-architecture-transcript-anchored-pressure.json",
            "rust-architecture-transcript-anchored-pressure",
            include_str!(
                "../../../system-defaults/atoms/refactor/rust-architecture-transcript-anchored-pressure.json"
            ),
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

    let workflow_value: serde_json::Value = serde_json::from_str(include_str!(
        "../../../system-defaults/workflows/refactor/arch-pathology-rust.json"
    ))
    .unwrap();
    let workflow_meta = install_artifact_value(
        &server.state,
        ArtifactInstallParams {
            kind: artifacts::ArtifactKind::Workflow,
            source: "system-defaults/workflows/refactor/arch-pathology-rust.json".into(),
            name: None,
            version: None,
            supersedes: None,
        },
        workflow_value.clone(),
    )
    .await
    .unwrap();
    assert_eq!(workflow_meta.kind, artifacts::ArtifactKind::Workflow);
    assert_eq!(workflow_meta.name, "arch-pathology-rust");
    assert!(workflow_meta.active);

    let workflow_spec: workflow::Workflow = serde_json::from_value(workflow_value).unwrap();
    let compiled = workflow::compile(workflow_spec).unwrap();
    validate_workflow_capabilities(&compiled, &server.state).unwrap();
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
