use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
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
}

pub(super) fn scan_registered_project_files(
    config: &ReindexConfig,
) -> Result<Vec<(String, u64, u64)>> {
    let mut files = Vec::new();
    for project in ProjectRegistry::load_records(&config.projects_path)? {
        let root = PathBuf::from(&project.canonical_path);
        scan_project_files(&root, &mut files)?;
    }
    Ok(files)
}

pub(super) fn index_registered_projects_standalone(
    config: &ReindexConfig,
    f: FieldHandles,
    writer: &mut IndexWriter,
    meta: &mut HashMap<String, FileMeta>,
) -> Result<ProjectIndexStats> {
    let mut stats = ProjectIndexStats::default();
    for project in ProjectRegistry::load_records(&config.projects_path)? {
        let root = PathBuf::from(&project.canonical_path);
        if !root.exists() {
            continue;
        }
        index_project(&project, &root, f, writer, meta, &mut stats)?;
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
    let entity_id = EntityRef::ProjectFile {
        project_id: chunk.project_id.clone(),
        rel_path_hash: chunk.rel_path_hash.clone(),
        chunk_hash: chunk.chunk_hash.clone(),
        occurrence_idx: chunk.occurrence_idx,
    }
    .to_string();
    let mut doc = TantivyDocument::new();
    doc.add_text(f.doc_type, "project_file");
    doc.add_text(f.parser_version, entity_ref::PARSER_VERSION);
    doc.add_text(f.content, &chunk.content);
    doc.add_text(f.session_id, "");
    doc.add_text(f.account, "project_file");
    doc.add_text(f.project, &project.canonical_path);
    doc.add_text(f.role, "file");
    doc.add_text(f.file_path, absolute_path.to_string_lossy());
    doc.add_u64(f.byte_offset, chunk.byte_start);
    doc.add_u64(f.is_subagent, 0);
    doc.add_text(f.chunk_kind, &chunk.chunk_kind);
    doc.add_text(f.chunk_hash, &chunk.chunk_hash);
    doc.add_text(f.entity_id, &entity_id);
    if let Some(language) = &chunk.language {
        doc.add_text(f.language, language);
    }
    if let Some(repo_id) = &project.repo_id {
        doc.add_text(f.repo_id, repo_id);
    }
    if let Some(commit_sha) = commit_sha {
        doc.add_text(f.commit_sha, commit_sha);
    }
    doc
}

fn index_project(
    project: &ProjectRecord,
    root: &Path,
    f: FieldHandles,
    writer: &mut IndexWriter,
    meta: &mut HashMap<String, FileMeta>,
    stats: &mut ProjectIndexStats,
) -> Result<()> {
    let registry = chunker::default_registry();
    let commit_sha = current_head(root);
    let mut files = Vec::new();
    scan_project_files(root, &mut files)?;
    for (path_str, mtime, size) in files {
        if let Some(prev) = meta.get(path_str.as_str()) {
            if prev.mtime == mtime && prev.size == size {
                stats.skipped += 1;
                continue;
            }
            writer.delete_term(Term::from_field_text(f.file_path, &path_str));
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
            stats.skipped += 1;
            continue;
        }
        let sniff_len = bytes.len().min(4096);
        let Some(format) = registry
            .iter()
            .find(|chunker| chunker.claims(&path, &bytes[..sniff_len]))
        else {
            stats.skipped += 1;
            continue;
        };
        let (chunks, edges) = format
            .chunk(&path, &bytes)
            .with_context(|| format!("chunking {} as {}", path.display(), format.format_id()))?;
        let rel_path = path.strip_prefix(root).unwrap_or(&path);
        let chunks = finalize_chunks(project, rel_path, chunks);
        let edges = derive_edges(&chunks, edges);
        for bounded in bound_chunks(&chunks) {
            let doc = build_project_file_doc(&bounded, project, &path, commit_sha.as_deref(), f);
            writer.add_document(doc)?;
            stats.indexed_docs += 1;
        }
        stats.emitted_edges += edges.len() as u64;
        meta.insert(path_str, FileMeta { mtime, size });
        stats.indexed_files += 1;
    }
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
    )
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
    for chunk in chunks {
        if chunk.chunk_kind == "doc_section" {
            for kind in crate::chunker::markdown::markdown_edge_kinds(&chunk.content) {
                edges.push(Edge {
                    source: chunk_ref(chunk),
                    kind: kind.to_string(),
                    target: chunk_ref(chunk),
                    provenance: EdgeProvenance::Derived,
                    confidence: EdgeConfidence::Heuristic,
                });
            }
        }
    }
    edges
}

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

fn current_head(root: &Path) -> Option<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["rev-parse", "HEAD"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let sha = String::from_utf8(output.stdout).ok()?;
    let sha = sha.trim();
    if sha.is_empty() {
        None
    } else {
        Some(sha.to_string())
    }
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
        };
        let chunk = Chunk {
            project_id: "proj1234".into(),
            file_path: PathBuf::from("design/agentic-corpus.md"),
            rel_path_hash: "abcd1234".into(),
            chunk_kind: "doc_section".into(),
            chunk_hash: "f".repeat(64),
            occurrence_idx: 0,
            language: Some("md".into()),
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
