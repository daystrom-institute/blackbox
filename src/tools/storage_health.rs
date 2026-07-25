use crate::server::BlackboxServer;
use crate::storage_health;

use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::CallToolResult;
use rmcp::schemars;
use rmcp::{tool, tool_router};
use serde::{Deserialize, Serialize};

pub(crate) fn router() -> ToolRouter<BlackboxServer> {
    BlackboxServer::storage_health_tools()
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
pub(crate) struct StorageHealthParams {
    /// Optional project filter: project_id, canonical_path, or absolute path.
    /// When the value is not found in the project registry, it is passed
    /// through as a raw filter string so that unregistered project_ids still work.
    #[serde(default)]
    pub project: Option<String>,
    /// Include per-file details in the response. Default false: compact
    /// totals only.
    #[serde(default)]
    pub include_files: Option<bool>,
}

#[tool_router(router = storage_health_tools)]
impl BlackboxServer {
    #[tool(
        name = "bbox_storage_health",
        description = "Read-only edge sidecar inventory. Totals active legacy, managed derived, backups, temps, orphan classes, inactive snapshots, and observed history; include_files=true for exact rows."
    )]
    pub(crate) async fn bbox_storage_health(
        &self,
        Parameters(p): Parameters<StorageHealthParams>,
    ) -> CallToolResult {
        let server = self.clone();
        Self::run_blocking("bbox_storage_health", move || {
            let edges_dir = storage_health::find_edges_dir(&server.state.store_dir, None);

            let registered: std::collections::HashSet<String> = server
                .state
                .records_provider
                .records_snapshot()
                .corpus_project_ids
                .iter()
                .cloned()
                .collect();

            // Filter-class engine resolution (phase-2 §9.2 B6): a resolving
            // selector narrows by identity; a miss keeps the literal
            // pass-through (tagged v1 compatibility, and the catalog-mode
            // literal-filter semantics).
            let project_filter: Option<String> = p.project.as_ref().map(|project| {
                match server
                    .resolve_project_filter(project)
                    .and_then(|resolution| resolution.project_id().map(str::to_owned))
                {
                    Some(project_id) => project_id,
                    None => {
                        server.state.resolver_compat.record(
                            "bbox_storage_health",
                            crate::server::resolver_compat::CompatLane::UnregisteredLiteralFilter,
                        );
                        project.clone()
                    }
                }
            });

            let include_files = p.include_files.unwrap_or(false);
            let report = storage_health::scan_storage_health(
                &edges_dir,
                &registered,
                project_filter.as_deref(),
                include_files,
            )?;

            Ok(serde_json::to_string_pretty(&serde_json::json!({
                "status": "ok",
                "totals": report.totals,
                "top_offenders": report.top_offenders,
                "files": report.files,
            }))?)
        })
        .await
    }
}
