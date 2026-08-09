//! bbox-corpus-core — daemon-internal foundation crate.
//!
//! Extracted from the root `blackbox` crate as stage 0 of the
//! code-intelligence cluster split (gap-fe4dd97f). Holds the entity-ref
//! identity/parse types, repo-file transaction protocol, and git porcelain
//! helpers that the cluster (chunker, lsp, refactor, code_nav, macros) depends
//! on downward.
//!
//! Invariant: this crate must NOT depend on `blackbox` (that would be a
//! workspace cycle). The one former upward reach — `git::notes_namespace`
//! loading `blackbox::config` — is inverted to a startup injection via
//! [`git::set_notes_namespace`], which the daemon calls once after it loads
//! config.

pub mod blame_transport;
pub mod built_from;
pub mod code_project_identity;
pub mod edit;
pub mod entity_ref;
pub mod git;
pub mod git_overlay;
pub mod git_transport_cutover;
pub mod identity;
pub mod json_store;
pub mod language;
pub mod lsp_config;
pub mod project_catalog;
pub mod project_catalog_snapshot;
pub mod project_record;
pub mod project_selector;
pub mod query;
pub mod search;
pub mod template;
pub mod transaction;
pub mod util;
