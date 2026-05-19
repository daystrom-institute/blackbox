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
mod dispatch_mcp;
#[cfg(test)]
pub(crate) use dispatch_mcp::dispatch_mcp_url;
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

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

use parking_lot::RwLock;

use axum::extract::{Query, State as AxumState};
use axum::response::IntoResponse;
use axum::response::sse::{Event, Sse};
use futures::{StreamExt, stream::Stream};
use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::tool::ToolCallContext;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{
    CallToolRequestParams, CallToolResult, ErrorCode, InitializeRequestParams, InitializeResult,
    IntoContents, ListToolsResult, ServerCapabilities, ServerInfo,
};
use rmcp::service::RequestContext;
use rmcp::{ErrorData, RoleServer, ServerHandler, tool, tool_handler, tool_router};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;

use index::TranscriptIndex;
use knowledge::Knowledge;
use notes::Notes;
use orchestration::providers::{ExecOpts, Provider};
use orchestration::tail::TailEvent;
use orchestration::{self as orch, TaskStore};
use packets::Packets;
use pins::{AmbientPinQuery, PinParams, Pins};
use projects::{
    ProjectListResponse, ProjectRecord, ProjectRegisterParams, ProjectRegistry,
    ProjectRenameParams, ProjectUnregisterParams,
};
use providers::ProviderContext;
use roadmap::Roadmap;
use threads::Threads;

static AGENT_QUERY_EMBED_CACHE: OnceLock<RwLock<BTreeMap<String, Vec<f32>>>> = OnceLock::new();

use artifacts::{
    ArtifactInstallParams, ArtifactListParams, ArtifactRemoveParams, ArtifactSupersedeParams,
};
use embed::ReembedParams;
use inbox::InboxParams;
use index::{
    CiteParams, ContextParams, MessagesParams, ReindexParams, SearchParams, SessionParams,
    SessionsListParams, TopicsParams,
};
use knowledge::{
    AbsorbParams, BootstrapParams, DecideParams, ForgetParams, KnowledgeLinkParams,
    KnowledgeListParams, LearnParams, RememberParams, RenderParams, ResponseFormat, ReviewParams,
};
use mcp_tools::blame::BlameParams;
use mcp_tools::bundle_evidence::BundleEvidenceParams;
use mcp_tools::discover_seed::DiscoverSeedParams;
use mcp_tools::find_paths::FindPathsParams;
use mcp_tools::hybrid_search::HybridSearchParams;
use mcp_tools::inspect::InspectEntityParams;
use mcp_tools::provenance::ProvenanceParams;
use mcp_tools::ref_size::RefSizeParams;
use notes::{NoteListParams, NoteParams};
use packets::{
    ApplyParams as PacketApplyParams, AuditParams, CompileParams, EventsParams, GapParams,
    PacketListParams, apply_with as apply_packet_with, packet_matches_query, packet_summary,
};
use refactor::{
    RefactorApplyParams, RefactorPlanParams, RefactorProjectRefsParams, RefactorRunParams,
    RefactorStatusParams,
};
pub(crate) use server::*;
use threads::{ThreadListParams, ThreadParams};
pub(crate) use tools::bro_helpers::*;
pub(crate) use tools::bro_params::*;
pub(crate) use tools::bro_runtime_params::*;
