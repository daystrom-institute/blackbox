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
    /// Dry-run mode: report extraction plan without applying. Default true.
    #[serde(default = "default_true")]
    pub dry_run: bool,
    /// Optional project filter: project_id, canonical_path, or absolute path.
    /// Required for apply mode (dry_run=false).
    #[serde(default)]
    pub project: Option<String>,
}

fn default_true() -> bool {
    true
}

#[tool_router(router = storage_migration_tools)]
impl BlackboxServer {
    #[tool(
        name = "bbox_storage_migrate_legacy_edges",
        description = "Dry-run or apply legacy edge sidecar migration into lifecycle-owned explicit/observed lanes. Drops derived only when managed replacement exists; quarantines malformed lines."
    )]
    pub(crate) async fn bbox_storage_migrate_legacy_edges(
        &self,
        Parameters(p): Parameters<StorageMigrationParams>,
    ) -> CallToolResult {
        let server = self.clone();
        Self::run_blocking("bbox_storage_migrate_legacy_edges", move || {
            let edges_dir = storage_health::find_edges_dir(&server.state.store_dir, None);

            let registered = server.state.corpus_registered_project_ids();

            if p.dry_run {
                let mut results = Vec::new();
                let targets =
                    resolve_dry_run_targets(&registered, &edges_dir, p.project.as_deref());
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
                return Ok(serde_json::to_string_pretty(&serde_json::json!({
                    "mode": "dry_run",
                    "projects": results,
                }))?);
            }

            let Some(ref project) = p.project else {
                anyhow::bail!("apply mode requires a project parameter");
            };
            // Filter-class engine resolution (phase-2 §9.2 B6): resolution
            // narrows by identity, and apply still requires the resolved or
            // literal id to name a registered corpus project.
            let project_id = {
                let resolved = server
                    .resolve_project_filter(project)
                    .and_then(|resolution| resolution.project_id().map(str::to_owned));
                match resolved {
                    Some(id) if registered.contains(&id) => id,
                    _ => {
                        if registered.contains(project) {
                            server.state.resolver_compat.record(
                                "bbox_storage_migration",
                                crate::server::resolver_compat::CompatLane::UnregisteredLiteralFilter,
                            );
                            project.clone()
                        } else {
                            anyhow::bail!(
                                "project '{}' is not registered; apply requires a registered project",
                                project
                            )
                        }
                    }
                }
            };
            let recovery = migration::recover_pending_migrations(&edges_dir)?;
            if !recovery.is_empty() {
                tracing::info!(?recovery, "recovered pending migrations before apply");
            }
            let manifest = migration::apply_migration(&edges_dir, &project_id)?;

            Ok(serde_json::to_string_pretty(&serde_json::json!({
                "mode": "apply",
                "migration": manifest,
            }))?)
        })
        .await
    }
}

// false positive: called from bbox_storage_migrate_legacy_edges' run_blocking closure.
#[allow(clippy::disallowed_methods)]
fn resolve_dry_run_targets(
    registered: &std::collections::HashSet<String>,
    edges_dir: &Path,
    project_filter: Option<&str>,
) -> Vec<String> {
    if let Some(filter) = project_filter {
        if registered.contains(filter) {
            return vec![filter.to_string()];
        }
        return Vec::new();
    }

    let mut targets = Vec::new();
    let Ok(entries) = std::fs::read_dir(edges_dir) else {
        return targets;
    };
    for entry in entries.filter_map(Result::ok) {
        let path = entry.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("jsonl") {
            continue;
        }
        if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
            if registered.contains(stem) {
                targets.push(stem.to_string());
            }
        }
    }
    targets.sort();
    targets
}
