use crate::artifacts::{
    ArtifactInstallParams, ArtifactListParams, ArtifactRemoveParams, ArtifactSupersedeParams,
};
use crate::server::BlackboxServer;
use crate::server::routes::{deactivate_artifact, install_artifact_from_params};

use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::CallToolResult;
use rmcp::{tool, tool_router};

pub(crate) fn router() -> ToolRouter<BlackboxServer> {
    BlackboxServer::artifacts_tools()
}

#[tool_router(router = artifacts_tools)]
impl BlackboxServer {
    #[tool(
        name = "bbox_artifact_install",
        description = "Install a workflow, packet, brofile, agent, atom, team, or cron artifact from a local JSON file path or http(s) URL into the versioned artifact catalog."
    )]
    pub(crate) async fn bbox_artifact_install(
        &self,
        Parameters(p): Parameters<ArtifactInstallParams>,
    ) -> CallToolResult {
        match install_artifact_from_params(&self.state, p).await {
            Ok(meta) => Self::ok_json(&serde_json::to_value(meta).unwrap_or_default()),
            Err(e) => Self::err_text(&format!("artifact install failed: {e:#}")),
        }
    }

    #[tool(
        name = "bbox_artifact_list",
        description = "List installed workflow, packet, brofile, agent, atom, team, and cron artifacts with version, source, active status, and supersession metadata."
    )]
    pub(crate) fn bbox_artifact_list(
        &self,
        Parameters(p): Parameters<ArtifactListParams>,
    ) -> CallToolResult {
        Self::run("bbox_artifact_list", || {
            let rows = self.state.artifacts.read().list(&p)?;
            Ok(serde_json::to_string_pretty(
                &serde_json::json!({ "artifacts": rows }),
            )?)
        })
    }

    #[tool(
        name = "bbox_artifact_supersede",
        description = "Mark one installed artifact superseded by another artifact of the same kind."
    )]
    pub(crate) async fn bbox_artifact_supersede(
        &self,
        Parameters(p): Parameters<ArtifactSupersedeParams>,
    ) -> CallToolResult {
        // supersede holds artifacts.write() across a flock + fsync + rename.
        let server = self.clone();
        Self::run_blocking("bbox_artifact_supersede", move || {
            let kind = p.kind;
            let name = p.name.clone();
            let meta =
                server
                    .state
                    .artifacts
                    .write()
                    .supersede(p.kind, &p.name, &p.superseded_by)?;
            deactivate_artifact(&server.state, kind, &name)?;
            Ok(serde_json::to_string_pretty(&meta)?)
        })
        .await
    }

    #[tool(
        name = "bbox_artifact_remove",
        description = "Hard-remove one installed artifact."
    )]
    pub(crate) async fn bbox_artifact_remove(
        &self,
        Parameters(p): Parameters<ArtifactRemoveParams>,
    ) -> CallToolResult {
        // remove_hard runs flock'd store rewrites + file removals.
        let server = self.clone();
        Self::run_blocking("bbox_artifact_remove", move || {
            if !p.dry_run && !p.confirm {
                anyhow::bail!("hard artifact removal requires confirm=true");
            }
            if !p.dry_run {
                server
                    .state
                    .artifacts
                    .read()
                    .remove_hard(p.kind, &p.name, true, true)?;
                deactivate_artifact(&server.state, p.kind, &p.name)?;
            }
            let result = server
                .state
                .artifacts
                .write()
                .remove_hard(p.kind, &p.name, p.dry_run, p.confirm)?;
            Ok(serde_json::to_string_pretty(&result)?)
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::artifacts;
    use crate::orchestration;
    use crate::server::routes::{install_artifact_value, restore_runtime_artifacts_from_catalog};
    use crate::server::state::SharedState;
    use crate::{packets, workflow};
    use serde_json::{Value, json};
    use std::sync::Arc;

    fn test_server(tmp: &tempfile::TempDir) -> BlackboxServer {
        BlackboxServer::new(Arc::new(SharedState::for_test(&tmp.path().join("bro"))))
    }
    #[tokio::test]
    async fn artifact_install_wires_f3_workflow_and_packet() {
        let tmp = tempfile::tempdir().unwrap();
        let server = test_server(&tmp);
        let workflow_value: Value = serde_json::from_str(include_str!(
            "../../system-defaults/agentic-corpus/workflows/schema-migration-arc.json"
        ))
        .unwrap();
        let packet_value: Value = serde_json::from_str(include_str!(
            "../../system-defaults/agentic-corpus/packets/workflow-policy/arc-budget.json"
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
                source: "system-defaults/agentic-corpus/packets/workflow-policy/arc-budget.json"
                    .into(),
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

    async fn install_team_brofile(server: &BlackboxServer, name: &str) {
        install_artifact_value(
            &server.state,
            ArtifactInstallParams {
                kind: artifacts::ArtifactKind::Brofile,
                source: format!("{name}.json"),
                name: None,
                version: Some("1".into()),
                supersedes: None,
            },
            json!({"name": name, "provider": "glm"}),
        )
        .await
        .unwrap();
    }

    async fn install_team_value(
        server: &BlackboxServer,
        value: Value,
        version: &str,
    ) -> anyhow::Result<artifacts::ArtifactMetadata> {
        install_artifact_value(
            &server.state,
            ArtifactInstallParams {
                kind: artifacts::ArtifactKind::Team,
                source: "team.json".into(),
                name: None,
                version: Some(version.into()),
                supersedes: None,
            },
            value,
        )
        .await
    }

    #[tokio::test]
    async fn team_artifact_install_materializes_teamplate_and_team() {
        // gap-37a280a6: install must reach the runtime stores — ensemble
        // actors resolve instantiated teams only (load_team, no teamplate
        // fallback), so a stored-only artifact is a dispatch-time trap.
        let tmp = tempfile::tempdir().unwrap();
        let server = test_server(&tmp);
        install_team_brofile(&server, "tm-specialist").await;

        install_team_value(
            &server,
            json!({
                "name": "tm-panel",
                "members": [{"brofile": "tm-specialist", "alias": "lens", "count": 2}]
            }),
            "1",
        )
        .await
        .unwrap();

        let store_dir = &server.state.store_dir;
        assert!(
            orchestration::team::resolve_teamplate("tm-panel", store_dir, None).is_some(),
            "teamplate store written"
        );
        let team = orchestration::team::load_team("tm-panel", store_dir)
            .expect("team instantiated under the teamplate's own name");
        assert_eq!(team.members.len(), 2, "count expansion applied");
        assert_eq!(team.members[0].name, "lens-1");
    }

    #[tokio::test]
    async fn team_artifact_install_fails_on_missing_member_brofile() {
        let tmp = tempfile::tempdir().unwrap();
        let server = test_server(&tmp);
        let err = install_team_value(
            &server,
            json!({"name": "tm-broken", "members": [{"brofile": "no-such-brofile"}]}),
            "1",
        )
        .await
        .unwrap_err();
        assert!(
            err.to_string().contains("member brofile not found"),
            "got: {err:#}"
        );
        assert!(
            orchestration::team::load_team("tm-broken", &server.state.store_dir).is_none(),
            "failed install must not half-instantiate"
        );
    }

    #[tokio::test]
    async fn team_artifact_install_rejects_advisor_teamplates() {
        let tmp = tempfile::tempdir().unwrap();
        let server = test_server(&tmp);
        install_team_brofile(&server, "tm-adv").await;
        let err = install_team_value(
            &server,
            json!({
                "name": "tm-advised",
                "members": [{"brofile": "tm-adv"}],
                "advisor": {"brofile": "tm-adv", "charter": "watch the panel"}
            }),
            "1",
        )
        .await
        .unwrap_err();
        assert!(
            err.to_string().contains("advisor"),
            "advisor teamplates need bro_team create (live dispatch): {err:#}"
        );
    }

    #[tokio::test]
    async fn team_artifact_reinstall_preserves_live_team_state() {
        let tmp = tempfile::tempdir().unwrap();
        let server = test_server(&tmp);
        install_team_brofile(&server, "tm-live").await;
        install_team_value(
            &server,
            json!({"name": "tm-durable", "members": [{"brofile": "tm-live"}]}),
            "1",
        )
        .await
        .unwrap();

        // A member acquires live session state between installs.
        let store_dir = server.state.store_dir.clone();
        let mut team = orchestration::team::load_team("tm-durable", &store_dir).unwrap();
        team.members[0].session_id = Some("sess-live".into());
        orchestration::team::save_team(&team, &store_dir);

        install_team_value(
            &server,
            json!({"name": "tm-durable", "members": [{"brofile": "tm-live", "count": 3}]}),
            "2",
        )
        .await
        .unwrap();

        let team = orchestration::team::load_team("tm-durable", &store_dir).unwrap();
        assert_eq!(
            team.members[0].session_id.as_deref(),
            Some("sess-live"),
            "re-install must not clobber a live team's member sessions"
        );
        assert_eq!(team.members.len(), 1, "live roster untouched by upgrade");
        // The refreshed teamplate IS picked up for future creates.
        let tp =
            orchestration::team::resolve_teamplate("tm-durable", &store_dir, None).unwrap();
        assert_eq!(tp.members[0].count, 3);
    }

    #[tokio::test]
    async fn active_workflow_artifact_restores_runtime_registry_on_boot() {
        let tmp = tempfile::tempdir().unwrap();
        let server = test_server(&tmp);
        let workflow_value: Value = serde_json::from_str(include_str!(
            "../../system-defaults/agentic-corpus/workflows/schema-migration-arc.json"
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
            "../../system-defaults/agentic-corpus/packets/phase-decompose/dag-structure.json"
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

        // Boot restore runs on every daemon start: re-running it must not
        // mint another copy of an unchanged packet (the pre-fix behavior
        // grew the store by one file per artifact per restart).
        let count_after_first = server.state.packets.read().list_all().unwrap().len();
        let restored_again = restore_runtime_artifacts_from_catalog(&server.state).unwrap();
        assert_eq!(restored_again, 1);
        assert_eq!(
            server.state.packets.read().list_all().unwrap().len(),
            count_after_first,
            "second restore of an unchanged packet artifact must be idempotent"
        );
    }
    #[tokio::test]
    async fn artifact_install_wires_project_bootstrap_arc() {
        let tmp = tempfile::tempdir().unwrap();
        let server = test_server(&tmp);
        let workflow_value: Value = serde_json::from_str(include_str!(
            "../../system-defaults/agentic-corpus/workflows/project-bootstrap-arc.json"
        ))
        .unwrap();

        install_artifact_value(
            &server.state,
            ArtifactInstallParams {
                kind: artifacts::ArtifactKind::Workflow,
                source: "system-defaults/agentic-corpus/workflows/project-bootstrap-arc.json"
                    .into(),
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
            "../../system-defaults/agentic-corpus/workflows/embed-compaction-arc.json"
        ))
        .unwrap();
        let policy_value: Value = serde_json::from_str(include_str!(
            "../../system-defaults/agentic-corpus/packets/embed/compaction-policy.json"
        ))
        .unwrap();
        let cron_routing_value: Value = serde_json::from_str(include_str!(
            "../../system-defaults/agentic-corpus/packets/cron-routing/embed-compaction.json"
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
                source: "system-defaults/agentic-corpus/packets/embed/compaction-policy.json"
                    .into(),
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
            "../../system-defaults/maintenance/workflows/daily-compaction-arc.json"
        ))
        .unwrap();
        let arc_budget_value: Value = serde_json::from_str(include_str!(
            "../../system-defaults/agentic-corpus/packets/workflow-policy/arc-budget.json"
        ))
        .unwrap();
        let embed_policy_value: Value = serde_json::from_str(include_str!(
            "../../system-defaults/agentic-corpus/packets/embed/compaction-policy.json"
        ))
        .unwrap();
        let cron_routing_value: Value = serde_json::from_str(include_str!(
            "../../system-defaults/maintenance/packets/cron-routing/daily-compaction.json"
        ))
        .unwrap();
        let cron_value: Value = serde_json::from_str(include_str!(
            "../../system-defaults/maintenance/crons/daily-compaction.json"
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
                source: "system-defaults/agentic-corpus/packets/workflow-policy/arc-budget.json"
                    .into(),
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
                source: "system-defaults/agentic-corpus/packets/embed/compaction-policy.json"
                    .into(),
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
                source: "system-defaults/maintenance/packets/cron-routing/daily-compaction.json"
                    .into(),
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
    async fn artifact_install_wires_m3_auto_digest_artifacts_and_audit() {
        let tmp = tempfile::tempdir().unwrap();
        let server = test_server(&tmp);
        let brofile_value: Value = serde_json::from_str(include_str!(
            "../../system-defaults/agentic-corpus/brofiles/digest-extractor.json"
        ))
        .unwrap();
        assert_eq!(
            brofile_value["disallow_tools"],
            serde_json::json!(["Edit", "Write", "Bash"])
        );
        let trust_value: Value = serde_json::from_str(include_str!(
            "../../system-defaults/agentic-corpus/packets/bro-trust/per-brofile.json"
        ))
        .unwrap();
        let quality_value: Value = serde_json::from_str(include_str!(
            "../../system-defaults/agentic-corpus/packets/auto-digest/entry-quality.json"
        ))
        .unwrap();
        let routing_value: Value = serde_json::from_str(include_str!(
            "../../system-defaults/agentic-corpus/packets/auto-digest/task-completed-routing.json"
        ))
        .unwrap();
        let workflow_value: Value = serde_json::from_str(include_str!(
            "../../system-defaults/agentic-corpus/workflows/auto-digest-arc.json"
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
                source: "system-defaults/agentic-corpus/packets/auto-digest/entry-quality.json"
                    .into(),
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
            orchestration::brofile::resolve_brofile(
                "digest-extractor",
                &server.state.store_dir,
                None
            )
            .is_some()
        );

        let cases: Value =
            serde_json::from_str(include_str!("../../eval/audit/auto-digest/cases.json")).unwrap();
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
            "../../system-defaults/agentic-corpus/workflows/contradiction-review-arc.json"
        ))
        .unwrap();
        let packet_value: Value = serde_json::from_str(include_str!(
            "../../system-defaults/agentic-corpus/packets/contradiction/review-synthesis.json"
        ))
        .unwrap();
        let brofiles: [(&str, Value); 4] = [
            (
                "contradiction-provenance",
                serde_json::from_str(include_str!(
                    "../../system-defaults/agentic-corpus/brofiles/contradiction-provenance.json"
                ))
                .unwrap(),
            ),
            (
                "contradiction-lifecycle",
                serde_json::from_str(include_str!(
                    "../../system-defaults/agentic-corpus/brofiles/contradiction-lifecycle.json"
                ))
                .unwrap(),
            ),
            (
                "contradiction-coherence",
                serde_json::from_str(include_str!(
                    "../../system-defaults/agentic-corpus/brofiles/contradiction-coherence.json"
                ))
                .unwrap(),
            ),
            (
                "contradiction-facilitator",
                serde_json::from_str(include_str!(
                    "../../system-defaults/agentic-corpus/brofiles/contradiction-facilitator.json"
                ))
                .unwrap(),
            ),
        ];

        install_artifact_value(
            &server.state,
            ArtifactInstallParams {
                kind: artifacts::ArtifactKind::Packet,
                source:
                    "system-defaults/agentic-corpus/packets/contradiction/review-synthesis.json"
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
                source: "system-defaults/agentic-corpus/workflows/contradiction-review-arc.json"
                    .into(),
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
            "../../system-defaults/agentic-corpus/packets/auto-edge/vote-aggregate.json"
        ))
        .unwrap();
        let workflow_value: Value = serde_json::from_str(include_str!(
            "../../system-defaults/agentic-corpus/workflows/auto-edge-arc.json"
        ))
        .unwrap();
        let brofiles: [(&str, Value); 6] = [
        (
            "describe-prose-signal",
            serde_json::from_str(include_str!(
                "../../system-defaults/agentic-corpus/brofiles/describe-prose-signal.json"
            ))
            .unwrap(),
        ),
        (
            "describe-symbol-fit",
            serde_json::from_str(include_str!(
                "../../system-defaults/agentic-corpus/brofiles/describe-symbol-fit.json"
            ))
            .unwrap(),
        ),
        (
            "describe-narrative-cohesion",
            serde_json::from_str(include_str!(
                "../../system-defaults/agentic-corpus/brofiles/describe-narrative-cohesion.json"
            ))
            .unwrap(),
        ),
        (
            "reference-citation-precision",
            serde_json::from_str(include_str!(
                "../../system-defaults/agentic-corpus/brofiles/reference-citation-precision.json"
            ))
            .unwrap(),
        ),
        (
            "reference-target-existence",
            serde_json::from_str(include_str!(
                "../../system-defaults/agentic-corpus/brofiles/reference-target-existence.json"
            ))
            .unwrap(),
        ),
        (
            "reference-context-fit",
            serde_json::from_str(include_str!(
                "../../system-defaults/agentic-corpus/brofiles/reference-context-fit.json"
            ))
            .unwrap(),
        ),
    ];

        install_artifact_value(
            &server.state,
            ArtifactInstallParams {
                kind: artifacts::ArtifactKind::Packet,
                source: "system-defaults/agentic-corpus/packets/auto-edge/vote-aggregate.json"
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
            assert!(
                orchestration::brofile::resolve_brofile(name, &server.state.store_dir, None)
                    .is_some()
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
            serde_json::from_str::<Value>(include_str!(
                "../../eval/audit/auto-edge/describes.json"
            ))
            .unwrap(),
            serde_json::from_str::<Value>(include_str!(
                "../../eval/audit/auto-edge/references.json"
            ))
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
            let value: Value =
                serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
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
}
