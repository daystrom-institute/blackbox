use crate::code_nav::{
    CodeNodeDescribeParams, CodeQueryParams, CodeRefsParams, CodeSymbolSearchParams,
    code_node_describe, code_query, code_refs, code_symbols,
};
use crate::server::*;
use crate::*;

pub(crate) fn router() -> ToolRouter<BlackboxServer> {
    BlackboxServer::code_nav_tools()
}

#[tool_router(router = code_nav_tools)]
impl BlackboxServer {
    #[tool(
        name = "bbox_code_query",
        description = "Run a tree-sitter query against one source file. Return syntactic matches plus handoff hints for refactor/status grounding."
    )]
    pub(crate) fn bbox_code_query(
        &self,
        Parameters(p): Parameters<CodeQueryParams>,
    ) -> CallToolResult {
        Self::run("bbox_code_query", || code_query(&p))
    }

    #[tool(
        name = "bbox_code_symbols",
        description = "Find refactorable syntax symbols across a project and return exact line ranges plus refactor/project-ref handoff hints."
    )]
    pub(crate) fn bbox_code_symbols(
        &self,
        Parameters(p): Parameters<CodeSymbolSearchParams>,
    ) -> CallToolResult {
        Self::run("bbox_code_symbols", || {
            let projects = self.state.projects.read().list();
            let idx = self.state.idx.read();
            code_symbols(&p, &projects, Some(&*idx))
        })
    }

    #[tool(
        name = "bbox_code_node_describe",
        description = "Describe the smallest named AST node at a source position and suggest the next refactor/status grounding call."
    )]
    pub(crate) fn bbox_code_node_describe(
        &self,
        Parameters(p): Parameters<CodeNodeDescribeParams>,
    ) -> CallToolResult {
        Self::run("bbox_code_node_describe", || code_node_describe(&p))
    }

    #[tool(
        name = "bbox_code_refs",
        description = "Extract syntactic references (calls, imports, fields, identifiers) from one source file. Per-language tree-sitter queries; identifiers fallback for unsupported languages. Records are syntax-only with edge_confidence=\"heuristic\"."
    )]
    pub(crate) fn bbox_code_refs(
        &self,
        Parameters(p): Parameters<CodeRefsParams>,
    ) -> CallToolResult {
        Self::run("bbox_code_refs", || code_refs(&p))
    }
}
