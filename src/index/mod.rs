use std::fs;
use std::path::{Path, PathBuf};
use std::time::Instant;

use anyhow::Result;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use tantivy::schema::*;
use tantivy::{Index, IndexReader, ReloadPolicy};

pub const INDEX_SCHEMA_VERSION: &str = "agentic-corpus-f3";
const SCHEMA_VERSION_FILE: &str = "schema_version.txt";

/// Metadata about an indexed file, for incremental updates.
#[derive(Serialize, Deserialize)]
pub(super) struct FileMeta {
    pub(super) mtime: u64,
    pub(super) size: u64,
}

/// Field handles extracted for sharing with the background reindex thread.
/// All fields are `Copy` — they're just integer indices into the schema.
#[derive(Clone, Copy)]
pub struct FieldHandles {
    pub content: Field,
    pub session_id: Field,
    pub account: Field,
    pub project: Field,
    pub role: Field,
    pub timestamp: Field,
    pub file_path: Field,
    pub byte_offset: Field,
    pub git_branch: Field,
    pub is_subagent: Field,
    pub agent_slug: Field,
    pub doc_type: Field,
    #[allow(dead_code)]
    pub chunk_kind: Field,
    #[allow(dead_code)]
    pub language: Field,
    #[allow(dead_code)]
    pub symbol: Field,
    #[allow(dead_code)]
    pub symbol_exact: Field,
    #[allow(dead_code)]
    pub code_content: Field,
    #[allow(dead_code)]
    pub chunk_hash: Field,
    #[allow(dead_code)]
    pub entity_id: Field,
    pub parser_version: Field,
    #[allow(dead_code)]
    pub commit_sha: Field,
    #[allow(dead_code)]
    pub repo_id: Field,
}

/// Config needed by the background reindex thread.
#[derive(Clone)]
pub struct ReindexConfig {
    pub roots: Vec<(String, PathBuf)>,
    pub codex_root: Option<PathBuf>,
    pub meta_path: PathBuf,
    pub projects_path: PathBuf,
}

pub struct TranscriptIndex {
    index: Index,
    reader: IndexReader,
    #[allow(dead_code)]
    schema: Schema,
    fields: FieldHandles,
    config: ReindexConfig,
    /// TTL cache for `stats()` output. The expensive part of stats is
    /// walking every account's `projects/` tree — dominates the call
    /// time for a corpus of any size. Wrapped in an inner Mutex so
    /// stats() can mutate it through a shared `&TranscriptIndex`
    /// (the whole struct is already behind RwLock in SharedState).
    pub(super) stats_cache: Mutex<Option<(Instant, String)>>,
}

impl TranscriptIndex {
    pub fn open_or_create(
        index_path: &Path,
        roots: Vec<(String, PathBuf)>,
        codex_root: Option<PathBuf>,
        projects_path: PathBuf,
    ) -> Result<Self> {
        reset_index_on_schema_mismatch(index_path)?;
        let meta_path = index_path.join("_meta.json");

        let (schema, fields) = build_schema();

        fs::create_dir_all(index_path)?;

        // Try opening existing index, fall back to creating new
        let index = match Index::open_in_dir(index_path) {
            Ok(idx) => {
                tracing::info!("Opened existing index at {}", index_path.display());
                idx
            }
            Err(_) => {
                tracing::info!("Creating new index at {}", index_path.display());
                Index::create_in_dir(index_path, schema.clone())?
            }
        };
        write_schema_version_marker(index_path)?;

        let reader = index
            .reader_builder()
            .reload_policy(ReloadPolicy::OnCommitWithDelay)
            .try_into()?;

        let config = ReindexConfig {
            roots,
            codex_root,
            meta_path,
            projects_path,
        };

        Ok(Self {
            index,
            reader,
            schema,
            fields,
            config,
            stats_cache: Mutex::new(None),
        })
    }

    /// Get a clone of the Index handle for the background thread.
    pub fn index_handle(&self) -> Index {
        self.index.clone()
    }

    /// Get the field handles for the background thread.
    pub fn field_handles(&self) -> FieldHandles {
        self.fields
    }

    /// Get the reindex config for the background thread.
    pub fn reindex_config(&self) -> ReindexConfig {
        self.config.clone()
    }

    pub fn is_empty(&self) -> bool {
        let searcher = self.reader.searcher();
        searcher.num_docs() == 0
    }
}

pub(crate) fn build_schema() -> (Schema, FieldHandles) {
    let mut builder = Schema::builder();
    let fields = FieldHandles {
        content: builder.add_text_field("content", TEXT | STORED),
        session_id: builder.add_text_field("session_id", STRING | STORED),
        account: builder.add_text_field("account", STRING | STORED),
        project: builder.add_text_field("project", TEXT | STORED),
        role: builder.add_text_field("role", STRING | STORED),
        timestamp: builder.add_text_field("timestamp", STRING | STORED),
        file_path: builder.add_text_field("file_path", STRING | STORED),
        byte_offset: builder.add_u64_field("byte_offset", STORED),
        git_branch: builder.add_text_field("git_branch", STRING | STORED),
        is_subagent: builder.add_u64_field("is_subagent", INDEXED | STORED),
        agent_slug: builder.add_text_field("agent_slug", STRING | STORED),
        doc_type: builder.add_text_field("doc_type", STRING | STORED),
        chunk_kind: builder.add_text_field("chunk_kind", STRING | STORED),
        language: builder.add_text_field("language", STRING | STORED),
        symbol: builder.add_text_field("symbol", TEXT | STORED),
        symbol_exact: builder.add_text_field("symbol_exact", STRING | STORED),
        code_content: builder.add_text_field("code_content", TEXT | STORED),
        chunk_hash: builder.add_text_field("chunk_hash", STRING | STORED),
        entity_id: builder.add_text_field("entity_id", STRING | STORED),
        parser_version: builder.add_text_field("parser_version", STRING | STORED),
        commit_sha: builder.add_text_field("commit_sha", STRING | STORED),
        repo_id: builder.add_text_field("repo_id", STRING | STORED),
    };
    (builder.build(), fields)
}

fn reset_index_on_schema_mismatch(index_path: &Path) -> Result<()> {
    if !index_path.exists() {
        return Ok(());
    }
    let marker_path = index_path.join(SCHEMA_VERSION_FILE);
    let should_reset = match fs::read_to_string(&marker_path) {
        Ok(raw) => raw.trim() != INDEX_SCHEMA_VERSION,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            index_path.read_dir()?.next().is_some()
        }
        Err(err) => return Err(err.into()),
    };
    if should_reset {
        tracing::info!(
            path = %index_path.display(),
            schema_version = INDEX_SCHEMA_VERSION,
            "dropping transcript index for schema migration"
        );
        fs::remove_dir_all(index_path)?;
    }
    Ok(())
}

fn write_schema_version_marker(index_path: &Path) -> Result<()> {
    fs::write(
        index_path.join(SCHEMA_VERSION_FILE),
        format!("{INDEX_SCHEMA_VERSION}\n"),
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn index_open_writes_schema_version_marker() {
        let dir = tempfile::tempdir().unwrap();
        let index_path = dir.path().join("index");

        let _index = TranscriptIndex::open_or_create(
            &index_path,
            Vec::new(),
            None,
            dir.path().join("projects.json"),
        )
        .unwrap();

        let marker = fs::read_to_string(index_path.join(SCHEMA_VERSION_FILE)).unwrap();
        assert_eq!(marker.trim(), INDEX_SCHEMA_VERSION);
    }

    #[test]
    fn schema_version_mismatch_drops_existing_index_directory() {
        let dir = tempfile::tempdir().unwrap();
        let index_path = dir.path().join("index");
        fs::create_dir_all(&index_path).unwrap();
        fs::write(index_path.join(SCHEMA_VERSION_FILE), "old-schema\n").unwrap();
        fs::write(index_path.join("stale-file"), "stale").unwrap();

        let _index = TranscriptIndex::open_or_create(
            &index_path,
            Vec::new(),
            None,
            dir.path().join("projects.json"),
        )
        .unwrap();

        assert!(!index_path.join("stale-file").exists());
        let marker = fs::read_to_string(index_path.join(SCHEMA_VERSION_FILE)).unwrap();
        assert_eq!(marker.trim(), INDEX_SCHEMA_VERSION);
    }
}

mod helpers;
mod project_files;
mod reindex;
mod search;

pub use helpers::find_session_file;
pub use reindex::spawn_reindex_thread;
pub use search::{
    CiteParams, ContextParams, MessagesParams, ReindexParams, SearchParams, SessionParams,
    SessionsListParams, TopicsParams,
};
