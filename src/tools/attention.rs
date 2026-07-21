use crate::inbox::InboxParams;
use crate::pins::PinParams;
use crate::server::BlackboxServer;
use crate::{gap_spool, inbox};

use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::CallToolResult;
use rmcp::{tool, tool_router};

pub(crate) fn router() -> ToolRouter<BlackboxServer> {
    BlackboxServer::attention_tools()
}

#[tool_router(router = attention_tools)]
impl BlackboxServer {
    #[tool(
        name = "bbox_pin",
        description = "Persist scoped ambient context for an active execution lane. Pins survive daemon restarts, are never rendered into repo agent files, and are injected only when the current dispatch matches their session/bro/thread/work-item scope."
    )]
    pub(crate) async fn bbox_pin(&self, Parameters(p): Parameters<PinParams>) -> CallToolResult {
        let start = std::time::Instant::now();
        let action = p.action.clone();
        let server = self.clone();
        // Project resolution does fs/git probes — keep it (and the store
        // mutation behind it) off the tokio workers.
        let pin_result = tokio::task::spawn_blocking(move || {
            let mut p = p;
            // Durable pin scope is the registered base project: a worktree
            // caller's path is resolved so the pin injects for every dispatch
            // of the same project, not just the ephemeral worktree cwd. On
            // list, the literal path stays matchable as an alias so pins
            // keyed pre-rescope remain visible.
            if let Some(raw) = p.project.clone().filter(|s| !s.trim().is_empty()) {
                let (scope, _write_dir) = server.resolve_project_write_scope(&raw);
                if scope != raw {
                    p.project_alias = Some(raw);
                }
                p.project = Some(scope);
            }
            server.state.pins.write().pin(&p)
        })
        .await
        .map_err(|e| anyhow::anyhow!("pin task failed: {e}"))
        .and_then(std::convert::identity);
        let text = match pin_result {
            Ok(text) => text,
            Err(e) => {
                let ms = start.elapsed().as_secs_f64() * 1000.0;
                tracing::warn!(target: "blackbox::tool", tool = "bbox_pin", elapsed_ms = ms, error = %e, "err");
                return Self::err_text(&format!("Error: {e:#}"));
            }
        };

        if action != "list" {
            if let Err(e) = self.state.persist_pins_durable().await {
                let ms = start.elapsed().as_secs_f64() * 1000.0;
                tracing::warn!(target: "blackbox::tool", tool = "bbox_pin", elapsed_ms = ms, error = %e, "err");
                return Self::err_text(&format!("Error: {e:#}"));
            }
        }

        let ms = start.elapsed().as_secs_f64() * 1000.0;
        tracing::info!(target: "blackbox::tool", tool = "bbox_pin", elapsed_ms = ms, bytes = text.len(), "ok");
        Self::ok_text(&text)
    }

    #[tool(
        name = "bbox_inbox",
        description = "Aggregate attention layer across every store."
    )]
    pub(crate) async fn bbox_inbox(
        &self,
        Parameters(p): Parameters<InboxParams>,
    ) -> CallToolResult {
        // Gap-spool import is full-store disk I/O under the gaps write lock,
        // and compute_inbox stacks five store read guards — run on the
        // blocking pool, not a tokio worker. (The import's guards drop before
        // the read stack below; only in-memory work happens under the stack.)
        let server = self.clone();
        Self::run_blocking("bbox_inbox", move || {
            // Worktree filter paths map to the registered base (where every
            // aggregated store keys its state); substring filters pass
            // through untouched.
            let mut p = p;
            if let Some(base) = p
                .project
                .as_deref()
                .and_then(|raw| server.rescope_project_filter_value(raw))
            {
                p.project = Some(base);
            }
            let import_report = if p.import_gap_spool.unwrap_or(false) {
                let projects = server.state.projects.read();
                let mut gaps = server.state.gaps.write();
                let state_dir = server.state.config.read().paths.state_dir.clone();
                let roots: Vec<String> = projects
                    .list()
                    .into_iter()
                    .map(|record| record.canonical_path)
                    .collect();
                Some(gap_spool::import_gap_spool(&mut gaps, &roots, &state_dir)?)
            } else {
                None
            };

            let knowledge_view =
                server.session_knowledge_view(p.project.as_deref(), p.provisional.as_deref())?;
            let threads = server.state.threads.read();
            let notes = server.state.notes.read();
            let gaps = server.state.gaps.read();
            let task_store = server.state.task_store.read();
            let failed_rows = collect_failed_tasks(&task_store);
            let vector_alerts = collect_vector_connectivity_alerts();
            let cron_alerts = collect_cron_schedule_alerts(&server.state);
            let inbox = inbox::compute_inbox(
                &knowledge_view.knowledge,
                &threads,
                &notes,
                &gaps,
                &failed_rows,
                &vector_alerts,
                &cron_alerts,
                &server.state.whiteboards,
                &p,
            )?;
            if let Some(report) = import_report {
                let rendered = report.render();
                if rendered.is_empty() {
                    Ok(inbox)
                } else {
                    Ok(format!("{rendered}{inbox}"))
                }
            } else {
                Ok(inbox)
            }
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use crate::server::BlackboxServer;
    use crate::server::state::SharedState;
    use rmcp::handler::server::wrapper::Parameters;
    use std::path::Path;
    use std::sync::Arc;

    fn run_git(cwd: &Path, args: &[&str]) {
        let out = std::process::Command::new("git")
            .arg("-C")
            .arg(cwd)
            .args(args)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    /// A pin set from inside an in-tree linked worktree must key to the
    /// registered BASE project (the durable scope) and inject for dispatches
    /// rooted in the base AND in the worktree — exact-match pin scoping was
    /// the sharpest silent failure of the worktree corpus-ops class.
    #[tokio::test]
    async fn bbox_pin_from_worktree_keys_base_and_injects_for_both_cwds() {
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().join("repo");
        std::fs::create_dir_all(&base).unwrap();
        run_git(&base, &["init", "-b", "main"]);
        run_git(&base, &["config", "user.email", "t@example.com"]);
        run_git(&base, &["config", "user.name", "T"]);
        std::fs::write(base.join("README.md"), "base").unwrap();
        run_git(&base, &["add", "."]);
        run_git(&base, &["commit", "-m", "init"]);
        let base_canon = base.canonicalize().unwrap();

        let worktree = base.join(".claude").join("worktrees").join("wt");
        std::fs::create_dir_all(worktree.parent().unwrap()).unwrap();
        run_git(
            &base,
            &[
                "worktree",
                "add",
                "-b",
                "arc/pin",
                worktree.to_str().unwrap(),
                "HEAD",
            ],
        );
        let wt = worktree
            .canonicalize()
            .unwrap()
            .to_string_lossy()
            .into_owned();
        let base_str = base_canon.to_string_lossy().into_owned();

        let server = BlackboxServer::new(Arc::new(SharedState::for_test(tmp.path())));
        server
            .state
            .projects
            .write()
            .register_path(&base_canon)
            .unwrap();

        let set = server
            .bbox_pin(Parameters(crate::pins::PinParams {
                action: "set".into(),
                content: Some("WORKTREE_PIN_MARKER guidance".into()),
                title: Some("arc pin".into()),
                scope: Some("bro".into()),
                target: Some("executor".into()),
                project: Some(wt.clone()),
                ..Default::default()
            }))
            .await;
        assert_ne!(set.is_error, Some(true), "pin set failed: {set:?}");

        // Injects for a dispatch rooted at the base…
        let from_base = server
            .ambient_pin_block(Some(&base_str), Some("executor"), None, None, None)
            .expect("base-rooted dispatch should receive the pin");
        assert!(from_base.contains("WORKTREE_PIN_MARKER"));

        // …and for a dispatch rooted in the worktree (cwd resolves to base).
        let from_worktree = server
            .ambient_pin_block(Some(&wt), Some("executor"), None, None, None)
            .expect("worktree-rooted dispatch should receive the pin");
        assert!(from_worktree.contains("WORKTREE_PIN_MARKER"));

        // A different project still doesn't receive it.
        assert!(
            server
                .ambient_pin_block(Some("/repo/other"), Some("executor"), None, None, None)
                .is_none()
        );
    }

    /// The silent-maintenance class (gap-f268badd): a shipped cron-routing
    /// packet compiled with no cron scheduling it must alert; installing the
    /// matching cron clears it; removing the packet out from under the live
    /// cron raises the inverse alert. Non-cron-routing packets never alert.
    #[test]
    fn cron_schedule_alerts_flag_unscheduled_and_orphaned_crons() {
        let tmp = tempfile::tempdir().unwrap();
        let state = SharedState::for_test(tmp.path());

        let routing_value: serde_json::Value = serde_json::from_str(include_str!(
            "../../system-defaults/maintenance/packets/cron-routing/daily-compaction.json"
        ))
        .unwrap();
        let routing: crate::packets::CompileParams = serde_json::from_value(routing_value).unwrap();
        state.packets.read().compile(&routing).unwrap();

        // A policy packet without a cron-routing segment never alerts.
        let policy_value: serde_json::Value = serde_json::from_str(include_str!(
            "../../system-defaults/agentic-corpus/packets/embed/compaction-policy.json"
        ))
        .unwrap();
        let policy: crate::packets::CompileParams = serde_json::from_value(policy_value).unwrap();
        state.packets.read().compile(&policy).unwrap();

        let alerts = super::collect_cron_schedule_alerts(&state);
        assert_eq!(
            alerts.len(),
            1,
            "expected one unscheduled-packet alert: {alerts:?}"
        );
        assert!(matches!(
            &alerts[0],
            crate::inbox::CronScheduleAlert::UnscheduledRoutingPacket { domain }
                if domain == "cron-routing/daily-compaction"
        ));

        // Installing the shipped cron spec clears the alert.
        let cron: crate::crons::CronSpec = serde_json::from_str(include_str!(
            "../../system-defaults/maintenance/crons/daily-compaction.json"
        ))
        .unwrap();
        state.crons.install(cron);
        assert!(super::collect_cron_schedule_alerts(&state).is_empty());

        // Removing the packet out from under the live cron raises the inverse.
        state
            .packets
            .read()
            .remove_domain("cron-routing/daily-compaction")
            .unwrap();
        let alerts = super::collect_cron_schedule_alerts(&state);
        assert_eq!(
            alerts.len(),
            1,
            "expected one missing-packet alert: {alerts:?}"
        );
        assert!(matches!(
            &alerts[0],
            crate::inbox::CronScheduleAlert::CronMissingPacket { cron_name, domain }
                if cron_name == "daily-compaction" && domain == "cron-routing/daily-compaction"
        ));
    }
}

/// HNSW connectivity breaches for the inbox (gap-1168b0bd b). Caller-side
/// adapter like `collect_failed_tasks`: the inbox crate sits below
/// bbox-vectors in the DAG and takes plain rows. Reads non-blocking
/// metrics so the inbox never stalls behind a long write-lock rebuild,
/// and degrades to empty during cold-start warmup.
fn collect_vector_connectivity_alerts() -> Vec<crate::inbox::VectorConnectivityAlert> {
    let Some(metrics) = bbox_vectors::metrics_nonblocking() else {
        return Vec::new();
    };
    metrics
        .into_values()
        .filter(|m| m.connectivity_breach(bbox_vectors::NOTIFY_CONNECTIVITY_RATIO))
        .map(|m| {
            let hnsw = m.hnsw.as_ref().expect("breach implies hnsw metrics");
            crate::inbox::VectorConnectivityAlert {
                route: m.route.clone(),
                active_nodes: hnsw.active_nodes,
                zero_in_degree_nodes: hnsw.zero_in_degree_nodes,
                risk_ratio: m.connectivity_risk_ratio(),
            }
        })
        .collect()
}

/// Cron scheduling gaps for the inbox (gap-f268badd). A packet whose
/// domain carries a `cron-routing` path segment declares "a cron drives
/// this workflow" — when no live cron references it, the maintenance
/// behind it silently never runs (the class behind the 118 GB edges-dir
/// incident). The inverse — a live cron whose `domain:` routing packet
/// is missing — fires ticks that dispatch nowhere. Caller-side adapter
/// like `collect_failed_tasks`: the inbox crate sits below the packet
/// and cron stores in the DAG and takes plain rows.
fn collect_cron_schedule_alerts(
    state: &crate::server::state::SharedState,
) -> Vec<crate::inbox::CronScheduleAlert> {
    use std::collections::BTreeSet;

    let crons = state.crons.list();
    let packets = match state.packets.read().list_all() {
        Ok(packets) => packets,
        Err(e) => {
            tracing::warn!("inbox cron-schedule check: packet list failed: {e:#}");
            return Vec::new();
        }
    };

    let packet_domains: BTreeSet<&str> = packets.iter().map(|p| p.domain.as_str()).collect();
    let scheduled_domains: BTreeSet<&str> = crons
        .iter()
        .filter_map(|c| c.routing_packet.strip_prefix("domain:"))
        .collect();

    let mut alerts = Vec::new();
    for domain in &packet_domains {
        let is_cron_routing = domain.split('/').any(|seg| seg == "cron-routing");
        if is_cron_routing && !scheduled_domains.contains(domain) {
            alerts.push(crate::inbox::CronScheduleAlert::UnscheduledRoutingPacket {
                domain: (*domain).to_string(),
            });
        }
    }
    let mut crons_missing: Vec<_> = crons
        .iter()
        .filter_map(|c| {
            let domain = c.routing_packet.strip_prefix("domain:")?;
            (!packet_domains.contains(domain)).then(|| {
                crate::inbox::CronScheduleAlert::CronMissingPacket {
                    cron_name: c.name.clone(),
                    domain: domain.to_string(),
                }
            })
        })
        .collect();
    crons_missing.sort_by(|a, b| {
        let key = |alert: &crate::inbox::CronScheduleAlert| match alert {
            crate::inbox::CronScheduleAlert::CronMissingPacket { cron_name, .. } => {
                cron_name.clone()
            }
            _ => String::new(),
        };
        key(a).cmp(&key(b))
    });
    alerts.extend(crons_missing);
    alerts
}

/// Extract (task_id, provider, started_at) rows for every failed task.
/// Caller-side adapter for `inbox::compute_inbox` — the inbox store sits
/// below orchestration in the crate DAG and takes plain rows instead of
/// a `TaskStore`.
fn collect_failed_tasks(
    task_store: &crate::orchestration::TaskStore,
) -> Vec<(String, String, u64)> {
    task_store
        .all_tasks()
        .iter()
        .filter_map(|t| {
            let inner = t.inner.lock();
            if inner.status == crate::orchestration::TaskStatus::Failed {
                Some((
                    inner.id.clone(),
                    format!("{:?}", inner.provider),
                    inner.started_at,
                ))
            } else {
                None
            }
        })
        .collect()
}
