mod background;
pub(crate) mod checkout_access;
pub(crate) mod code_source;
pub mod dispatch;
mod gap_view;
pub mod handler;
mod knowledge_lifecycle;
mod knowledge_merge_gate;
mod knowledge_view;
mod mcp;
mod open;
pub mod progress;
pub mod response;
mod restore;
pub mod routes;
mod run;
mod runtime_metrics;
pub mod schema;
mod shutdown;
mod startup;
pub mod state;
pub mod storage_gc;
pub mod store_helpers;
pub mod surface;
pub mod tail;
pub mod workflow_capabilities;
pub mod workflow_runtime;

pub(crate) use dispatch::*;
pub(crate) use routes::*;
pub use run::run;
pub(crate) use state::*;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum KnowledgeOverlayRefreshOutcome {
    Converged,
    PreservedTransient,
    Invalid,
    Superseded,
}

impl BlackboxServer {
    pub(crate) const MCP_RESPONSE_CAP_BYTES: usize = 80 * 1024;

    pub(crate) fn new(state: std::sync::Arc<SharedState>) -> Self {
        Self {
            state,
            tool_router: crate::tools::projects::router()
                + crate::tools::notes::router()
                + crate::tools::gaps::router()
                + crate::tools::threads::router()
                + crate::tools::artifacts::router()
                + crate::tools::packets::router()
                + crate::tools::attention::router()
                + crate::tools::graph::router()
                + crate::tools::transcripts::router()
                + crate::tools::sessions::router()
                + crate::tools::knowledge::router()
                + crate::tools::render::router()
                + crate::tools::roadmap::router()
                + crate::tools::whiteboards::router()
                + crate::tools::badgey::router()
                + crate::tools::consultant::router()
                + crate::tools::agents::router()
                + crate::tools::atoms::router()
                + crate::tools::orchestrate::router()
                + crate::tools::roster::router()
                + crate::tools::config::router()
                + crate::tools::dispatch::router()
                + crate::tools::mcp_surface::router()
                + crate::tools::doctor::router()
                + crate::tools::storage_health::router()
                + crate::tools::storage_gc::router()
                + crate::tools::storage_migration::router()
                + crate::tools::workspace::router()
                + crate::tools::system_events::router(),
            surface: std::sync::OnceLock::new(),
            surface_project: std::sync::OnceLock::new(),
            session_checkout: std::sync::OnceLock::new(),
        }
    }
}
