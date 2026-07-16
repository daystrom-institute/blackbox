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
    if !config.roots.is_empty() {
        adapters.push(Box::new(ClaudeTranscriptAdapter::new(config.roots.clone())));
    }
    if let Some(codex_root) = config.codex_root.clone() {
        adapters.push(Box::new(CodexTranscriptAdapter::new(codex_root)));
    }
    if let Some(tmp_root) = config.gemini_tmp_root.clone() {
        adapters.push(Box::new(GeminiTranscriptAdapter::new(tmp_root)));
    }
    TranscriptAdapterRegistry::new(adapters)
}
