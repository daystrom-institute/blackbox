mod background;
pub mod dispatch;
pub mod handler;
mod mcp;
pub mod progress;
pub mod response;
mod restore;
pub mod routes;
mod run;
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
pub(crate) use progress::*;
pub(crate) use routes::*;
pub use run::run;
pub(crate) use state::*;
pub(crate) use tail::*;
pub(crate) use workflow_capabilities::*;

impl BlackboxServer {
    pub(crate) const MCP_RESPONSE_CAP_BYTES: usize = 80 * 1024;

    pub(crate) fn new(state: std::sync::Arc<SharedState>) -> Self {
        Self {
            state,
            tool_router: crate::tools::projects::router()
                + crate::tools::notes::router()
                + crate::tools::threads::router()
                + crate::tools::refactor::router()
                + crate::tools::code_nav::router()
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
                + crate::tools::agents::router()
                + crate::tools::atoms::router()
                + crate::tools::orchestrate::router()
                + crate::tools::councils::router()
                + crate::tools::roster::router()
                + crate::tools::config::router()
                + crate::tools::dispatch::router()
                + crate::tools::mcp_surface::router()
                + crate::tools::storage_health::router()
                + crate::tools::storage_gc::router()
                + crate::tools::storage_migration::router()
                + crate::tools::workspace::router()
                + crate::tools::system_events::router(),
            surface: std::sync::OnceLock::new(),
        }
    }
}
