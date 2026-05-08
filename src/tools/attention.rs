use crate::server::*;
use crate::*;

pub(crate) fn router() -> ToolRouter<BlackboxServer> {
    BlackboxServer::attention_tools()
}

#[tool_router(router = attention_tools)]
impl BlackboxServer {
    #[tool(
        name = "bbox_pin",
        description = "Persist scoped ambient context for an active execution lane. Pins survive daemon restarts, are never rendered into repo agent files, and are injected only when the current dispatch matches their session/bro/thread/work-item scope."
    )]
    pub(crate) fn bbox_pin(&self, Parameters(p): Parameters<PinParams>) -> CallToolResult {
        Self::run("bbox_pin", || self.state.pins.write().pin(&p))
    }

    #[tool(
        name = "bbox_inbox",
        description = "Aggregate attention layer across every store."
    )]
    pub(crate) fn bbox_inbox(&self, Parameters(p): Parameters<InboxParams>) -> CallToolResult {
        Self::run("bbox_inbox", || {
            let kb = self.state.kb.read();
            let threads = self.state.threads.read();
            let notes = self.state.notes.read();
            let task_store = self.state.task_store.read();
            inbox::compute_inbox(
                &kb,
                &threads,
                &notes,
                &task_store,
                &self.state.whiteboards,
                &p,
            )
        })
    }
}
