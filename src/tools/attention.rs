use crate::inbox::InboxParams;
use crate::pins::PinParams;
use crate::server::BlackboxServer;
use crate::{gap_spool, inbox};

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
    pub(crate) async fn bbox_pin(&self, Parameters(p): Parameters<PinParams>) -> CallToolResult {
        let start = std::time::Instant::now();
        let text = match self.state.pins.write().pin(&p) {
            Ok(text) => text,
            Err(e) => {
                let ms = start.elapsed().as_secs_f64() * 1000.0;
                tracing::warn!(target: "blackbox::tool", tool = "bbox_pin", elapsed_ms = ms, error = %e, "err");
                return Self::err_text(&format!("Error: {e:#}"));
            }
        };

        if p.action != "list" {
            if let Err(e) = self.state.persist_pins_durable().await {
                let ms = start.elapsed().as_secs_f64() * 1000.0;
                tracing::warn!(target: "blackbox::tool", tool = "bbox_pin", elapsed_ms = ms, error = %e, "err");
                return Self::err_text(&format!("Error: {e:#}"));
            }
        }

        let ms = start.elapsed().as_secs_f64() * 1000.0;
        tracing::info!(target: "blackbox::tool", tool = "bbox_pin", elapsed_ms = ms, bytes = text.len(), "ok");
        Self::ok_text(&text)
    }

    #[tool(
        name = "bbox_inbox",
        description = "Aggregate attention layer across every store."
    )]
    pub(crate) async fn bbox_inbox(
        &self,
        Parameters(p): Parameters<InboxParams>,
    ) -> CallToolResult {
        // Gap-spool import is full-store disk I/O under the gaps write lock,
        // and compute_inbox stacks five store read guards — run on the
        // blocking pool, not a tokio worker. (The import's guards drop before
        // the read stack below; only in-memory work happens under the stack.)
        let server = self.clone();
        Self::run_blocking("bbox_inbox", move || {
            let import_report = if p.import_gap_spool.unwrap_or(false) {
                let projects = server.state.projects.read();
                let mut gaps = server.state.gaps.write();
                let state_dir = server.state.config.read().paths.state_dir.clone();
                Some(gap_spool::import_gap_spool(
                    &mut gaps, &projects, &state_dir,
                )?)
            } else {
                None
            };

            let kb = server.state.kb.read();
            let threads = server.state.threads.read();
            let notes = server.state.notes.read();
            let gaps = server.state.gaps.read();
            let task_store = server.state.task_store.read();
            let inbox = inbox::compute_inbox(
                &kb,
                &threads,
                &notes,
                &gaps,
                &task_store,
                &server.state.whiteboards,
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
        .await
    }
}
