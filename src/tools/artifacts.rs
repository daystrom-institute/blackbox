use crate::artifacts::{
    ArtifactInstallParams, ArtifactListParams, ArtifactRemoveParams, ArtifactSupersedeParams,
};
use crate::server::BlackboxServer;
use crate::server::routes::{
    deactivate_artifact, install_artifact_from_params, install_artifact_value,
};

use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::CallToolResult;
use rmcp::schemars;
use rmcp::{tool, tool_router};

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct ArtifactInstallToolParams {
    pub kind: crate::artifacts::ArtifactKind,
    /// Explicit HTTP(S) JSON URL. Supply exactly one of source or artifact; filesystem paths are not accepted.
    #[serde(default)]
    pub source: Option<String>,
    /// Inline artifact object, validated against the selected kind's existing schema (maximum 1 MiB).
    #[serde(default)]
    pub artifact: Option<serde_json::Map<String, serde_json::Value>>,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub version: Option<String>,
    #[serde(default)]
    pub supersedes: Option<String>,
}

fn artifact_install_input(
    p: ArtifactInstallToolParams,
) -> anyhow::Result<(ArtifactInstallParams, Option<serde_json::Value>)> {
    let (source, artifact) = match (p.source, p.artifact) {
        (Some(source), None) => {
            let url = reqwest::Url::parse(&source).ok().filter(|url| matches!(url.scheme(), "http" | "https") && url.host_str().is_some())
                .ok_or_else(|| anyhow::anyhow!("source must be an explicit HTTP(S) URL; caller filesystem paths are not readable by this MCP server. Supply artifact with the inline JSON object instead"))?;
            (url.to_string(), None)
        }
        (None, Some(artifact)) => {
            let artifact = serde_json::Value::Object(artifact);
            if serde_json::to_vec(&artifact)?.len() > 1024 * 1024 {
                anyhow::bail!("inline artifact exceeds the 1 MiB input limit");
            }
            ("inline:mcp".to_owned(), Some(artifact))
        }
        _ => anyhow::bail!(
            "Supply exactly one of source (HTTP(S) URL) or artifact (inline JSON object)"
        ),
    };
    Ok((
        ArtifactInstallParams {
            kind: p.kind,
            source,
            name: p.name,
            version: p.version,
            supersedes: p.supersedes,
        },
        artifact,
    ))
}

fn installed_artifact_response(meta: &crate::artifacts::ArtifactMetadata) -> serde_json::Value {
    let mut response = serde_json::json!({"kind": meta.kind, "name": meta.name, "version": meta.version, "active": meta.active});
    if !meta.install_warnings.is_empty() {
        response["warnings"] = serde_json::json!(meta.install_warnings);
    }
    if let Some(replacement) = &meta.superseded_by {
        response["superseded_by"] = serde_json::json!(replacement);
    }
    response
}

#[derive(Debug, serde::Deserialize, rmcp::schemars::JsonSchema)]
pub(crate) struct ArtifactCatalogListParams {
    #[serde(flatten)]
    pub filters: ArtifactListParams,
    /// Maximum summaries, default 20, maximum 100.
    #[serde(default)]
    pub limit: Option<usize>,
    /// Continue with next_offset, ordered by kind, name, newest installation, version.
    #[serde(default)]
    pub offset: Option<usize>,
    /// Include installation time and supersession history. Storage paths are never returned.
    #[serde(default)]
    pub detail: bool,
}

fn artifact_list_page(
    mut rows: Vec<crate::artifacts::ArtifactListEntry>,
    p: &ArtifactCatalogListParams,
) -> anyhow::Result<serde_json::Value> {
    rows.sort_by(|a, b| {
        a.kind
            .as_str()
            .cmp(b.kind.as_str())
            .then_with(|| a.name.cmp(&b.name))
            .then_with(|| b.installed_at.cmp(&a.installed_at))
            .then_with(|| a.version.cmp(&b.version))
    });
    let total = rows.len();
    let offset = p.offset.unwrap_or(0);
    let limit = p.limit.unwrap_or(20).clamp(1, 100);
    let artifacts: Vec<_> = rows.into_iter().skip(offset).take(limit).map(|entry| {
        let mut row = serde_json::json!({"kind": entry.kind, "name": entry.name, "version": entry.version, "active": entry.active});
        if let Some(description) = entry.description {
            let preview: String = if p.detail { description.clone() } else { description.chars().take(200).collect() };
            if preview.len() < description.len() { row["description_truncated"] = serde_json::json!(true); }
            row["description"] = serde_json::json!(preview);
        }
        if let Some(replacement) = entry.superseded_by { row["superseded_by"] = serde_json::json!(replacement); }
        if p.detail {
            row["installed_at"] = serde_json::json!(entry.installed_at);
            row["supersedes_chain"] = serde_json::json!(entry.supersedes_chain);
        }
        row
    }).collect();
    let next_offset = offset.saturating_add(artifacts.len());
    bbox_corpus_core::response_page::bound_page(
        serde_json::json!({"artifacts": artifacts, "total": total, "limit": limit, "offset": offset,
            "next_offset": (next_offset < total).then_some(next_offset),
            "order": "kind_asc,name_asc,installed_at_desc,version_asc",
            "detail_hint": "bbox_artifact_list(kind=<kind>,name=<name>,detail=true)",
        }),
        "artifacts",
    )
}

pub(crate) fn router() -> ToolRouter<BlackboxServer> {
    BlackboxServer::artifacts_tools()
}

#[tool_router(router = artifacts_tools)]
impl BlackboxServer {
    #[tool(
        name = "bbox_artifact_install",
        description = "Install a typed artifact from an inline artifact object or explicit HTTP(S) source URL. Supply exactly one; caller filesystem paths are rejected. The selected kind controls validation. Returns activation state and actionable warnings without source credentials or storage paths."
    )]
    pub(crate) async fn bbox_artifact_install(
        &self,
        Parameters(p): Parameters<ArtifactInstallToolParams>,
    ) -> CallToolResult {
        let (params, artifact) = match artifact_install_input(p) {
            Ok(input) => input,
            Err(error) => return Self::err_text(&error.to_string()),
        };
        let result = match artifact {
            Some(value) => install_artifact_value(&self.state, params, value).await,
            None => install_artifact_from_params(&self.state, params).await,
        };
        match result {
            Ok(meta) => Self::ok_json(&installed_artifact_response(&meta)),
            Err(error) => {
                if let Some(network) = error
                    .chain()
                    .find_map(|cause| cause.downcast_ref::<reqwest::Error>())
                {
                    return Self::err_text(&format!(
                        "Artifact source request failed (HTTP status {:?}); verify the URL and server access. The source URL is withheld because it may contain credentials",
                        network.status()
                    ));
                }
                if let Some(failure) =
                    error.downcast_ref::<crate::server::routes::ArtifactInstallFailure>()
                {
                    return Self::err_text(&failure.response().to_string());
                }
                Self::err_text(&serde_json::json!({
                    "error": "error.artifact_source_read_failed", "completed": [],
                    "reason": "The source could not be loaded or decoded; no installation steps ran"
                }).to_string())
            }
        }
    }

    #[tool(
        name = "bbox_artifact_list",
        description = "List installed artifact summary pages (default 20, maximum 100). Continue with next_offset; filter by kind/name and set detail=true for installation and supersession metadata. Storage paths and source credentials are omitted."
    )]
    pub(crate) fn bbox_artifact_list(
        &self,
        Parameters(p): Parameters<ArtifactCatalogListParams>,
    ) -> CallToolResult {
        Self::run("bbox_artifact_list", || {
            let rows = self.state.artifacts.read().list(&p.filters)?;
            Ok(serde_json::to_string(&artifact_list_page(rows, &p)?)?)
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

    #[test]
    fn artifact_summary_pages_omit_storage_paths_and_bound_descriptions() {
        let rows: Vec<_> = (0..105)
            .rev()
            .map(|i| artifacts::ArtifactListEntry {
                kind: artifacts::ArtifactKind::Agent,
                name: format!("agent-{i:03}"),
                version: "1".into(),
                source: "https://example.test/?token=synthetic-secret".into(),
                installed_at: "2026-01-01T00:00:00Z".into(),
                active: true,
                supersedes_chain: vec!["previous".into()],
                path: "/private/daemon/artifact.json".into(),
                superseded_by: None,
                description: Some("界".repeat(300)),
            })
            .collect();
        let mut p: ArtifactCatalogListParams =
            serde_json::from_value(json!({"limit": 1000})).unwrap();
        let first = artifact_list_page(rows.clone(), &p).unwrap();
        let returned = first["artifacts"].as_array().unwrap().len();
        assert!(returned > 0 && returned <= 100);
        assert_eq!(first["next_offset"], returned);
        assert!(
            serde_json::to_vec(&first).unwrap().len()
                <= bbox_corpus_core::response_page::PAGE_BUDGET_BYTES
        );
        assert_eq!(first["artifacts"][0]["name"], "agent-000");
        assert_eq!(first["artifacts"][0]["description_truncated"], true);
        assert!(first["artifacts"][0].get("supersedes_chain").is_none());
        p.offset = Some(100);
        p.detail = true;
        let last = artifact_list_page(rows, &p).unwrap();
        assert_eq!(last["artifacts"].as_array().unwrap().len(), 5);
        assert!(last["next_offset"].is_null());
        assert_eq!(
            last["artifacts"][0]["supersedes_chain"],
            json!(["previous"])
        );
        for response in [first, last] {
            assert!(!response.to_string().contains("synthetic-secret"));
            assert!(!response.to_string().contains("/private/daemon"));
        }
    }

    #[test]
    fn artifact_expanded_nested_row_refuses_without_losing_continuation() {
        let entry = artifacts::ArtifactListEntry {
            kind: artifacts::ArtifactKind::Agent,
            name: "large-history".into(),
            version: "1".into(),
            source: "inline".into(),
            installed_at: "2026-01-01T00:00:00Z".into(),
            active: true,
            supersedes_chain: vec!["界".repeat(1000); 50],
            path: "/private/daemon/artifact.json".into(),
            superseded_by: None,
            description: Some("界".repeat(1000)),
        };
        let p: ArtifactCatalogListParams = serde_json::from_value(json!({"detail": true})).unwrap();
        assert!(
            artifact_list_page(vec![entry.clone()], &p)
                .unwrap_err()
                .to_string()
                .contains("collection_row_too_large")
        );
        let p: ArtifactCatalogListParams = serde_json::from_value(json!({})).unwrap();
        let summary = artifact_list_page(vec![entry], &p).unwrap();
        assert_eq!(summary["artifacts"][0]["description_truncated"], true);
        assert!(summary["next_offset"].is_null());
    }

    #[tokio::test]
    async fn artifact_install_mcp_rejects_existing_caller_file_before_installing() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let server = test_server(&tmp);
        let file = root.join("artifact.json");
        std::fs::write(&file, r#"{"name":"unexpected-local","provider":"glm"}"#).unwrap();
        let p: ArtifactInstallToolParams = serde_json::from_value(json!({
            "kind": "brofile", "source": file,
        }))
        .unwrap();
        let result = server.bbox_artifact_install(Parameters(p)).await;
        assert_eq!(result.is_error, Some(true));
        assert!(
            result.content[0]
                .as_text()
                .unwrap()
                .text
                .contains("HTTP(S)")
        );
        assert!(
            server
                .state
                .artifacts
                .read()
                .list(&ArtifactListParams {
                    kind: None,
                    name: None,
                    include_superseded: true
                })
                .unwrap()
                .is_empty()
        );
        assert!(
            orchestration::brofile::resolve_brofile(
                "unexpected-local",
                &server.state.store_dir,
                None
            )
            .is_none()
        );
    }

    #[tokio::test]
    async fn artifact_install_mcp_validates_inline_kind_and_returns_compact_activation() {
        let tmp = tempfile::tempdir().unwrap();
        let server = test_server(&tmp);
        let invalid: ArtifactInstallToolParams = serde_json::from_value(json!({
            "kind": "brofile", "artifact": {"name": "missing-provider"}, "version": "1",
        }))
        .unwrap();
        assert_eq!(
            server
                .bbox_artifact_install(Parameters(invalid))
                .await
                .is_error,
            Some(true)
        );
        let p: ArtifactInstallToolParams = serde_json::from_value(json!({
            "kind": "brofile", "artifact": {"name": "inline-example", "provider": "glm"}, "version": "1",
        })).unwrap();
        let result = server.bbox_artifact_install(Parameters(p)).await;
        assert_ne!(result.is_error, Some(true), "{result:?}");
        let response: Value =
            serde_json::from_str(&result.content[0].as_text().unwrap().text).unwrap();
        assert_eq!(response["name"], "inline-example");
        assert_eq!(response["active"], true);
        assert!(response.get("source").is_none());
        assert!(response.get("path").is_none());
        assert!(
            orchestration::brofile::resolve_brofile(
                "inline-example",
                &server.state.store_dir,
                None
            )
            .is_some()
        );
    }

    #[tokio::test]
    async fn artifact_install_override_matches_persisted_runtime_identity() {
        let tmp = tempfile::tempdir().unwrap();
        let server = test_server(&tmp);
        let p = serde_json::from_value(json!({"kind": "brofile", "name": "renamed", "version": "1", "artifact": {"name": "original", "provider": "glm"}})).unwrap();
        let response = server.bbox_artifact_install(Parameters(p)).await;
        assert_ne!(response.is_error, Some(true), "{response:?}");
        assert!(
            orchestration::brofile::resolve_brofile("renamed", &server.state.store_dir, None)
                .is_some()
        );
        assert!(
            orchestration::brofile::resolve_brofile("original", &server.state.store_dir, None)
                .is_none()
        );
        let value = server
            .state
            .artifacts
            .read()
            .load_artifact_value(artifacts::ArtifactKind::Brofile, "renamed")
            .unwrap()
            .unwrap();
        assert_eq!(value["name"], "renamed");
    }

    #[tokio::test]
    async fn artifact_install_missing_version_has_no_runtime_effects() {
        let tmp = tempfile::tempdir().unwrap();
        let server = test_server(&tmp);
        let p = serde_json::from_value(
            json!({"kind": "brofile", "artifact": {"name": "example", "provider": "glm"}}),
        )
        .unwrap();
        let response = server.bbox_artifact_install(Parameters(p)).await;
        assert_eq!(response.is_error, Some(true));
        let failure: Value =
            serde_json::from_str(&response.content[0].as_text().unwrap().text).unwrap();
        assert_eq!(failure["completed"], json!([]));
        assert_eq!(failure["failed"], "validation");
        assert!(
            orchestration::brofile::resolve_brofile("example", &server.state.store_dir, None)
                .is_none()
        );
    }

    #[tokio::test]
    async fn artifact_install_team_write_failure_reports_completed_effects() {
        for blocked_stage in ["teamplates", "teams"] {
            let tmp = tempfile::tempdir().unwrap();
            let root = tmp.path().canonicalize().unwrap();
            let server = test_server(&tmp);
            let member: orchestration::brofile::Brofile =
                serde_json::from_value(json!({"name": "example", "provider": "glm"})).unwrap();
            orchestration::brofile::save_brofile(&member, "global", &server.state.store_dir, None)
                .unwrap();
            std::fs::create_dir_all(&server.state.store_dir).unwrap();
            std::fs::write(server.state.store_dir.join(blocked_stage), "blocked").unwrap();
            let p = serde_json::from_value(json!({"kind": "team", "version": "1", "artifact": {"name": "example", "members": [{"brofile": "example", "count": 1}]}})).unwrap();
            let response = server.bbox_artifact_install(Parameters(p)).await;
            assert_eq!(response.is_error, Some(true), "{response:?}");
            let failure: Value =
                serde_json::from_str(&response.content[0].as_text().unwrap().text).unwrap();
            assert!(!failure.to_string().contains(root.to_str().unwrap()));
            let completed = failure["completed"].as_array().unwrap();
            assert_eq!(
                completed.contains(&json!("teamplate_file")),
                blocked_stage == "teams"
            );
            assert_eq!(
                failure["failed"],
                if blocked_stage == "teams" {
                    "team_instance"
                } else {
                    "teamplate_file"
                }
            );
            assert!(
                failure["not_attempted"]
                    .as_array()
                    .unwrap()
                    .contains(&json!("catalog_persistence"))
            );
            assert!(
                server
                    .state
                    .artifacts
                    .read()
                    .metadata_for(artifacts::ArtifactKind::Team, "example")
                    .unwrap()
                    .is_none()
            );
        }
    }

    #[tokio::test]
    async fn artifact_install_catalog_failure_reports_persisted_runtime() {
        let tmp = tempfile::tempdir().unwrap();
        let server = test_server(&tmp);
        let catalog_root = server.state.artifacts.read().root().to_owned();
        std::fs::write(catalog_root.join("brofile"), "blocked").unwrap();
        let p = serde_json::from_value(json!({"kind": "brofile", "version": "1", "artifact": {"name": "example", "provider": "glm"}})).unwrap();
        let response = server.bbox_artifact_install(Parameters(p)).await;
        assert_eq!(response.is_error, Some(true));
        let failure: Value =
            serde_json::from_str(&response.content[0].as_text().unwrap().text).unwrap();
        assert_eq!(failure["failed"], "catalog_persistence");
        assert!(
            failure["completed"]
                .as_array()
                .unwrap()
                .contains(&json!("brofile_file"))
        );
        assert!(
            orchestration::brofile::resolve_brofile("example", &server.state.store_dir, None)
                .is_some()
        );
    }

    #[test]
    fn artifact_install_input_requires_one_inline_object_or_http_url() {
        for payload in [
            json!({"kind": "brofile"}),
            json!({"kind": "brofile", "artifact": {}, "source": "https://example.test/artifact.json"}),
            json!({"kind": "brofile", "source": "file:///private/credential.json"}),
            json!({"kind": "brofile", "source": "relative/file.json"}),
            json!({"kind": "brofile", "artifact": {"lens": "x".repeat(1024*1024)}}),
        ] {
            assert!(artifact_install_input(serde_json::from_value(payload).unwrap()).is_err());
        }
        let p = serde_json::from_value(json!({"kind": "brofile", "source": "https://example.test/artifact.json?key=synthetic-secret"})).unwrap();
        let (params, value) = artifact_install_input(p).unwrap();
        assert!(value.is_none());
        assert_eq!(
            params.source,
            "https://example.test/artifact.json?key=synthetic-secret"
        );
    }

    fn test_server(tmp: &tempfile::TempDir) -> BlackboxServer {
        BlackboxServer::new(Arc::new(SharedState::for_test(&tmp.path().join("bro"))))
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
            err.to_string()
                .contains("automatic team advisors are retired"),
            "retired automatic advisors must be rejected by artifact install: {err:#}"
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
        let tp = orchestration::team::resolve_teamplate("tm-durable", &store_dir, None).unwrap();
        assert_eq!(tp.members[0].count, 3);
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
