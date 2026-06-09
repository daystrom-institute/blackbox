use crate::refactor::{
    self, RefactorApplyParams, RefactorPlanKindsParams, RefactorPlanParams,
    RefactorProjectRefsParams, RefactorRunParams, RefactorStatusParams,
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
    pub(crate) async fn bbox_refactor_status(
        &self,
        Parameters(p): Parameters<RefactorStatusParams>,
    ) -> CallToolResult {
        Self::run_blocking("bbox_refactor_status", move || refactor::status(&p)).await
    }

    #[tool(
        name = "bbox_refactor_project_refs",
        description = "Ground current project_file entity refs for a source file using the same chunk and hash rules as the agentic corpus."
    )]
    pub(crate) async fn bbox_refactor_project_refs(
        &self,
        Parameters(p): Parameters<RefactorProjectRefsParams>,
    ) -> CallToolResult {
        Self::run_blocking("bbox_refactor_project_refs", move || {
            refactor::project_refs(&p)
        })
        .await
    }

    #[tool(
        name = "bbox_refactor_plan_kinds",
        description = "List a compact machine-readable catalog of supported refactor plan kinds by language, safety class, backend, required fields, and suggested preflight."
    )]
    pub(crate) fn bbox_refactor_plan_kinds(
        &self,
        Parameters(p): Parameters<RefactorPlanKindsParams>,
    ) -> CallToolResult {
        Self::run("bbox_refactor_plan_kinds", || refactor::plan_kinds(&p))
    }

    #[tool(
        name = "bbox_refactor_plan",
        description = "Create a dry-run structural refactor plan using a supported generic or language-scoped plan kind."
    )]
    pub(crate) async fn bbox_refactor_plan(
        &self,
        Parameters(p): Parameters<RefactorPlanParams>,
    ) -> CallToolResult {
        let server = self.clone();
        Self::run_blocking("bbox_refactor_plan", move || {
            let ctx = refactor::PlanContext {
                lsp: Some(server.state.lsp_sessions.clone()),
            };
            refactor::plan_with_ctx(&p, &ctx)
        })
        .await
    }

    #[tool(
        name = "bbox_refactor_apply",
        description = "Apply a previously generated refactor plan with hash checks, supported-source parse validation, atomic writes, and rollback on write failure."
    )]
    pub(crate) async fn bbox_refactor_apply(
        &self,
        Parameters(p): Parameters<RefactorApplyParams>,
    ) -> CallToolResult {
        let server = self.clone();
        Self::run_blocking("bbox_refactor_apply", move || {
            let projects = server.state.projects.read().list();
            refactor::apply(&p, &projects)
        })
        .await
    }

    #[tool(
        name = "bbox_refactor_run",
        description = "Compose primitive refactor plans into one transactional run with rollback across touched files."
    )]
    pub(crate) async fn bbox_refactor_run(
        &self,
        Parameters(p): Parameters<RefactorRunParams>,
    ) -> CallToolResult {
        let server = self.clone();
        Self::run_blocking("bbox_refactor_run", move || {
            let projects = server.state.projects.read().list();
            let ctx = refactor::PlanContext {
                lsp: Some(server.state.lsp_sessions.clone()),
            };
            refactor::run_with_ctx(&p, &projects, &ctx)
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn refactor_run_tool_schema_exposes_structured_step_variants() {
        let router = super::router();
        let tool = router
            .list_all()
            .into_iter()
            .find(|tool| tool.name.as_ref() == "bbox_refactor_run")
            .expect("bbox_refactor_run should be registered");
        let schema = serde_json::Value::Object((*tool.input_schema).clone());
        let steps = &schema["properties"]["steps"];
        assert_ne!(
            steps["items"]["type"], "string",
            "bbox_refactor_run tool schema must not advertise steps as string[]: {steps}"
        );
        assert!(
            steps["items"].get("oneOf").is_some()
                || steps["items"].get("anyOf").is_some()
                || steps["items"].get("$ref").is_some(),
            "bbox_refactor_run tool schema should expose structured RefactorRunStep variants: {steps}"
        );
    }
}
