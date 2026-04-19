use std::fs;
use std::path::{Path, PathBuf};
use std::time::Instant;

use anyhow::Result;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use tantivy::schema::*;
use tantivy::{Index, IndexReader, ReloadPolicy};

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
}

/// Config needed by the background reindex thread.
#[derive(Clone)]
pub struct ReindexConfig {
    pub roots: Vec<(String, PathBuf)>,
    pub codex_root: Option<PathBuf>,
    pub meta_path: PathBuf,
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
    ) -> Result<Self> {
        let meta_path = index_path.join("_meta.json");

        // Build schema
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
        };
        let schema = builder.build();

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

        let reader = index
            .reader_builder()
            .reload_policy(ReloadPolicy::OnCommitWithDelay)
            .try_into()?;

        let config = ReindexConfig {
            roots,
            codex_root,
            meta_path,
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

mod helpers;
mod reindex;
mod search;

pub use helpers::find_session_file;
pub use reindex::spawn_reindex_thread;
pub use search::{
    CiteParams, ContextParams, MessagesParams, ReindexParams, SearchParams, SessionParams,
    SessionsListParams, TopicsParams,
};
