use std::collections::HashMap;
use std::fs;
use std::io::Write as _;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant, UNIX_EPOCH};

use anyhow::Result;
use tantivy::schema::*;
use tantivy::{Index, IndexWriter};
use walkdir::WalkDir;

use super::knowledge_docs;
use super::project_files;
use super::tool_edges::ToolEdgeContext;
use super::writer_actor::IndexWriterActor;
use super::{FieldHandles, FileMeta, ReindexConfig};
use crate::projects::ProjectRecord;
use crate::transcripts::adapters::{
    TranscriptAdapterRegistry, TranscriptReadAdapter, TranscriptScanTarget,
};
use crate::transcripts::projection::{normalized_to_doc, normalized_to_tool_call_doc};
use crate::transcripts::types::TranscriptLocation;

// At the default 120s interval this is one full refresh per day. Full
// project refreshes rewrite managed derived sidecars and trigger legacy
// sidecar compaction, preventing append-only graph edges from growing forever.
const DEFAULT_BACKGROUND_FULL_REINDEX_EVERY_TICKS: u64 = 720;

fn background_startup_delay(interval: Duration) -> Duration {
    std::env::var("BLACKBOX_REINDEX_STARTUP_DELAY_SECS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .map(Duration::from_secs)
        .unwrap_or(interval)
}

pub(super) fn conservative_log_merge_policy() -> tantivy::merge_policy::LogMergePolicy {
    let mut policy = tantivy::merge_policy::LogMergePolicy::default();
    policy.set_min_num_segments(20);
    policy.set_max_docs_before_merge(500_000);
    policy.set_del_docs_ratio_before_merge(0.3);
    policy
}

pub(super) fn segment_count(index: &Index) -> usize {
    index
        .searchable_segment_metas()
        .map(|segments| segments.len())
        .unwrap_or(0)
}

pub(super) fn load_meta(path: &Path) -> Result<HashMap<String, FileMeta>> {
    if !path.exists() {
        return Ok(HashMap::new());
    }
    let raw = fs::read_to_string(path)?;
    Ok(serde_json::from_str(&raw)?)
}

pub(super) fn save_meta(path: &Path, meta: &HashMap<String, FileMeta>) -> Result<()> {
    let raw = serde_json::to_string(meta)?;
    let tmp_path = path.with_extension("json.tmp");
    let mut file = fs::File::create(&tmp_path)?;
    file.write_all(raw.as_bytes())?;
    file.sync_all()?;
    drop(file);
    fs::rename(&tmp_path, path)?;
    Ok(())
}

pub(super) fn dir_size(path: &Path) -> u64 {
    WalkDir::new(path)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter_map(|e| e.metadata().ok())
        .filter(|m| m.is_file())
        .map(|m| m.len())
        .sum()
}

pub(super) fn count_jsonl_files(dir: &Path) -> usize {
    WalkDir::new(dir)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.path().extension().is_some_and(|ext| ext == "jsonl"))
        .count()
}

// ── Background auto-reindex ────────────────────────────────────────

/// Collect (path, mtime, size) for all JSONL files in a directory tree.
pub(super) fn scan_jsonl_dir(dir: &Path, out: &mut Vec<(String, u64, u64)>) {
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
pub(super) fn scan_single_file(path: &Path, out: &mut Vec<(String, u64, u64)>) {
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
pub(super) fn scan_source_files(config: &ReindexConfig) -> Vec<(String, u64, u64)> {
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

pub(super) fn scan_all_source_files(config: &ReindexConfig) -> Vec<(String, u64, u64)> {
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
    files
}

/// Check if any source files have changed since last index.
/// Returns true if reindexing is needed (cheap — stat only, no I/O on file contents).
pub(super) fn needs_reindex(config: &ReindexConfig) -> bool {
    let meta = load_meta(&config.meta_path).unwrap_or_default();
    let files = scan_all_source_files(config);
    let current_paths: std::collections::HashSet<&str> =
        files.iter().map(|(p, _, _)| p.as_str()).collect();
    // Check for new or changed files
    for (path, mtime, size) in &files {
        match meta.get(path.as_str()) {
            Some(prev) if prev.mtime == *mtime && prev.size == *size => continue,
            _ => return true,
        }
    }
    // Check for deleted files (in meta but not on disk)
    for path in meta.keys() {
        if !current_paths.contains(path.as_str()) {
            return true;
        }
    }
    false
}

/// Restores the reindex-dirty flag when a *triggered* pass exits before
/// committing (writer lock busy, or any phase/commit error via `?`/panic).
/// Disarmed on a committed pass or a genuine no-op, so a satisfied trigger is
/// not replayed forever. Events arriving *during* a pass re-set the flag
/// independently (after the gate's `swap(false)`) and are therefore preserved.
struct DirtyRestore<'a> {
    flag: &'a AtomicBool,
    armed: bool,
}

impl DirtyRestore<'_> {
    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for DirtyRestore<'_> {
    fn drop(&mut self) {
        if self.armed {
            self.flag.store(true, Ordering::Relaxed);
        }
    }
}

/// Gate + dispatch for one scheduled reindex tick. The cheap speculative
/// scan runs here on the scheduler thread; the pass itself executes inside
/// the IndexWriterActor (the daemon's only in-process tantivy writer), so
/// the in-process LockBusy/silent-skip class is gone — small ops queued
/// during a pass drain into the pass's own commit.
///
/// `reindex_dirty` lets out-of-band sources (the `.bbox/knowledge` watcher,
/// daemon startup) force one pass even when `needs_reindex` sees no tracked
/// source-file change — repo-owned `.bbox/knowledge` files are deliberately not
/// in the meta-tracked source set, so they can only be picked up this way.
fn scheduled_reindex_tick(
    actor: &IndexWriterActor,
    config: &ReindexConfig,
    full: bool,
    reindex_dirty: &AtomicBool,
) -> Result<()> {
    // Speculative scan — cheap, no writer allocation. `dirty` forces a pass
    // (and is consumed here); a failed pass restores it via the guard.
    let dirty = reindex_dirty.swap(false, Ordering::Relaxed);
    if !full && !needs_reindex(config) && !dirty {
        tracing::debug!("auto-reindex: no changes detected");
        return Ok(());
    }
    let mut dirty_guard = DirtyRestore {
        flag: reindex_dirty,
        armed: dirty,
    };
    actor.run_reindex_pass(full, dirty)?;
    // Committed (or a genuine no-op) — the trigger is satisfied; don't replay it.
    dirty_guard.disarm();
    Ok(())
}

/// Execute one reindex pass with a writer owned by the caller (the
/// IndexWriterActor). `drain` is invoked at phase boundaries so small ops
/// queued behind the pass land in the same commit instead of waiting for
/// the next cycle. Returns the human-readable summary line.
pub(super) fn execute_reindex_pass(
    index: &Index,
    config: &ReindexConfig,
    fields: FieldHandles,
    full: bool,
    dirty: bool,
    writer: &mut IndexWriter,
    drain: &mut dyn FnMut(&mut IndexWriter),
) -> Result<String> {
    // Reload meta at pass start (a prior pass may have committed since the
    // scheduler's speculative scan).
    let mut meta = if full {
        tracing::info!("auto-reindex: periodic full rebuild requested");
        writer.delete_all_documents()?;
        // Don't commit yet — let the rebuild work and the trailing
        // writer.commit() atomically commit delete+adds together.
        // If we commit delete now and a later step fails, the index
        // is empty while _meta.json still says sources are current.
        HashMap::new()
    } else {
        load_meta(&config.meta_path).unwrap_or_default()
    };

    // 4. Index changed files
    let mut indexed_files = 0u64;
    let mut indexed_docs = 0u64;
    let mut skipped = 0u64;
    let tool_edges = ToolEdgeContext::from_config(config, !full)?;

    let transcript_phase = Instant::now();
    index_transcripts_via_adapters(
        config,
        fields,
        writer,
        &mut meta,
        &mut indexed_files,
        &mut indexed_docs,
        &mut skipped,
        &tool_edges,
        !full,
    )?;
    tracing::info!(
        full,
        elapsed_ms = transcript_phase.elapsed().as_millis(),
        indexed_files,
        indexed_docs,
        skipped,
        "auto-reindex: transcript phase complete"
    );

    drain(writer);

    let project_phase = Instant::now();
    let project_stats = project_files::index_registered_projects_standalone(
        config,
        fields,
        &mut *writer,
        &mut meta,
        full,
    )?;
    indexed_files += project_stats.indexed_files;
    indexed_docs += project_stats.indexed_docs;
    skipped += project_stats.skipped;
    tracing::info!(
        full,
        elapsed_ms = project_phase.elapsed().as_millis(),
        indexed_files = project_stats.indexed_files,
        indexed_docs = project_stats.indexed_docs,
        skipped = project_stats.skipped,
        emitted_edges = project_stats.emitted_edges,
        indexed_commits = project_stats.indexed_commits,
        "auto-reindex: project phase complete"
    );
    if project_stats.emitted_edges > 0 {
        tracing::debug!(
            emitted_edges = project_stats.emitted_edges,
            indexed_commits = project_stats.indexed_commits,
            call_edges = project_stats.call_edges,
            resolved_call_edges = project_stats.resolved_call_edges,
            "auto-reindex: accumulated project-file edges"
        );
    }

    drain(writer);

    let stores_phase = Instant::now();
    let knowledge_docs = knowledge_docs::reindex_knowledge_store_standalone(
        &config.knowledge_path,
        &config.projects_path,
        fields,
        &mut *writer,
        &mut meta,
    )?;
    if knowledge_docs > 0 {
        indexed_files += 1;
        indexed_docs += knowledge_docs;
    }
    let thread_docs = super::thread_docs::reindex_threads_store_standalone(
        &config.threads_path,
        fields,
        &mut *writer,
        &mut meta,
    )?;
    if thread_docs > 0 {
        indexed_files += 1;
        indexed_docs += thread_docs;
    }
    let record_docs = super::thread_docs::reindex_project_records_standalone(
        &config.projects_path,
        &config.threads_path,
        fields,
        &mut *writer,
    )?;
    indexed_docs += record_docs;
    let roadmap_docs = super::roadmap_docs::reindex_roadmap_store_standalone(
        &config.roadmap_path,
        fields,
        &mut *writer,
        &mut meta,
    )?;
    if roadmap_docs > 0 {
        indexed_files += 1;
        indexed_docs += roadmap_docs;
    }
    tracing::info!(
        full,
        elapsed_ms = stores_phase.elapsed().as_millis(),
        knowledge_docs,
        thread_docs,
        roadmap_docs,
        "auto-reindex: store-doc phase complete"
    );

    // 4b. Purge documents for deleted source files
    let purge_phase = Instant::now();
    let current_files = scan_all_source_files(config);
    let current_paths: std::collections::HashSet<String> =
        current_files.iter().map(|(p, _, _)| p.clone()).collect();
    let mut purged = 0u64;
    let stale_paths: Vec<String> = meta
        .keys()
        .filter(|p| !current_paths.contains(p.as_str()))
        .cloned()
        .collect();
    for path in &stale_paths {
        let term = Term::from_field_text(fields.file_path, path);
        writer.delete_term(term);
        meta.remove(path.as_str());
        purged += 1;
    }
    tracing::info!(
        full,
        elapsed_ms = purge_phase.elapsed().as_millis(),
        current_files = current_files.len(),
        purged,
        "auto-reindex: purge phase complete"
    );

    drain(writer);

    // A dirty-triggered pass must still commit even when no *tracked* source
    // file changed: the knowledge reindex above may have deleted/re-added repo
    // entries (e.g. a deleted `.bbox/knowledge` file) whose delete_term must
    // land. Only short-circuit when nothing triggered us. (Drained small ops
    // still need a commit, but the actor commits its own batches; an op that
    // landed in this no-op pass commits with the actor's next cycle or the
    // pass's caller — never lost, reconciled by the next triggered pass.)
    if !full && indexed_files == 0 && purged == 0 && !dirty {
        let summary = "auto-reindex: no changes after re-check".to_string();
        tracing::debug!("{}", summary);
        // Commit anyway when ops were drained into this writer mid-pass;
        // detecting that precisely isn't worth the bookkeeping — an empty
        // commit is cheap and keeps drained ops from straddling passes.
        writer.commit()?;
        return Ok(summary);
    }

    // 5. Commit + atomic meta save
    let commit_phase = Instant::now();
    writer.commit()?;
    save_meta(&config.meta_path, &meta)?;
    tracing::info!(
        full,
        elapsed_ms = commit_phase.elapsed().as_millis(),
        "auto-reindex: commit phase complete"
    );

    let segments = segment_count(index);
    let summary = format!(
        "auto-reindex: indexed {indexed_files} files ({indexed_docs} docs), skipped {skipped} unchanged, purged {purged} deleted, segments {segments}"
    );
    tracing::info!("{}", summary);
    Ok(summary)
}

/// Spawn the background reindex thread. Runs every `interval` seconds.
///
/// `reindex_dirty` is a shared out-of-band trigger: the `.bbox/knowledge`
/// watcher (and daemon startup) set it so repo-owned knowledge changes that
/// `needs_reindex` cannot see still drive one incremental pass.
pub(crate) fn spawn_reindex_thread(
    actor: super::writer_actor::IndexWriterActor,
    config: ReindexConfig,
    interval: Duration,
    reindex_dirty: Arc<AtomicBool>,
) {
    std::thread::Builder::new()
        .name("blackbox-reindex".into())
        .spawn(move || {
            tracing::info!(
                "background reindex thread started (interval: {:?})",
                interval
            );
            let full_reindex_every_ticks = std::env::var("BLACKBOX_BACKGROUND_FULL_REINDEX_TICKS")
                .ok()
                .and_then(|v| v.parse::<u64>().ok())
                .unwrap_or(DEFAULT_BACKGROUND_FULL_REINDEX_EVERY_TICKS);
            if full_reindex_every_ticks == 0 {
                tracing::info!("background full reindex disabled");
            } else {
                tracing::info!(
                    ticks = full_reindex_every_ticks,
                    "background full reindex enabled"
                );
            }
            let startup_delay = background_startup_delay(interval);
            tracing::info!(
                delay_secs = startup_delay.as_secs(),
                "background reindex startup delay configured"
            );
            std::thread::sleep(startup_delay);
            let mut tick = 0_u64;
            loop {
                tick = tick.wrapping_add(1);
                let full =
                    full_reindex_every_ticks != 0 && tick.is_multiple_of(full_reindex_every_ticks);
                if let Err(e) = scheduled_reindex_tick(&actor, &config, full, &reindex_dirty) {
                    tracing::error!("background reindex failed: {:#}", e);
                }
                std::thread::sleep(interval);
            }
        })
        .expect("failed to spawn reindex thread");
}

// ── Standalone indexing functions (no &self — usable from background thread) ──

pub(super) fn should_skip_file(
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
pub(crate) fn index_transcripts_via_adapters(
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
fn index_adapter_location(
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

/// Walk all indexed transcripts and retroactively emit observed tool-call edges
/// (EDITED_FILE / READ_FILE / RAN_BASH) for a newly registered project.
///
/// Idempotent: uses `append_edges_dedup` so re-running produces no duplicates.
/// Returns the number of new edges written.
pub(crate) fn backfill_tool_edges_for_project(
    config: &ReindexConfig,
    project: &ProjectRecord,
) -> Result<usize> {
    let edges_dir = crate::edge_index::edges_dir_from_projects_path(&config.projects_path);
    let ctx = ToolEdgeContext::for_project(project.clone(), edges_dir.clone());
    let registry = TranscriptAdapterRegistry::from_reindex_config(config);
    let mut collected: Vec<crate::edge_index::Edge> = Vec::new();

    for adapter in registry.adapters() {
        for target in [
            TranscriptScanTarget::Sessions,
            TranscriptScanTarget::History,
        ] {
            let locations = match adapter.scan_locations(target) {
                Ok(locs) => locs,
                Err(err) => {
                    tracing::warn!(
                        error = %err,
                        "backfill: adapter scan failed, skipping"
                    );
                    continue;
                }
            };
            for location in locations {
                let source_label = location.source.label();
                let account = location.account.as_deref().unwrap_or(source_label);
                let snapshot = match adapter.load_snapshot(&location) {
                    Ok(s) => s,
                    Err(_) => continue,
                };
                for event in &snapshot.events {
                    let Some(parsed) = event.to_parsed_event() else {
                        continue;
                    };
                    let line_offset = event.raw.byte_offset.unwrap_or(0);
                    let event_idx = event.raw.event_idx.unwrap_or(0);
                    match ctx.build_event_edges(&parsed, account, line_offset, event_idx) {
                        Ok(Some(edge)) => collected.push(edge),
                        Ok(None) => {}
                        Err(err) => {
                            tracing::debug!(
                                error = %err,
                                "backfill: skipping event edge build error"
                            );
                        }
                    }
                }
            }
        }
    }

    if collected.is_empty() {
        return Ok(0);
    }

    let observed_dir = edges_dir.join("observed");
    crate::edge_index::append_edges_dedup(&observed_dir, &project.project_id, &collected)
}

pub(super) fn human_bytes(bytes: u64) -> String {
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

#[cfg(test)]
mod tests {
    use crate::orchestration::providers::Provider;
    use super::*;
    use crate::entity_ref;
    use crate::parser::{MessageRole, ParsedEvent};
    use crate::transcripts::types::{
        NormalizedTranscriptEvent, RawTranscriptRef, TranscriptStorage,
    };
    use tantivy::TantivyDocument;

    #[test]
    fn transcript_docs_include_doc_type_and_parser_version() {
        let (_schema, fields) = crate::index::build_schema();
        let parsed = ParsedEvent {
            role: MessageRole::User,
            content: "schema migration smoke".to_string(),
            session_id: "session-1".to_string(),
            timestamp: None,
            git_branch: None,
            is_subagent: false,
            agent_slug: None,
            cwd: None,
            tool_call: None,
        };
        let raw = RawTranscriptRef::jsonl(
            crate::transcripts::types::TranscriptSource::Harness(Provider::Brodex),
            TranscriptStorage::JsonlFile,
            "/tmp/session.jsonl",
            0,
            0,
            0,
        );
        let normalized =
            NormalizedTranscriptEvent::from_parsed_event(
                crate::transcripts::types::TranscriptSource::Harness(Provider::Brodex),
                parsed,
                raw,
            );
        let doc = normalized_to_doc(
            &normalized,
            "codex",
            "/tmp/session.jsonl",
            false,
            "",
            fields,
        )
        .expect("normalized event is indexable");

        assert_eq!(first_text(&doc, fields.doc_type), "transcript");
        assert_eq!(
            first_text(&doc, fields.parser_version),
            entity_ref::PARSER_VERSION
        );
    }

    fn first_text(doc: &TantivyDocument, field: Field) -> String {
        doc.get_all(field)
            .next()
            .and_then(|v| match v {
                tantivy::schema::OwnedValue::Str(s) => Some(s.clone()),
                _ => None,
            })
            .unwrap_or_default()
    }

    /// End-to-end: a synthetic harness session event log under
    /// `harness_sessions_dir` is discovered by the adapter registry and
    /// indexed into tantivy with role/session/timestamp/project intact.
    #[test]
    fn harness_session_event_log_indexes_into_test_index() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let sessions_dir = root.join("bro-home").join("harness-sessions");
        std::fs::create_dir_all(&sessions_dir).unwrap();
        std::fs::write(
            sessions_dir.join("sess-hx.events.jsonl"),
            concat!(
                r#"{"ts":"2026-06-10T01:00:00.000Z","event":{"type":"harness_milestone","milestone":"session_start","session_id":"sess-hx","transport":"openai-responses","model":"gpt-5.5","cwd":"/repo/hx","provider":"brodex"}}"#,
                "\n",
                r#"{"ts":"2026-06-10T01:00:01.000Z","event":{"type":"user","session_id":"sess-hx","message":{"role":"user","content":[{"type":"text","text":"investigate the flaky test"}]}}}"#,
                "\n",
                r#"{"ts":"2026-06-10T01:00:09.000Z","event":{"type":"assistant","session_id":"sess-hx","message":{"role":"assistant","content":[{"type":"text","text":"the test races the reindex thread"},{"type":"tool_use","id":"t1","name":"shell_run","input":{"command":"cargo test --lib reindex"}}]}}}"#,
                "\n",
            ),
        )
        .unwrap();

        let config = ReindexConfig {
            roots: Vec::new(),
            codex_root: None,
            meta_path: root.join("_meta.json"),
            projects_path: root.join("projects.json"),
            knowledge_path: root.join("kb.json"),
            threads_path: root.join("threads.json"),
            roadmap_path: root.join("roadmap.json"),
            harness_sessions_dir: Some(sessions_dir),
            gemini_tmp_root: None,
        };

        let (schema, fields) = crate::index::build_schema();
        let idx_dir = root.join("idx");
        std::fs::create_dir_all(&idx_dir).unwrap();
        let index = tantivy::Index::create_in_dir(&idx_dir, schema).unwrap();
        crate::index::register_code_tokenizer(&index);
        let mut writer = index.writer(50_000_000).unwrap();
        let mut meta = HashMap::new();
        let (mut files, mut docs, mut skipped) = (0u64, 0u64, 0u64);
        let tool_edges = ToolEdgeContext::from_config(&config, false).unwrap();

        index_transcripts_via_adapters(
            &config,
            fields,
            &mut writer,
            &mut meta,
            &mut files,
            &mut docs,
            &mut skipped,
            &tool_edges,
            false,
        )
        .unwrap();
        writer.commit().unwrap();

        assert_eq!(files, 1, "one session log discovered");
        // user + assistant text + tool_use transcript docs, plus the tool_call
        // doc projected from the shell_run tool_use.
        assert_eq!(docs, 4, "indexed docs");

        let reader = index.reader().unwrap();
        let searcher = reader.searcher();
        let query = tantivy::query::TermQuery::new(
            Term::from_field_text(fields.session_id, "sess-hx"),
            tantivy::schema::IndexRecordOption::Basic,
        );
        let hits = searcher
            .search(&query, &tantivy::collector::TopDocs::with_limit(10))
            .unwrap();
        assert_eq!(hits.len(), 4);

        let mut saw_user = false;
        for (_score, addr) in hits {
            let doc: TantivyDocument = searcher.doc(addr).unwrap();
            assert_eq!(first_text(&doc, fields.account), "brodex");
            assert_eq!(first_text(&doc, fields.project), "/repo/hx");
            assert!(first_text(&doc, fields.timestamp).starts_with("2026-06-10T01:00:0"));
            if first_text(&doc, fields.role) == "user"
                && first_text(&doc, fields.doc_type) == "transcript"
            {
                saw_user = true;
                assert_eq!(
                    first_text(&doc, fields.timestamp),
                    "2026-06-10T01:00:01.000Z"
                );
            }
        }
        assert!(saw_user, "user prompt doc present");
    }
}
