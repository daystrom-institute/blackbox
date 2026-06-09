use crate::knowledge::{AbsorbParams, BootstrapParams, RenderParams, ReviewParams};
use crate::server::BlackboxServer;

use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::CallToolResult;
use rmcp::{tool, tool_router};

pub(crate) fn router() -> ToolRouter<BlackboxServer> {
    BlackboxServer::render_tools()
}

#[tool_router(router = render_tools)]
impl BlackboxServer {
    #[tool(
        name = "bbox_render",
        description = "Render entries into CLAUDE.md / AGENTS.md / GEMINI.md."
    )]
    pub(crate) async fn bbox_render(
        &self,
        Parameters(p): Parameters<RenderParams>,
    ) -> CallToolResult {
        let server = self.clone();
        Self::run_blocking("bbox_render", move || server.state.kb.read().render(&p)).await
    }

    #[tool(
        name = "bbox_absorb",
        description = "Compatibility no-op for the old rendered-file import path."
    )]
    pub(crate) async fn bbox_absorb(
        &self,
        Parameters(p): Parameters<AbsorbParams>,
    ) -> CallToolResult {
        let server = self.clone();
        Self::run_blocking("bbox_absorb", move || {
            let out = server.state.kb.write().absorb(&p)?;
            // Central KB persistence is write-behind here: this body runs on
            // the blocking pool where the durable ack can't be awaited.
            server.state.kb_persister.request();
            Ok(out)
        })
        .await
    }

    #[tool(
        name = "bbox_lint",
        description = "Health check for contradictions, stale entries, duplicates."
    )]
    pub(crate) async fn bbox_lint(&self) -> CallToolResult {
        let server = self.clone();
        Self::run_blocking("bbox_lint", move || server.state.kb.read().lint()).await
    }

    #[tool(
        name = "bbox_review",
        description = "Approve or reject entries awaiting review."
    )]
    pub(crate) async fn bbox_review(
        &self,
        Parameters(p): Parameters<ReviewParams>,
    ) -> CallToolResult {
        let server = self.clone();
        Self::run_blocking("bbox_review", move || {
            let out = server.state.kb.write().review(&p)?;
            // Central KB persistence is write-behind here: this body runs on
            // the blocking pool where the durable ack can't be awaited.
            server.state.kb_persister.request();
            Ok(out)
        })
        .await
    }

    #[tool(
        name = "bbox_bootstrap",
        description = "Onboard a new repo into the blackbox knowledge system."
    )]
    pub(crate) async fn bbox_bootstrap(
        &self,
        Parameters(p): Parameters<BootstrapParams>,
    ) -> CallToolResult {
        let server = self.clone();
        Self::run_blocking("bbox_bootstrap", move || {
            server.state.kb.read().bootstrap(&p)
        })
        .await
    }
}
