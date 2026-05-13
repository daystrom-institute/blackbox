use crate::server::*;
use crate::*;
use rmcp::schemars;
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
    pub(crate) fn bbox_storage_migrate_legacy_edges(
        &self,
        Parameters(p): Parameters<StorageMigrationParams>,
    ) -> CallToolResult {
        Self::run("bbox_storage_migrate_legacy_edges", || {
            let edges_dir = crate::storage_health::find_edges_dir(&self.state.store_dir, None);

            let registered: std::collections::HashSet<String> = {
                let guard = self.state.projects.read();
                guard.list().into_iter().map(|r| r.project_id).collect()
            };

            if p.dry_run {
                let mut results = Vec::new();
                let targets =
                    resolve_migration_targets(&registered, &edges_dir, p.project.as_deref());
                for project_id in targets {
                    match crate::edge_index::plan_legacy_edge_extraction(&edges_dir, &project_id) {
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

            let mut results = Vec::new();
            let targets = resolve_migration_targets(&registered, &edges_dir, p.project.as_deref());
            for project_id in targets {
                match crate::migration::apply_migration(&edges_dir, &project_id) {
                    Ok(manifest) => {
                        results.push(serde_json::to_value(&manifest).unwrap_or_default());
                    }
                    Err(err) => {
                        let mut obj = serde_json::Map::new();
                        obj.insert("project_id".into(), serde_json::Value::String(project_id));
                        obj.insert("error".into(), serde_json::Value::String(err.to_string()));
                        results.push(serde_json::Value::Object(obj));
                    }
                }
            }

            Ok(serde_json::to_string_pretty(&serde_json::json!({
                "mode": "apply",
                "migrations": results,
            }))?)
        })
    }
}

fn resolve_migration_targets(
    registered: &std::collections::HashSet<String>,
    edges_dir: &Path,
    project_filter: Option<&str>,
) -> Vec<String> {
    if let Some(filter) = project_filter {
        if registered.contains(filter) {
            return vec![filter.to_string()];
        }
        for pid in registered {
            if pid.starts_with(&filter[..filter.len().min(8)]) {
                return vec![pid.clone()];
            }
        }
        return vec![filter.to_string()];
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
