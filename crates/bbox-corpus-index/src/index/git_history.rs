use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::Result;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tantivy::{IndexWriter, TantivyDocument, Term};

use super::{FieldHandles, FileMeta};
use bbox_chunker::{Chunk, Edge, EdgeConfidence, EdgeProvenance};
use bbox_corpus_core::entity_ref::{self, EntityRef};
use bbox_corpus_core::git::GitCommit;
use bbox_corpus_core::project_record::ProjectRecord;

const MAX_COMMIT_MESSAGE_BYTES: usize = 16 * 1024;
/// Suffix stamped on a commit message that exceeded [`MAX_COMMIT_MESSAGE_BYTES`].
///
/// Public because the history generation store detects truncated messages by
/// this exact marker; a duplicated copy there would drift silently.
pub const TRUNCATED_COMMIT_MESSAGE_SUFFIX: &str = "\n\n[... message truncated]";

#[derive(Debug, Default)]
pub struct GitIndexStats {
    pub indexed_commits: u64,
    pub emitted_edges: u64,
}

pub struct GitIndexContext<'a> {
    pub f: FieldHandles,
    pub writer: &'a mut IndexWriter,
    pub meta: &'a mut HashMap<String, FileMeta>,
    pub edges_dir: &'a Path,
    pub git_meta_dir: &'a Path,
    pub force_full: bool,
    pub publication: &'a mut super::project_files::ProjectIndexPublicationBundle,
    /// The identity's display name. P3-E: a commit document's `project` field
    /// drops `canonical_path` for this value while its namespace/sha identity
    /// (`repo_id`, `commit_sha`, `entity_id`) stays byte-identical.
    pub project_display: &'a str,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct GitIngestMeta {
    last_ingested_sha: Option<String>,
}

#[derive(Debug)]
pub(crate) struct GitHistoryPublication {
    edges_dir: PathBuf,
    project_id: String,
    edges: Vec<Edge>,
    force_full: bool,
    git_meta_path: PathBuf,
    git_meta: GitIngestMeta,
}

impl GitHistoryPublication {
    pub(crate) fn publish(self) -> Result<()> {
        if self.force_full {
            bbox_edge_sidecar::edge_sidecar::replace_materialized_edges(
                &self.edges_dir,
                "git",
                &self.project_id,
                &self.edges,
            )?;
        } else {
            bbox_edge_sidecar::edge_sidecar::merge_materialized_edges(
                &self.edges_dir,
                "git",
                &self.project_id,
                &self.edges,
            )?;
        }
        save_git_meta(&self.git_meta_path, &self.git_meta)
    }
}

pub fn git_source_key(project_id: &str) -> String {
    format!("git:{project_id}")
}

pub fn git_meta_dir_from_projects_path(projects_path: &Path) -> PathBuf {
    projects_path
        .parent()
        .map(|parent| parent.join("git_meta"))
        .unwrap_or_else(|| PathBuf::from("git_meta"))
}

// executes inside the IndexWriterActor pass (sanctioned single-writer).
#[allow(clippy::disallowed_methods)]
pub fn index_git_history_for_project(
    project: &ProjectRecord,
    root: &Path,
    project_chunks: &HashMap<String, EntityRef>,
    ctx: &mut GitIndexContext<'_>,
) -> Result<GitIndexStats> {
    let Some(repo_id) = project.repo_id.as_deref() else {
        return Ok(GitIndexStats::default());
    };
    let Some(head) = bbox_corpus_core::git::current_head(root) else {
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
                size: bbox_corpus_core::git::head_fingerprint(root).unwrap_or_default(),
                mat_version: None,
                source: Default::default(),
            },
        );
        return Ok(GitIndexStats::default());
    }

    let since = if ctx.force_full {
        None
    } else {
        git_meta.last_ingested_sha.as_deref()
    };
    let commits = bbox_corpus_core::git::commit_log(root, since)?;
    if commits.is_empty() {
        git_meta.last_ingested_sha = Some(head);
        ctx.publication.stage_git_history(GitHistoryPublication {
            edges_dir: ctx.edges_dir.to_path_buf(),
            project_id: project.project_id.clone(),
            edges: Vec::new(),
            force_full: ctx.force_full,
            git_meta_path,
            git_meta,
        });
        ctx.meta.insert(
            source_key,
            FileMeta {
                mtime: 0,
                size: bbox_corpus_core::git::head_fingerprint(root).unwrap_or_default(),
                mat_version: None,
                source: Default::default(),
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
        ctx.writer.add_document(build_commit_doc(
            &commit,
            repo_id,
            &project.project_id,
            ctx.project_display,
            ctx.f,
        ))?;
        super::embed_hook::emit_git_message(
            &entity_id,
            &commit_message_hash(&commit.message),
            &commit.message,
        );
        stats.indexed_commits += 1;
        edges.extend(commit_edges(root, repo_id, &commit, project_chunks)?);
    }
    stats.emitted_edges = edges.len() as u64;
    // Stage the managed Git sidecar and ingest cursor together. The daemon
    // publishes both only inside its final checkout-authority fence.
    git_meta.last_ingested_sha = Some(head);
    ctx.publication.stage_git_history(GitHistoryPublication {
        edges_dir: ctx.edges_dir.to_path_buf(),
        project_id: project.project_id.clone(),
        edges,
        force_full: ctx.force_full,
        git_meta_path,
        git_meta,
    });
    ctx.meta.insert(
        source_key,
        FileMeta {
            mtime: 0,
            size: bbox_corpus_core::git::head_fingerprint(root).unwrap_or_default(),
            mat_version: None,
            source: Default::default(),
        },
    );
    Ok(stats)
}

pub fn build_commit_doc(
    commit: &GitCommit,
    repo_id: &str,
    project_id: &str,
    project_display: &str,
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
    let subject = commit_subject_tokens(&message);
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
    doc.add_text(f.project, project_display);
    doc.add_text(f.role, "commit");
    // Not a filesystem path: the per-project history source key, which is the
    // delete term the purge loops' absolute-path arm uses for this lane.
    doc.add_text(f.file_path, git_source_key(project_id));
    doc.add_u64(f.byte_offset, 0);
    doc.add_u64(f.is_subagent, 0);
    doc
}

/// Join a project-relative path onto its scope's `bbox_root_relpath`,
/// yielding the path Git reports for that file.
///
/// The two directions of this mapping are the whole reason monorepo
/// consolidation is possible: per-project staging owns project-relative
/// paths, Git owns repo-relative ones, and `bbox_root_relpath` is the only
/// bridge. Keeping both directions beside each other is what makes a
/// mismatch a compile-adjacent edit rather than a silent no-match.
pub fn repo_relative_path_for_scope(bbox_root_relpath: &str, project_relative: &str) -> String {
    if bbox_root_relpath == "." {
        return project_relative.to_string();
    }
    format!("{bbox_root_relpath}/{project_relative}")
}

/// Inverse of [`repo_relative_path_for_scope`]: the project-relative path for
/// a repo-relative one, or `None` when the file lies outside this project's
/// subtree.
///
/// `None` is the load-bearing arm for consolidated ingestion. One walk sees
/// every changed path in the repository, and a sibling project must emit NO
/// edge for a path outside its own root; returning `None` rather than a
/// best-effort guess is what keeps a monorepo sibling's edges out of its
/// neighbour's graph.
pub fn project_relative_path_within_scope(
    bbox_root_relpath: &str,
    repo_relative: &str,
) -> Option<String> {
    if bbox_root_relpath == "." {
        return Some(repo_relative.to_string());
    }
    // The separator check is what rejects `crates/foo-extra/x.rs` for the
    // scope `crates/foo`: a bare `starts_with` on the prefix would accept it.
    let rest = repo_relative.strip_prefix(bbox_root_relpath)?;
    let rest = rest.strip_prefix('/')?;
    if rest.is_empty() {
        return None;
    }
    Some(rest.to_string())
}

/// The repo-level edges one commit contributes: its parent chain.
///
/// Repo-level by construction - both endpoints are `Commit` refs carrying a
/// namespace and a sha and no project at all - which is why consolidated
/// ingestion may materialize the identical set under every member project
/// without creating cross-project contamination or an ordering dependency.
pub fn commit_parent_edges(repo_id: &str, commit: &GitCommit) -> Vec<Edge> {
    let source = EntityRef::Commit {
        repo_id: repo_id.to_string(),
        sha: commit.sha.clone(),
    };
    commit
        .parent_shas
        .iter()
        .map(|parent| {
            edge(
                source.clone(),
                "COMMIT_PARENT",
                EntityRef::Commit {
                    repo_id: repo_id.to_string(),
                    sha: parent.clone(),
                },
                EdgeConfidence::Exact,
            )
        })
        .collect()
}

/// The `COMMIT_TOUCHED_FILE` edges one commit contributes to ONE project,
/// given the repo-relative paths the commit changed.
///
/// Emits nothing for a path outside the project's `bbox_root_relpath`, and
/// nothing for a path the project's active generation does not currently
/// chunk (a deleted file, a binary, an excluded extension).
pub fn commit_touched_file_edges(
    repo_id: &str,
    commit: &GitCommit,
    bbox_root_relpath: &str,
    changed_repo_paths: &[String],
    project_chunks: &HashMap<String, EntityRef>,
) -> Vec<Edge> {
    let source = EntityRef::Commit {
        repo_id: repo_id.to_string(),
        sha: commit.sha.clone(),
    };
    let mut edges = Vec::new();
    for repo_path in changed_repo_paths {
        let Some(project_path) = project_relative_path_within_scope(bbox_root_relpath, repo_path)
        else {
            continue;
        };
        if let Some(target) = project_chunks.get(&project_path) {
            edges.push(edge(
                source.clone(),
                "COMMIT_TOUCHED_FILE",
                target.clone(),
                EdgeConfidence::Heuristic,
            ));
        }
    }
    edges
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
    for file in bbox_corpus_core::git::changed_files_for_commit(root, &commit.sha)? {
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

/// The subject line mirrored into `path_tokens` so a commit whose subject
/// matches gets the same boost a project-file path match does. Shared with
/// the generation-row builder so the stored row and the tantivy document can
/// never disagree about what the field holds.
pub fn commit_subject_tokens(indexed_message: &str) -> &str {
    indexed_message.lines().next().unwrap_or("").trim()
}

pub fn commit_entity_id(repo_id: &str, sha: &str) -> String {
    EntityRef::Commit {
        repo_id: repo_id.to_string(),
        sha: sha.to_string(),
    }
    .to_string()
}

/// SHA-256 over the exact bytes handed in.
///
/// CALLER CONTRACT: the commit document hashes the TRUNCATED message while
/// the vector enqueue hashes the RAW one. That divergence is deliberate and
/// pre-existing; the two hashes are never compared to each other, and any
/// code that starts comparing them is asserting an equality that does not
/// hold for an oversized commit message.
pub fn commit_message_hash(message: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(message.as_bytes());
    format!("{:x}", hasher.finalize())
}

/// The commit-message text as it is actually indexed: the raw message, or a
/// UTF-8-boundary-safe prefix stamped with [`TRUNCATED_COMMIT_MESSAGE_SUFFIX`]
/// when it exceeds the size cap.
pub fn indexable_commit_message(message: &str) -> String {
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

pub fn current_chunk_targets(
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
            aliases: Default::default(),
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
        let doc = build_commit_doc(
            &commit,
            "repo1234",
            &project().project_id,
            "test-display",
            fields,
        );
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
            visual_payload: None,
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
        let doc = build_commit_doc(
            &commit,
            "repo1234",
            &project().project_id,
            "test-display",
            fields,
        );
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
            repo_id: bbox_corpus_core::entity_ref::repo_id_for_path(repo.path()).ok(),
            canonical_path: repo.path().to_string_lossy().to_string(),
            registered_at: "2026-05-05T17:30:00Z".into(),
            is_git_repo: true,
            languages: Default::default(),
            aliases: Default::default(),
        };
        let mut meta: HashMap<String, FileMeta> = HashMap::new();
        let edges_dir = state.path().join("edges");
        let git_meta_dir = state.path().join("git_meta");
        let mut publication = crate::index::project_files::ProjectIndexPublicationBundle::default();
        let mut ctx = GitIndexContext {
            f: fields,
            writer: &mut writer,
            meta: &mut meta,
            edges_dir: &edges_dir,
            git_meta_dir: &git_meta_dir,
            force_full: true,
            publication: &mut publication,
            project_display: "test-display",
        };
        let stats = index_git_history_for_project(&project, repo.path(), &HashMap::new(), &mut ctx)
            .unwrap();
        drop(ctx);
        assert_eq!(stats.indexed_commits, 2);
        assert!(
            !git_meta_dir.join("proj1234.json").exists(),
            "Git ingest metadata must remain staged until final publication"
        );
        assert!(
            bbox_edge_sidecar::edge_sidecar::read_managed_derived_edges(
                &edges_dir, "git", "proj1234",
            )
            .unwrap()
            .is_empty(),
            "Git sidecars must remain unchanged until final publication"
        );
        let publication_result = publication.publish().unwrap();
        assert!(git_meta_dir.join("proj1234.json").exists());
        writer.commit().unwrap();
        publication_result.finalize_publications();

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
