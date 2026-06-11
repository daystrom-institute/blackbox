use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::Result;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tantivy::{IndexWriter, TantivyDocument, Term};

use super::{FieldHandles, FileMeta};
use crate::chunker::{Chunk, Edge, EdgeConfidence, EdgeProvenance};
use crate::entity_ref::{self, EntityRef};
use crate::git::GitCommit;
use bbox_corpus_core::project_record::ProjectRecord;

const MAX_COMMIT_MESSAGE_BYTES: usize = 16 * 1024;
const TRUNCATED_COMMIT_MESSAGE_SUFFIX: &str = "\n\n[... message truncated]";

#[derive(Debug, Default)]
pub(super) struct GitIndexStats {
    pub indexed_commits: u64,
    pub emitted_edges: u64,
}

pub(super) struct GitIndexContext<'a> {
    pub f: FieldHandles,
    pub writer: &'a mut IndexWriter,
    pub meta: &'a mut HashMap<String, FileMeta>,
    pub edges_dir: &'a Path,
    pub git_meta_dir: &'a Path,
    pub force_full: bool,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct GitIngestMeta {
    last_ingested_sha: Option<String>,
}

pub(super) fn git_source_key(project_id: &str) -> String {
    format!("git:{project_id}")
}

pub(super) fn git_meta_dir_from_projects_path(projects_path: &Path) -> PathBuf {
    projects_path
        .parent()
        .map(|parent| parent.join("git_meta"))
        .unwrap_or_else(|| PathBuf::from("git_meta"))
}

// executes inside the IndexWriterActor pass (sanctioned single-writer).
#[allow(clippy::disallowed_methods)]
pub(super) fn index_git_history_for_project(
    project: &ProjectRecord,
    root: &Path,
    project_chunks: &HashMap<String, EntityRef>,
    ctx: &mut GitIndexContext<'_>,
) -> Result<GitIndexStats> {
    let Some(repo_id) = project.repo_id.as_deref() else {
        return Ok(GitIndexStats::default());
    };
    let Some(head) = crate::git::current_head(root) else {
        return Ok(GitIndexStats::default());
    };
    let source_key = git_source_key(&project.project_id);
    let git_meta_path = ctx
        .git_meta_dir
        .join(format!("{}.json", project.project_id));
    let mut git_meta = if ctx.force_full {
        GitIngestMeta::default()
    } else {
        load_git_meta(&git_meta_path)?
    };
    if !ctx.force_full && git_meta.last_ingested_sha.as_deref() == Some(head.as_str()) {
        ctx.meta.insert(
            source_key,
            FileMeta {
                mtime: 0,
                size: crate::git::head_fingerprint(root).unwrap_or_default(),
                mat_version: None,
            },
        );
        return Ok(GitIndexStats::default());
    }

    let since = if ctx.force_full {
        None
    } else {
        git_meta.last_ingested_sha.as_deref()
    };
    let commits = crate::git::commit_log(root, since)?;
    if commits.is_empty() {
        git_meta.last_ingested_sha = Some(head);
        save_git_meta(&git_meta_path, &git_meta)?;
        ctx.meta.insert(
            source_key,
            FileMeta {
                mtime: 0,
                size: crate::git::head_fingerprint(root).unwrap_or_default(),
                mat_version: None,
            },
        );
        return Ok(GitIndexStats::default());
    }

    let mut edges = Vec::new();
    let mut stats = GitIndexStats::default();
    for commit in commits {
        let entity_id = commit_entity_id(repo_id, &commit.sha);
        ctx.writer
            .delete_term(Term::from_field_text(ctx.f.entity_id, &entity_id));
        ctx.writer
            .add_document(build_commit_doc(&commit, repo_id, project, ctx.f))?;
        super::embed_hook::emit_git_message(
            &entity_id,
            &commit_message_hash(&commit.message),
            &commit.message,
        );
        stats.indexed_commits += 1;
        edges.extend(commit_edges(root, repo_id, &commit, project_chunks)?);
    }
    stats.emitted_edges = edges.len() as u64;
    // Phase 2: git commit edges are materialized (Derived). Use managed
    // merge: full replacement on force_full, incremental deduped merge on
    // incremental ingest. Deferred: repo-scoped content-addressed lane keyed
    // by commit SHA.
    if ctx.force_full {
        crate::edge_index::replace_materialized_edges(
            ctx.edges_dir,
            "git",
            &project.project_id,
            &edges,
        )?;
    } else {
        crate::edge_index::merge_materialized_edges(
            ctx.edges_dir,
            "git",
            &project.project_id,
            &edges,
        )?;
    }
    git_meta.last_ingested_sha = Some(head);
    save_git_meta(&git_meta_path, &git_meta)?;
    ctx.meta.insert(
        source_key,
        FileMeta {
            mtime: 0,
            size: crate::git::head_fingerprint(root).unwrap_or_default(),
            mat_version: None,
        },
    );
    Ok(stats)
}

pub(crate) fn build_commit_doc(
    commit: &GitCommit,
    repo_id: &str,
    project: &ProjectRecord,
    f: FieldHandles,
) -> TantivyDocument {
    let entity_id = commit_entity_id(repo_id, &commit.sha);
    let message = indexable_commit_message(&commit.message);
    let mut doc = TantivyDocument::new();
    doc.add_text(f.doc_type, "commit");
    doc.add_text(f.chunk_kind, "git_message");
    doc.add_text(f.entity_id, &entity_id);
    doc.add_text(f.content, &message);
    // Mirror the commit subject into path_tokens so commits get the same
    // 1.5x boost as project-file path matches when the query mentions
    // subject keywords. Without this, the path_tokens boost asymmetrically
    // pushes project_file results above commits whose subject also matches.
    let subject = message.lines().next().unwrap_or("").trim();
    if !subject.is_empty() {
        doc.add_text(f.path_tokens, subject);
    }
    doc.add_text(f.chunk_hash, commit_message_hash(&message));
    doc.add_text(f.parser_version, entity_ref::PARSER_VERSION);
    doc.add_text(f.repo_id, repo_id);
    doc.add_text(f.commit_sha, &commit.sha);
    doc.add_text(f.commit_author_name, &commit.author_name);
    doc.add_text(f.commit_author_email, &commit.author_email);
    doc.add_text(f.session_id, "");
    doc.add_text(f.account, "git");
    doc.add_text(f.project, &project.canonical_path);
    doc.add_text(f.role, "commit");
    doc.add_text(f.file_path, git_source_key(&project.project_id));
    doc.add_u64(f.byte_offset, 0);
    doc.add_u64(f.is_subagent, 0);
    doc
}

fn commit_edges(
    root: &Path,
    repo_id: &str,
    commit: &GitCommit,
    project_chunks: &HashMap<String, EntityRef>,
) -> Result<Vec<Edge>> {
    let source = EntityRef::Commit {
        repo_id: repo_id.to_string(),
        sha: commit.sha.clone(),
    };
    let mut edges = Vec::new();
    for parent in &commit.parent_shas {
        edges.push(edge(
            source.clone(),
            "COMMIT_PARENT",
            EntityRef::Commit {
                repo_id: repo_id.to_string(),
                sha: parent.clone(),
            },
            EdgeConfidence::Exact,
        ));
    }
    for file in crate::git::changed_files_for_commit(root, &commit.sha)? {
        if let Some(target) = project_chunks.get(&file) {
            edges.push(edge(
                source.clone(),
                "COMMIT_TOUCHED_FILE",
                target.clone(),
                EdgeConfidence::Heuristic,
            ));
        }
    }
    Ok(edges)
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

fn commit_entity_id(repo_id: &str, sha: &str) -> String {
    EntityRef::Commit {
        repo_id: repo_id.to_string(),
        sha: sha.to_string(),
    }
    .to_string()
}

fn commit_message_hash(message: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(message.as_bytes());
    format!("{:x}", hasher.finalize())
}

fn indexable_commit_message(message: &str) -> String {
    if message.len() <= MAX_COMMIT_MESSAGE_BYTES {
        return message.to_string();
    }
    let content_limit = MAX_COMMIT_MESSAGE_BYTES - TRUNCATED_COMMIT_MESSAGE_SUFFIX.len();
    let mut end = content_limit;
    while !message.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}{}", &message[..end], TRUNCATED_COMMIT_MESSAGE_SUFFIX)
}

fn load_git_meta(path: &Path) -> Result<GitIngestMeta> {
    if !path.exists() {
        return Ok(GitIngestMeta::default());
    }
    Ok(serde_json::from_str(&fs::read_to_string(path)?)?)
}

fn save_git_meta(path: &Path, meta: &GitIngestMeta) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let tmp = path.with_file_name(format!(
        "{}.tmp",
        path.file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("git_meta.json")
    ));
    fs::write(&tmp, serde_json::to_vec(meta)?)?;
    fs::rename(tmp, path)?;
    Ok(())
}

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

pub(super) fn current_chunk_targets(
    chunks: &[Chunk],
    snapshot_id: Option<&str>,
) -> HashMap<String, EntityRef> {
    let mut targets = HashMap::new();
    for chunk in chunks {
        let rel = chunk.file_path.to_string_lossy().to_string();
        targets
            .entry(rel)
            .or_insert_with(|| chunk_ref(chunk, snapshot_id));
    }
    targets
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::path::Path;
    use std::process::Command;

    use super::*;
    use crate::index::{FileMeta, build_schema, register_code_tokenizer};

    fn project() -> ProjectRecord {
        ProjectRecord {
            project_id: "proj1234".into(),
            repo_id: Some("repo1234".into()),
            canonical_path: "/tmp/repo".into(),
            registered_at: "2026-05-05T17:30:00Z".into(),
            is_git_repo: true,
            languages: Default::default(),
        }
    }

    #[test]
    fn commit_doc_populates_agentic_fields() {
        let (_schema, fields) = build_schema();
        let commit = GitCommit {
            sha: "a".repeat(40),
            parent_shas: vec!["b".repeat(40)],
            author_name: "A".into(),
            author_email: "a@example.test".into(),
            message: "phase S4: EdgeIndex projection".into(),
        };
        let doc = build_commit_doc(&commit, "repo1234", &project(), fields);
        assert_eq!(text(&doc, fields.doc_type), "commit");
        assert_eq!(text(&doc, fields.chunk_kind), "git_message");
        assert_eq!(text(&doc, fields.repo_id), "repo1234");
        assert_eq!(text(&doc, fields.commit_sha), commit.sha);
        assert_eq!(text(&doc, fields.commit_author_name), "A");
        assert_eq!(text(&doc, fields.commit_author_email), "a@example.test");
        assert_eq!(
            text(&doc, fields.entity_id),
            format!("commit:repo1234:{}", commit.sha)
        );
    }

    #[test]
    fn current_chunk_targets_uses_relative_paths() {
        let chunk = Chunk {
            project_id: "proj1234".into(),
            file_path: Path::new("src/main.rs").to_path_buf(),
            rel_path_hash: "rel12345".into(),
            chunk_kind: "paragraph".into(),
            chunk_hash: "hash".into(),
            occurrence_idx: 0,
            language: None,
            symbol: None,
            symbol_exact: None,
            symbol_kind: None,
            parent_kind: None,
            line_start: None,
            line_end: None,
            content: "body".into(),
            byte_start: 0,
            byte_end: 4,
        };
        let targets = current_chunk_targets(&[chunk], None);
        assert!(targets.contains_key("src/main.rs"));
    }

    #[test]
    fn commit_doc_truncates_oversized_messages_before_hashing() {
        let (_schema, fields) = build_schema();
        let commit = GitCommit {
            sha: "a".repeat(40),
            parent_shas: Vec::new(),
            author_name: "A".into(),
            author_email: "a@example.test".into(),
            message: "x".repeat(20 * 1024),
        };
        let doc = build_commit_doc(&commit, "repo1234", &project(), fields);
        let content = text(&doc, fields.content);
        assert!(content.len() <= MAX_COMMIT_MESSAGE_BYTES);
        assert!(content.ends_with(TRUNCATED_COMMIT_MESSAGE_SUFFIX));
        assert_eq!(text(&doc, fields.chunk_hash), commit_message_hash(&content));
    }

    #[test]
    fn git_history_indexes_messages_and_parent_edges() {
        let repo = tempfile::tempdir().unwrap();
        run_git(repo.path(), &["init"]);
        run_git(repo.path(), &["config", "user.name", "Test User"]);
        run_git(repo.path(), &["config", "user.email", "test@example.test"]);
        fs::write(repo.path().join("README.md"), "one\n").unwrap();
        run_git(repo.path(), &["add", "README.md"]);
        run_git(
            repo.path(),
            &["commit", "-m", "initial searchable git ingestion fixture"],
        );
        fs::write(repo.path().join("README.md"), "two\n").unwrap();
        run_git(repo.path(), &["add", "README.md"]);
        run_git(
            repo.path(),
            &["commit", "-m", "second searchable git ingestion fixture"],
        );

        let state = tempfile::tempdir().unwrap();
        let (schema, fields) = build_schema();
        let index = tantivy::Index::create_in_ram(schema);
        register_code_tokenizer(&index);
        let mut writer = index.writer(50_000_000).unwrap();
        let project = ProjectRecord {
            project_id: "proj1234".into(),
            repo_id: crate::entity_ref::repo_id_for_path(repo.path()).ok(),
            canonical_path: repo.path().to_string_lossy().to_string(),
            registered_at: "2026-05-05T17:30:00Z".into(),
            is_git_repo: true,
            languages: Default::default(),
        };
        let mut meta: HashMap<String, FileMeta> = HashMap::new();
        let edges_dir = state.path().join("edges");
        let git_meta_dir = state.path().join("git_meta");
        let mut ctx = GitIndexContext {
            f: fields,
            writer: &mut writer,
            meta: &mut meta,
            edges_dir: &edges_dir,
            git_meta_dir: &git_meta_dir,
            force_full: true,
        };
        let stats = index_git_history_for_project(&project, repo.path(), &HashMap::new(), &mut ctx)
            .unwrap();
        assert_eq!(stats.indexed_commits, 2);
        writer.commit().unwrap();

        let reader = index.reader().unwrap();
        let searcher = reader.searcher();
        let parser = tantivy::query::QueryParser::for_index(&index, vec![fields.content]);
        let query = parser
            .parse_query("\"second searchable git ingestion fixture\"")
            .unwrap();
        let hits = searcher
            .search(&query, &tantivy::collector::TopDocs::with_limit(5))
            .unwrap();
        assert_eq!(hits.len(), 1);

        let sidecar = fs::read_to_string(
            state
                .path()
                .join("edges")
                .join("derived")
                .join("git")
                .join("proj1234.jsonl"),
        )
        .unwrap();
        assert!(sidecar.contains("COMMIT_PARENT"), "{sidecar}");
    }

    fn text(doc: &TantivyDocument, field: tantivy::schema::Field) -> String {
        doc.get_all(field)
            .next()
            .and_then(|value| match value {
                tantivy::schema::OwnedValue::Str(text) => Some(text.clone()),
                _ => None,
            })
            .unwrap_or_default()
    }

    fn run_git(root: &Path, args: &[&str]) {
        let output = Command::new("git")
            .arg("-C")
            .arg(root)
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git {:?} failed: {}",
            args,
            String::from_utf8_lossy(&output.stderr)
        );
    }
}
