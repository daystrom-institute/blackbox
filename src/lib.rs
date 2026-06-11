// Phase 4 concurrency enforcement (concurrency-model §5): the clippy.toml
// disallowed_methods list warns crate-wide by default. The store / index /
// boot layers legitimately do blocking fs on actor threads and blocking-pool
// contexts, so the crate root allows the lint and the enforcement surfaces
// re-deny it: src/tools/mod.rs (MCP handlers) — plus scripts/
// lint-concurrency.sh as the syntactic backstop for handler bodies.
#![allow(clippy::disallowed_methods)]
// Library crate shell. Modules move here after their dependencies are extracted.

// Edition 2024 enabled stricter lints whose suggestions are stylistic rather
// than behavioral. We opt out of the noisiest categories crate-wide so we can
// focus the lint surface on substantive issues.
#![allow(
    clippy::collapsible_if,
    clippy::doc_overindented_list_items,
    clippy::doc_lazy_continuation,
    clippy::too_many_arguments,
    clippy::type_complexity,
    clippy::large_enum_variant,
    clippy::enum_variant_names,
    clippy::let_and_return
)]

extern crate self as blackbox;

#[cfg(test)]
#[path = "../eval/agents/check.rs"]
mod agent_eval_check;
mod artifacts;
#[cfg(test)]
#[path = "../eval/badgey/check.rs"]
mod badgey_eval_check;
// chunker extracted into the bbox-chunker crate (stage 1); aliased back to
// `crate::chunker` so existing call sites resolve unchanged.
use bbox_chunker as chunker;
pub mod code_nav;
pub mod config;
mod council;
mod crons;
pub mod dispatch_mcp;
mod edge_index;
mod embed;
mod embed_queue;
mod entity_loader;
// `entity_ref` extracted into the bbox-corpus-core foundation crate (stage 0).
// Aliased back so existing `crate::entity_ref::*` paths resolve unchanged.
pub use bbox_corpus_core::entity_ref;
#[cfg(test)]
#[path = "../eval/check.rs"]
mod eval_check;
mod gap_closeout;
mod gap_spool;
mod gaps;
// `git` extracted into bbox-corpus-core (stage 0); aliased back to `crate::git`.
use bbox_corpus_core::git;
mod inbox;
mod index;
pub mod json_store;
mod knowledge;
// `lsp` extracted into bbox-lsp (stage 2); aliased back to `crate::lsp`.
use bbox_lsp as lsp;
// `macros` extracted into bbox-macros (stage 5); aliased back to
// `crate::macros` so existing call sites resolve unchanged.
use bbox_macros as macros;
mod managed_worktrees;
mod manifest;
mod mcp_client;
mod mcp_tools;
mod migration;
mod notes;
mod orchestration;
mod packets;
/// The transcript parser lives in the shared `bro-transcript` crate (the
/// daemon's indexer and the `bro` cockpit both link it). Re-exported as
/// `crate::parser` so the ~8 in-crate `crate::parser::*` users don't churn.
pub use bro_transcript as parser;
mod path_cache;
mod pins;
mod pollers;
mod projects;
mod providers;
mod query;
// `refactor` extracted into bbox-refactor (stage 3); aliased back to
// `crate::refactor` so existing call sites resolve unchanged.
use bbox_refactor as refactor;
pub mod render;
mod roadmap;
mod routing;
mod search;
pub mod secrets;
pub mod server;
pub mod slack_channel_bindings;
pub mod slack_proposal_links;
mod slack_thread_store;
mod slices;
mod snapshot;
mod storage_health;
mod store_persister;
mod system_events;
mod system_memory;
mod template;
mod threads;
mod tool_docs;
mod tools;
mod transcripts;
pub mod util;
// `vectors` was extracted into the `bbox-vectors` workspace crate (build-time
// decomposition). Aliased back to `crate::vectors` so existing `crate::vectors::*`
// call sites resolve unchanged.
use bbox_vectors as vectors;
mod watcher;
mod webhooks;
mod whiteboards;
mod workflow;

use std::collections::BTreeMap;
use std::sync::OnceLock;

use parking_lot::RwLock;

static AGENT_QUERY_EMBED_CACHE: OnceLock<RwLock<BTreeMap<String, Vec<f32>>>> = OnceLock::new();
