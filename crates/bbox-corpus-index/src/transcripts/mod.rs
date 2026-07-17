#![allow(dead_code)]

//! Transcript reading + tantivy projection.
//!
//! The tantivy-free reading layer (source types, read-adapter trait +
//! registry, cursor store, interactive adapters, and the harness-sessions
//! reader including the strict prefix reader `read_fleet_event_log_prefix`)
//! lives in the `bbox-transcript-read` leaf crate and is re-exported here so
//! existing `transcripts::{types,adapters,cursor_store,interactive}::*` and
//! `transcripts::harness_sessions::{read_fleet_event_log_prefix, ...}`
//! consumer paths keep compiling unchanged. The tantivy PROJECTION half stays
//! local: `projection` (doc builders) and the projector functions in
//! `harness_sessions`.

pub use bbox_transcript_read::{adapters, cursor_store, interactive, types};

pub mod harness_sessions;
pub mod projection;

use crate::index::ReindexConfig;
use adapters::TranscriptAdapterRegistry;

/// Build the index-time transcript adapter registry from a [`ReindexConfig`].
///
/// Re-homed here (out of the config-agnostic `bbox-transcript-read` leaf)
/// because it needs the tantivy-side reindex config; the leaf keeps only
/// [`TranscriptAdapterRegistry::from_runtime_config`], whose deps are all
/// leaf-local. Every source root must be explicit in the config — harness
/// sessions dir, interactive claude roots, codex root, gemini tmp root are all
/// `None`/empty in hermetic test indexes — so reindex never silently scans the
/// operator's real state.
pub fn registry_from_reindex_config(config: &ReindexConfig) -> TranscriptAdapterRegistry {
    use adapters::TranscriptReadAdapter;
    use bbox_transcript_read::harness_sessions::HarnessSessionsAdapter;
    use bbox_transcript_read::interactive::{
        ClaudeTranscriptAdapter, CodexTranscriptAdapter, GeminiTranscriptAdapter,
    };

    let mut adapters: Vec<Box<dyn TranscriptReadAdapter>> = Vec::new();
    if let Some(dir) = &config.harness_sessions_dir {
        adapters.extend(HarnessSessionsAdapter::all_for_dir(dir));
    }
    for dir in &config.additional_harness_sessions_dirs {
        adapters.extend(HarnessSessionsAdapter::all_for_dir(dir));
    }
    let (collector_claude_roots, collector_codex_roots) = config
        .collector_archive_root
        .as_deref()
        .map(scan_collector_archive_roots)
        .unwrap_or_default();
    let mut claude_roots = config.roots.clone();
    claude_roots.extend(collector_claude_roots);
    if !claude_roots.is_empty() {
        adapters.push(Box::new(ClaudeTranscriptAdapter::new(claude_roots)));
    }
    if let Some(codex_root) = config.codex_root.clone() {
        adapters.push(Box::new(CodexTranscriptAdapter::new(codex_root)));
    }
    for codex_root in collector_codex_roots {
        adapters.push(Box::new(CodexTranscriptAdapter::new(codex_root)));
    }
    if let Some(tmp_root) = config.gemini_tmp_root.clone() {
        adapters.push(Box::new(GeminiTranscriptAdapter::new(tmp_root)));
    }
    TranscriptAdapterRegistry::new(adapters)
}

/// Discover satellite-collector archive account directories beneath the
/// corpus-owned collector archive root. Layout (enforced traversal-safe at
/// ingest by `inline_transcript_increments`):
/// `<root>/<host>/<source>/<account>/<source-relative-path>`, where each
/// account dir mirrors a source machine's provider root, so it plugs into the
/// interactive adapters unchanged. The scan runs on every registry build,
/// which means new satellite hosts start indexing on the next reindex pass
/// without a daemon restart. Unknown source dirs are skipped: ingest gates
/// sources to the supported set, so anything else here is inert until a
/// reader lane exists for it.
// Index-time discovery walks corpus-owned private state synchronously, like
// every other adapter root scan in this module tree.
#[allow(clippy::disallowed_methods)]
fn scan_collector_archive_roots(
    root: &std::path::Path,
) -> (Vec<(String, std::path::PathBuf)>, Vec<std::path::PathBuf>) {
    let mut claude_roots = Vec::new();
    let mut codex_roots = Vec::new();
    let hosts = match std::fs::read_dir(root) {
        Ok(entries) => entries,
        Err(_) => return (claude_roots, codex_roots),
    };
    for host in hosts.filter_map(Result::ok) {
        if !host.file_type().is_ok_and(|kind| kind.is_dir()) {
            continue;
        }
        for source in ["claude", "codex"] {
            let source_dir = host.path().join(source);
            let Ok(accounts) = std::fs::read_dir(&source_dir) else {
                continue;
            };
            for account in accounts.filter_map(Result::ok) {
                if !account.file_type().is_ok_and(|kind| kind.is_dir()) {
                    continue;
                }
                let label = account.file_name().to_string_lossy().into_owned();
                match source {
                    "claude" => claude_roots.push((label, account.path())),
                    _ => codex_roots.push(account.path()),
                }
            }
        }
    }
    claude_roots.sort();
    codex_roots.sort();
    (claude_roots, codex_roots)
}
