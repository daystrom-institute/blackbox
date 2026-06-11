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
use bbox_artifacts::artifacts;
#[cfg(test)]
#[path = "../eval/badgey/check.rs"]
mod badgey_eval_check;
// chunker extracted into the bbox-chunker crate (stage 1); aliased back to
// `crate::chunker` so existing call sites resolve unchanged.
use bbox_chunker as chunker;
pub mod code_nav;
pub use bbox_config::config;
mod crons;
pub mod dispatch_mcp;
use bbox_edge_index::edge_index;
mod embed;
mod embed_queue;
mod entity_loader;
// `entity_ref` extracted into the bbox-corpus-core foundation crate (stage 0).
// Aliased back so existing `crate::entity_ref::*` paths resolve unchanged.
pub use bbox_corpus_core::entity_ref;
#[cfg(test)]
#[path = "../eval/check.rs"]
mod eval_check;
use bbox_gaps::gap_spool;
use bbox_gaps::gaps;
// `git` extracted into bbox-corpus-core (stage 0); aliased back to `crate::git`.
use bbox_corpus_core::git;
use bbox_inbox::inbox;
mod index;
// `json_store` extracted into bbox-corpus-core; aliased back to
// `crate::json_store` so existing call sites resolve unchanged.
pub use bbox_corpus_core::json_store;
use bbox_knowledge::knowledge;
// `lsp` extracted into bbox-lsp (stage 2); aliased back to `crate::lsp`.
use bbox_lsp as lsp;
// `macros` extracted into bbox-macros (stage 5); aliased back to
// `crate::macros` so existing call sites resolve unchanged.
use bbox_macros as macros;
mod managed_worktrees;
use bbox_edge_sidecar::manifest;
mod mcp_client;
mod mcp_tools;
use bbox_edge_index::migration;
use bbox_threads::notes;
mod orchestration;
// `packets` extracted into bbox-packets (root-crate split); aliased back to
// `crate::packets` so existing call sites resolve unchanged.
use bbox_packets as packets;
/// The transcript parser lives in the shared `bro-transcript` crate (the
/// daemon's indexer and the `bro` cockpit both link it). Re-exported as
/// `crate::parser` so the ~8 in-crate `crate::parser::*` users don't churn.
pub use bro_transcript as parser;
mod path_cache;
use bbox_stores::pins;
mod pollers;
mod projects;
mod providers;
// `search` (rrf/rerank) and `template` extracted into bbox-corpus-core;
// aliased back so existing call sites resolve unchanged. (`query` callers
// now use bbox_corpus_core::query directly.)
// `refactor` extracted into bbox-refactor (stage 3); aliased back to
// `crate::refactor` so existing call sites resolve unchanged.
use bbox_refactor as refactor;
pub use bbox_knowledge::render;
use bbox_stores::roadmap;
mod routing;
use bbox_corpus_core::search;
pub use bbox_config::secrets;
pub mod server;
pub use bbox_slack::slack_channel_bindings;
pub use bbox_slack::slack_proposal_links;
use bbox_slack::slack_thread_store;
mod slices;
use bbox_edge_index::storage_health;
use bbox_stores::store_persister;
mod system_events;
// `system_memory` extracted into bbox-system-memory (root-crate split);
// aliased back to `crate::system_memory` so existing call sites resolve
// unchanged.
use bbox_system_memory as system_memory;
use bbox_corpus_core::template;
use bbox_threads::threads;
use bbox_tool_docs::tool_docs;
mod tools;
use bbox_corpus_index::transcripts;
pub use bbox_util::util;
// `vectors` was extracted into the `bbox-vectors` workspace crate (build-time
// decomposition). Aliased back to `crate::vectors` so existing `crate::vectors::*`
// call sites resolve unchanged.
use bbox_vectors as vectors;
use bbox_artifacts::watcher;
mod webhooks;
use bbox_whiteboards::whiteboards;
mod workflow;

/// Initialize the process-wide system-memory catalog for tests. The
/// repo-root `system-defaults/memories` path is owned here (the root crate),
/// not by bbox-system-memory, whose manifest dir is the crate subdirectory.
#[cfg(test)]
pub(crate) fn init_system_memory_for_tests() {
    let defaults = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("system-defaults")
        .join("memories");
    system_memory::init_for_tests_from(&defaults);
}

use std::collections::BTreeMap;
use std::sync::OnceLock;

use parking_lot::RwLock;

static AGENT_QUERY_EMBED_CACHE: OnceLock<RwLock<BTreeMap<String, Vec<f32>>>> = OnceLock::new();
