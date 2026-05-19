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
        description = "Read an exact text slice from a file by line range, markers, exact text, or byte range."
    )]
    pub(crate) fn bbox_slice_read(
        &self,
        Parameters(p): Parameters<SliceReadParams>,
    ) -> CallToolResult {
        Self::run("bbox_slice_read", || slices::read_slice(&p))
    }

    #[tool(
        name = "bbox_slice_move",
        description = "Move an exact text slice from one file/location to another with a dry-run plan by default and confirmed writes via the refactor apply safety path."
    )]
    pub(crate) fn bbox_slice_move(
        &self,
        Parameters(p): Parameters<SliceMoveParams>,
    ) -> CallToolResult {
        Self::run("bbox_slice_move", || {
            let projects = self.state.projects.read().list();
            slices::move_slice(&p, &projects)
        })
    }

    #[tool(
        name = "bbox_slice_copy",
        description = "Copy an exact text slice into another file/location with a dry-run plan by default and confirmed writes via the refactor apply safety path."
    )]
    pub(crate) fn bbox_slice_copy(
        &self,
        Parameters(p): Parameters<SliceCopyParams>,
    ) -> CallToolResult {
        Self::run("bbox_slice_copy", || {
            let projects = self.state.projects.read().list();
            slices::copy_slice(&p, &projects)
        })
    }

    #[tool(
        name = "bbox_slice_delete",
        description = "Delete an exact text slice with a dry-run plan by default and confirmed writes via the refactor apply safety path."
    )]
    pub(crate) fn bbox_slice_delete(
        &self,
        Parameters(p): Parameters<SliceDeleteParams>,
    ) -> CallToolResult {
        Self::run("bbox_slice_delete", || {
            let projects = self.state.projects.read().list();
            slices::delete_slice(&p, &projects)
        })
    }

    #[tool(
        name = "bbox_slice_insert_text",
        description = "Insert literal text at a file location with a dry-run plan by default and confirmed writes via the refactor apply safety path."
    )]
    pub(crate) fn bbox_slice_insert_text(
        &self,
        Parameters(p): Parameters<SliceInsertTextParams>,
    ) -> CallToolResult {
        Self::run("bbox_slice_insert_text", || {
            let projects = self.state.projects.read().list();
            slices::insert_text_slice(&p, &projects)
        })
    }

    #[tool(
        name = "bbox_slice_replace",
        description = "Replace an exact text slice with literal text or another selected slice, dry-run by default and confirmed through the refactor apply safety path."
    )]
    pub(crate) fn bbox_slice_replace(
        &self,
        Parameters(p): Parameters<SliceReplaceParams>,
    ) -> CallToolResult {
        Self::run("bbox_slice_replace", || {
            let projects = self.state.projects.read().list();
            slices::replace_slice(&p, &projects)
        })
    }
}
