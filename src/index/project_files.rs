use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use anyhow::{Context, Result};
use ignore::{DirEntry, WalkBuilder};
use sha2::{Digest, Sha256};
use tantivy::{IndexWriter, TantivyDocument, Term};

use super::{FieldHandles, FileMeta, ReindexConfig};
use crate::chunker::{self, Chunk, Edge, EdgeConfidence, EdgeProvenance};
use crate::entity_ref::{self, EntityRef};
use crate::projects::{ProjectRecord, ProjectRegistry};

const MAX_FILE_BYTES: u64 = 2 * 1024 * 1024;
const SKIP_DIRS: &[&str] = &["target", "node_modules", "_build", ".worktrees"];

#[derive(Debug, Default)]
pub(super) struct ProjectIndexStats {
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

pub(super) fn scan_registered_project_files(
    config: &ReindexConfig,
) -> Result<Vec<(String, u64, u64)>> {
    let mut files = Vec::new();
    for project in ProjectRegistry::load_records(&config.projects_path)? {
        let root = PathBuf::from(&project.canonical_path);
        scan_project_files(&root, &mut files)?;
        if project.repo_id.is_some() {
            if let Some(head) = crate::git::head_fingerprint(&root) {
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

pub(super) fn index_registered_projects_standalone(
    config: &ReindexConfig,
    f: FieldHandles,
    writer: &mut IndexWriter,
    meta: &mut HashMap<String, FileMeta>,
    force_git_full: bool,
) -> Result<ProjectIndexStats> {
    let mut stats = ProjectIndexStats::default();
    let edges_dir = crate::edge_index::edges_dir_from_projects_path(&config.projects_path);
    let git_meta_dir = super::git_history::git_meta_dir_from_projects_path(&config.projects_path);
    for project in ProjectRegistry::load_records(&config.projects_path)? {
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

pub(crate) fn build_project_file_doc(
    chunk: &Chunk,
    project: &ProjectRecord,
    absolute_path: &Path,
    commit_sha: Option<&str>,
    f: FieldHandles,
) -> TantivyDocument {
    let entity_id = crate::embed_queue::project_file_entity_id(chunk);
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
    doc.add_u64(f.is_subagent, 0);
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
    if let Some(repo_id) = &project.repo_id {
        doc.add_text(f.repo_id, repo_id);
    }
    if let Some(commit_sha) = commit_sha {
        doc.add_text(f.commit_sha, commit_sha);
    }
    doc
}

pub(crate) fn resolve_current_chunk_entity(
    project: &ProjectRecord,
    root: &Path,
    absolute_path: &Path,
    byte_range: Option<(u64, u64)>,
) -> Result<Option<EntityRef>> {
    let bytes = match fs::read(absolute_path) {
        Ok(bytes) => bytes,
        Err(_) => return Ok(None),
    };
    if is_binary(&bytes) {
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

fn index_project(
    project: &ProjectRecord,
    root: &Path,
    ctx: &mut ProjectIndexContext<'_>,
) -> Result<()> {
    let registry = chunker::default_registry();
    let commit_sha = crate::git::current_head(root);
    let mut files = Vec::new();
    let mut pending = Vec::new();
    let mut project_edges = Vec::new();
    scan_project_files(root, &mut files)?;
    for (path_str, mtime, size) in files {
        if let Some(prev) = ctx.meta.get(path_str.as_str()) {
            if prev.mtime == mtime && prev.size == size {
                ctx.stats.skipped += 1;
                continue;
            }
            ctx.writer
                .delete_term(Term::from_field_text(ctx.f.file_path, &path_str));
        }

        let path = PathBuf::from(&path_str);
        let bytes = match fs::read(&path) {
            Ok(bytes) => bytes,
            Err(err) => {
                tracing::warn!(path = %path.display(), error = %err, "failed to read project file");
                continue;
            }
        };
        if is_binary(&bytes) {
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
        let edges = derive_edges(&bounded_chunks, edges);
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

    let symbol_table = build_symbol_table(&pending);
    let mut current_chunk_targets = HashMap::new();
    for file in pending {
        let code_edges = derive_code_edges(&file.chunks, &symbol_table, ctx.stats);
        ctx.stats.emitted_edges += code_edges.len() as u64;
        project_edges.extend(code_edges);
        current_chunk_targets.extend(super::git_history::current_chunk_targets(&file.chunks));
        for chunk in file.chunks {
            let doc = build_project_file_doc(
                &chunk,
                project,
                &file.absolute_path,
                commit_sha.as_deref(),
                ctx.f,
            );
            let entity_id = crate::embed_queue::project_file_entity_id(&chunk);
            crate::embed_queue::enqueue_project_file(&chunk, &entity_id);
            ctx.writer.add_document(doc)?;
            ctx.stats.indexed_docs += 1;
        }
        ctx.meta.insert(
            file.path_str,
            FileMeta {
                mtime: file.mtime,
                size: file.size,
            },
        );
        ctx.stats.indexed_files += 1;
    }
    if ctx.force_git_full {
        crate::edge_index::replace_project_edges(
            ctx.edges_dir,
            "project",
            &project.project_id,
            &project_edges,
        )?;
    } else {
        crate::edge_index::append_project_edges(
            ctx.edges_dir,
            &project.project_id,
            &project_edges,
        )?;
    }
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
        if meta.len() > MAX_FILE_BYTES {
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
    entry
        .file_name()
        .to_str()
        .is_some_and(|name| SKIP_DIRS.contains(&name) || (name.starts_with('.') && name != ".bbox"))
}

fn is_supported_text_path(path: &Path) -> bool {
    matches!(
        path.extension().and_then(|ext| ext.to_str()),
        Some(
            "md" | "markdown" | "mdown" | "json" | "toml" | "yaml" | "yml" | "txt" | "text" | "log"
        )
    ) || crate::chunker::code::language_for_path(path).is_some()
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

fn derive_edges(chunks: &[Chunk], mut edges: Vec<Edge>) -> Vec<Edge> {
    for pair in chunks.windows(2) {
        edges.push(Edge {
            source: chunk_ref(&pair[0]),
            kind: "NEXT_SECTION".to_string(),
            target: chunk_ref(&pair[1]),
            provenance: EdgeProvenance::Derived,
            confidence: EdgeConfidence::Exact,
        });
    }
    // TODO(S4+): emit LINKS_TO_FILE/SECTION + EMBEDS_CODE_FENCE with resolved
    // targets when EdgeIndex provides chunk lookup by (file, hash).
    edges
}

fn build_symbol_table(files: &[PendingProjectFile]) -> HashMap<String, EntityRef> {
    let mut symbols = HashMap::new();
    for chunk in files.iter().flat_map(|file| file.chunks.iter()) {
        if chunk.chunk_kind != "code_block" {
            continue;
        }
        let Some(qualified_name) = &chunk.symbol else {
            continue;
        };
        let symbol = symbol_ref(chunk, qualified_name);
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
) -> Vec<Edge> {
    let mut edges = Vec::new();
    for chunk in chunks
        .iter()
        .filter(|chunk| chunk.chunk_kind == "code_block")
    {
        let file_ref = chunk_ref(chunk);
        if let Some(qualified_name) = &chunk.symbol {
            let symbol = symbol_ref(chunk, qualified_name);
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

fn symbol_ref(chunk: &Chunk, qualified_name: &str) -> EntityRef {
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

fn chunk_ref(chunk: &Chunk) -> EntityRef {
    EntityRef::ProjectFile {
        project_id: chunk.project_id.clone(),
        rel_path_hash: chunk.rel_path_hash.clone(),
        chunk_hash: chunk.chunk_hash.clone(),
        occurrence_idx: chunk.occurrence_idx,
    }
}

fn is_binary(bytes: &[u8]) -> bool {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chunker::SourceFormatChunker;
    use crate::index::build_schema;
    use tantivy::schema::Field;

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
    fn tier_a_call_edges_resolve_against_symbol_table() {
        let project = ProjectRecord {
            project_id: "proj1234".into(),
            repo_id: Some("repo1234".into()),
            canonical_path: "/tmp/repo".into(),
            registered_at: "2026-05-05T17:30:00Z".into(),
            is_git_repo: true,
            languages: Default::default(),
        };
        let chunks = finalize_chunks(
            &project,
            Path::new("src/lib.rs"),
            vec![
                crate::chunker::placeholder_chunk(
                    Path::new("src/lib.rs"),
                    "code_block",
                    Some("rust"),
                    "fn helper() {}",
                    0,
                    14,
                    0,
                ),
                crate::chunker::placeholder_chunk(
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
        let symbols = build_symbol_table(&pending);
        let mut stats = ProjectIndexStats::default();
        let edges = derive_code_edges(&pending[0].chunks, &symbols, &mut stats);
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
        };
        let chunks = finalize_chunks(
            &project,
            Path::new("src/lib.rs"),
            vec![
                crate::chunker::placeholder_chunk(
                    Path::new("src/lib.rs"),
                    "code_block",
                    Some("rust"),
                    "trait LocalTrait {}",
                    0,
                    19,
                    0,
                ),
                crate::chunker::placeholder_chunk(
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
        let symbols = build_symbol_table(&pending);
        let mut stats = ProjectIndexStats::default();
        let edges = derive_code_edges(&pending[0].chunks, &symbols, &mut stats);

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
        };
        let left = br#"
        {
          "b": 2,
          "a": { "z": true }
        }
        "#;
        let right = br#"{"a":{"z":true},"b":2}"#;

        let left_chunks = crate::chunker::config::JsonChunker
            .chunk(Path::new("config.json"), left)
            .unwrap()
            .0;
        let right_chunks = crate::chunker::config::JsonChunker
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
}
