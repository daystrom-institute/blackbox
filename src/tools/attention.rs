use crate::server::BlackboxServer;
use crate::{gap_spool, inbox};
use crate::{InboxParams, PinParams};

use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::CallToolResult;
use rmcp::{tool, tool_router};

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
            let import_report = if p.import_gap_spool.unwrap_or(false) {
                let projects = self.state.projects.read();
                let mut notes = self.state.notes.write();
                let state_dir = self.state.config.read().paths.state_dir.clone();
                Some(gap_spool::import_gap_spool(
                    &mut notes, &projects, &state_dir,
                )?)
            } else {
                None
            };

            let kb = self.state.kb.read();
            let threads = self.state.threads.read();
            let notes = self.state.notes.read();
            let task_store = self.state.task_store.read();
            let inbox = inbox::compute_inbox(
                &kb,
                &threads,
                &notes,
                &task_store,
                &self.state.whiteboards,
                &p,
            )?;
            if let Some(report) = import_report {
                let rendered = report.render();
                if rendered.is_empty() {
                    Ok(inbox)
                } else {
                    Ok(format!("{rendered}{inbox}"))
                }
            } else {
                Ok(inbox)
            }
        })
    }
}
