use std::path::{Path, PathBuf};
use std::sync::Arc;

use parking_lot::RwLock;

use crate::orchestration;
use crate::orchestration as orch;
use crate::orchestration::TaskStore;
use crate::orchestration::providers::Provider;
use crate::orchestration::providers::dispatch_prelude::*;

// ---------------------------------------------------------------------------
// Progress notifications — MCP progressToken plumbing for blocking waits
// ---------------------------------------------------------------------------
//
// Per MCP spec, progress notifications are correlated to a pending request via
// the progressToken the caller put in `_meta`. The server MUST echo that exact
// token back; otherwise clients drop the notification as unknown. Servers MUST
// NOT send progress notifications unless the caller asked for them.

pub(crate) const PROGRESS_TICK_SECS: u64 = 15;

fn ellipsize_chars(s: &str, max_chars: usize) -> String {
    let mut chars = s.chars();
    let prefix: String = chars.by_ref().take(max_chars).collect();
    if chars.next().is_some() {
        format!("{prefix}…")
    } else {
        prefix
    }
}

pub(crate) fn format_bro_line(task: &orch::Task, store_dir: &Path) -> (String, bool) {
    let inner = task.inner.lock();
    let terminal = inner.status.is_terminal();
    let bro_name = orchestration::team::find_bro_name_for_task(&inner.id, store_dir);
    let label = bro_name.unwrap_or_else(|| inner.id[..inner.id.len().min(8)].to_string());
    let elapsed = orch::format_elapsed(inner.started_at, inner.completed_at);
    let events = inner.events.len();
    let activity = if terminal {
        format!("{:?}", inner.status)
    } else {
        inner
            .last_assistant_message
            .as_deref()
            .map(|m| {
                let c = m.replace('\n', " ");
                ellipsize_chars(&c, 80)
            })
            .unwrap_or_else(|| {
                if events == 0 {
                    "starting…".into()
                } else {
                    "working…".into()
                }
            })
    };
    (
        format!("[{label}] {elapsed} | {events} ev | {activity}"),
        terminal,
    )
}

pub(crate) fn format_progress_snapshot(
    tasks: &[Arc<orch::Task>],
    store_dir: &Path,
) -> (String, bool) {
    let mut all_terminal = true;
    let lines: Vec<String> = tasks
        .iter()
        .map(|t| {
            let (line, terminal) = format_bro_line(t, store_dir);
            if !terminal {
                all_terminal = false;
            }
            line
        })
        .collect();
    (lines.join("\n"), all_terminal)
}

/// Load the effective tool filter set for a dispatch (global + project
/// overlay + default recursion guard unless `allow_recursion`), then
/// translate to provider-specific CLI args. For Gemini, also writes a
/// per-dispatch policy file and returns the path so the caller can
/// clean it up after the child exits.
pub(crate) struct DispatchFilters {
    pub(crate) args: Vec<String>,
    /// Tempfile path for Gemini policy cleanup; None for other providers.
    pub(crate) policy_file: Option<PathBuf>,
    pub(crate) filters: orchestration::mcp::McpFilters,
}

/// Build a per-dispatch McpFilters overlay from a tool's allow/disallow
/// param vectors. Returns None when both are empty so callers can pass
/// None directly into resolve_dispatch_filters without an empty merge.
pub(crate) fn extra_filters_from_params(
    allow: Option<&[String]>,
    disallow: Option<&[String]>,
) -> Option<orchestration::mcp::McpFilters> {
    let allow = allow.unwrap_or(&[]);
    let disallow = disallow.unwrap_or(&[]);
    if allow.is_empty() && disallow.is_empty() {
        return None;
    }
    Some(orchestration::mcp::McpFilters {
        allow: allow
            .iter()
            .map(|p| orchestration::mcp::normalize_filter_pattern(p))
            .collect(),
        disallow: disallow
            .iter()
            .map(|p| orchestration::mcp::normalize_filter_pattern(p))
            .collect(),
    })
}

/// Combine brofile-embedded filters with per-dispatch params overlay.
/// Brofile applies first (persona scope), then per-dispatch (call scope).
/// Returns None when both are empty/absent.
pub(crate) fn combine_dispatch_filters(
    brofile_filters: Option<&orchestration::mcp::McpFilters>,
    params_filters: Option<&orchestration::mcp::McpFilters>,
) -> Option<orchestration::mcp::McpFilters> {
    match (brofile_filters, params_filters) {
        (None, None) => None,
        (Some(b), None) => Some(b.clone()),
        (None, Some(p)) => Some(p.clone()),
        (Some(b), Some(p)) => {
            let mut combined = b.clone();
            combined.merge_from(p);
            Some(combined)
        }
    }
}

pub(crate) fn resolve_dispatch_filters(
    provider: Provider,
    project_dir: Option<&str>,
    allow_recursion: bool,
    _task_id: &str,
    extra: Option<&orchestration::mcp::McpFilters>,
) -> Result<DispatchFilters, String> {
    let global = orchestration::mcp::global_store_path()
        .and_then(|p| orchestration::mcp::McpStore::load(&p).ok())
        .unwrap_or_default();
    let project = project_dir
        .map(|pd| orchestration::mcp::project_store_path(Path::new(pd)))
        .and_then(|p| orchestration::mcp::McpStore::load(&p).ok());

    let mut eff = orchestration::mcp::resolve_effective(
        &global,
        project.as_ref(),
        /* include_default_guard */ !allow_recursion,
    );

    // Per-dispatch overlay merges last (after global, project, and the
    // default recursion guard) so callers can tighten or open the filter set
    // for a single invocation. Disallow patterns in `extra` add to the deny
    // set; allow patterns add to the allow set. Recursion guard still wins
    // because allow doesn't override disallow at provider level.
    //
    // Surface-packet governance for in-process sessions (harness-daemon-boundary.md
    // §6): originally surface selection was MCP-endpoint-only (the rmcp wire head
    // filters list_tools/call_tool by `?surface=`). But in-process harness
    // sessions call tools directly and never hit that head, so they would escape
    // surface governance entirely. The fix is to let callers fold a surface
    // verdict into this client-side plane via `extra`: a brofile names a surface,
    // the caller evaluates it with `surface::dispatch_surface_filters` (the same
    // evaluate_tool_surface authority the wire head uses), and merges the result
    // here. Surface, recursion guard, and brofile/per-dispatch filters all
    // compose with disallow-wins.
    if let Some(extra) = extra {
        eff.filters.merge_from(extra);
    }

    let args = provider.build_filter_args(&eff.filters);
    let policy_file = None;

    Ok(DispatchFilters {
        args,
        policy_file,
        filters: eff.filters,
    })
}

/// Delete a Gemini policy tempfile once the associated task reaches a
/// terminal state. Spawned as a detached tokio task from the dispatch
/// path. No-op if path is None.
pub(crate) fn cleanup_policy_file_when_done(
    task: std::sync::Arc<orch::Task>,
    path: Option<PathBuf>,
) {
    let Some(path) = path else { return };
    tokio::spawn(async move {
        loop {
            {
                let inner = task.inner.lock();
                if inner.status.is_terminal() {
                    break;
                }
            }
            tokio::select! {
                _ = task.notify.notified() => {}
                _ = tokio::time::sleep(std::time::Duration::from_secs(30)) => {}
            }
        }
        if let Err(e) = std::fs::remove_file(&path) {
            tracing::debug!("gemini policy cleanup {}: {e}", path.display());
        }
    });
}

pub(crate) fn try_acquire_resume_lease(
    task_store: &RwLock<TaskStore>,
    leases: &orchestration::resume_lease::ResumeLeaseRegistry,
    provider: Provider,
    session_id: &str,
) -> Result<tokio::sync::OwnedMutexGuard<()>, String> {
    if let Some(lease) = leases.try_acquire(provider, session_id) {
        return Ok(lease);
    }
    let running_task = running_task_for_session(task_store, provider, session_id)
        .unwrap_or_else(|| "<unknown>".to_string());
    Err(format!(
        "session {session_id} for provider {provider} already has an in-flight resume task ({running_task}). Wait for it with bro_wait(task_id=\"{running_task}\", timeout_seconds=120) or cancel it with bro_cancel(task_id=\"{running_task}\") before calling bro_resume again."
    ))
}

pub(crate) fn running_task_for_session(
    task_store: &RwLock<TaskStore>,
    provider: Provider,
    session_id: &str,
) -> Option<String> {
    task_store.read().all_tasks().into_iter().find_map(|task| {
        let inner = task.inner.lock();
        (inner.provider == provider
            && inner.session_id == session_id
            && inner.status == orch::TaskStatus::Running)
            .then(|| inner.id.clone())
    })
}

pub(crate) fn release_resume_lease_when_done(
    task: std::sync::Arc<orch::Task>,
    lease: tokio::sync::OwnedMutexGuard<()>,
) {
    tokio::spawn(async move {
        orch::wait_for_task(&task).await;
        drop(lease);
    });
}

pub(crate) fn spawn_progress_notifier(
    tasks: Vec<Arc<orch::Task>>,
    peer: rmcp::service::Peer<rmcp::RoleServer>,
    progress_token: rmcp::model::ProgressToken,
    store_dir: PathBuf,
) -> tokio::task::JoinHandle<()> {
    tracing::info!(target: "blackbox::progress", token = ?progress_token, tasks = tasks.len(), "notifier spawned");
    tokio::spawn(async move {
        let mut tick = 0u64;
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(PROGRESS_TICK_SECS)).await;
            tick += 1;

            let (msg, all_terminal) = format_progress_snapshot(&tasks, &store_dir);

            let send_result = peer
                .send_notification(rmcp::model::ServerNotification::ProgressNotification(
                    rmcp::model::Notification::new(rmcp::model::ProgressNotificationParam {
                        progress_token: progress_token.clone(),
                        progress: tick as f64,
                        total: None,
                        message: Some(msg.clone()),
                    }),
                ))
                .await;
            match send_result {
                Ok(()) => {
                    tracing::debug!(target: "blackbox::progress", tick, terminal = all_terminal, msg_len = msg.len(), "tick sent")
                }
                Err(e) => {
                    tracing::warn!(target: "blackbox::progress", tick, error = %e, "tick send failed")
                }
            }

            if all_terminal {
                break;
            }
        }
    })
}

#[cfg(test)]
mod progress_tests {
    use super::*;

    #[test]
    fn ellipsize_chars_handles_utf8_boundaries() {
        let input = format!("{}’{}", "x".repeat(79), "tail");
        let output = ellipsize_chars(&input, 80);

        assert_eq!(output.chars().count(), 81);
        assert!(output.ends_with('…'));
    }

    #[test]
    fn ellipsize_chars_leaves_short_text_unchanged() {
        assert_eq!(ellipsize_chars("working…", 80), "working…");
    }
}
