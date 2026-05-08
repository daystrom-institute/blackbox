use crate::server::*;
use crate::*;

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
            let result = { self.state.threads.write().thread(&p) }?;
            if p.action != "get" {
                if let Err(err) = self
                    .state
                    .idx
                    .write()
                    .index_threads_store(&self.state.threads.read())
                {
                    tracing::warn!(error = %err, "thread index sync failed after bbox_thread mutation");
                }
                self.rebuild_edge_index_from_stores();
            }
            Ok(result)
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
