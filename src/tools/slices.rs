use crate::server::BlackboxServer;
use crate::slices::{
    self, SliceCopyParams, SliceDeleteParams, SliceInsertTextParams, SliceMoveParams,
    SliceReadParams, SliceReplaceParams,
};

use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::CallToolResult;
use rmcp::{tool, tool_router};

pub(crate) fn router() -> ToolRouter<BlackboxServer> {
    BlackboxServer::slice_tools()
}

#[tool_router(router = slice_tools)]
impl BlackboxServer {
    #[tool(
        name = "bbox_slice_read",
        description = "Read an exact text slice from a file. Pass range as JSON string in MCP, e.g. {\"type\":\"lines\",\"start_line\":10,\"end_line\":20}; object form is also accepted by direct clients."
    )]
    pub(crate) async fn bbox_slice_read(
        &self,
        Parameters(p): Parameters<SliceReadParams>,
    ) -> CallToolResult {
        Self::run_blocking("bbox_slice_read", move || slices::read_slice(&p)).await
    }

    #[tool(
        name = "bbox_slice_move",
        description = "Move an exact text slice. In MCP, pass source_range and insert as JSON strings, e.g. source_range={\"type\":\"lines\",\"start_line\":10,\"end_line\":20}, insert={\"type\":\"append\"}. Dry-run by default; confirmed writes use refactor apply safety."
    )]
    pub(crate) async fn bbox_slice_move(
        &self,
        Parameters(p): Parameters<SliceMoveParams>,
    ) -> CallToolResult {
        let server = self.clone();
        Self::run_blocking("bbox_slice_move", move || {
            let projects = server.state.projects.read().list();
            slices::move_slice(&p, &projects)
        })
        .await
    }

    #[tool(
        name = "bbox_slice_copy",
        description = "Copy an exact text slice. In MCP, pass source_range and insert as JSON strings, e.g. source_range={\"type\":\"exact_text\",\"text\":\"...\"}, insert={\"type\":\"append\"}. Dry-run by default; confirmed writes use refactor apply safety."
    )]
    pub(crate) async fn bbox_slice_copy(
        &self,
        Parameters(p): Parameters<SliceCopyParams>,
    ) -> CallToolResult {
        let server = self.clone();
        Self::run_blocking("bbox_slice_copy", move || {
            let projects = server.state.projects.read().list();
            slices::copy_slice(&p, &projects)
        })
        .await
    }

    #[tool(
        name = "bbox_slice_delete",
        description = "Delete an exact text slice. In MCP, pass source_range as a JSON string, e.g. {\"type\":\"lines\",\"start_line\":10,\"end_line\":20}. Dry-run by default; confirmed writes use refactor apply safety."
    )]
    pub(crate) async fn bbox_slice_delete(
        &self,
        Parameters(p): Parameters<SliceDeleteParams>,
    ) -> CallToolResult {
        let server = self.clone();
        Self::run_blocking("bbox_slice_delete", move || {
            let projects = server.state.projects.read().list();
            slices::delete_slice(&p, &projects)
        })
        .await
    }

    #[tool(
        name = "bbox_slice_insert_text",
        description = "Insert literal text. In MCP, pass insert as a JSON string, e.g. {\"type\":\"line\",\"line\":12,\"placement\":\"before\"} or {\"type\":\"append\"}. Dry-run by default; confirmed writes use refactor apply safety."
    )]
    pub(crate) async fn bbox_slice_insert_text(
        &self,
        Parameters(p): Parameters<SliceInsertTextParams>,
    ) -> CallToolResult {
        let server = self.clone();
        Self::run_blocking("bbox_slice_insert_text", move || {
            let projects = server.state.projects.read().list();
            slices::insert_text_slice(&p, &projects)
        })
        .await
    }

    #[tool(
        name = "bbox_slice_replace",
        description = "Replace an exact text slice. In MCP, pass target_range and optional source_range as JSON strings. Use new_text for literal replacement or source+source_range for slice replacement. Dry-run by default; confirmed writes use refactor apply safety."
    )]
    pub(crate) async fn bbox_slice_replace(
        &self,
        Parameters(p): Parameters<SliceReplaceParams>,
    ) -> CallToolResult {
        let server = self.clone();
        Self::run_blocking("bbox_slice_replace", move || {
            let projects = server.state.projects.read().list();
            slices::replace_slice(&p, &projects)
        })
        .await
    }
}
