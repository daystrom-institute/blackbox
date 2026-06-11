//! bbox-corpus-index — daemon-internal transcript/code index engine.
//!
//! Extracted from the root `blackbox` crate (root-crate-peels arc). Owns the
//! tantivy schema and `TranscriptIndex`, search, the transcript adapter
//! registry and projection, the project-file / git-history / tool-edge index
//! passes, and the scan/meta utilities reindex orchestration is built from.
//!
//! The daemon side (store->doc builders, the writer actor, reindex
//! orchestration) lives above this crate and reaches in through public APIs;
//! the engine reaches daemon machinery only through the startup-injected
//! hooks in [`index::embed_hook`] (embed enqueue + the manual store-doc
//! pass), which no-op when unregistered.

pub mod index;
pub mod transcripts;
