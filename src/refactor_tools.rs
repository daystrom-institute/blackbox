use super::*;

pub(super) fn router() -> ToolRouter<BlackboxServer> {
    BlackboxServer::refactor_tools()
}

#[tool_router(router = refactor_tools)]
impl BlackboxServer {
    #[tool(
        name = "bbox_refactor_status",
        description = "Inspect a supported source file for tree-sitter parse health and refactorable items."
    )]
    fn bbox_refactor_status(
        &self,
        Parameters(p): Parameters<RefactorStatusParams>,
    ) -> CallToolResult {
        Self::run("bbox_refactor_status", || refactor::status(&p))
    }

    #[tool(
        name = "bbox_refactor_plan",
        description = "Create a dry-run structural refactor plan. Supports Rust top-level extraction, Rust impl-method extraction, module declarations, and Rust router-sum updates."
    )]
    fn bbox_refactor_plan(&self, Parameters(p): Parameters<RefactorPlanParams>) -> CallToolResult {
        Self::run("bbox_refactor_plan", || refactor::plan(&p))
    }

    #[tool(
        name = "bbox_refactor_apply",
        description = "Apply a previously generated refactor plan with hash checks, Rust parse validation, atomic writes, and rollback on write failure."
    )]
    fn bbox_refactor_apply(
        &self,
        Parameters(p): Parameters<RefactorApplyParams>,
    ) -> CallToolResult {
        Self::run("bbox_refactor_apply", || {
            let projects = self.state.projects.read().list();
            refactor::apply(&p, &projects)
        })
    }
}
