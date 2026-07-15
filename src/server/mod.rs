mod background;
pub mod dispatch;
pub mod handler;
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
pub(crate) mod worker_rpc;
pub mod workflow_capabilities;
pub mod workflow_runtime;

pub(crate) use dispatch::*;
pub(crate) use routes::*;
pub use run::run;
pub(crate) use state::*;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RuntimeRole {
    Corpus,
    Compatibility,
}

impl RuntimeRole {
    pub(crate) fn from_env() -> anyhow::Result<Self> {
        let value = std::env::var("BLACKBOX_RUNTIME_ROLE").unwrap_or_else(|_| "corpus".into());
        Self::parse(&value)
    }

    fn parse(value: &str) -> anyhow::Result<Self> {
        match value.trim() {
            "corpus" => Ok(Self::Corpus),
            "compatibility" => Ok(Self::Compatibility),
            value => anyhow::bail!(
                "BLACKBOX_RUNTIME_ROLE must be 'corpus' or 'compatibility', got {value:?}"
            ),
        }
    }

    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Corpus => "corpus",
            Self::Compatibility => "compatibility",
        }
    }
}

#[cfg(test)]
mod runtime_role_tests {
    use super::RuntimeRole;

    #[test]
    fn runtime_role_is_strict_and_corpus_first() {
        assert_eq!(RuntimeRole::parse("corpus").unwrap(), RuntimeRole::Corpus);
        assert_eq!(
            RuntimeRole::parse(" compatibility ").unwrap(),
            RuntimeRole::Compatibility
        );
        assert!(RuntimeRole::parse("legacy").is_err());
        assert!(RuntimeRole::parse("").is_err());
    }
}

impl BlackboxServer {
    pub(crate) const MCP_RESPONSE_CAP_BYTES: usize = 80 * 1024;

    pub(crate) fn new(state: std::sync::Arc<SharedState>) -> Self {
        Self {
            state,
            runtime_role: RuntimeRole::Compatibility,
            tool_router: crate::tools::projects::router()
                + crate::tools::notes::router()
                + crate::tools::gaps::router()
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
                + crate::tools::slices::router()
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
                + crate::tools::system_events::router()
                + crate::tools::macros::router(),
            surface: std::sync::OnceLock::new(),
            surface_project: std::sync::OnceLock::new(),
        }
    }

    pub(crate) fn new_corpus(state: std::sync::Arc<SharedState>) -> Self {
        Self {
            state,
            runtime_role: RuntimeRole::Corpus,
            tool_router: crate::tools::projects::router()
                + crate::tools::notes::router()
                + crate::tools::gaps::router()
                + crate::tools::threads::router()
                + crate::tools::attention::router()
                + crate::tools::graph::router()
                + crate::tools::transcripts::router()
                + crate::tools::sessions::router()
                + crate::tools::knowledge::router()
                + crate::tools::render::router()
                + crate::tools::roadmap::router()
                + crate::tools::slices::router()
                + crate::tools::mcp_surface::router()
                + crate::tools::doctor::router()
                + crate::tools::storage_health::router()
                + crate::tools::storage_gc::router()
                + crate::tools::storage_migration::router(),
            surface: std::sync::OnceLock::new(),
            surface_project: std::sync::OnceLock::new(),
        }
    }
}
