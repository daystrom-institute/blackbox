use std::path::Path;

use crate::server::BlackboxServer;
use crate::{edge_index, migration, storage_health};

use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::CallToolResult;
use rmcp::schemars;
use rmcp::{tool, tool_router};
use serde::{Deserialize, Serialize};

pub(crate) fn router() -> ToolRouter<BlackboxServer> {
    BlackboxServer::storage_migration_tools()
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
pub(crate) struct StorageMigrationParams {
    /// Dry-run mode: report extraction plans without applying. Default true.
    #[serde(default = "default_true")]
    pub dry_run: bool,
    /// Optional project selector: project_id, operator alias, or registered
    /// path. Resolved through the same authority as apply mode; unknown
    /// selectors fail identically in both modes. Required when dry_run=false.
    #[serde(default)]
    pub project: Option<String>,
    /// Dry-run plan page size. Default 20, minimum 1, maximum 100. Refused
    /// for apply, which migrates exactly one project.
    #[serde(default)]
    pub limit: Option<usize>,
    /// Dry-run plans to skip in project order. Follow next_offset to
    /// continue. The sidecar set can change between pages; restart at 0
    /// after lifecycle actions.
    #[serde(default)]
    pub offset: Option<usize>,
}

fn default_true() -> bool {
    true
}

#[tool_router(router = storage_migration_tools)]
impl BlackboxServer {
    #[tool(
        name = "bbox_storage_migrate_legacy_edges",
        description = "Dry-run or apply daemon-local legacy edge sidecar migration into lifecycle-owned explicit/observed lanes. Project selectors (project_id, operator alias, registered path) resolve through one authority in dry-run and apply; unknown selectors fail both modes. Dry-run plans are paged (limit default 20, max 100; follow next_offset). Drops derived only when managed replacement exists; quarantines malformed lines."
    )]
    pub(crate) async fn bbox_storage_migrate_legacy_edges(
        &self,
        Parameters(p): Parameters<StorageMigrationParams>,
    ) -> CallToolResult {
        let server = self.clone();
        Self::run_blocking("bbox_storage_migrate_legacy_edges", move || {
            run_storage_migration(&server, &p)
        })
        .await
    }
}

pub(crate) fn run_storage_migration(
    server: &BlackboxServer,
    p: &StorageMigrationParams,
) -> anyhow::Result<String> {
    if !p.dry_run && (p.limit.is_some() || p.offset.is_some()) {
        anyhow::bail!(
            "limit and offset page dry-run plans; omit them for apply, which migrates one project"
        );
    }
    let edges_dir = storage_health::find_edges_dir(&server.state.store_dir, None);
    let registered = server.state.corpus_registered_project_ids();

    if p.dry_run {
        let (targets, unregistered_sidecars_skipped) = match p.project.as_deref() {
            Some(filter) => (
                vec![resolve_migration_target(server, &registered, filter)?],
                0,
            ),
            None => resolve_dry_run_targets(&registered, &edges_dir),
        };
        let mut results = Vec::new();
        for project_id in targets {
            match edge_index::plan_legacy_edge_extraction(&edges_dir, &project_id) {
                Ok(plan) => {
                    results.push(serde_json::to_value(&plan).unwrap_or_default());
                }
                Err(err) => {
                    let mut obj = serde_json::Map::new();
                    obj.insert("project_id".into(), serde_json::Value::String(project_id));
                    obj.insert("error".into(), serde_json::Value::String(err.to_string()));
                    results.push(serde_json::Value::Object(obj));
                }
            }
        }
        let mut page = bbox_corpus_core::response_page::collection_page(
            results, "projects", p.limit, p.offset,
        )?;
        page["mode"] = serde_json::json!("dry_run");
        if p.project.is_none() {
            page["unregistered_sidecars_skipped"] =
                serde_json::json!(unregistered_sidecars_skipped);
        }
        return Ok(serde_json::to_string_pretty(&page)?);
    }

    let Some(ref project) = p.project else {
        anyhow::bail!("apply mode requires a project parameter");
    };
    let project_id = resolve_migration_target(server, &registered, project)?;
    let recovery = migration::recover_pending_migrations(&edges_dir)?;
    if !recovery.is_empty() {
        tracing::info!(?recovery, "recovered pending migrations before apply");
    }
    let manifest = migration::apply_migration(&edges_dir, &project_id)?;

    Ok(serde_json::to_string_pretty(&serde_json::json!({
        "mode": "apply",
        "migration": manifest,
    }))?)
}

/// A03: one authoritative selector contract for dry-run and apply. The
/// filter engine narrows by identity first (project_id, alias, registered
/// path); the registered-literal fallback stays a tagged compatibility
/// lane; unknown selectors refuse identically in both modes.
fn resolve_migration_target(
    server: &BlackboxServer,
    registered: &std::collections::HashSet<String>,
    filter: &str,
) -> anyhow::Result<String> {
    let resolved = server
        .resolve_project_filter(filter)
        .and_then(|resolution| resolution.project_id().map(str::to_owned));
    match resolved {
        Some(id) if registered.contains(&id) => Ok(id),
        _ => {
            if registered.contains(filter) {
                server.state.resolver_compat.record(
                    "bbox_storage_migration",
                    crate::server::resolver_compat::CompatLane::UnregisteredLiteralFilter,
                );
                Ok(filter.to_string())
            } else {
                anyhow::bail!(
                    "project '{}' is not registered; storage migration resolves project_id, \
                     operator alias, and registered path selectors, and both dry-run and apply \
                     refuse unknown selectors",
                    filter
                )
            }
        }
    }
}

// false positive: called from bbox_storage_migrate_legacy_edges' run_blocking closure.
#[allow(clippy::disallowed_methods)]
fn resolve_dry_run_targets(
    registered: &std::collections::HashSet<String>,
    edges_dir: &Path,
) -> (Vec<String>, usize) {
    let mut targets = Vec::new();
    let mut unregistered_sidecars_skipped = 0usize;
    let Ok(entries) = std::fs::read_dir(edges_dir) else {
        return (targets, unregistered_sidecars_skipped);
    };
    for entry in entries.filter_map(Result::ok) {
        let path = entry.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("jsonl") {
            continue;
        }
        if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
            if registered.contains(stem) {
                targets.push(stem.to_string());
            } else {
                unregistered_sidecars_skipped += 1;
            }
        }
    }
    targets.sort();
    (targets, unregistered_sidecars_skipped)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;
    use std::sync::Arc;

    fn test_server(root: &Path) -> BlackboxServer {
        BlackboxServer::new(Arc::new(crate::server::state::SharedState::for_test(root)))
    }

    #[test]
    fn dry_run_and_apply_resolve_selectors_through_one_contract() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let server = test_server(&root);
        let project_root = root.join("project");
        std::fs::create_dir_all(&project_root).unwrap();
        std::fs::create_dir_all(project_root.join("nested")).unwrap();
        let registry = server.state.project_authority.bridge_registry().unwrap();
        let record = registry.write().register_path(&project_root).unwrap();
        registry
            .write()
            .sync_declared_aliases(
                &record.project_id,
                &BTreeSet::from(["probe-alias".to_string()]),
            )
            .unwrap();
        let registered = server.state.corpus_registered_project_ids();
        assert!(registered.contains(&record.project_id));

        let edges = root.join("edges");
        std::fs::create_dir_all(edges.join("derived").join("project")).unwrap();
        std::fs::write(
            edges.join(format!("{}.jsonl", record.project_id)),
            "not-json\n\n",
        )
        .unwrap();
        std::fs::write(
            edges
                .join("derived")
                .join("project")
                .join(format!("{}.jsonl", record.project_id)),
            "",
        )
        .unwrap();

        for selector in [
            record.project_id.as_str(),
            record.canonical_path.as_str(),
            "probe-alias",
            project_root.join("nested").to_str().unwrap(),
        ] {
            let resolved = resolve_migration_target(&server, &registered, selector);
            assert_eq!(resolved.unwrap(), record.project_id, "selector: {selector}");
            let params = StorageMigrationParams {
                dry_run: true,
                project: Some(selector.to_string()),
                limit: None,
                offset: None,
            };
            let rendered = run_storage_migration(&server, &params).unwrap();
            let value: serde_json::Value = serde_json::from_str(&rendered).unwrap();
            assert_eq!(value["mode"], "dry_run", "selector: {selector}");
            assert_eq!(value["total"], 1, "selector: {selector}");
            assert_eq!(
                value["projects"][0]["project_id"], record.project_id,
                "selector: {selector}"
            );
        }

        let unknown = run_storage_migration(
            &server,
            &StorageMigrationParams {
                dry_run: true,
                project: Some("ghost-project".into()),
                limit: None,
                offset: None,
            },
        )
        .unwrap_err();
        assert!(
            unknown.to_string().contains("not registered"),
            "unknown dry-run selector must refuse: {unknown}"
        );
        assert!(resolve_migration_target(&server, &registered, "ghost-project").is_err());
    }

    #[test]
    fn registered_literal_selector_keeps_tagged_compatibility_lane() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let server = test_server(&root);
        let mut registered = std::collections::HashSet::new();
        registered.insert("legacy-literal".to_string());
        let resolved = resolve_migration_target(&server, &registered, "legacy-literal");
        assert_eq!(resolved.unwrap(), "legacy-literal");
    }

    #[test]
    fn dry_run_pages_totals_and_reports_skipped_sidecars() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let server = test_server(&root);
        let registry = server.state.project_authority.bridge_registry().unwrap();
        let edges = root.join("edges");
        std::fs::create_dir_all(&edges).unwrap();
        let mut ids = Vec::new();
        for name in ["alpha", "beta", "gamma"] {
            let dir = root.join(name);
            std::fs::create_dir_all(&dir).unwrap();
            let record = registry.write().register_path(&dir).unwrap();
            ids.push(record.project_id);
        }
        ids.sort();
        for id in &ids {
            std::fs::write(edges.join(format!("{id}.jsonl")), "not-json\n").unwrap();
        }
        std::fs::write(edges.join("orphan-sidecar.jsonl"), "not-json\n").unwrap();

        let first = run_storage_migration(
            &server,
            &StorageMigrationParams {
                dry_run: true,
                project: None,
                limit: Some(2),
                offset: None,
            },
        )
        .unwrap();
        let value: serde_json::Value = serde_json::from_str(&first).unwrap();
        assert_eq!(value["total"], 3);
        assert_eq!(value["count"], 2);
        assert_eq!(value["next_offset"], 2);
        assert_eq!(value["projects"][0]["project_id"], ids[0]);
        assert_eq!(value["unregistered_sidecars_skipped"], 1);

        let second = run_storage_migration(
            &server,
            &StorageMigrationParams {
                dry_run: true,
                project: None,
                limit: Some(2),
                offset: Some(2),
            },
        )
        .unwrap();
        let value: serde_json::Value = serde_json::from_str(&second).unwrap();
        assert_eq!(value["count"], 1);
        assert_eq!(value["next_offset"], serde_json::Value::Null);
        assert_eq!(value["projects"][0]["project_id"], ids[2]);
    }

    #[test]
    fn oversized_single_plan_row_refuses_instead_of_silent_trim() {
        let rows = vec![serde_json::json!({"blob": "x".repeat(30_000)})];
        let err = bbox_corpus_core::response_page::collection_page(rows, "projects", None, None)
            .unwrap_err();
        assert!(err.to_string().contains("collection_row_too_large"));
    }

    #[test]
    fn apply_rejects_paging_params_before_any_scan() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let server = test_server(&root);
        let paging = run_storage_migration(
            &server,
            &StorageMigrationParams {
                dry_run: false,
                project: Some("any".into()),
                limit: Some(5),
                offset: None,
            },
        )
        .unwrap_err();
        assert!(
            paging.to_string().contains("limit and offset"),
            "apply must reject paging params before scanning: {paging}"
        );
        let missing = run_storage_migration(
            &server,
            &StorageMigrationParams {
                dry_run: false,
                project: None,
                limit: None,
                offset: None,
            },
        )
        .unwrap_err();
        assert!(
            missing
                .to_string()
                .contains("apply mode requires a project")
        );
    }
}
