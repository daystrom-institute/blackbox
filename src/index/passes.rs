//! Engine-side reindex utilities: transcript/source scanning, file-meta
//! load/save, adapter-driven transcript indexing, merge policy, and stats
//! helpers. Split out of `reindex.rs` so the orchestration that binds the
//! stores (execute_reindex_pass, spawn_reindex_thread) can stay daemon-side
//! while the engine owns the mechanics.

use std::collections::HashMap;
use std::fs;
use std::io::Write as _;
use std::path::Path;
use std::time::UNIX_EPOCH;

use anyhow::Result;
use tantivy::schema::*;
use tantivy::{Index, IndexWriter};
use walkdir::WalkDir;

use super::project_files;
use super::tool_edges::ToolEdgeContext;
use super::{FieldHandles, FileMeta, ReindexConfig};
use crate::transcripts::adapters::{
    TranscriptAdapterRegistry, TranscriptReadAdapter, TranscriptScanTarget,
};
use crate::transcripts::projection::{normalized_to_doc, normalized_to_tool_call_doc};
use crate::transcripts::types::TranscriptLocation;

pub fn conservative_log_merge_policy() -> tantivy::merge_policy::LogMergePolicy {
    let mut policy = tantivy::merge_policy::LogMergePolicy::default();
    policy.set_min_num_segments(20);
    policy.set_max_docs_before_merge(500_000);
    policy.set_del_docs_ratio_before_merge(0.3);
    policy
}


pub fn segment_count(index: &Index) -> usize {
    index
        .searchable_segment_metas()
        .map(|segments| segments.len())
        .unwrap_or(0)
}


pub fn load_meta(path: &Path) -> Result<HashMap<String, FileMeta>> {
    if !path.exists() {
        return Ok(HashMap::new());
    }
    let raw = fs::read_to_string(path)?;
    Ok(serde_json::from_str(&raw)?)
}


pub fn save_meta(path: &Path, meta: &HashMap<String, FileMeta>) -> Result<()> {
    let raw = serde_json::to_string(meta)?;
    let tmp_path = path.with_extension("json.tmp");
    let mut file = fs::File::create(&tmp_path)?;
    file.write_all(raw.as_bytes())?;
    file.sync_all()?;
    drop(file);
    fs::rename(&tmp_path, path)?;
    Ok(())
}


pub fn dir_size(path: &Path) -> u64 {
    WalkDir::new(path)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter_map(|e| e.metadata().ok())
        .filter(|m| m.is_file())
        .map(|m| m.len())
        .sum()
}


pub fn count_jsonl_files(dir: &Path) -> usize {
    WalkDir::new(dir)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.path().extension().is_some_and(|ext| ext == "jsonl"))
        .count()
}


// ── Background auto-reindex ────────────────────────────────────────

/// Collect (path, mtime, size) for all JSONL files in a directory tree.
pub fn scan_jsonl_dir(dir: &Path, out: &mut Vec<(String, u64, u64)>) {
    for entry in WalkDir::new(dir)
        .follow_links(true)
        .into_iter()
        .filter_map(|e| e.ok())
    {
        let path = entry.path();
        if path.extension().map(|e| e != "jsonl").unwrap_or(true) {
            continue;
        }
        let meta = match entry.metadata() {
            Ok(m) => m,
            Err(_) => continue,
        };
        let mtime = match meta.modified() {
            Ok(t) => t.duration_since(UNIX_EPOCH).unwrap_or_default().as_secs(),
            Err(_) => continue,
        };
        out.push((path.to_string_lossy().to_string(), mtime, meta.len()));
    }
}


/// Stat a single file and push if not too recent.
pub fn scan_single_file(path: &Path, out: &mut Vec<(String, u64, u64)>) {
    if let Ok(meta) = fs::metadata(path) {
        let mtime = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .map(|d| d.as_secs())
            .unwrap_or(0);
        out.push((path.to_string_lossy().to_string(), mtime, meta.len()));
    }
}


/// Collect (path, mtime, size) for all JSONL files across all roots.
pub fn scan_source_files(config: &ReindexConfig) -> Vec<(String, u64, u64)> {
    let mut files = Vec::new();
    for (_name, root) in &config.roots {
        let projects_dir = root.join("projects");
        if projects_dir.exists() {
            scan_jsonl_dir(&projects_dir, &mut files);
        }
        let history = root.join("history.jsonl");
        if history.exists() {
            scan_single_file(&history, &mut files);
        }
    }

    if let Some(ref codex_root) = config.codex_root {
        let sessions_dir = codex_root.join("sessions");
        if sessions_dir.exists() {
            scan_jsonl_dir(&sessions_dir, &mut files);
        }
        let history = codex_root.join("history.jsonl");
        if history.exists() {
            scan_single_file(&history, &mut files);
        }
    }

    files
}


/// Collect (path, mtime, size) for every transcript file owned by a
/// registered transcript adapter (harness session event logs, gemini tmp
/// sessions — anything not covered by the legacy roots walk above).
///
/// This scan is load-bearing for two consumers, not just change detection:
/// the purge phase treats any indexed `file_path` absent from
/// `scan_all_source_files` as a deleted source and removes its docs, so an
/// adapter source missing here is silently purged in the same pass that
/// indexed it (observed live: gap-4629bbeb probe sessions, 2026-06-10
/// 18:09 pass "purged 2 deleted").
pub fn scan_adapter_source_files(config: &ReindexConfig, files: &mut Vec<(String, u64, u64)>) {
    let registry = TranscriptAdapterRegistry::from_reindex_config(config);
    for adapter in registry.adapters() {
        for target in [
            TranscriptScanTarget::Sessions,
            TranscriptScanTarget::History,
        ] {
            match adapter.scan_locations(target) {
                Ok(locations) => {
                    for location in locations {
                        scan_single_file(&location.path, files);
                    }
                }
                Err(err) => {
                    tracing::warn!(
                        source = ?adapter.source(),
                        error = %err,
                        "adapter source scan failed; its indexed sessions are purge-exposed this pass"
                    );
                }
            }
        }
    }
}


pub fn scan_all_source_files(config: &ReindexConfig) -> Vec<(String, u64, u64)> {
    let mut files = scan_source_files(config);
    if config.knowledge_path.exists() {
        scan_single_file(&config.knowledge_path, &mut files);
    }
    if config.threads_path.exists() {
        scan_single_file(&config.threads_path, &mut files);
    }
    if config.roadmap_path.exists() {
        scan_single_file(&config.roadmap_path, &mut files);
    }
    match project_files::scan_registered_project_files(config) {
        Ok(mut project_files) => files.append(&mut project_files),
        Err(err) => tracing::warn!(error = %err, "failed to scan registered project files"),
    }
    scan_adapter_source_files(config, &mut files);
    // Adapter-owned files can overlap the roots walk (interactive claude/codex
    // adapters discover the same jsonl files); purge and needs_reindex both
    // treat this as a set, so dedupe by path.
    files.sort_by(|a, b| a.0.cmp(&b.0));
    files.dedup_by(|a, b| a.0 == b.0);
    files
}


// ── Standalone indexing functions (no &self — usable from background thread) ──

pub fn should_skip_file(
    path_str: &str,
    mtime: u64,
    size: u64,
    meta: &HashMap<String, FileMeta>,
) -> bool {
    if let Some(prev) = meta.get(path_str) {
        prev.mtime == mtime && prev.size == size
    } else {
        false
    }
}


/// Adapter-driven transcript indexing. Replaces the per-provider standalone
/// loops with a uniform pipeline: each registered adapter discovers locations
/// (sessions + history), then `read_since` projects normalized events that
/// flow through `normalized_to_doc` into Tantivy. Tool-edge sidecars stay on
/// the existing `ParsedEvent` path via `to_parsed_event()`.
#[allow(clippy::too_many_arguments)]
pub fn index_transcripts_via_adapters(
    config: &ReindexConfig,
    fields: FieldHandles,
    writer: &mut IndexWriter,
    meta: &mut HashMap<String, FileMeta>,
    indexed_files: &mut u64,
    indexed_docs: &mut u64,
    skipped: &mut u64,
    tool_edges: &ToolEdgeContext,
    commit_progress: bool,
) -> Result<()> {
    let registry = TranscriptAdapterRegistry::from_reindex_config(config);
    for adapter in registry.adapters() {
        let sessions = adapter
            .scan_locations(TranscriptScanTarget::Sessions)
            .map_err(|err| anyhow::anyhow!("adapter sessions scan failed: {err}"))?;
        for location in sessions {
            index_adapter_location(
                adapter,
                &location,
                fields,
                writer,
                meta,
                indexed_files,
                indexed_docs,
                skipped,
                tool_edges,
                commit_progress,
            )?;
        }
        let history = adapter
            .scan_locations(TranscriptScanTarget::History)
            .map_err(|err| anyhow::anyhow!("adapter history scan failed: {err}"))?;
        for location in history {
            index_adapter_location(
                adapter,
                &location,
                fields,
                writer,
                meta,
                indexed_files,
                indexed_docs,
                skipped,
                tool_edges,
                false,
            )?;
        }
    }
    Ok(())
}


#[allow(clippy::too_many_arguments)]
pub fn index_adapter_location(
    adapter: &dyn TranscriptReadAdapter,
    location: &TranscriptLocation,
    fields: FieldHandles,
    writer: &mut IndexWriter,
    meta: &mut HashMap<String, FileMeta>,
    indexed_files: &mut u64,
    indexed_docs: &mut u64,
    skipped: &mut u64,
    tool_edges: &ToolEdgeContext,
    commit_progress: bool,
) -> Result<()> {
    let path_str = location.path.to_string_lossy().to_string();
    let file_meta = match fs::metadata(&location.path) {
        Ok(m) => m,
        Err(_) => return Ok(()),
    };
    let mtime = match file_meta.modified() {
        Ok(t) => t.duration_since(UNIX_EPOCH).unwrap_or_default().as_secs(),
        Err(_) => return Ok(()),
    };
    let size = file_meta.len();

    if should_skip_file(&path_str, mtime, size, meta) {
        *skipped += 1;
        return Ok(());
    }
    if meta.contains_key(&path_str) {
        writer.delete_term(Term::from_field_text(fields.file_path, &path_str));
    }

    let source_label = location.source.label();
    let account = location.account.as_deref().unwrap_or(source_label);
    let project_fallback = location.project.as_deref().unwrap_or("");

    let snapshot = match adapter.load_snapshot(location) {
        Ok(s) => s,
        Err(err) => {
            tracing::debug!(
                path = %location.path.display(),
                error = %err,
                "skipping transcript file the adapter could not read"
            );
            return Ok(());
        }
    };

    for event in &snapshot.events {
        let Some(parsed) = event.to_parsed_event() else {
            continue;
        };
        let line_offset = event.raw.byte_offset.unwrap_or(0);
        let event_idx = event.raw.event_idx.unwrap_or(0);
        if let Err(err) = tool_edges.emit_event_edges(&parsed, account, line_offset, event_idx) {
            tracing::debug!(
                error = %err,
                source = %location.source,
                "failed to emit transcript tool-call edge"
            );
        }
        let Some(doc) = normalized_to_doc(
            event,
            account,
            &path_str,
            location.is_subagent,
            project_fallback,
            fields,
        ) else {
            continue;
        };
        writer.add_document(doc)?;
        *indexed_docs += 1;
        if let Some(tool_doc) = normalized_to_tool_call_doc(
            event,
            account,
            &path_str,
            location.is_subagent,
            project_fallback,
            fields,
        ) {
            writer.add_document(tool_doc)?;
            *indexed_docs += 1;
        }
    }

    meta.insert(
        path_str,
        FileMeta {
            mtime,
            size,
            mat_version: None,
        },
    );
    *indexed_files += 1;
    if commit_progress && (*indexed_files).is_multiple_of(500) {
        tracing::info!("Indexed {} files ({} docs)...", indexed_files, indexed_docs);
        writer.commit()?;
    }
    Ok(())
}


pub fn human_bytes(bytes: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = 1024 * 1024;
    const GB: u64 = 1024 * 1024 * 1024;
    if bytes >= GB {
        format!("{:.1} GB", bytes as f64 / GB as f64)
    } else if bytes >= MB {
        format!("{:.1} MB", bytes as f64 / MB as f64)
    } else if bytes >= KB {
        format!("{:.1} KB", bytes as f64 / KB as f64)
    } else {
        format!("{} B", bytes)
    }
}
