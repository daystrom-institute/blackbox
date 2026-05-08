use crate::server::*;
use crate::*;

pub(crate) fn router() -> ToolRouter<BlackboxServer> {
    BlackboxServer::render_tools()
}

#[tool_router(router = render_tools)]
impl BlackboxServer {
    #[tool(
        name = "bbox_render",
        description = "Render entries into CLAUDE.md / AGENTS.md / GEMINI.md."
    )]
    pub(crate) fn bbox_render(&self, Parameters(p): Parameters<RenderParams>) -> CallToolResult {
        Self::run("bbox_render", || self.state.kb.read().render(&p))
    }

    #[tool(
        name = "bbox_absorb",
        description = "Compatibility no-op for the old rendered-file import path."
    )]
    pub(crate) fn bbox_absorb(&self, Parameters(p): Parameters<AbsorbParams>) -> CallToolResult {
        Self::run("bbox_absorb", || self.state.kb.write().absorb(&p))
    }

    #[tool(
        name = "bbox_lint",
        description = "Health check for contradictions, stale entries, duplicates."
    )]
    pub(crate) fn bbox_lint(&self) -> CallToolResult {
        Self::run("bbox_lint", || self.state.kb.read().lint())
    }

    #[tool(
        name = "bbox_review",
        description = "Approve or reject entries awaiting review."
    )]
    pub(crate) fn bbox_review(&self, Parameters(p): Parameters<ReviewParams>) -> CallToolResult {
        Self::run("bbox_review", || self.state.kb.write().review(&p))
    }

    #[tool(
        name = "bbox_bootstrap",
        description = "Onboard a new repo into the blackbox knowledge system."
    )]
    pub(crate) fn bbox_bootstrap(
        &self,
        Parameters(p): Parameters<BootstrapParams>,
    ) -> CallToolResult {
        Self::run("bbox_bootstrap", || self.state.kb.read().bootstrap(&p))
    }
}
