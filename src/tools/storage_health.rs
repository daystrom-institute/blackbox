use crate::server::*;
use crate::*;
use rmcp::schemars;
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
        description = "Read-only edge sidecar inventory. Totals by kind; include_files=true for rows."
    )]
    pub(crate) fn bbox_storage_health(
        &self,
        Parameters(p): Parameters<StorageHealthParams>,
    ) -> CallToolResult {
        Self::run("bbox_storage_health", || {
            let edges_dir = crate::storage_health::find_edges_dir(&self.state.store_dir, None);

            let registered: std::collections::HashSet<String> = {
                let guard = self.state.projects.read();
                guard.list().into_iter().map(|r| r.project_id).collect()
            };

            let project_filter: Option<String> = if let Some(ref project) = p.project {
                let guard = self.state.projects.read();
                match guard.resolve(project) {
                    Ok(Some(record)) => Some(record.project_id),
                    _ => Some(project.clone()),
                }
            } else {
                None
            };

            let include_files = p.include_files.unwrap_or(false);
            let report = crate::storage_health::scan_storage_health(
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
    }
}
