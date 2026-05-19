use crate::server::BlackboxServer;
use crate::threads::{ThreadListParams, ThreadParams};

use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::CallToolResult;
use rmcp::{tool, tool_router};

pub(crate) fn router() -> ToolRouter<BlackboxServer> {
    BlackboxServer::threads_tools()
}

#[tool_router(router = threads_tools)]
impl BlackboxServer {
    #[tool(
        name = "bbox_thread",
        description = "Open / continue / resolve / promote / rename / link a work thread."
    )]
    pub(crate) fn bbox_thread(&self, Parameters(p): Parameters<ThreadParams>) -> CallToolResult {
        Self::run("bbox_thread", || {
            let mutation = { self.state.threads.write().thread_mutation(&p) }?;
            if let Some(thread) = mutation.changed_thread.as_ref() {
                if let Err(err) = self.state.idx.write().index_thread(thread) {
                    tracing::warn!(
                        thread_id = %thread.id,
                        error = %err,
                        "thread index sync failed after bbox_thread mutation"
                    );
                }
            }
            if mutation.changed_edges {
                self.rebuild_edge_index_from_stores();
            }
            Ok(mutation.message)
        })
    }

    #[tool(
        name = "bbox_thread_list",
        description = "Scan threads by lifecycle status and idle age."
    )]
    pub(crate) fn bbox_thread_list(
        &self,
        Parameters(p): Parameters<ThreadListParams>,
    ) -> CallToolResult {
        Self::run("bbox_thread_list", || {
            self.state.threads.read().thread_list(&p)
        })
    }
}
