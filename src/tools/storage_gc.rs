use crate::server::BlackboxServer;
use crate::storage_health;

use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::CallToolResult;
use rmcp::schemars;
use rmcp::{tool, tool_router};
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
    /// Prune orphan/unregistered sidecars. Default false.
    #[serde(default)]
    pub prune_orphans: bool,
    /// Prune compact temp files older than 24h. Default true.
    #[serde(default = "default_true")]
    pub prune_temps: bool,
    /// Prune inactive snapshot files not referenced by any manifest after
    /// snapshot retention policy is applied. Default false.
    #[serde(default)]
    pub prune_inactive_snapshots: bool,
    /// Maximum age in days for backup files. If set, backups older than this
    /// are candidates even if they are the newest retained.
    #[serde(default)]
    pub max_backup_age_days: Option<u64>,
    /// Number of newest backups to retain per source. Default 1.
    #[serde(default = "default_keep_newest")]
    pub keep_newest_backup_per_source: u64,
    /// Backup byte cap across all scanned backups. Retained newest backups are
    /// never deleted solely to satisfy this cap. Default 2 GiB.
    #[serde(default = "default_backup_cap")]
    pub max_backup_total_bytes: Option<u64>,
    /// Number of recent inactive snapshot directories retained per workspace.
    /// Default 3.
    #[serde(default = "default_keep_recent_workspace")]
    pub keep_recent_snapshots_per_workspace: u64,
    /// Number of recent inactive snapshot directories retained per repo.
    /// Default 10.
    #[serde(default = "default_keep_recent_repo")]
    pub keep_recent_snapshots_per_repo: u64,
    /// Grace window after branch switches before inactive snapshots can prune.
    /// Default 60 minutes.
    #[serde(default = "default_branch_grace_minutes")]
    pub branch_switch_grace_minutes: u64,
    /// Maximum age in days for inactive snapshots. Default 14.
    #[serde(default = "default_snapshot_max_age_days")]
    pub max_snapshot_age_days: Option<u64>,
    /// Count budget for retained inactive snapshots per workspace. Bounds the
    /// age-based keep (floors always retain and consume the budget): at
    /// multi-agent commit rates, age-only retention reaches ~100 GB steady
    /// state. Default 32.
    #[serde(default = "default_snapshot_max_count")]
    pub max_snapshots_per_workspace: Option<u64>,
    /// Byte budget for retained inactive snapshots per workspace, consumed
    /// newest-first; bounds the age-based keep like the count budget.
    /// Default 16 GiB.
    #[serde(default = "default_snapshot_max_total_bytes")]
    pub max_snapshot_total_bytes_per_workspace: Option<u64>,
    /// Auto-prune dangling_path and legacy_unknown orphans after this many
    /// days. Default 30.
    #[serde(default = "default_orphan_after_days")]
    pub orphan_auto_prune_after_days: u64,
    /// Prune explicitly_unregistered storage after orphan_auto_prune_after_days.
    /// Default false; this is a separate operator decision.
    #[serde(default)]
    pub prune_explicitly_unregistered: bool,
    /// Optional observed lane cap per project. Over-cap observed history is
    /// reported as operator-review only; it is not auto-deleted.
    #[serde(default)]
    pub max_observed_bytes_per_project: Option<u64>,
    /// Prune duplicate rule-packets: among byte-identical copies for the same
    /// domain/scope/project, keep only the newest (boot-restore used to mint
    /// one copy per daemon restart). Copies referenced by another packet's
    /// Apply antecedent are protected. Honors dry_run. Default false.
    #[serde(default)]
    pub prune_duplicate_packets: bool,
}

fn default_true() -> bool {
    true
}
fn default_keep_newest() -> u64 {
    1
}
fn default_backup_cap() -> Option<u64> {
    Some(2 * 1024 * 1024 * 1024)
}
fn default_keep_recent_workspace() -> u64 {
    3
}
fn default_keep_recent_repo() -> u64 {
    10
}
fn default_branch_grace_minutes() -> u64 {
    60
}
fn default_snapshot_max_age_days() -> Option<u64> {
    Some(14)
}
fn default_snapshot_max_count() -> Option<u64> {
    Some(storage_health::DEFAULT_SNAPSHOT_MAX_COUNT_PER_WORKSPACE)
}
fn default_snapshot_max_total_bytes() -> Option<u64> {
    Some(storage_health::DEFAULT_SNAPSHOT_MAX_TOTAL_BYTES_PER_WORKSPACE)
}
fn default_orphan_after_days() -> u64 {
    30
}

#[tool_router(router = storage_gc_tools)]
impl BlackboxServer {
    #[tool(
        name = "bbox_storage_gc",
        description = "Dry-run or apply edge sidecar garbage collection. Reports exact candidates with path, bytes, and rule for temps, backups, orphan classes, inactive snapshots, and observed cap warnings."
    )]
    pub(crate) async fn bbox_storage_gc(
        &self,
        Parameters(p): Parameters<StorageGcParams>,
    ) -> CallToolResult {
        let server = self.clone();
        Self::run_blocking("bbox_storage_gc", move || {
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
                            "bbox_storage_gc",
                            crate::server::resolver_compat::CompatLane::UnregisteredLiteralFilter,
                        );
                        project.clone()
                    }
                }
            });

            let gc_params = storage_health::GcParams {
                dry_run: p.dry_run,
                project_filter,
                prune_backups: p.prune_backups,
                prune_orphans: p.prune_orphans,
                prune_temps: p.prune_temps,
                prune_inactive_snapshots: p.prune_inactive_snapshots,
                max_backup_age_days: p.max_backup_age_days,
                keep_newest_backup_per_source: p.keep_newest_backup_per_source,
            };

            let policy = storage_health::GcPolicy {
                materialized_snapshots: storage_health::SnapshotRetentionPolicy {
                    keep_active: true,
                    keep_recent_per_workspace: p.keep_recent_snapshots_per_workspace,
                    keep_recent_per_repo: p.keep_recent_snapshots_per_repo,
                    branch_switch_grace_minutes: p.branch_switch_grace_minutes,
                    max_age_days: p.max_snapshot_age_days,
                    max_count_per_workspace: p.max_snapshots_per_workspace,
                    max_total_bytes_per_workspace: p.max_snapshot_total_bytes_per_workspace,
                },
                backups: storage_health::BackupRetentionPolicy {
                    max_total_bytes: p.max_backup_total_bytes,
                },
                orphans: storage_health::OrphanRetentionPolicy {
                    auto_prune_after_days: p.orphan_auto_prune_after_days,
                    prune_explicitly_unregistered: p.prune_explicitly_unregistered,
                },
                observed: storage_health::ObservedRetentionPolicy {
                    max_bytes_per_project: p.max_observed_bytes_per_project,
                },
            };

            // Marker-driven GC exclusion (Phase 6 plan section 10.2). It runs
            // BEFORE any candidate is planned, because its migrated-origin arm
            // is a refusal to sweep at all, not a filter: a marker that is
            // absent, corrupt, or incomplete means nothing can vouch for the
            // rollback assets, and reporting candidates first would invite an
            // operator to act on a list this store had no authority to
            // produce. Fresh-v2 and non-catalog stores pass through exempt.
            let (projects_path, index_path) = {
                let config = server.state.config.read();
                (
                    config.paths.projects_path.clone(),
                    config.paths.index_path.clone(),
                )
            };
            let exclusions = bbox_indexing::project_catalog_store::plan_catalog_gc_exclusions(
                &projects_path,
                &index_path,
            )
            .map_err(|error| {
                anyhow::anyhow!(
                    "{error} (refusing to sweep a migrated store whose rollback inventory \
                     cannot be authorized)"
                )
            })?;

            let candidates =
                storage_health::plan_gc_with_policy(&edges_dir, &registered, &gc_params, &policy)?;

            // The protected-root filter is a SECOND line, deliberately kept
            // even though today's candidates are edge-sidecar paths that do
            // not normally fall under these roots. The roots are retention
            // inventory (section 10.1) and the cost of the check is one prefix
            // comparison; the cost of omitting it is discovering, after a
            // future planner learns a new candidate class, that the sweep
            // deleted a rebuild generation.
            let candidates: Vec<storage_health::GcCandidate> = candidates
                .into_iter()
                .filter(|candidate| {
                    candidate.path.is_empty()
                        || !exclusions.protects(std::path::Path::new(&candidate.path))
                })
                .collect();

            let deletable: Vec<&storage_health::GcCandidate> = candidates
                .iter()
                .filter(|c| c.deletable && !c.path.is_empty())
                .collect();
            let deletable_count = deletable.len();
            let deletable_bytes = deletable.iter().map(|c| c.bytes).sum::<u64>();

            let (deleted, delete_errors) = if p.dry_run {
                (None, None)
            } else {
                let (d, e) = storage_health::apply_gc(&edges_dir, &candidates);
                (Some(d), if e.is_empty() { None } else { Some(e) })
            };

            let result = storage_health::GcResult {
                applied: !p.dry_run,
                candidates,
                deletable_count,
                deletable_bytes,
                deleted,
                delete_errors,
            };

            let packet_gc = if p.prune_duplicate_packets {
                let report = server
                    .state
                    .packets
                    .read()
                    .gc_duplicate_packets(!p.dry_run)?;
                Some(report)
            } else {
                None
            };

            let mut out = serde_json::json!({
                "status": if p.dry_run { "dry_run" } else { "applied" },
                "result": result,
                // Reported, not merely applied: an external sweep is the real
                // consumer of section 10.2, and it can only honour the
                // exclusion set if this surface publishes it.
                "catalog_gc_exclusions": exclusions,
            });
            if let Some(report) = packet_gc {
                out["packet_gc"] = serde_json::to_value(&report)?;
            }
            Ok(serde_json::to_string_pretty(&out)?)
        })
        .await
    }
}
