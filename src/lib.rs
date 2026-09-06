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
// chunker extracted into the bbox-chunker crate (stage 1); aliased back to
// `crate::chunker` so existing call sites resolve unchanged.
use bbox_chunker as chunker;
pub use bbox_config::config;
pub mod dispatch_mcp;
mod doctor;
use bbox_edge_index::edge_index;
use bbox_embed::embed;
use bbox_embed::embed_queue;
mod embed_runtime;
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
use bbox_indexing::index;
// `json_store` extracted into bbox-corpus-core; aliased back to
// `crate::json_store` so existing call sites resolve unchanged.
pub use bbox_corpus_core::json_store;
use bbox_knowledge::knowledge;
mod managed_worktrees;
use bbox_edge_index::migration;
use bbox_edge_sidecar::manifest;
use bbox_mcp_tools::mcp_tools;
use bbox_threads::notes;
mod orchestration;
// `packets` extracted into bbox-packets (root-crate split); aliased back to
// `crate::packets` so existing call sites resolve unchanged.
use bbox_mcp_tools::path_cache;
use bbox_packets as packets;
use bbox_stores::checkout_mutations;
use bbox_stores::pins;
/// The transcript parser lives in the shared `bro-transcript` crate (the
/// daemon's indexer and the `bro` cockpit both link it). Re-exported as
/// `crate::parser` so the ~8 in-crate `crate::parser::*` users don't churn.
pub use bro_transcript as parser;
pub mod project_catalog_rebuild_admin;
pub mod project_catalog_stamper;
use bbox_indexing::projects;
use bbox_providers::providers;
mod project_graph_read;
mod providers_ext;
// `search` (rrf/rerank) and `template` extracted into bbox-corpus-core;
// aliased back so existing call sites resolve unchanged. (`query` callers
// now use bbox_corpus_core::query directly.)
pub use bbox_config::secrets;
pub use bbox_knowledge::render;
use bbox_stores::roadmap;
pub mod server;
use bbox_edge_index::storage_health;
pub use bbox_slack::slack_channel_bindings;
pub use bbox_slack::slack_proposal_links;
use bbox_stores::store_persister;
use bbox_system_events::system_events;
// `system_memory` extracted into bbox-system-memory (root-crate split);
// aliased back to `crate::system_memory` so existing call sites resolve
// unchanged.
use bbox_corpus_core::template;
use bbox_system_memory as system_memory;
use bbox_threads::threads;
use bbox_tool_docs::tool_docs;
mod tools;
use bbox_corpus_index::transcripts;
pub use bbox_util::util;
// `vectors` was extracted into the `bbox-vectors` workspace crate (build-time
// decomposition). Aliased back to `crate::vectors` so existing `crate::vectors::*`
// call sites resolve unchanged.
use bbox_artifacts::watcher;
use bbox_vectors as vectors;
mod vector_maintenance;
use bbox_whiteboards::whiteboards;

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
