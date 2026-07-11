use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use anyhow::{Context, Result};
use ignore::{DirEntry, WalkBuilder};
use sha2::{Digest, Sha256};
use tantivy::{IndexWriter, TantivyDocument, Term};

use super::{FieldHandles, FileMeta, ReindexConfig};
use bbox_chunker::{self as chunker, Chunk, Edge, EdgeConfidence, EdgeProvenance};
use bbox_corpus_core::entity_ref::{self, EntityRef};
use bbox_corpus_core::project_record::{ProjectRecord, load_project_records};

const MAX_FILE_BYTES: u64 = 2 * 1024 * 1024;
/// Binary document containers (PDF, OOXML, spreadsheets) get a larger
/// byte budget: their size is dominated by embedded images/fonts, not
/// extractable text, so the 2 MiB text-file guard silently dropped
/// real-world documents (a 5 MB scanned-letterhead PDF can carry 20 KB
/// of text). The chunkers bound their own OUTPUT (per-sheet row/char
/// caps, per-page chunks, catch_unwind degradation), so the input budget
/// only guards parse cost, not index bloat.
const MAX_DOCUMENT_FILE_BYTES: u64 = 25 * 1024 * 1024;

fn max_bytes_for_path(path: &Path) -> u64 {
    match path
        .extension()
        .and_then(|ext| ext.to_str())
        .map(|ext| ext.to_ascii_lowercase())
        .as_deref()
    {
        Some("pdf" | "docx" | "pptx" | "xlsx" | "xlsm" | "xlam" | "xlsb" | "xls" | "ods") => {
            MAX_DOCUMENT_FILE_BYTES
        }
        _ => MAX_FILE_BYTES,
    }
}
const SKIP_DIRS: &[&str] = &["target", "node_modules", "_build", ".worktrees"];

#[derive(Debug, Default)]
pub struct ProjectIndexStats {
    pub indexed_files: u64,
    pub indexed_docs: u64,
    pub skipped: u64,
    pub emitted_edges: u64,
    pub indexed_commits: u64,
    pub call_edges: u64,
    pub resolved_call_edges: u64,
}

struct PendingProjectFile {
    path_str: String,
    absolute_path: PathBuf,
    mtime: u64,
    size: u64,
    chunks: Vec<Chunk>,
}

struct ProjectIndexContext<'a> {
    f: FieldHandles,
    writer: &'a mut IndexWriter,
    meta: &'a mut HashMap<String, FileMeta>,
    stats: &'a mut ProjectIndexStats,
    edges_dir: &'a Path,
    git_meta_dir: &'a Path,
    force_git_full: bool,
}

fn project_refs_v2_enabled() -> bool {
    std::env::var("BBOX_PROJECT_REFS_V2")
        .map(|value| matches!(value.as_str(), "1" | "true" | "TRUE" | "yes" | "YES"))
        .unwrap_or(false)
}

fn ref_snapshot_id(
    project: &ProjectRecord,
    root: &Path,
    files: &[(String, u64, u64)],
    commit_sha: Option<&str>,
) -> Option<String> {
    if !project_refs_v2_enabled() {
        return None;
    }
    if let (Some(repo_id), Some(head_sha)) = (project.repo_id.as_deref(), commit_sha) {
        return Some(bbox_edge_sidecar::snapshot::clean_snapshot_id(
            repo_id,
            &project.project_id,
            head_sha,
        ));
    }

    let mut hasher = Sha256::new();
    hasher.update(root.to_string_lossy().as_bytes());
    for (path, mtime, size) in files {
        hasher.update(path.as_bytes());
        hasher.update(mtime.to_le_bytes());
        hasher.update(size.to_le_bytes());
    }
    let fingerprint = hex::encode(hasher.finalize());
    Some(bbox_edge_sidecar::snapshot::nongit_snapshot_id(
        &project.project_id,
        &fingerprint,
    ))
}

pub fn scan_registered_project_files(config: &ReindexConfig) -> Result<Vec<(String, u64, u64)>> {
    let mut files = Vec::new();
    for project in load_project_records(&config.projects_path)? {
        let root = PathBuf::from(&project.canonical_path);
        scan_project_files(&root, &mut files)?;
        if project.repo_id.is_some() {
            if let Some(head) = bbox_corpus_core::git::head_fingerprint(&root) {
                files.push((
                    super::git_history::git_source_key(&project.project_id),
                    0,
                    head,
                ));
            }
        }
    }
    Ok(files)
}

pub fn index_registered_projects_standalone(
    config: &ReindexConfig,
    f: FieldHandles,
    writer: &mut IndexWriter,
    meta: &mut HashMap<String, FileMeta>,
    force_git_full: bool,
) -> Result<ProjectIndexStats> {
    let mut stats = ProjectIndexStats::default();
    let edges_dir =
        bbox_edge_sidecar::edge_sidecar::edges_dir_from_projects_path(&config.projects_path);
    let git_meta_dir = super::git_history::git_meta_dir_from_projects_path(&config.projects_path);
    for project in load_project_records(&config.projects_path)? {
        let root = PathBuf::from(&project.canonical_path);
        if !root.exists() {
            continue;
        }
        let mut ctx = ProjectIndexContext {
            f,
            writer,
            meta,
            stats: &mut stats,
            edges_dir: &edges_dir,
            git_meta_dir: &git_meta_dir,
            force_git_full,
        };
        index_project(&project, &root, &mut ctx)?;
    }
    Ok(stats)
}

pub fn build_project_file_doc(
    chunk: &Chunk,
    project: &ProjectRecord,
    absolute_path: &Path,
    commit_sha: Option<&str>,
    snapshot_id: Option<&str>,
    f: FieldHandles,
) -> TantivyDocument {
    let entity_id = super::embed_hook::project_file_entity_id_for_snapshot(chunk, snapshot_id);
    let mut doc = TantivyDocument::new();
    doc.add_text(f.doc_type, "project_file");
    doc.add_text(f.parser_version, entity_ref::PARSER_VERSION);
    doc.add_text(f.content, &chunk.content);
    if chunk.chunk_kind == "code_block" {
        doc.add_text(f.code_content, &chunk.content);
    }
    doc.add_text(f.session_id, "");
    doc.add_text(f.account, "project_file");
    doc.add_text(f.project, &project.canonical_path);
    doc.add_text(f.role, "file");
    let path_str = absolute_path.to_string_lossy();
    doc.add_text(f.file_path, &*path_str);
    // Reuse the same string for the tokenized path field; the code tokenizer
    // splits on `/`, `_`, `.`, etc., so /home/x/src/embed/voyage.rs becomes
    // tokens [home, x, src, embed, voyage, rs] available to BM25 ranking.
    doc.add_text(f.path_tokens, &*path_str);
    if let Some(symbol) = &chunk.symbol {
        // Symbol path also tokenized for BM25 boost — `Witness.Authority` →
        // [Witness, Authority] so symbol-named queries surface correctly.
        doc.add_text(f.path_tokens, symbol.as_str());
    }
    doc.add_u64(f.byte_offset, chunk.byte_start);
    doc.add_u64(f.byte_end, chunk.byte_end);
    if let Some(line_start) = chunk.line_start {
        doc.add_u64(f.line_start, line_start as u64);
    }
    if let Some(line_end) = chunk.line_end {
        doc.add_u64(f.line_end, line_end as u64);
    }
    doc.add_u64(f.is_subagent, 0);
    doc.add_text(f.project_id, &project.project_id);
    doc.add_text(f.chunk_kind, &chunk.chunk_kind);
    doc.add_text(f.chunk_hash, &chunk.chunk_hash);
    doc.add_text(f.entity_id, &entity_id);
    if let Some(language) = &chunk.language {
        doc.add_text(f.language, language);
    }
    if let Some(symbol) = &chunk.symbol {
        doc.add_text(f.symbol, symbol);
    }
    if let Some(symbol_exact) = &chunk.symbol_exact {
        doc.add_text(f.symbol_exact, symbol_exact);
    }
    if let Some(symbol_kind) = &chunk.symbol_kind {
        doc.add_text(f.symbol_kind, symbol_kind);
    }
    if let Some(parent_kind) = &chunk.parent_kind {
        doc.add_text(f.parent_kind, parent_kind);
    }
    if let Some(repo_id) = &project.repo_id {
        doc.add_text(f.repo_id, repo_id);
    }
    if let Some(commit_sha) = commit_sha {
        doc.add_text(f.commit_sha, commit_sha);
    }
    doc
}

pub fn resolve_current_chunk_entity(
    project: &ProjectRecord,
    root: &Path,
    absolute_path: &Path,
    byte_range: Option<(u64, u64)>,
) -> Result<Option<EntityRef>> {
    let bytes = match fs::read(absolute_path) {
        Ok(bytes) => bytes,
        Err(_) => return Ok(None),
    };
    if is_binary(absolute_path, &bytes) {
        return Ok(None);
    }
    let registry = chunker::default_registry();
    let sniff_len = bytes.len().min(4096);
    let Some(format) = registry
        .iter()
        .find(|chunker| chunker.claims(absolute_path, &bytes[..sniff_len]))
    else {
        return Ok(None);
    };
    let (chunks, _edges) = format.chunk(absolute_path, &bytes)?;
    let rel_path = absolute_path.strip_prefix(root).unwrap_or(absolute_path);
    let chunks = bound_chunks(&finalize_chunks(project, rel_path, chunks));
    let selected = byte_range
        .and_then(|(start, _end)| {
            chunks
                .iter()
                .find(|chunk| chunk.byte_start <= start && start <= chunk.byte_end)
        })
        .or_else(|| chunks.first());
    Ok(selected.map(|chunk| EntityRef::ProjectFile {
        project_id: chunk.project_id.clone(),
        rel_path_hash: chunk.rel_path_hash.clone(),
        chunk_hash: chunk.chunk_hash.clone(),
        occurrence_idx: chunk.occurrence_idx,
    }))
}

#[derive(Debug, PartialEq, Eq)]
enum ProjectFileAction {
    /// mtime+size+materialization version all match — leave as-is.
    Skip,
    /// mtime+size match but the stored version is unknown (pre-field entry):
    /// stamp the current version without re-chunking.
    AdoptVersion,
    /// New file, changed content, or a known-different materialization version
    /// (a real indexer/chunker/parser bump) — must re-chunk.
    Reindex,
}

/// Decide what to do with a scanned project file given its previously indexed
/// metadata. The version dimension forces a re-chunk after an
/// indexer/chunker/parser bump even when the file is byte-for-byte unchanged,
/// while an unknown stored version is adopted rather than re-chunked so that
/// introducing the field never triggers a full re-chunk of an already-current
/// corpus.
fn classify_project_file(
    prev: Option<&FileMeta>,
    mtime: u64,
    size: u64,
    mat_version: &str,
) -> ProjectFileAction {
    let Some(prev) = prev else {
        return ProjectFileAction::Reindex;
    };
    if prev.mtime != mtime || prev.size != size {
        return ProjectFileAction::Reindex;
    }
    match prev.mat_version.as_deref() {
        Some(v) if v == mat_version => ProjectFileAction::Skip,
        None => ProjectFileAction::AdoptVersion,
        Some(_) => ProjectFileAction::Reindex,
    }
}

fn index_project(
    project: &ProjectRecord,
    root: &Path,
    ctx: &mut ProjectIndexContext<'_>,
) -> Result<()> {
    let registry = chunker::default_registry();
    let commit_sha = bbox_corpus_core::git::current_head(root);
    let mut files = Vec::new();
    let mut pending = Vec::new();
    let mut project_edges = Vec::new();
    scan_project_files(root, &mut files)?;
    let snapshot_id = ref_snapshot_id(project, root, &files, commit_sha.as_deref());
    let mat_version = bbox_edge_sidecar::snapshot::current_materialization_version();
    // On-disk text-file set for this project, captured before `files` is moved.
    // Used to detect tracked-file deletions (in meta, absent on disk) so their
    // derived edges are purged rather than lingering in the materialized graph.
    let current_paths: std::collections::HashSet<String> =
        files.iter().map(|(p, _, _)| p.clone()).collect();
    for (path_str, mtime, size) in files {
        match classify_project_file(ctx.meta.get(path_str.as_str()), mtime, size, &mat_version) {
            ProjectFileAction::Skip => {
                ctx.stats.skipped += 1;
                continue;
            }
            ProjectFileAction::AdoptVersion => {
                // Identical content (mtime+size) but the stored materialization
                // version is unknown (entry predates this field). Stamp the
                // current version WITHOUT re-chunking: the deployed
                // indexer/chunker/parser is what produced these edges, so a
                // re-chunk would only reproduce byte-identical edges. Persisting
                // the stamp means a genuine *future* version bump records a
                // different version and falls through to Reindex. (To force a
                // true full re-chunk after a suspected past untracked bump, bump
                // INDEX_SCHEMA_VERSION or clear _meta.json.)
                ctx.meta.insert(
                    path_str,
                    FileMeta {
                        mtime,
                        size,
                        mat_version: Some(mat_version.clone()),
                    },
                );
                ctx.stats.skipped += 1;
                continue;
            }
            ProjectFileAction::Reindex => {
                if ctx.meta.contains_key(path_str.as_str()) {
                    ctx.writer
                        .delete_term(Term::from_field_text(ctx.f.file_path, &path_str));
                }
            }
        }

        let path = PathBuf::from(&path_str);
        let bytes = match fs::read(&path) {
            Ok(bytes) => bytes,
            Err(err) => {
                tracing::warn!(path = %path.display(), error = %err, "failed to read project file");
                continue;
            }
        };
        if is_binary(&path, &bytes) {
            ctx.stats.skipped += 1;
            continue;
        }
        let sniff_len = bytes.len().min(4096);
        let Some(format) = registry
            .iter()
            .find(|chunker| chunker.claims(&path, &bytes[..sniff_len]))
        else {
            ctx.stats.skipped += 1;
            continue;
        };
        let (chunks, edges) = format
            .chunk(&path, &bytes)
            .with_context(|| format!("chunking {} as {}", path.display(), format.format_id()))?;
        let rel_path = path.strip_prefix(root).unwrap_or(&path);
        let chunks = finalize_chunks(project, rel_path, chunks);
        let bounded_chunks = bound_chunks(&chunks);
        let edges = derive_edges(&bounded_chunks, edges, snapshot_id.as_deref());
        ctx.stats.emitted_edges += edges.len() as u64;
        project_edges.extend(edges);
        pending.push(PendingProjectFile {
            path_str,
            absolute_path: path,
            mtime,
            size,
            chunks: bounded_chunks,
        });
    }

    // Captured before `pending` is consumed below. Combined with the git
    // commit count after history indexing, this is the per-project signal that
    // lets `snapshot_after_reindex` skip re-materializing byte-identical edges.
    let files_changed = !pending.is_empty();
    let symbol_table = build_symbol_table(&pending, snapshot_id.as_deref());
    let mut current_chunk_targets = HashMap::new();
    for file in pending {
        let code_edges = derive_code_edges(
            &file.chunks,
            &symbol_table,
            ctx.stats,
            snapshot_id.as_deref(),
        );
        ctx.stats.emitted_edges += code_edges.len() as u64;
        project_edges.extend(code_edges);
        current_chunk_targets.extend(super::git_history::current_chunk_targets(
            &file.chunks,
            snapshot_id.as_deref(),
        ));
        for chunk in file.chunks {
            let doc = build_project_file_doc(
                &chunk,
                project,
                &file.absolute_path,
                commit_sha.as_deref(),
                snapshot_id.as_deref(),
                ctx.f,
            );
            let entity_id = super::embed_hook::project_file_entity_id_for_snapshot(
                &chunk,
                snapshot_id.as_deref(),
            );
            super::embed_hook::emit_project_file(&chunk, &entity_id);
            ctx.writer.add_document(doc)?;
            ctx.stats.indexed_docs += 1;
        }
        ctx.meta.insert(
            file.path_str,
            FileMeta {
                mtime: file.mtime,
                size: file.size,
                mat_version: Some(mat_version.clone()),
            },
        );
        ctx.stats.indexed_files += 1;
    }
    bbox_edge_sidecar::edge_sidecar::replace_materialized_edges_incremental(
        ctx.edges_dir,
        "project",
        &project.project_id,
        &project_edges,
    )?;
    let mut git_ctx = super::git_history::GitIndexContext {
        f: ctx.f,
        writer: ctx.writer,
        meta: ctx.meta,
        edges_dir: ctx.edges_dir,
        git_meta_dir: ctx.git_meta_dir,
        force_full: ctx.force_git_full,
    };
    let git_stats = super::git_history::index_git_history_for_project(
        project,
        root,
        &current_chunk_targets,
        &mut git_ctx,
    )?;
    ctx.stats.indexed_commits += git_stats.indexed_commits;
    ctx.stats.indexed_docs += git_stats.indexed_commits;
    if git_stats.indexed_commits > 0 {
        ctx.stats.indexed_files += 1;
    }
    ctx.stats.emitted_edges += git_stats.emitted_edges;
    if ctx.force_git_full {
        match bbox_edge_sidecar::edge_sidecar::compact_legacy_sidecar(
            ctx.edges_dir,
            &project.project_id,
            true,
        ) {
            Ok(stats) if stats.applied => {
                tracing::info!(
                    project_id = %project.project_id,
                    removed = stats.derived_edges_removed,
                    retained = stats.retained_lines,
                    bytes_before = stats.bytes_before,
                    bytes_after = stats.bytes_after,
                    "compacted legacy edge sidecar after full project refresh"
                );
            }
            Ok(_) => {}
            Err(err) => {
                tracing::warn!(
                    project_id = %project.project_id,
                    error = %err,
                    "failed to compact legacy edge sidecar after full project refresh"
                );
            }
        }
    }
    // Purge derived edges for tracked files that were deleted (or are no longer
    // indexable) this pass: present in meta under `root` but absent from the
    // current on-disk scan. The Tantivy docs are purged separately by the
    // reindex deletion sweep; without this, the file's file-anchored edges
    // (NEXT_SECTION / DEFINED_IN / CONTAINS_SYMBOL) survive in the materialized
    // graph. Matched by rel_path_hash, mirroring the incremental-replace
    // granularity; symbol→symbol edges (CALLS/USES_TYPE) carry no file ref and
    // age out with the snapshot id rather than being purged here.
    let deleted_rel_hashes: std::collections::HashSet<String> = ctx
        .meta
        .keys()
        .filter(|key| !current_paths.contains(key.as_str()))
        .filter_map(|key| {
            let rel = Path::new(key).strip_prefix(root).ok()?;
            Some(short_hash(rel.to_string_lossy().as_bytes()))
        })
        .collect();
    let deletions_purged = if deleted_rel_hashes.is_empty() {
        0
    } else {
        bbox_edge_sidecar::edge_sidecar::purge_managed_edges_for_path_hashes(
            ctx.edges_dir,
            "project",
            &project.project_id,
            &deleted_rel_hashes,
        )?
    };

    let materialization_changed =
        files_changed || git_stats.indexed_commits > 0 || deletions_purged > 0;
    snapshot_after_reindex(project, root, ctx.edges_dir, materialization_changed)?;
    Ok(())
}

fn scan_project_files(root: &Path, out: &mut Vec<(String, u64, u64)>) -> Result<()> {
    let walker = WalkBuilder::new(root)
        .hidden(false)
        .filter_entry(|entry| !is_skipped_entry(entry))
        .build();
    for entry in walker.filter_map(|entry| entry.ok()) {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        if !is_supported_text_path(path) {
            continue;
        }
        let meta = match entry.metadata() {
            Ok(meta) => meta,
            Err(_) => continue,
        };
        if meta.len() > max_bytes_for_path(path) {
            continue;
        }
        let mtime = meta
            .modified()
            .ok()
            .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
            .map(|duration| duration.as_secs())
            .unwrap_or_default();
        out.push((path.to_string_lossy().to_string(), mtime, meta.len()));
    }
    Ok(())
}

fn is_skipped_entry(entry: &DirEntry) -> bool {
    // `.bbox/` is blackbox's own control directory: project config, MCP wiring,
    // catalog-owned artifacts, and (per the repo-owned-project-state design)
    // structured knowledge owned by a dedicated spooler. It must NOT be pulled
    // into the generic project_file corpus, or its JSON/TOML/MD gets indexed as
    // project source — duplicating catalog/knowledge entities with confusing
    // search hits. Skip it like any other dotdir.
    entry
        .file_name()
        .to_str()
        .is_some_and(|name| SKIP_DIRS.contains(&name) || name.starts_with('.'))
}

fn is_supported_text_path(path: &Path) -> bool {
    matches!(
        path.extension().and_then(|ext| ext.to_str()),
        Some(
            "md" | "markdown"
                | "mdown"
                | "json"
                | "toml"
                | "yaml"
                | "yml"
                | "txt"
                | "text"
                | "log"
                | "pdf"
                | "ipynb"
                // X-XLSX (crates/bbox-chunker/src/xlsx.rs): the whole
                // spreadsheet family calamine's open_workbook_auto_from_rs
                // auto-detects, same reasoning as the chunker's own
                // extension gate.
                | "xlsx"
                | "xlsm"
                | "xlam"
                | "xlsb"
                | "xls"
                | "ods"
                | "docx"
                | "pptx"
                | "vtt"
                | "srt"
                // .html/.htm are already admitted below via
                // `code::language_for_path` (mapped to the "html"
                // tree-sitter grammar); .xhtml is not, so it is listed
                // explicitly here for the X-HTML chunker
                // (design/corpus/agentic-corpus/agentic-corpus-multimodal-chunkers.md).
                | "xhtml"
        )
    ) || bbox_chunker::code::language_for_path(path).is_some()
}

fn finalize_chunks(project: &ProjectRecord, rel_path: &Path, chunks: Vec<Chunk>) -> Vec<Chunk> {
    let rel_path_hash = short_hash(rel_path.to_string_lossy().as_bytes());
    chunks
        .into_iter()
        .enumerate()
        .map(|(idx, mut chunk)| {
            let chunk_hash = full_hash(chunk.content.as_bytes());
            chunk.project_id = project.project_id.clone();
            chunk.file_path = rel_path.to_path_buf();
            chunk.rel_path_hash.clone_from(&rel_path_hash);
            chunk.chunk_hash = chunk_hash;
            chunk.occurrence_idx = idx as u32;
            chunk
        })
        .collect()
}

fn split_oversized_chunk(chunk: &Chunk) -> Vec<Chunk> {
    if chunk.content.len() <= chunker::MAX_CHUNK_BYTES {
        return vec![chunk.clone()];
    }
    let mut out = Vec::new();
    let mut start = 0usize;
    while start < chunk.content.len() {
        let mut end = (start + chunker::MAX_CHUNK_BYTES).min(chunk.content.len());
        while !chunk.content.is_char_boundary(end) {
            end -= 1;
        }
        let content = chunk.content[start..end].to_string();
        let mut split = chunk.clone();
        split.content = content;
        split.byte_start = chunk.byte_start + start as u64;
        split.byte_end = chunk.byte_start + end as u64;
        split.chunk_hash = full_hash(split.content.as_bytes());
        split.occurrence_idx = out.len() as u32;
        out.push(split);
        start = end;
    }
    out
}

fn bound_chunks(chunks: &[Chunk]) -> Vec<Chunk> {
    chunks
        .iter()
        .flat_map(split_oversized_chunk)
        .enumerate()
        .map(|(idx, mut chunk)| {
            chunk.occurrence_idx = idx as u32;
            chunk
        })
        .collect()
}

fn derive_edges(chunks: &[Chunk], mut edges: Vec<Edge>, snapshot_id: Option<&str>) -> Vec<Edge> {
    for pair in chunks.windows(2) {
        edges.push(Edge {
            source: chunk_ref(&pair[0], snapshot_id),
            kind: "NEXT_SECTION".to_string(),
            target: chunk_ref(&pair[1], snapshot_id),
            provenance: EdgeProvenance::Derived,
            confidence: EdgeConfidence::Exact,
        });
    }
    // Markdown file/section link extraction is separate from storage lifecycle:
    // this pass indexes current project-file chunks and code-symbol edges.
    edges
}

fn build_symbol_table(
    files: &[PendingProjectFile],
    snapshot_id: Option<&str>,
) -> HashMap<String, EntityRef> {
    let mut symbols = HashMap::new();
    for chunk in files.iter().flat_map(|file| file.chunks.iter()) {
        if chunk.chunk_kind != "code_block" {
            continue;
        }
        let Some(qualified_name) = &chunk.symbol else {
            continue;
        };
        let symbol = symbol_ref(chunk, qualified_name, snapshot_id);
        symbols
            .entry(qualified_name.clone())
            .or_insert(symbol.clone());
        if let Some(bare) = &chunk.symbol_exact {
            symbols.entry(bare.clone()).or_insert(symbol);
        }
    }
    symbols
}

fn derive_code_edges(
    chunks: &[Chunk],
    symbols: &HashMap<String, EntityRef>,
    stats: &mut ProjectIndexStats,
    snapshot_id: Option<&str>,
) -> Vec<Edge> {
    let mut edges = Vec::new();
    for chunk in chunks
        .iter()
        .filter(|chunk| chunk.chunk_kind == "code_block")
    {
        let file_ref = chunk_ref(chunk, snapshot_id);
        if let Some(qualified_name) = &chunk.symbol {
            let symbol = symbol_ref(chunk, qualified_name, snapshot_id);
            edges.push(edge(
                symbol.clone(),
                "DEFINED_IN",
                file_ref.clone(),
                EdgeConfidence::Exact,
            ));
            edges.push(edge(
                file_ref.clone(),
                "CONTAINS_SYMBOL",
                symbol.clone(),
                EdgeConfidence::Exact,
            ));
            edges.extend(derive_has_field_edges(chunk, &symbol, symbols));
            edges.extend(derive_impl_trait_edges(chunk, &symbol, symbols));
            for callee in call_names(&chunk.content) {
                if let Some(target) = symbols.get(&callee) {
                    edges.push(edge(
                        symbol.clone(),
                        "CALLS",
                        target.clone(),
                        EdgeConfidence::Heuristic,
                    ));
                    stats.call_edges += 1;
                    stats.resolved_call_edges += 1;
                }
            }
            for type_name in type_names(&chunk.content) {
                if let Some(target) = symbols.get(&type_name) {
                    edges.push(edge(
                        symbol.clone(),
                        "USES_TYPE",
                        target.clone(),
                        EdgeConfidence::Heuristic,
                    ));
                }
            }
        }
    }
    edges
}

fn edge(source: EntityRef, kind: &str, target: EntityRef, confidence: EdgeConfidence) -> Edge {
    Edge {
        source,
        kind: kind.to_string(),
        target,
        provenance: EdgeProvenance::Derived,
        confidence,
    }
}

fn symbol_ref(chunk: &Chunk, qualified_name: &str, snapshot_id: Option<&str>) -> EntityRef {
    if let Some(snapshot_id) = snapshot_id {
        return EntityRef::SymbolV2 {
            project_id: chunk.project_id.clone(),
            snapshot_id: snapshot_id.to_string(),
            qualified_name: qualified_name.to_string(),
            defn_hash: chunk.chunk_hash.clone(),
        };
    }
    EntityRef::Symbol {
        project_id: chunk.project_id.clone(),
        qualified_name: qualified_name.to_string(),
        defn_hash: chunk.chunk_hash.clone(),
    }
}

fn resolve_symbol<'a>(
    symbols: &'a HashMap<String, EntityRef>,
    name: &str,
) -> Option<&'a EntityRef> {
    symbols.get(name).or_else(|| {
        name.rsplit_once("::")
            .and_then(|(_, bare)| symbols.get(bare))
            .or_else(|| {
                name.rsplit_once('.')
                    .and_then(|(_, bare)| symbols.get(bare))
            })
    })
}

fn derive_has_field_edges(
    chunk: &Chunk,
    source: &EntityRef,
    symbols: &HashMap<String, EntityRef>,
) -> Vec<Edge> {
    let Some(struct_name) = &chunk.symbol else {
        return Vec::new();
    };
    if !chunk.content.contains("struct ") {
        return Vec::new();
    }
    field_names(&chunk.content)
        .into_iter()
        .filter_map(|field| {
            let target = resolve_symbol(symbols, &format!("{struct_name}::{field}"))?;
            Some(edge(
                source.clone(),
                "HAS_FIELD",
                target.clone(),
                EdgeConfidence::Heuristic,
            ))
        })
        .collect()
}

fn derive_impl_trait_edges(
    chunk: &Chunk,
    source: &EntityRef,
    symbols: &HashMap<String, EntityRef>,
) -> Vec<Edge> {
    let header = chunk.content.split('{').next().unwrap_or_default().trim();
    let Some(rest) = header.strip_prefix("impl ") else {
        return Vec::new();
    };
    let Some((trait_name, _target)) = rest.split_once(" for ") else {
        return Vec::new();
    };
    let Some(target) = resolve_symbol(symbols, trait_name.trim()) else {
        return Vec::new();
    };
    vec![edge(
        source.clone(),
        "IMPLEMENTS_TRAIT",
        target.clone(),
        EdgeConfidence::Heuristic,
    )]
}

fn call_names(content: &str) -> Vec<String> {
    let call_pattern = regex::Regex::new(r"\b([A-Za-z_][A-Za-z0-9_]*)\s*\(").unwrap();
    call_pattern
        .captures_iter(content)
        .filter_map(|capture| capture.get(1).map(|name| name.as_str()))
        .filter(|name| !CALL_KEYWORDS.contains(name))
        .map(str::to_string)
        .collect()
}

fn type_names(content: &str) -> Vec<String> {
    let type_pattern = regex::Regex::new(r"\b([A-Z][A-Za-z0-9_]{2,})\b").unwrap();
    type_pattern
        .captures_iter(content)
        .filter_map(|capture| capture.get(1).map(|name| name.as_str().to_string()))
        .collect()
}

fn field_names(content: &str) -> Vec<String> {
    content
        .lines()
        .filter_map(|line| {
            let trimmed = line.trim();
            if trimmed.starts_with("//") || !trimmed.contains(':') {
                return None;
            }
            let left = trimmed.split(':').next()?.trim();
            let name = left.split_whitespace().last()?.trim_start_matches("pub ");
            if name
                .chars()
                .all(|ch| ch == '_' || ch.is_ascii_alphanumeric())
            {
                Some(name.to_string())
            } else {
                None
            }
        })
        .collect()
}

const CALL_KEYWORDS: &[&str] = &[
    "as",
    "assert",
    "async",
    "await",
    "break",
    "case",
    "catch",
    "const",
    "continue",
    "def",
    "defer",
    "delete",
    "do",
    "else",
    "finally",
    "fn",
    "for",
    "from",
    "function",
    "go",
    "if",
    "import",
    "in",
    "instanceof",
    "is",
    "lambda",
    "let",
    "loop",
    "match",
    "nameof",
    "new",
    "of",
    "raise",
    "return",
    "select",
    "sizeof",
    "switch",
    "then",
    "throw",
    "try",
    "typeof",
    "unless",
    "using",
    "var",
    "when",
    "where",
    "while",
    "with",
    "yield",
];

fn chunk_ref(chunk: &Chunk, snapshot_id: Option<&str>) -> EntityRef {
    if let Some(snapshot_id) = snapshot_id {
        return EntityRef::ProjectFileV2 {
            project_id: chunk.project_id.clone(),
            snapshot_id: snapshot_id.to_string(),
            rel_path_hash: chunk.rel_path_hash.clone(),
            chunk_hash: chunk.chunk_hash.clone(),
            occurrence_idx: chunk.occurrence_idx,
        };
    }
    EntityRef::ProjectFile {
        project_id: chunk.project_id.clone(),
        rel_path_hash: chunk.rel_path_hash.clone(),
        chunk_hash: chunk.chunk_hash.clone(),
        occurrence_idx: chunk.occurrence_idx,
    }
}

/// PDFs and spreadsheet workbooks are legitimately binary (embedded
/// streams, xref/trailer binary markers, font/image data for PDF; ZIP
/// central-directory/OLE2 CFB structure and compressed part data for
/// xlsx/xlsm/xlam/xlsb/xls/ods) and are expected to contain NUL bytes in
/// their first 4096 bytes; the null-byte heuristic below would otherwise
/// exclude nearly every real-world file of these formats before it ever
/// reaches the chunker registry's own `claims()` (magic-header) check.
/// `PdfChunker::claims` (crates/bbox-chunker/src/pdf.rs) and
/// `XlsxChunker::claims` (crates/bbox-chunker/src/xlsx.rs) are the real
/// gates for whether such a file's content is extractable, so the blanket
/// binary sniff is bypassed by extension here rather than tightened
/// generically.
fn is_binary(path: &Path, bytes: &[u8]) -> bool {
    if matches!(
        path.extension().and_then(|ext| ext.to_str()),
        Some("pdf" | "xlsx" | "xlsm" | "xlam" | "xlsb" | "xls" | "ods" | "docx" | "pptx")
    ) {
        return false;
    }
    bytes.iter().take(4096).any(|byte| *byte == 0)
}

fn full_hash(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hex::encode(hasher.finalize())
}

fn short_hash(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    let digest = hasher.finalize();
    hex::encode(&digest[..4])
}

/// Returns true iff the on-disk materialization for `project_id` already
/// reflects the current HEAD, indexer/chunker version, and worktree dirty state
/// — i.e. re-running `switch_to_*` would reproduce byte-identical edges and only
/// churn mtimes. Any inconsistency (cold start, version bump, branch switch,
/// dirty↔clean drift, GC'd active path) returns false so we materialize as today.
///
/// Gates on the `ManifestIndex` (the loader authority via
/// `active_materialized_paths`), not `WorkspaceManifest`, whose `dirty`/
/// `dirty_fingerprint` fields have no runtime reader and may drift on
/// metadata-only changes (`git add`, same-HEAD branch relabel).
fn materialization_is_current(
    edges_dir: &Path,
    project_id: &str,
    repo_id: &str,
    head_sha: &str,
    worktree_dirty: bool,
) -> bool {
    let Ok(idx) = bbox_edge_sidecar::manifest::ManifestIndex::load(edges_dir) else {
        // No manifest-index yet (cold start / never materialized) ⇒ materialize.
        return false;
    };
    let Some(entry) = idx.workspaces.get(project_id) else {
        return false;
    };

    // Version-aware expected snapshot for the current HEAD. `clean_snapshot_id`
    // folds INDEXER_VERSION/CHUNKER_VERSION, so a version bump with unchanged
    // mtimes yields a different id ⇒ mismatch ⇒ not skipped.
    let expected_snap =
        bbox_edge_sidecar::snapshot::clean_snapshot_id(repo_id, project_id, head_sha);
    let expected_snap_rel =
        bbox_edge_sidecar::snapshot::active_snapshot_rel(project_id, &expected_snap);
    if entry.active_snapshot.as_deref() != Some(expected_snap_rel.as_str()) {
        return false;
    }

    // Dirty-state consistency across three sources: current worktree, the
    // ManifestIndex overlay pointer, and the overlay dir on disk. Any
    // disagreement forces re-materialization (e.g. a clean checkout that left a
    // stale overlay, which `switch_to_clean_snapshot` must clear).
    let overlay_rel = bbox_edge_sidecar::snapshot::dirty_overlay_rel(project_id);
    let overlay_in_manifest = entry.dirty_overlay.as_deref() == Some(overlay_rel.as_str());
    let overlay_on_disk =
        bbox_edge_sidecar::snapshot::dirty_overlay_dir(edges_dir, project_id).is_dir();
    if worktree_dirty {
        if !overlay_in_manifest || !overlay_on_disk {
            return false;
        }
    } else {
        if entry.dirty_overlay.is_some() || overlay_on_disk {
            return false;
        }
        // Clean ⇒ the active snapshot dir is what the loader reads; it must
        // exist. `active_materialized_paths` silently drops missing dirs, so a
        // GC between passes would lose this project from the graph if we skipped.
        if !bbox_edge_sidecar::snapshot::snapshot_dir(edges_dir, project_id, &expected_snap)
            .is_dir()
        {
            return false;
        }
    }

    true
}

fn snapshot_after_reindex(
    project: &ProjectRecord,
    root: &Path,
    edges_dir: &Path,
    materialization_changed: bool,
) -> Result<()> {
    let Some(repo_id) = project.repo_id.as_deref() else {
        return Ok(());
    };
    let Some(head_sha) = bbox_corpus_core::git::current_head(root) else {
        return Ok(());
    };
    let branch = bbox_corpus_core::git::current_branch(root);
    let worktree_dirty = bbox_corpus_core::git::is_worktree_dirty(root);

    // Writer-side materialization idempotency. Re-running `switch_to_*` rewrites
    // the dirty overlay via temp-dir + atomic rename, which stamps fresh mtimes
    // on `dirty-current/*.jsonl`. The edge-index rebuild watcher sums sidecar
    // mtimes, so a byte-identical re-materialization still trips a full 18-21s
    // EdgeIndex rebuild. When this pass changed nothing for the project and the
    // on-disk materialization already matches the current head/version/worktree
    // state, skip it. Correctness rests on: derived overlay/snapshot edge content
    // is a deterministic function of (head_sha, changed-file set + contents). No
    // re-chunked file (empty `pending`) and no indexed commit ⇒ identical edges.
    if !materialization_changed
        && materialization_is_current(
            edges_dir,
            &project.project_id,
            repo_id,
            &head_sha,
            worktree_dirty,
        )
    {
        return Ok(());
    }

    let project_edges = bbox_edge_sidecar::edge_sidecar::read_managed_derived_edges(
        edges_dir,
        "project",
        &project.project_id,
    )?;
    let git_edges = bbox_edge_sidecar::edge_sidecar::read_managed_derived_edges(
        edges_dir,
        "git",
        &project.project_id,
    )?;

    let snapshot_edges: Vec<bbox_edge_sidecar::edge_sidecar::Edge> = project_edges
        .iter()
        .map(|e| bbox_edge_sidecar::edge_sidecar::Edge {
            source: e.source.clone(),
            kind: e.kind.clone(),
            target: e.target.clone(),
            provenance: e.provenance,
            confidence: e.confidence,
            metadata: Default::default(),
        })
        .collect();
    let git_snapshot_edges: Vec<bbox_edge_sidecar::edge_sidecar::Edge> = git_edges
        .iter()
        .map(|e| bbox_edge_sidecar::edge_sidecar::Edge {
            source: e.source.clone(),
            kind: e.kind.clone(),
            target: e.target.clone(),
            provenance: e.provenance,
            confidence: e.confidence,
            metadata: Default::default(),
        })
        .collect();

    if worktree_dirty {
        let fp = bbox_corpus_core::git::dirty_fingerprint(root).unwrap_or_default();
        bbox_edge_sidecar::snapshot::switch_to_dirty_overlay(
            edges_dir,
            &project.project_id,
            repo_id,
            branch.as_deref(),
            &head_sha,
            &fp,
            snapshot_edges,
            vec![],
            git_snapshot_edges,
        )?;
    } else {
        bbox_edge_sidecar::snapshot::switch_to_clean_snapshot(
            edges_dir,
            &project.project_id,
            repo_id,
            branch.as_deref(),
            &head_sha,
            snapshot_edges,
            vec![],
            git_snapshot_edges,
        )?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::build_schema;
    use bbox_chunker::SourceFormatChunker;
    use tantivy::schema::Field;

    #[test]
    fn document_containers_get_the_larger_byte_budget() {
        use std::path::Path;
        assert_eq!(
            max_bytes_for_path(Path::new("deck.pdf")),
            MAX_DOCUMENT_FILE_BYTES
        );
        assert_eq!(
            max_bytes_for_path(Path::new("Board.DOCX")),
            MAX_DOCUMENT_FILE_BYTES
        );
        assert_eq!(max_bytes_for_path(Path::new("main.rs")), MAX_FILE_BYTES);
        assert_eq!(max_bytes_for_path(Path::new("notes.md")), MAX_FILE_BYTES);
    }

    #[test]
    fn project_file_doc_includes_agentic_fields() {
        let (_schema, fields) = build_schema();
        let project = ProjectRecord {
            project_id: "proj1234".into(),
            repo_id: Some("repo1234".into()),
            canonical_path: "/tmp/repo".into(),
            registered_at: "2026-05-05T17:30:00Z".into(),
            is_git_repo: true,
            languages: Default::default(),
            aliases: Default::default(),
        };
        let chunk = Chunk {
            project_id: "proj1234".into(),
            file_path: PathBuf::from("design/agentic-corpus.md"),
            rel_path_hash: "abcd1234".into(),
            chunk_kind: "doc_section".into(),
            chunk_hash: "f".repeat(64),
            occurrence_idx: 0,
            language: Some("md".into()),
            symbol: None,
            symbol_exact: None,
            symbol_kind: None,
            parent_kind: None,
            line_start: None,
            line_end: None,
            content: "agentic-corpus design".into(),
            byte_start: 10,
            byte_end: 32,
        };

        let commit_sha = "a".repeat(40);
        let doc = build_project_file_doc(
            &chunk,
            &project,
            Path::new("/tmp/repo/design/agentic-corpus.md"),
            Some(commit_sha.as_str()),
            None,
            fields,
        );

        assert_eq!(first_text(&doc, fields.doc_type), "project_file");
        assert_eq!(first_text(&doc, fields.chunk_kind), "doc_section");
        assert_eq!(first_text(&doc, fields.language), "md");
        assert_eq!(first_text(&doc, fields.repo_id), "repo1234");
        assert_eq!(
            first_text(&doc, fields.entity_id),
            format!("project_file:proj1234:abcd1234:{}:0", "f".repeat(64))
        );
    }

    #[test]
    fn project_file_doc_can_emit_snapshot_specific_entity_id() {
        let (_schema, fields) = build_schema();
        let project = ProjectRecord {
            project_id: "proj1234".into(),
            repo_id: Some("repo1234".into()),
            canonical_path: "/tmp/repo".into(),
            registered_at: "2026-05-05T17:30:00Z".into(),
            is_git_repo: true,
            languages: Default::default(),
            aliases: Default::default(),
        };
        let chunk = Chunk {
            project_id: "proj1234".into(),
            file_path: PathBuf::from("src/lib.rs"),
            rel_path_hash: "abcd1234".into(),
            chunk_kind: "code_block".into(),
            chunk_hash: "f".repeat(64),
            occurrence_idx: 3,
            language: Some("rust".into()),
            symbol: Some("helper".into()),
            symbol_exact: Some("crate::helper".into()),
            symbol_kind: Some("function".into()),
            parent_kind: None,
            line_start: Some(1),
            line_end: Some(4),
            content: "fn helper() {}".into(),
            byte_start: 0,
            byte_end: 14,
        };

        let commit_sha = "a".repeat(40);
        let doc = build_project_file_doc(
            &chunk,
            &project,
            Path::new("/tmp/repo/src/lib.rs"),
            Some(commit_sha.as_str()),
            Some("head-repo1234-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"),
            fields,
        );

        assert_eq!(
            first_text(&doc, fields.entity_id),
            format!(
                "project_file_v2:proj1234:head-repo1234-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa:abcd1234:{}:3",
                "f".repeat(64)
            )
        );
    }

    #[test]
    fn tier_a_call_edges_resolve_against_symbol_table() {
        let project = ProjectRecord {
            project_id: "proj1234".into(),
            repo_id: Some("repo1234".into()),
            canonical_path: "/tmp/repo".into(),
            registered_at: "2026-05-05T17:30:00Z".into(),
            is_git_repo: true,
            languages: Default::default(),
            aliases: Default::default(),
        };
        let chunks = finalize_chunks(
            &project,
            Path::new("src/lib.rs"),
            vec![
                bbox_chunker::placeholder_chunk(
                    Path::new("src/lib.rs"),
                    "code_block",
                    Some("rust"),
                    "fn helper() {}",
                    0,
                    14,
                    0,
                ),
                bbox_chunker::placeholder_chunk(
                    Path::new("src/lib.rs"),
                    "code_block",
                    Some("rust"),
                    "fn caller() { helper(); }",
                    15,
                    39,
                    1,
                ),
            ],
        )
        .into_iter()
        .enumerate()
        .map(|(idx, mut chunk)| {
            if idx == 0 {
                chunk.symbol = Some("helper".into());
                chunk.symbol_exact = Some("helper".into());
            } else {
                chunk.symbol = Some("caller".into());
                chunk.symbol_exact = Some("caller".into());
            }
            chunk
        })
        .collect::<Vec<_>>();
        let pending = vec![PendingProjectFile {
            path_str: "/tmp/repo/src/lib.rs".into(),
            absolute_path: PathBuf::from("/tmp/repo/src/lib.rs"),
            mtime: 1,
            size: 39,
            chunks,
        }];
        let symbols = build_symbol_table(&pending, None);
        let mut stats = ProjectIndexStats::default();
        let edges = derive_code_edges(&pending[0].chunks, &symbols, &mut stats, None);
        assert!(edges.iter().any(|edge| edge.kind == "CALLS"));
        assert!(stats.call_edges >= 1);
        assert_eq!(stats.resolved_call_edges, stats.call_edges);
    }

    #[test]
    fn tier_a_edges_skip_external_symbol_targets() {
        let project = ProjectRecord {
            project_id: "proj1234".into(),
            repo_id: Some("repo1234".into()),
            canonical_path: "/tmp/repo".into(),
            registered_at: "2026-05-05T17:30:00Z".into(),
            is_git_repo: true,
            languages: Default::default(),
            aliases: Default::default(),
        };
        let chunks = finalize_chunks(
            &project,
            Path::new("src/lib.rs"),
            vec![
                bbox_chunker::placeholder_chunk(
                    Path::new("src/lib.rs"),
                    "code_block",
                    Some("rust"),
                    "trait LocalTrait {}",
                    0,
                    19,
                    0,
                ),
                bbox_chunker::placeholder_chunk(
                    Path::new("src/lib.rs"),
                    "code_block",
                    Some("rust"),
                    "impl LocalTrait for Thing {}\nuse std::fmt::Display;",
                    20,
                    72,
                    1,
                ),
            ],
        )
        .into_iter()
        .enumerate()
        .map(|(idx, mut chunk)| {
            if idx == 0 {
                chunk.symbol = Some("LocalTrait".into());
                chunk.symbol_exact = Some("LocalTrait".into());
            } else {
                chunk.symbol = Some("Thing::impl".into());
                chunk.symbol_exact = Some("impl".into());
            }
            chunk
        })
        .collect::<Vec<_>>();
        let pending = vec![PendingProjectFile {
            path_str: "/tmp/repo/src/lib.rs".into(),
            absolute_path: PathBuf::from("/tmp/repo/src/lib.rs"),
            mtime: 1,
            size: 72,
            chunks,
        }];
        let symbols = build_symbol_table(&pending, None);
        let mut stats = ProjectIndexStats::default();
        let edges = derive_code_edges(&pending[0].chunks, &symbols, &mut stats, None);

        assert!(edges.iter().any(|edge| edge.kind == "IMPLEMENTS_TRAIT"));
        assert!(!edges.iter().any(|edge| edge.kind == "IMPORTS"));
    }

    #[test]
    fn call_names_skip_flow_control_keywords() {
        let names = call_names("if (cond) { foo(); }");

        assert!(!names.iter().any(|name| name == "if"));
        assert!(names.iter().any(|name| name == "foo"));
    }

    #[test]
    fn json_chunk_hashes_survive_noncanonical_formatting() {
        let project = ProjectRecord {
            project_id: "proj1234".into(),
            repo_id: Some("repo1234".into()),
            canonical_path: "/tmp/repo".into(),
            registered_at: "2026-05-05T17:30:00Z".into(),
            is_git_repo: true,
            languages: Default::default(),
            aliases: Default::default(),
        };
        let left = br#"
        {
          "b": 2,
          "a": { "z": true }
        }
        "#;
        let right = br#"{"a":{"z":true},"b":2}"#;

        let left_chunks = bbox_chunker::config::JsonChunker
            .chunk(Path::new("config.json"), left)
            .unwrap()
            .0;
        let right_chunks = bbox_chunker::config::JsonChunker
            .chunk(Path::new("config.json"), right)
            .unwrap()
            .0;
        let left_chunks = finalize_chunks(&project, Path::new("config.json"), left_chunks);
        let right_chunks = finalize_chunks(&project, Path::new("config.json"), right_chunks);
        let left_hashes = left_chunks
            .iter()
            .map(|chunk| (chunk.content.clone(), chunk.chunk_hash.clone()))
            .collect::<Vec<_>>();
        let right_hashes = right_chunks
            .iter()
            .map(|chunk| (chunk.content.clone(), chunk.chunk_hash.clone()))
            .collect::<Vec<_>>();

        assert_eq!(left_hashes, right_hashes);
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

    // --- materialization idempotency guard (issue #2 follow-up) ---
    //
    // These exercise `materialization_is_current`, the decision behind skipping a
    // no-op `snapshot_after_reindex`. Skipping when it returns true is what keeps
    // a byte-identical re-materialization from re-stamping overlay mtimes and
    // tripping the edge-index rebuild watcher; the "force" cases guard against
    // skipping when the on-disk graph would actually go stale.

    const MAT_REPO: &str = "repo-mat";
    const MAT_PROJ: &str = "proj-mat";
    const MAT_HEAD: &str = "abc123def456";

    fn mat_edge(id: &str, target: &str) -> bbox_edge_sidecar::edge_sidecar::Edge {
        bbox_edge_sidecar::edge_sidecar::Edge {
            source: EntityRef::Knowledge { id: id.into() },
            kind: "DESCRIBES".into(),
            target: EntityRef::Knowledge { id: target.into() },
            provenance: EdgeProvenance::Derived,
            confidence: EdgeConfidence::Exact,
            metadata: Default::default(),
        }
    }

    fn seed_clean(edges_dir: &Path) {
        bbox_edge_sidecar::snapshot::switch_to_clean_snapshot(
            edges_dir,
            MAT_PROJ,
            MAT_REPO,
            Some("main"),
            MAT_HEAD,
            vec![mat_edge("k1", "k2")],
            vec![],
            vec![],
        )
        .unwrap();
    }

    fn seed_dirty(edges_dir: &Path) {
        bbox_edge_sidecar::snapshot::switch_to_dirty_overlay(
            edges_dir,
            MAT_PROJ,
            MAT_REPO,
            Some("main"),
            MAT_HEAD,
            "fp-dirty",
            vec![mat_edge("k_dirty", "k2")],
            vec![],
            vec![],
        )
        .unwrap();
    }

    #[test]
    fn classify_project_file_covers_skip_adopt_reindex() {
        let v = bbox_edge_sidecar::snapshot::current_materialization_version();
        let current = FileMeta {
            mtime: 100,
            size: 200,
            mat_version: Some(v.clone()),
        };
        // Fully current → skip.
        assert_eq!(
            classify_project_file(Some(&current), 100, 200, &v),
            ProjectFileAction::Skip
        );
        // mtime or size drift → reindex.
        assert_eq!(
            classify_project_file(Some(&current), 101, 200, &v),
            ProjectFileAction::Reindex
        );
        assert_eq!(
            classify_project_file(Some(&current), 100, 201, &v),
            ProjectFileAction::Reindex
        );
        // Known-different version with identical content → reindex (real bump).
        let stale = FileMeta {
            mtime: 100,
            size: 200,
            mat_version: Some("older-version".into()),
        };
        assert_eq!(
            classify_project_file(Some(&stale), 100, 200, &v),
            ProjectFileAction::Reindex
        );
        // Unknown stored version with identical content → adopt, NOT reindex.
        // This is what keeps introducing the field from forcing a full re-chunk.
        let legacy = FileMeta {
            mtime: 100,
            size: 200,
            mat_version: None,
        };
        assert_eq!(
            classify_project_file(Some(&legacy), 100, 200, &v),
            ProjectFileAction::AdoptVersion
        );
        // Unknown version but content drift → reindex (content wins).
        assert_eq!(
            classify_project_file(Some(&legacy), 101, 200, &v),
            ProjectFileAction::Reindex
        );
        // Never-seen file → reindex.
        assert_eq!(
            classify_project_file(None, 100, 200, &v),
            ProjectFileAction::Reindex
        );
    }

    #[test]
    fn materialization_cold_start_forces_rematerialize() {
        let dir = tempfile::tempdir().unwrap();
        // No manifest-index on disk yet (never materialized).
        assert!(!materialization_is_current(
            dir.path(),
            MAT_PROJ,
            MAT_REPO,
            MAT_HEAD,
            false
        ));
    }

    #[test]
    fn materialization_clean_steady_state_skips() {
        let dir = tempfile::tempdir().unwrap();
        seed_clean(dir.path());
        assert!(materialization_is_current(
            dir.path(),
            MAT_PROJ,
            MAT_REPO,
            MAT_HEAD,
            false
        ));
    }

    #[test]
    fn materialization_dirty_steady_state_skips() {
        let dir = tempfile::tempdir().unwrap();
        seed_dirty(dir.path());
        assert!(materialization_is_current(
            dir.path(),
            MAT_PROJ,
            MAT_REPO,
            MAT_HEAD,
            true
        ));
    }

    #[test]
    fn materialization_head_or_version_change_forces_rematerialize() {
        // A different HEAD — and equivalently an INDEXER/CHUNKER_VERSION bump,
        // since both feed the hashed snapshot_id — must re-materialize even when
        // file mtimes are unchanged.
        let dir = tempfile::tempdir().unwrap();
        seed_clean(dir.path());
        assert!(!materialization_is_current(
            dir.path(),
            MAT_PROJ,
            MAT_REPO,
            "deadbeefcafe",
            false
        ));
    }

    #[test]
    fn materialization_clean_with_stale_overlay_forces_rematerialize() {
        // Worktree is clean now but a dirty overlay is still active; the clean
        // switch must run to clear it, so skipping would leave the stale overlay.
        let dir = tempfile::tempdir().unwrap();
        seed_dirty(dir.path());
        assert!(bbox_edge_sidecar::snapshot::dirty_overlay_dir(dir.path(), MAT_PROJ).is_dir());
        assert!(!materialization_is_current(
            dir.path(),
            MAT_PROJ,
            MAT_REPO,
            MAT_HEAD,
            false
        ));
    }

    #[test]
    fn materialization_dirty_without_overlay_forces_rematerialize() {
        // Worktree just went dirty but only a clean snapshot is materialized.
        let dir = tempfile::tempdir().unwrap();
        seed_clean(dir.path());
        assert!(!materialization_is_current(
            dir.path(),
            MAT_PROJ,
            MAT_REPO,
            MAT_HEAD,
            true
        ));
    }

    #[test]
    fn materialization_missing_active_snapshot_dir_forces_rematerialize() {
        // Manifest still references a snapshot dir that has been GC'd off disk.
        // active_materialized_paths drops missing dirs silently, so re-materialize.
        let dir = tempfile::tempdir().unwrap();
        seed_clean(dir.path());
        let snap_id = bbox_edge_sidecar::snapshot::clean_snapshot_id(MAT_REPO, MAT_PROJ, MAT_HEAD);
        let snap = bbox_edge_sidecar::snapshot::snapshot_dir(dir.path(), MAT_PROJ, &snap_id);
        std::fs::remove_dir_all(&snap).unwrap();
        assert!(!materialization_is_current(
            dir.path(),
            MAT_PROJ,
            MAT_REPO,
            MAT_HEAD,
            false
        ));
    }

    #[test]
    fn scan_skips_bbox_control_dir() {
        use std::fs;
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();

        // Normal project source — must be indexed.
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(root.join("src/main.rs"), "fn main() {}").unwrap();
        fs::write(root.join("README.md"), "# project").unwrap();

        // blackbox control dir — must NOT be indexed (config, MCP wiring,
        // catalog-owned artifacts, and future structured knowledge live here,
        // owned by other subsystems).
        fs::create_dir_all(root.join(".bbox/knowledge")).unwrap();
        fs::write(root.join(".bbox/config.toml"), "x = 1").unwrap();
        fs::write(root.join(".bbox/mcp.json"), "{}").unwrap();
        fs::write(root.join(".bbox/knowledge/entry.json"), "{}").unwrap();

        // Another dotdir — already skipped; sanity anchor.
        fs::create_dir_all(root.join(".git")).unwrap();
        fs::write(root.join(".git/config"), "[core]").unwrap();

        let mut out = Vec::new();
        scan_project_files(root, &mut out).unwrap();
        let indexed: Vec<String> = out.iter().map(|(p, _, _)| p.clone()).collect();

        assert!(
            indexed.iter().any(|p| p.ends_with("src/main.rs")),
            "normal source should be indexed: {indexed:?}"
        );
        assert!(
            indexed.iter().any(|p| p.ends_with("README.md")),
            "top-level markdown should be indexed: {indexed:?}"
        );
        assert!(
            indexed.iter().all(|p| !p.contains("/.bbox/")),
            ".bbox control dir must be excluded from project_file indexing: {indexed:?}"
        );
    }

    #[test]
    fn html_and_xhtml_files_are_admitted_and_claimed_by_html_chunker_not_code_chunker() {
        use std::fs;
        // `code::language_for_path` maps .html/.htm to the "html" tree-sitter
        // grammar, so `CodeChunker::claims` also matches those extensions
        // (verified live: `ts_language_for_name("html")` resolves via
        // tree-sitter-language-pack). `HtmlChunker` MUST be registered
        // before `CodeChunker` in `chunker::default_registry()` for the
        // registry's first-match `find()` (see `index_project` /
        // `resolve_current_chunk_entity` above) to route .html/.htm/.xhtml
        // through prose sectioning rather than code-symbol extraction. This
        // guards that ordering at the registry-integration level, not just
        // inside bbox-chunker's own unit tests.
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::write(
            root.join("page.html"),
            "<html><body><h1>Title</h1><p>hello</p></body></html>",
        )
        .unwrap();
        fs::write(root.join("frag.xhtml"), "<p>fragment</p>").unwrap();

        let mut out = Vec::new();
        scan_project_files(root, &mut out).unwrap();
        let indexed: Vec<String> = out.iter().map(|(p, _, _)| p.clone()).collect();
        assert!(
            indexed.iter().any(|p| p.ends_with("page.html")),
            ".html must be admitted by the project-file walker: {indexed:?}"
        );
        assert!(
            indexed.iter().any(|p| p.ends_with("frag.xhtml")),
            ".xhtml must be admitted by the project-file walker: {indexed:?}"
        );

        let registry = chunker::default_registry();
        let bytes = fs::read(root.join("page.html")).unwrap();
        let sniff_len = bytes.len().min(4096);
        let claimed = registry
            .iter()
            .find(|c| c.claims(Path::new("page.html"), &bytes[..sniff_len]))
            .expect("some chunker must claim page.html");
        assert_eq!(
            claimed.format_id(),
            "html",
            "HtmlChunker must win the registry claim over CodeChunker for .html"
        );

        let (chunks, _edges) = claimed.chunk(Path::new("page.html"), &bytes).unwrap();
        assert!(
            chunks.iter().all(|chunk| chunk.chunk_kind == "web_section"),
            "expected web_section chunks, got {chunks:?}"
        );
    }
}
