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
mod chunker;
pub mod code_nav;
pub mod config;
mod council;
mod crons;
pub mod dispatch_mcp;
mod edge_index;
mod embed;
mod embed_queue;
mod entity_loader;
pub mod entity_ref;
#[cfg(test)]
#[path = "../eval/check.rs"]
mod eval_check;
mod gap_closeout;
mod gap_spool;
mod git;
mod inbox;
mod index;
pub mod json_store;
mod knowledge;
mod lsp;
mod manifest;
mod mcp_client;
mod mcp_tools;
mod migration;
mod notes;
mod orchestration;
mod packets;
mod parser;
mod path_cache;
mod pins;
mod pollers;
mod projects;
mod providers;
mod query;
mod refactor;
pub mod render;
mod roadmap;
mod routing;
mod search;
pub mod secrets;
pub mod server;
pub mod slack_channel_bindings;
pub mod slack_proposal_links;
mod slack_thread_store;
mod snapshot;
mod storage_health;
mod system_events;
mod system_memory;
mod template;
#[cfg(test)]
mod tests;
mod threads;
mod tool_docs;
mod tools;
mod transcripts;
pub mod util;
mod vectors;
mod watcher;
mod webhooks;
mod whiteboards;
mod workflow;

use std::collections::BTreeMap;
use std::sync::OnceLock;

use parking_lot::RwLock;

static AGENT_QUERY_EMBED_CACHE: OnceLock<RwLock<BTreeMap<String, Vec<f32>>>> = OnceLock::new();
