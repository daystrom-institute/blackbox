use crate::refactor::{
    self, RefactorApplyParams, RefactorPlanParams, RefactorProjectRefsParams, RefactorRunParams,
    RefactorStatusParams,
};
use crate::server::BlackboxServer;

use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::CallToolResult;
use rmcp::{tool, tool_router};

pub(crate) fn router() -> ToolRouter<BlackboxServer> {
    BlackboxServer::refactor_tools()
}

#[tool_router(router = refactor_tools)]
impl BlackboxServer {
    #[tool(
        name = "bbox_refactor_status",
        description = "Inspect a supported source file for tree-sitter parse health and refactorable items."
    )]
    pub(crate) fn bbox_refactor_status(
        &self,
        Parameters(p): Parameters<RefactorStatusParams>,
    ) -> CallToolResult {
        Self::run("bbox_refactor_status", || refactor::status(&p))
    }

    #[tool(
        name = "bbox_refactor_project_refs",
        description = "Ground current project_file entity refs for a source file using the same chunk and hash rules as the agentic corpus."
    )]
    pub(crate) fn bbox_refactor_project_refs(
        &self,
        Parameters(p): Parameters<RefactorProjectRefsParams>,
    ) -> CallToolResult {
        Self::run("bbox_refactor_project_refs", || refactor::project_refs(&p))
    }

    #[tool(
        name = "bbox_refactor_plan",
        description = "Create a dry-run structural refactor plan using a supported generic or language-scoped plan kind."
    )]
    pub(crate) fn bbox_refactor_plan(
        &self,
        Parameters(p): Parameters<RefactorPlanParams>,
    ) -> CallToolResult {
        let ctx = refactor::PlanContext {
            lsp: Some(self.state.lsp_sessions.clone()),
        };
        Self::run("bbox_refactor_plan", || refactor::plan_with_ctx(&p, &ctx))
    }

    #[tool(
        name = "bbox_refactor_apply",
        description = "Apply a previously generated refactor plan with hash checks, supported-source parse validation, atomic writes, and rollback on write failure."
    )]
    pub(crate) fn bbox_refactor_apply(
        &self,
        Parameters(p): Parameters<RefactorApplyParams>,
    ) -> CallToolResult {
        Self::run("bbox_refactor_apply", || {
            let projects = self.state.projects.read().list();
            refactor::apply(&p, &projects)
        })
    }

    #[tool(
        name = "bbox_refactor_run",
        description = "Compose primitive refactor plans into one transactional run with rollback across touched files."
    )]
    pub(crate) fn bbox_refactor_run(
        &self,
        Parameters(p): Parameters<RefactorRunParams>,
    ) -> CallToolResult {
        Self::run("bbox_refactor_run", || {
            let projects = self.state.projects.read().list();
            let ctx = refactor::PlanContext {
                lsp: Some(self.state.lsp_sessions.clone()),
            };
            refactor::run_with_ctx(&p, &projects, &ctx)
        })
    }
}
