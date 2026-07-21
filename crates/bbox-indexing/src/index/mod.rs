//! Daemon-side index facade.
//!
//! The index ENGINE (tantivy schema, `TranscriptIndex`, search, transcript
//! adapters, project-file/git/tool-edge passes, scan/meta utilities) lives in
//! the `bbox-corpus-index` crate and is re-exported here under the original
//! `crate::index::*` paths. This module keeps the daemon-side layers that
//! bind the stores: the store->doc builders, the single tantivy writer actor,
//! and reindex orchestration. Daemon wiring attaches to the engine through
//! `IndexWriterActor::spawn_for` / `writer_actor::register_index_store_hooks`
//! (embed enqueue + the manual store-doc pass).

pub use bbox_corpus_index::index::*;

mod knowledge_docs;
mod reindex;
mod roadmap_docs;
#[cfg(test)]
mod store_integration_tests;
mod thread_docs;
pub mod writer_actor;

pub use knowledge_docs::{
    KnowledgeIndexDocument, indexable_knowledge_entry, knowledge_chunk_hash, knowledge_entity_id,
};
pub use reindex::backfill_tool_edges_for_project;
pub use reindex::spawn_reindex_thread;
pub use roadmap_docs::{roadmap_chunk_hash, roadmap_entity_id};
pub use writer_actor::{IndexWriteOp, IndexWriterActor};
