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
#[serde(rename_all = "snake_case")]
pub(crate) enum InstallableArtifactKind {
    Packet,
    Brofile,
    Agent,
    Team,
}

impl From<InstallableArtifactKind> for crate::artifacts::ArtifactKind {
    fn from(kind: InstallableArtifactKind) -> Self {
        match kind {
            InstallableArtifactKind::Packet => Self::Packet,
            InstallableArtifactKind::Brofile => Self::Brofile,
            InstallableArtifactKind::Agent => Self::Agent,
            InstallableArtifactKind::Team => Self::Team,
        }
    }
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct ArtifactInstallToolParams {
    pub kind: InstallableArtifactKind,
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
            kind: p.kind.into(),
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
    compact_artifact_fields(&mut response);
    response
}

fn artifact_metadata_view(meta: &crate::artifacts::ArtifactMetadata) -> serde_json::Value {
    serde_json::json!({
        "kind": meta.kind, "name": meta.name, "version": meta.version,
        "active": meta.active && !matches!(meta.kind, crate::artifacts::ArtifactKind::Workflow | crate::artifacts::ArtifactKind::Atom | crate::artifacts::ArtifactKind::Cron),
        "retired": matches!(meta.kind, crate::artifacts::ArtifactKind::Workflow | crate::artifacts::ArtifactKind::Atom | crate::artifacts::ArtifactKind::Cron),
        "installed_at": meta.installed_at,
        "content_sha256": meta.content_sha256, "project_id": meta.project_id,
        "local": meta.local, "supersedes": meta.supersedes,
        "supersedes_chain": meta.supersedes_chain, "superseded_by": meta.superseded_by,
        "install_warnings": meta.install_warnings,
    })
}

fn compact_artifact_fields(row: &mut serde_json::Value) {
    let mut omitted = Vec::new();
    for (key, value) in row.as_object_mut().expect("artifact object") {
        let bytes = value.to_string().len();
        if bytes > 1024 {
            omitted.push(key.clone());
            *value = serde_json::json!({"detail_bytes": bytes});
        }
    }
    if !omitted.is_empty() {
        row["omitted_fields"] = serde_json::json!(omitted);
        row["exact_reader"] = serde_json::json!(
            "bbox_artifact_list with the same filters and body_limit=4096 recovers the inventory; metadata=true with kind/name/version recovers installation metadata"
        );
    }
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
    /// Read one complete redacted installation record. Requires kind and name;
    /// version selects a historical record, otherwise the current receipt.
    #[serde(default)]
    pub metadata: bool,
    #[serde(default)]
    pub version: Option<String>,
    /// Exact redacted inventory or metadata pages (4..=4096 bytes). Omit
    /// limit/offset. Source URLs and daemon paths are never returned.
    #[serde(default)]
    pub body_limit: Option<usize>,
    /// Continue body.next_cursor with the same selectors; changed content refuses.
    #[serde(default)]
    pub cursor: Option<String>,
}

fn artifact_list_page(
    mut rows: Vec<crate::artifacts::ArtifactListEntry>,
    p: &ArtifactCatalogListParams,
) -> anyhow::Result<serde_json::Value> {
    let exact = p.cursor.is_some() || p.body_limit.is_some();
    if exact && (p.limit.is_some() || p.offset.is_some()) {
        anyhow::bail!("exact inventory uses cursor/body_limit; omit limit and offset");
    }
    let retired = |kind| {
        matches!(
            kind,
            crate::artifacts::ArtifactKind::Workflow
                | crate::artifacts::ArtifactKind::Atom
                | crate::artifacts::ArtifactKind::Cron
        )
    };
    rows.retain(|entry| p.filters.kind.is_some() || !retired(entry.kind));
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
    let artifacts: Vec<_> = rows.into_iter().skip(if exact {0} else {offset}).take(if exact {usize::MAX} else {limit}).map(|entry| {
        let mut row = serde_json::json!({"kind": entry.kind, "name": entry.name, "version": entry.version, "active": entry.active && !retired(entry.kind)});
        if retired(entry.kind) { row["retired"] = serde_json::json!(true); }
        if let Some(description) = entry.description {
            let preview: String = if exact || p.detail { description.clone() } else { description.chars().take(200).collect() };
            if preview.len() < description.len() { row["description_truncated"] = serde_json::json!(true); }
            row["description"] = serde_json::json!(preview);
        }
        if let Some(replacement) = entry.superseded_by { row["superseded_by"] = serde_json::json!(replacement); }
        if exact || p.detail {
            row["installed_at"] = serde_json::json!(entry.installed_at);
            row["supersedes_chain"] = serde_json::json!(entry.supersedes_chain);
        }
        if !exact { compact_artifact_fields(&mut row); }
        row
    }).collect();
    if exact {
        let scope =
            serde_json::json!([p.filters.kind, p.filters.name, p.filters.include_superseded])
                .to_string();
        let body = bbox_corpus_core::response_page::json_body_page(
            &format!("artifact-inventory:{scope}"),
            &serde_json::json!({"artifacts": artifacts}),
            p.cursor.as_deref(),
            p.body_limit,
        )?;
        return Ok(serde_json::json!({"body": body}));
    }
    let next_offset = offset.saturating_add(artifacts.len());
    bbox_corpus_core::response_page::bound_page(
        serde_json::json!({"artifacts": artifacts, "total": total, "limit": limit, "offset": offset,
            "next_offset": (next_offset < total).then_some(next_offset),
            "pagination": "live_offset: installs and supersession can move rows; restart at offset 0 after changes",
            "exact_reader": "Same filters with body_limit=4096 and no limit/offset recover the complete redacted inventory",
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
        description = "Install a packet, brofile, simple agent or team from an inline artifact object or explicit HTTP(S) URL. Supply exactly one; caller filesystem paths are rejected. Workflow, atom and cron installation is retired."
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
        description = "List installed artifact summaries with live next_offset pages. body_limit/cursor recovers the complete redacted inventory; metadata=true with kind/name and optional version reads an exact installation receipt. Retired kinds require an explicit kind filter."
    )]
    pub(crate) fn bbox_artifact_list(
        &self,
        Parameters(p): Parameters<ArtifactCatalogListParams>,
    ) -> CallToolResult {
        Self::run("bbox_artifact_list", || {
            if p.metadata {
                if p.limit.is_some()
                    || p.offset.is_some()
                    || p.detail
                    || p.filters.include_superseded
                {
                    anyhow::bail!(
                        "metadata reads accept kind/name/version and body_limit/cursor, not list/detail selectors"
                    );
                }
                let kind = p
                    .filters
                    .kind
                    .ok_or_else(|| anyhow::anyhow!("metadata requires kind"))?;
                let name = p
                    .filters
                    .name
                    .as_deref()
                    .ok_or_else(|| anyhow::anyhow!("metadata requires name"))?;
                let store = self.state.artifacts.read();
                let meta = match p.version.as_deref() {
                    Some(version) => store.metadata_for_version(kind, name, version),
                    None => store.metadata_for(kind, name),
                }
                .map_err(|_| anyhow::anyhow!("error.artifact_metadata_unavailable: installation metadata could not be read"))?
                .ok_or_else(|| anyhow::anyhow!("artifact metadata not found"))?;
                let body = bbox_corpus_core::response_page::json_body_page(
                    &serde_json::json!(["artifact-metadata", kind, name, p.version]).to_string(),
                    &artifact_metadata_view(&meta),
                    p.cursor.as_deref(),
                    p.body_limit,
                )?;
                return Ok(serde_json::json!({"body": body}).to_string());
            }
            if p.version.is_some() {
                anyhow::bail!("version requires metadata=true");
            }
            let rows = self.state.artifacts.read().list(&p.filters).map_err(|_| {
                anyhow::anyhow!(
                    "error.artifact_inventory_unavailable: installation inventory could not be read"
                )
            })?;
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
                    .supersede(p.kind, &p.name, &p.superseded_by)
                    .map_err(|_| anyhow::anyhow!("error.artifact_supersede_failed: catalog supersession failed; inspect both installed names with bbox_artifact_list before retrying"))?;
            let mut response = installed_artifact_response(&meta);
            response["catalog_updated"] = serde_json::json!(true);
            match deactivate_artifact(&server.state, kind, &name) {
                Ok(()) => {
                    response["runtime_deactivated"] = serde_json::json!(true);
                }
                Err(_) => {
                    response["status"] = serde_json::json!("partial");
                    response["runtime_deactivated"] = serde_json::json!(false);
                    response["error"] = serde_json::json!(
                        "artifact catalog was updated but runtime deactivation failed"
                    );
                }
            }
            Ok(response.to_string())
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
    use crate::packets;
    use crate::server::routes::{install_artifact_value, restore_runtime_artifacts_from_catalog};
    use crate::server::state::SharedState;
    use serde_json::{Value, json};
    use std::sync::Arc;

    #[tokio::test]
    async fn artifact_fetch_failure_withholds_url_credentials_in_complete_reply() {
        use tokio::io::AsyncWriteExt;
        let tmp = tempfile::tempdir().unwrap();
        let server = test_server(&tmp);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let responder = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            socket
                .write_all(
                    b"HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                )
                .await
                .unwrap();
        });
        let result = server.bbox_artifact_install(Parameters(serde_json::from_value(json!({
            "kind":"brofile", "source":format!("http://synthetic-user:synthetic-password@{addr}/private-artifact?token=synthetic-query")
        })).unwrap())).await;
        responder.await.unwrap();
        assert_eq!(result.is_error, Some(true));
        let encoded = serde_json::to_string(&result).unwrap();
        for secret in [
            "synthetic-user",
            "synthetic-password",
            "synthetic-query",
            "private-artifact",
        ] {
            assert!(!encoded.contains(secret), "{encoded}");
        }
    }

    #[tokio::test]
    async fn artifact_supersede_and_metadata_withhold_source_credentials() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let server = BlackboxServer::new(Arc::new(SharedState::for_test(&root.join("bro"))));
        let source = "https://synthetic-user:synthetic-password@example.test/private-artifact?token=synthetic-query";
        let installed = server
            .state
            .artifacts
            .write()
            .install_value(
                artifacts::ArtifactKind::Brofile,
                source.into(),
                &json!({"name":"safe-artifact","version":1,"provider":"glm"}),
                None,
                None,
                None,
            )
            .unwrap();
        assert_eq!(installed.source, source);
        assert!(
            !installed_artifact_response(&installed)
                .to_string()
                .contains("synthetic-password")
        );
        server
            .state
            .artifacts
            .write()
            .install_value(
                artifacts::ArtifactKind::Brofile,
                "inline".into(),
                &json!({"name":"replacement","version":1,"provider":"glm"}),
                None,
                None,
                None,
            )
            .unwrap();
        let result = server
            .bbox_artifact_supersede(Parameters(
                serde_json::from_value(json!({
                    "kind":"brofile","name":"safe-artifact","superseded_by":"replacement"
                }))
                .unwrap(),
            ))
            .await;
        assert_ne!(result.is_error, Some(true), "{result:?}");
        let encoded = serde_json::to_string(&result).unwrap();
        for secret in [
            "synthetic-user",
            "synthetic-password",
            "synthetic-query",
            "private-artifact",
        ] {
            assert!(!encoded.contains(secret));
        }
        let mut args =
            json!({"kind":"brofile","name":"safe-artifact","metadata":true,"body_limit":128});
        let mut recovered = String::new();
        loop {
            let result = server
                .bbox_artifact_list(Parameters(serde_json::from_value(args.clone()).unwrap()));
            assert_ne!(result.is_error, Some(true), "{result:?}");
            let encoded = serde_json::to_string(&result).unwrap();
            assert!(encoded.len() < 65536);
            for secret in [
                source,
                "synthetic-password",
                "synthetic-query",
                root.to_str().unwrap(),
            ] {
                assert!(!encoded.contains(secret));
            }
            let page: Value =
                serde_json::from_str(&result.content[0].as_text().unwrap().text).unwrap();
            recovered.push_str(page["body"]["text"].as_str().unwrap());
            match page["body"]["next_cursor"].as_str() {
                Some(next) => args["cursor"] = json!(next),
                None => break,
            }
        }
        let metadata: Value = serde_json::from_str(&recovered).unwrap();
        assert_eq!(metadata["superseded_by"], "replacement");
        assert_eq!(metadata["active"], false);
        assert!(metadata.get("source").is_none());
        assert!(metadata.get("project_path").is_none());
    }

    #[tokio::test]
    async fn retired_artifact_kinds_cannot_activate_but_receipts_stay_readable() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let server = BlackboxServer::new(std::sync::Arc::new(
            crate::server::SharedState::for_test(&root),
        ));
        for kind in [
            artifacts::ArtifactKind::Workflow,
            artifacts::ArtifactKind::Atom,
            artifacts::ArtifactKind::Cron,
        ] {
            let request = json!({"kind":kind,"artifact":{"name":"archived","version":1}});
            assert!(serde_json::from_value::<ArtifactInstallToolParams>(request).is_err());
            let params = ArtifactInstallParams {
                kind,
                source: "http://127.0.0.1:1/never-fetch".into(),
                name: None,
                version: None,
                supersedes: None,
            };
            let error = install_artifact_from_params(&server.state, params)
                .await
                .unwrap_err();
            assert!(error.to_string().contains("retired_artifact_kind"));
            assert_eq!(
                serde_json::from_value::<artifacts::ArtifactKind>(json!(kind)).unwrap(),
                kind
            );
        }
        assert!(!root.join("workflows").exists());
        assert!(!root.join("crons").exists());
    }

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
    fn artifact_expanded_nested_row_has_exact_inventory_recovery() {
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
        let detail = artifact_list_page(vec![entry.clone()], &p).unwrap();
        assert!(
            detail["artifacts"][0]["omitted_fields"]
                .as_array()
                .unwrap()
                .contains(&json!("supersedes_chain"))
        );
        let mut p: ArtifactCatalogListParams =
            serde_json::from_value(json!({"body_limit":4096})).unwrap();
        let mut recovered = String::new();
        loop {
            let page = artifact_list_page(vec![entry.clone()], &p).unwrap();
            recovered.push_str(page["body"]["text"].as_str().unwrap());
            p.cursor = page["body"]["next_cursor"].as_str().map(str::to_owned);
            if p.cursor.is_none() {
                break;
            }
        }
        let recovered: Value = serde_json::from_str(&recovered).unwrap();
        assert_eq!(
            recovered["artifacts"][0]["supersedes_chain"],
            json!(entry.supersedes_chain)
        );
        assert_eq!(
            recovered["artifacts"][0]["description"],
            json!(entry.description)
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
