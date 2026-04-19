use std::collections::HashMap;
use std::fs;
use std::io::{BufRead, BufReader, Write as _};
use std::path::Path;
use std::time::{Duration, UNIX_EPOCH};

use anyhow::Result;
use tantivy::schema::*;
use tantivy::{Index, IndexWriter, TantivyDocument};
use walkdir::WalkDir;

use super::helpers::*;
use super::{FieldHandles, FileMeta, ReindexConfig};
use crate::parser;

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

/// Check if any source files have changed since last index.
/// Returns true if reindexing is needed (cheap — stat only, no I/O on file contents).
pub(super) fn needs_reindex(config: &ReindexConfig) -> bool {
    let meta = load_meta(&config.meta_path).unwrap_or_default();
    let files = scan_source_files(config);
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

/// Background reindex: speculative scan → try-lock → reload meta → index → commit.
/// Returns Ok(()) even when skipped (lock busy, no changes). Errors only on real failures.
fn try_background_reindex(
    index: &Index,
    config: &ReindexConfig,
    fields: FieldHandles,
) -> Result<()> {
    // 1. Speculative scan — cheap, no writer allocation
    if !needs_reindex(config) {
        tracing::debug!("auto-reindex: no changes detected");
        return Ok(());
    }

    // 2. Acquire writer — returns LockBusy immediately if another process holds it
    let mut writer: IndexWriter = match index.writer(100_000_000) {
        Ok(w) => w,
        Err(tantivy::TantivyError::LockFailure(_, _)) => {
            tracing::debug!("auto-reindex: writer lock busy, skipping");
            return Ok(());
        }
        Err(e) => return Err(e.into()),
    };

    // 3. Reload meta AFTER acquiring lock (another process may have committed)
    let mut meta = load_meta(&config.meta_path).unwrap_or_default();

    // 4. Index changed files
    let mut indexed_files = 0u64;
    let mut indexed_docs = 0u64;
    let mut skipped = 0u64;

    for (account_name, root) in &config.roots {
        let projects_dir = root.join("projects");
        if projects_dir.exists() {
            index_directory_standalone(
                &projects_dir,
                account_name,
                fields,
                &mut writer,
                &mut meta,
                &mut indexed_files,
                &mut indexed_docs,
                &mut skipped,
            )?;
        }
        let history = root.join("history.jsonl");
        if history.exists() {
            index_history_standalone(
                &history,
                account_name,
                fields,
                &mut writer,
                &mut meta,
                &mut indexed_files,
                &mut indexed_docs,
                &mut skipped,
            )?;
        }
    }

    if let Some(ref codex_root) = config.codex_root {
        let sessions_dir = codex_root.join("sessions");
        if sessions_dir.exists() {
            index_codex_directory_standalone(
                &sessions_dir,
                fields,
                &mut writer,
                &mut meta,
                &mut indexed_files,
                &mut indexed_docs,
                &mut skipped,
            )?;
        }
        let history = codex_root.join("history.jsonl");
        if history.exists() {
            index_codex_history_standalone(
                &history,
                fields,
                &mut writer,
                &mut meta,
                &mut indexed_files,
                &mut indexed_docs,
                &mut skipped,
            )?;
        }
    }

    // 4b. Purge documents for deleted source files
    let current_files = scan_source_files(config);
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

    if indexed_files == 0 && purged == 0 {
        tracing::debug!("auto-reindex: no changes after post-lock re-check");
        return Ok(());
    }

    // 5. Commit + atomic meta save (while still holding writer lock)
    writer.commit()?;
    save_meta(&config.meta_path, &meta)?;

    tracing::info!(
        "auto-reindex: indexed {} files ({} docs), skipped {} unchanged, purged {} deleted",
        indexed_files,
        indexed_docs,
        skipped,
        purged
    );
    Ok(())
}

/// Spawn the background reindex thread. Runs every `interval` seconds.
pub fn spawn_reindex_thread(
    index: Index,
    config: ReindexConfig,
    fields: FieldHandles,
    interval: Duration,
) {
    std::thread::Builder::new()
        .name("blackbox-reindex".into())
        .spawn(move || {
            tracing::info!(
                "background reindex thread started (interval: {:?})",
                interval
            );
            // First tick fires after a short delay to let the MCP handshake complete
            std::thread::sleep(Duration::from_secs(5));
            loop {
                if let Err(e) = try_background_reindex(&index, &config, fields) {
                    tracing::error!("background reindex failed: {:#}", e);
                }
                std::thread::sleep(interval);
            }
        })
        .expect("failed to spawn reindex thread");
}

// ── Standalone indexing functions (no &self — usable from background thread) ──

pub(super) fn event_to_doc_standalone(
    event: &parser::ParsedEvent,
    account: &str,
    file_path: &str,
    byte_offset: u64,
    is_subagent: bool,
    f: FieldHandles,
) -> TantivyDocument {
    let mut doc = TantivyDocument::new();
    doc.add_text(f.content, &event.content);
    doc.add_text(f.session_id, &event.session_id);
    doc.add_text(f.account, account);
    doc.add_text(f.project, event.cwd.as_deref().unwrap_or(""));
    doc.add_text(f.role, event.role.as_ref());
    doc.add_text(f.file_path, file_path);
    doc.add_u64(f.byte_offset, byte_offset);
    doc.add_u64(
        f.is_subagent,
        if event.is_subagent || is_subagent {
            1
        } else {
            0
        },
    );
    if let Some(ref ts) = event.timestamp {
        doc.add_text(f.timestamp, ts);
    }
    if let Some(ref branch) = event.git_branch {
        doc.add_text(f.git_branch, branch);
    }
    if let Some(ref slug) = event.agent_slug {
        doc.add_text(f.agent_slug, slug);
    }
    doc
}

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

// Many parameters, each already minimal (FieldHandles/account/writer/meta
// + three &mut u64 counters). Grouping the counters into a struct would
// obscure what's being mutated without adding type safety.
#[allow(clippy::too_many_arguments)]
pub(super) fn index_directory_standalone(
    dir: &Path,
    account_name: &str,
    f: FieldHandles,
    writer: &mut IndexWriter,
    meta: &mut HashMap<String, FileMeta>,
    indexed_files: &mut u64,
    indexed_docs: &mut u64,
    skipped: &mut u64,
) -> Result<()> {
    for entry in WalkDir::new(dir)
        .follow_links(true)
        .into_iter()
        .filter_map(|e| e.ok())
    {
        let path = entry.path();
        if path.extension().map(|e| e != "jsonl").unwrap_or(true) {
            continue;
        }

        let path_str = path.to_string_lossy().to_string();
        let file_meta = match entry.metadata() {
            Ok(m) => m,
            Err(_) => continue,
        };
        let mtime = match file_meta.modified() {
            Ok(t) => t.duration_since(UNIX_EPOCH).unwrap_or_default().as_secs(),
            Err(_) => continue,
        };

        if should_skip_file(&path_str, mtime, file_meta.len(), meta) {
            *skipped += 1;
            continue;
        }
        if meta.contains_key(&path_str) {
            writer.delete_term(Term::from_field_text(f.file_path, &path_str));
        }

        let is_subagent = path_str.contains("/subagents/");
        let project = extract_project_from_path(path, dir);

        let file = match fs::File::open(path) {
            Ok(f) => f,
            Err(_) => continue,
        };
        let reader = BufReader::new(file);
        let mut offset = 0u64;

        for line in reader.lines() {
            let line = match line {
                Ok(l) => l,
                Err(_) => continue,
            };
            let line_offset = offset;
            offset += line.len() as u64 + 1;

            for event in parser::parse_transcript_line(&line) {
                let is_sub = event.is_subagent || is_subagent;
                let proj = event.cwd.as_deref().unwrap_or(&project);

                let mut doc = TantivyDocument::new();
                doc.add_text(f.content, &event.content);
                doc.add_text(f.session_id, &event.session_id);
                doc.add_text(f.account, account_name);
                doc.add_text(f.project, proj);
                doc.add_text(f.role, event.role.as_ref());
                doc.add_text(f.file_path, &path_str);
                doc.add_u64(f.byte_offset, line_offset);
                doc.add_u64(f.is_subagent, if is_sub { 1 } else { 0 });
                if let Some(ref ts) = event.timestamp {
                    doc.add_text(f.timestamp, ts);
                }
                if let Some(ref branch) = event.git_branch {
                    doc.add_text(f.git_branch, branch);
                }
                if let Some(ref slug) = event.agent_slug {
                    doc.add_text(f.agent_slug, slug);
                }
                writer.add_document(doc)?;
                *indexed_docs += 1;
            }
        }

        meta.insert(
            path_str,
            FileMeta {
                mtime,
                size: file_meta.len(),
            },
        );
        *indexed_files += 1;
        if (*indexed_files).is_multiple_of(500) {
            tracing::info!("Indexed {} files ({} docs)...", indexed_files, indexed_docs);
            writer.commit()?;
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
pub(super) fn index_history_standalone(
    history: &Path,
    account_name: &str,
    f: FieldHandles,
    writer: &mut IndexWriter,
    meta: &mut HashMap<String, FileMeta>,
    indexed_files: &mut u64,
    indexed_docs: &mut u64,
    skipped: &mut u64,
) -> Result<()> {
    let path_str = history.to_string_lossy().to_string();
    let file_meta = fs::metadata(history)?;
    let mtime = file_meta.modified()?.duration_since(UNIX_EPOCH)?.as_secs();

    if should_skip_file(&path_str, mtime, file_meta.len(), meta) {
        *skipped += 1;
        return Ok(());
    }
    if meta.contains_key(&path_str) {
        writer.delete_term(Term::from_field_text(f.file_path, &path_str));
    }

    let file = fs::File::open(history)?;
    let reader = BufReader::new(file);
    let mut offset = 0u64;
    for line in reader.lines() {
        let line = match line {
            Ok(l) => l,
            Err(_) => continue,
        };
        let line_offset = offset;
        offset += line.len() as u64 + 1;
        for event in parser::parse_history_line(&line) {
            let doc =
                event_to_doc_standalone(&event, account_name, &path_str, line_offset, false, f);
            writer.add_document(doc)?;
            *indexed_docs += 1;
        }
    }
    meta.insert(
        path_str,
        FileMeta {
            mtime,
            size: file_meta.len(),
        },
    );
    *indexed_files += 1;
    Ok(())
}

pub(super) fn index_codex_directory_standalone(
    sessions_dir: &Path,
    f: FieldHandles,
    writer: &mut IndexWriter,
    meta: &mut HashMap<String, FileMeta>,
    indexed_files: &mut u64,
    indexed_docs: &mut u64,
    skipped: &mut u64,
) -> Result<()> {
    for entry in WalkDir::new(sessions_dir)
        .follow_links(true)
        .into_iter()
        .filter_map(|e| e.ok())
    {
        let path = entry.path();
        if path.extension().map(|e| e != "jsonl").unwrap_or(true) {
            continue;
        }

        let path_str = path.to_string_lossy().to_string();
        let file_meta = match entry.metadata() {
            Ok(m) => m,
            Err(_) => continue,
        };
        let mtime = match file_meta.modified() {
            Ok(t) => t.duration_since(UNIX_EPOCH).unwrap_or_default().as_secs(),
            Err(_) => continue,
        };

        if should_skip_file(&path_str, mtime, file_meta.len(), meta) {
            *skipped += 1;
            continue;
        }
        if meta.contains_key(&path_str) {
            writer.delete_term(Term::from_field_text(f.file_path, &path_str));
        }

        let session_id = extract_codex_session_id(path);
        let cwd = extract_codex_cwd(path);

        let file = match fs::File::open(path) {
            Ok(fl) => fl,
            Err(_) => continue,
        };
        let reader = BufReader::new(file);
        let mut offset = 0u64;

        for line in reader.lines() {
            let line = match line {
                Ok(l) => l,
                Err(_) => continue,
            };
            let line_offset = offset;
            offset += line.len() as u64 + 1;
            for mut event in parser::parse_codex_line(&line, &session_id) {
                if event.cwd.is_none() {
                    event.cwd = cwd.clone();
                }
                let doc =
                    event_to_doc_standalone(&event, "codex", &path_str, line_offset, false, f);
                writer.add_document(doc)?;
                *indexed_docs += 1;
            }
        }

        meta.insert(
            path_str,
            FileMeta {
                mtime,
                size: file_meta.len(),
            },
        );
        *indexed_files += 1;
        if (*indexed_files).is_multiple_of(500) {
            tracing::info!("Indexed {} files ({} docs)...", indexed_files, indexed_docs);
            writer.commit()?;
        }
    }
    Ok(())
}

pub(super) fn index_codex_history_standalone(
    history: &Path,
    f: FieldHandles,
    writer: &mut IndexWriter,
    meta: &mut HashMap<String, FileMeta>,
    indexed_files: &mut u64,
    indexed_docs: &mut u64,
    skipped: &mut u64,
) -> Result<()> {
    let path_str = history.to_string_lossy().to_string();
    let file_meta = fs::metadata(history)?;
    let mtime = file_meta.modified()?.duration_since(UNIX_EPOCH)?.as_secs();

    if should_skip_file(&path_str, mtime, file_meta.len(), meta) {
        *skipped += 1;
        return Ok(());
    }
    if meta.contains_key(&path_str) {
        writer.delete_term(Term::from_field_text(f.file_path, &path_str));
    }

    let file = fs::File::open(history)?;
    let reader = BufReader::new(file);
    let mut offset = 0u64;
    for line in reader.lines() {
        let line = match line {
            Ok(l) => l,
            Err(_) => continue,
        };
        let line_offset = offset;
        offset += line.len() as u64 + 1;
        for event in parser::parse_codex_history_line(&line) {
            let doc = event_to_doc_standalone(&event, "codex", &path_str, line_offset, false, f);
            writer.add_document(doc)?;
            *indexed_docs += 1;
        }
    }
    meta.insert(
        path_str,
        FileMeta {
            mtime,
            size: file_meta.len(),
        },
    );
    *indexed_files += 1;
    Ok(())
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
