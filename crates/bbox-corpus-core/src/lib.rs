//! bbox-corpus-core — daemon-internal foundation crate.
//!
//! Extracted from the root `blackbox` crate as stage 0 of the
//! code-intelligence cluster split (gap-fe4dd97f). Holds the entity-ref
//! identity/parse types, repo-file transaction protocol, and git porcelain
//! helpers that the cluster (chunker, lsp, refactor, code_nav, macros) depends
//! on downward.
//!
//! Invariant: this crate must NOT depend on `blackbox` (that would be a
//! workspace cycle).

pub mod built_from;
pub mod code_project_identity;
pub mod edge;
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
pub mod response_page;
pub mod search;
pub mod template;
pub mod transaction;
pub mod util;
