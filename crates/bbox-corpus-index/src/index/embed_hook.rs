//! Inversion seam between the index engine and the daemon's embed queue.
//!
//! The engine's project-file and git-history passes produce chunks whose
//! text should be queued for embedding, but the embed queue (workers,
//! buckets, vector store) is daemon-side machinery the engine must not
//! depend on. The daemon registers its enqueue functions here at startup
//! (`register_embed_hooks`); when nothing is registered — engine-only
//! tests, standalone use — emits are silently dropped, matching the
//! best-effort semantics embed enqueue already has.
//!
//! Same startup-injection pattern as `bbox_corpus_core::git::set_notes_namespace`.

use std::collections::HashMap;
use std::sync::OnceLock;

use bbox_corpus_core::entity_ref::EntityRef;
use tantivy::IndexWriter;

use bbox_chunker::Chunk;

use super::{FieldHandles, FileMeta, ReindexConfig};

/// Daemon-side embed enqueue functions, registered once at startup.
pub struct EmbedHooks {
    /// Queue a project-file chunk for embedding:
    /// `(chunk, project_display, entity_id)`. `project_display` is the
    /// identity's display name, which the text lanes prepend alongside the
    /// relative path (governing section 10.2) - never a host root.
    pub project_file: fn(&Chunk, &str, &str),
    /// Queue a git commit message: `(entity_id, chunk_hash, message)`.
    pub git_message: fn(&str, &str, &str),
    /// Does an ACTIVE commit-message vector already exist at this
    /// `(entity_id, content_hash)`? The vector store and the embedding route
    /// table are daemon-side, so the P3-E history re-emission cannot probe
    /// them directly; it asks through this hook instead
    /// (`verified-or-re-enqueued`, governing section 10.3).
    pub git_message_vector_active: fn(&str, &str) -> bool,
}

static EMBED_HOOKS: OnceLock<EmbedHooks> = OnceLock::new();

/// Register the daemon's embed enqueue hooks. Idempotent: the first
/// registration wins; later calls are ignored.
pub fn register_embed_hooks(hooks: EmbedHooks) {
    let _ = EMBED_HOOKS.set(hooks);
}

pub fn emit_project_file(chunk: &Chunk, project_display: &str, entity_id: &str) {
    if let Some(hooks) = EMBED_HOOKS.get() {
        (hooks.project_file)(chunk, project_display, entity_id);
    }
}

pub fn emit_git_message(entity_id: &str, chunk_hash: &str, message: &str) {
    if let Some(hooks) = EMBED_HOOKS.get() {
        (hooks.git_message)(entity_id, chunk_hash, message);
    }
}

/// `None` when no hook is registered (engine-only tests, standalone use). A
/// caller verifying vector coverage must treat `None` as "cannot prove
/// covered" and re-enqueue: enqueue is idempotent and the queue's own
/// `should_embed` dedup drops a row that is already fresh, so re-enqueueing an
/// already-covered row costs a queue push, while skipping an uncovered one
/// would commit a lexical-only history view whose vector view was promised.
pub fn git_message_vector_is_active(entity_id: &str, content_hash: &str) -> Option<bool> {
    EMBED_HOOKS
        .get()
        .map(|hooks| (hooks.git_message_vector_active)(entity_id, content_hash))
}

/// Store-document pass injected into the engine's manual rebuild
/// (`TranscriptIndex::build_index`). The daemon registers a pass that
/// reconciles store-backed documents (knowledge entries) into the same
/// writer/commit as the transcript and project passes. Returns the number
/// of documents indexed.
pub type ManualStorePass = fn(
    &ReindexConfig,
    FieldHandles,
    &mut IndexWriter,
    &mut HashMap<String, FileMeta>,
) -> anyhow::Result<u64>;

static MANUAL_STORE_PASS: OnceLock<ManualStorePass> = OnceLock::new();

/// Register the daemon's store-document pass for manual rebuilds.
/// Idempotent: first registration wins.
pub fn register_manual_store_pass(pass: ManualStorePass) {
    let _ = MANUAL_STORE_PASS.set(pass);
}

/// Run the registered store-document pass, or no-op (0 docs) when nothing
/// is registered (engine-only tests, standalone use).
pub fn run_manual_store_pass(
    config: &ReindexConfig,
    fields: FieldHandles,
    writer: &mut IndexWriter,
    meta: &mut HashMap<String, FileMeta>,
) -> anyhow::Result<u64> {
    match MANUAL_STORE_PASS.get() {
        Some(pass) => pass(config, fields, writer, meta),
        None => Ok(0),
    }
}

/// Entity id for a project-file chunk in the current (non-snapshot) lane.
pub fn project_file_entity_id(chunk: &Chunk) -> String {
    project_file_entity_id_for_snapshot(chunk, None)
}

/// Entity id for a project-file chunk, snapshot-qualified when a snapshot
/// id is present (ProjectFileV2 lane).
pub fn project_file_entity_id_for_snapshot(chunk: &Chunk, snapshot_id: Option<&str>) -> String {
    if let Some(snapshot_id) = snapshot_id {
        return EntityRef::ProjectFileV2 {
            project_id: chunk.project_id.clone(),
            snapshot_id: snapshot_id.to_string(),
            rel_path_hash: chunk.rel_path_hash.clone(),
            chunk_hash: chunk.chunk_hash.clone(),
            occurrence_idx: chunk.occurrence_idx,
        }
        .to_string();
    }
    EntityRef::ProjectFile {
        project_id: chunk.project_id.clone(),
        rel_path_hash: chunk.rel_path_hash.clone(),
        chunk_hash: chunk.chunk_hash.clone(),
        occurrence_idx: chunk.occurrence_idx,
    }
    .to_string()
}
