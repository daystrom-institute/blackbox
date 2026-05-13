use crate::server::*;
use crate::*;
use rmcp::schemars;
use serde::{Deserialize, Serialize};

pub(crate) fn router() -> ToolRouter<BlackboxServer> {
    BlackboxServer::storage_gc_tools()
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
pub(crate) struct StorageGcParams {
    /// Dry-run mode: report candidates without deleting. Default true.
    #[serde(default = "default_true")]
    pub dry_run: bool,
    /// Optional project filter: project_id, canonical_path, or absolute path.
    #[serde(default)]
    pub project: Option<String>,
    /// Prune compaction backup files. Default true.
    #[serde(default = "default_true")]
    pub prune_backups: bool,
    /// Prune orphan/unregistered sidecars. Default false; Phase 1 reports only.
    #[serde(default)]
    pub prune_orphans: bool,
    /// Prune compact temp files older than 24h. Default true.
    #[serde(default = "default_true")]
    pub prune_temps: bool,
    /// Maximum age in days for backup files. If set, backups older than this
    /// are candidates even if they are the newest retained.
    #[serde(default)]
    pub max_backup_age_days: Option<u64>,
    /// Number of newest backups to retain per source. Default 1.
    #[serde(default = "default_keep_newest")]
    pub keep_newest_backup_per_source: u64,
}

fn default_true() -> bool {
    true
}
fn default_keep_newest() -> u64 {
    1
}

#[tool_router(router = storage_gc_tools)]
impl BlackboxServer {
    #[tool(
        name = "bbox_storage_gc",
        description = "Dry-run or apply edge sidecar garbage collection. Reports exact candidates with path, bytes, and rule."
    )]
    pub(crate) fn bbox_storage_gc(
        &self,
        Parameters(p): Parameters<StorageGcParams>,
    ) -> CallToolResult {
        Self::run("bbox_storage_gc", || {
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

            let gc_params = crate::storage_health::GcParams {
                dry_run: p.dry_run,
                project_filter,
                prune_backups: p.prune_backups,
                prune_orphans: p.prune_orphans,
                prune_temps: p.prune_temps,
                max_backup_age_days: p.max_backup_age_days,
                keep_newest_backup_per_source: p.keep_newest_backup_per_source,
            };

            let candidates = crate::storage_health::plan_gc(&edges_dir, &registered, &gc_params)?;

            let deletable: Vec<&crate::storage_health::GcCandidate> = candidates
                .iter()
                .filter(|c| c.deletable && !c.path.is_empty())
                .collect();
            let deletable_count = deletable.len();
            let deletable_bytes = deletable.iter().map(|c| c.bytes).sum::<u64>();

            let (deleted, delete_errors) = if p.dry_run {
                (None, None)
            } else {
                let (d, e) = crate::storage_health::apply_gc(&candidates);
                (Some(d), if e.is_empty() { None } else { Some(e) })
            };

            let result = crate::storage_health::GcResult {
                applied: !p.dry_run,
                candidates,
                deletable_count,
                deletable_bytes,
                deleted,
                delete_errors,
            };

            Ok(serde_json::to_string_pretty(&serde_json::json!({
                "status": if p.dry_run { "dry_run" } else { "applied" },
                "result": result,
            }))?)
        })
    }
}
