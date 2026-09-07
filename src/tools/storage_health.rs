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
    /// File-page size, default 20 and maximum 100. Requires include_files=true.
    #[serde(default)]
    pub limit: Option<usize>,
    /// Continue with next_offset and the same project. Inventory may change between pages.
    #[serde(default)]
    pub offset: Option<usize>,
}

#[tool_router(router = storage_health_tools)]
impl BlackboxServer {
    #[tool(
        name = "bbox_storage_health",
        description = "Read daemon-owned edge storage totals and the ten largest contributors. include_files=true returns file pages (limit default 20, max 100); follow next_offset. File paths are relative to daemon storage, not caller-readable paths. Use bbox_storage_gc for managed cleanup. Each call rescans the selected daemon storage before projecting totals or file pages. Manifest and retention warnings remain visible."
    )]
    pub(crate) async fn bbox_storage_health(
        &self,
        Parameters(p): Parameters<StorageHealthParams>,
    ) -> CallToolResult {
        if !p.include_files.unwrap_or(false) && (p.limit.is_some() || p.offset.is_some()) {
            return Self::err_text("error.bad_input: limit and offset require include_files=true");
        }
        let server = self.clone();
        Self::run_blocking("bbox_storage_health", move || {
            let edges_dir = storage_health::find_edges_dir(&server.state.store_dir, None);

            let registered = server.state.corpus_registered_project_ids();

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

            let value = storage_health_response(serde_json::to_value(report)?, &edges_dir, &p)?;
            Ok(serde_json::to_string(&value)?)
        })
        .await
    }
}

fn storage_health_response(
    mut report: serde_json::Value,
    root: &std::path::Path,
    p: &StorageHealthParams,
) -> anyhow::Result<serde_json::Value> {
    use serde_json::json;
    report["status"] = json!("ok");
    // Observed usage has its own totals above. Detailed filesystem inventory
    // belongs to the explicit file page rather than a second unbounded array.
    report.as_object_mut().unwrap().remove("observed");
    if let Some(rows) = report["top_offenders"].as_array_mut() {
        for row in rows {
            row.as_object_mut().unwrap().remove("path");
        }
    }
    if !p.include_files.unwrap_or(false) {
        report.as_object_mut().unwrap().remove("files");
        return Ok(report);
    }
    let mut rows = report["files"]
        .as_array_mut()
        .map(std::mem::take)
        .unwrap_or_default();
    rows.sort_by(|a, b| {
        b["bytes"]
            .as_u64()
            .cmp(&a["bytes"].as_u64())
            .then_with(|| a["path"].as_str().cmp(&b["path"].as_str()))
    });
    let total = rows.len();
    let offset = p.offset.unwrap_or(0);
    let limit = p.limit.unwrap_or(20).clamp(1, 100);
    let rows: Vec<_> = rows
        .into_iter()
        .skip(offset)
        .take(limit)
        .map(|mut row| {
            if let Some(path) = row["path"].as_str() {
                let relative = std::path::Path::new(path)
                    .strip_prefix(root)
                    .map(|p| p.to_string_lossy().into_owned())
                    .ok();
                row.as_object_mut().unwrap().remove("path");
                if let Some(relative) = relative {
                    row["storage_relative_path"] = json!(relative);
                }
            }
            row
        })
        .collect();
    report["files"] = json!(rows);
    report["total"] = json!(total);
    report["offset"] = json!(offset);
    report["limit"] = json!(limit);
    report["order"] = json!("bytes_desc_path_asc");
    report["storage_owner"] =
        json!("daemon; paths are diagnostic coordinates, not caller filesystem paths");
    bbox_corpus_core::response_page::bound_page(report, "files")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn storage_pages_preserve_warnings_without_exposing_daemon_paths() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let rows: Vec<_> = (0..30)
            .map(|id| json!({"path":root.join(format!("entry-{id:02}")), "bytes":100}))
            .collect();
        let report = json!({"totals":{"total_files":30}, "files":rows, "top_offenders":[rows[0]], "observed":[{"path":"private"}], "observed_policy_warning":"retention warning"});
        let p = StorageHealthParams {
            project: None,
            include_files: None,
            limit: None,
            offset: None,
        };
        let summary = storage_health_response(report.clone(), &root, &p).unwrap();
        assert!(summary.get("files").is_none());
        assert!(summary["top_offenders"][0].get("path").is_none());
        assert_eq!(summary["observed_policy_warning"], "retention warning");
        let p = StorageHealthParams {
            include_files: Some(true),
            limit: Some(3),
            offset: Some(3),
            ..p
        };
        let page = storage_health_response(report, &root, &p).unwrap();
        assert_eq!(page["next_offset"], 6);
        assert_eq!(page["files"][0]["storage_relative_path"], "entry-03");
        assert!(!page.to_string().contains(root.to_str().unwrap()));
    }
}
