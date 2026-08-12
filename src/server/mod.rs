mod background;
pub(crate) mod blame_authority;
mod bridge_parity;
mod built_from;
/// Phase 5 plan section 14.4: the bridge parity proof. Test-only.
#[cfg(test)]
#[cfg(test)]
pub(crate) mod catalog_ownership_scan;
pub(crate) mod checkout_access;
pub(crate) mod code_source;
pub mod dispatch;
mod gap_view;
pub(crate) mod git_source;
pub mod handler;
pub(crate) mod history_activation;
pub mod instance_lock;
mod knowledge_lifecycle;
mod knowledge_merge_gate;
pub(crate) mod knowledge_source;
mod knowledge_view;
mod mcp;
mod open;
pub(crate) mod producer_auth;
pub mod progress;
pub(crate) mod provenance_authority;
pub(crate) mod provenance_import;
pub(crate) mod repo_io;
pub(crate) mod resolver_compat;
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
pub(crate) mod workspace_binding_mint;

pub(crate) use dispatch::*;
pub(crate) use knowledge_lifecycle::checkout_access_error_is_definitively_stale;
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
                + crate::tools::project_catalog::router()
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
            session_workspace_binding: std::sync::OnceLock::new(),
            session_operator_blame_binding: std::sync::OnceLock::new(),
            session_operator_provenance_binding: std::sync::OnceLock::new(),
        }
    }

    pub(crate) fn provider_context(&self) -> crate::providers::ProviderContext<'_> {
        let context = crate::providers::ProviderContext::new_with_ext(
            self.state.corpus_stores(),
            self.state.as_ref(),
        );
        match self.authoritative_session_checkout() {
            Some(checkout) => {
                context.with_checkout_selection(crate::providers::ProviderCheckoutSelection {
                    project_id: checkout.project_id.clone(),
                    checkout_id: checkout.checkout_id.clone(),
                    published_scope: checkout.published_scope.clone(),
                })
            }
            None => context,
        }
    }
}
