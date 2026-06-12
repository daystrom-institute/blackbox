use crate::notes::{NoteListParams, NoteParams, NoteResolveParams};
use crate::server::BlackboxServer;
use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::CallToolResult;
use rmcp::{tool, tool_router};

pub(crate) fn router() -> ToolRouter<BlackboxServer> {
    BlackboxServer::notes_tools()
}

#[tool_router(router = notes_tools)]
impl BlackboxServer {
    #[tool(
        name = "bbox_note",
        description = "Record a structured side-channel note while working."
    )]
    pub(crate) async fn bbox_note(&self, Parameters(p): Parameters<NoteParams>) -> CallToolResult {
        let start = std::time::Instant::now();
        let server = self.clone();
        // Project resolution does fs/git probes — keep it (and the store
        // mutation behind it) off the tokio workers. Notes key by the
        // registered base scope so orchestrators filtering at round
        // boundaries see worktree-authored notes.
        let create_result = tokio::task::spawn_blocking(move || {
            let mut p = p;
            if let Some(raw) = p.project.clone().filter(|s| !s.trim().is_empty()) {
                let (scope, _write_dir) = server.resolve_project_write_scope(&raw);
                p.project = Some(scope);
            }
            server.state.notes.write().create(&p)
        })
        .await
        .map_err(|e| anyhow::anyhow!("note task failed: {e}"))
        .and_then(std::convert::identity);
        let text = match create_result {
            Ok(text) => text,
            Err(e) => {
                let ms = start.elapsed().as_secs_f64() * 1000.0;
                tracing::warn!(target: "blackbox::tool", tool = "bbox_note", elapsed_ms = ms, error = %e, "err");
                return Self::err_text(&format!("Error: {e:#}"));
            }
        };

        match self.state.persist_notes_durable().await {
            Ok(()) => {
                let ms = start.elapsed().as_secs_f64() * 1000.0;
                tracing::info!(target: "blackbox::tool", tool = "bbox_note", elapsed_ms = ms, bytes = text.len(), "ok");
                Self::ok_text(&text)
            }
            Err(e) => {
                let ms = start.elapsed().as_secs_f64() * 1000.0;
                tracing::warn!(target: "blackbox::tool", tool = "bbox_note", elapsed_ms = ms, error = %e, "err");
                Self::err_text(&format!("Error: {e:#}"))
            }
        }
    }

    #[tool(
        name = "bbox_notes",
        description = "List / filter notes by exact id, kind, project, session, thread, resolution."
    )]
    pub(crate) fn bbox_notes(&self, Parameters(p): Parameters<NoteListParams>) -> CallToolResult {
        Self::run("bbox_notes", || {
            // Worktree filter paths map to the registered base (where notes
            // are keyed); substring filters pass through untouched.
            let mut p = p;
            if let Some(base) = p
                .project
                .as_deref()
                .and_then(|raw| self.rescope_project_filter_value(raw))
            {
                p.project = Some(base);
            }
            self.state.notes.read().list(&p)
        })
    }

    #[tool(
        name = "bbox_note_resolve",
        description = "Mark a note acknowledged or addressed."
    )]
    pub(crate) async fn bbox_note_resolve(
        &self,
        Parameters(p): Parameters<NoteResolveParams>,
    ) -> CallToolResult {
        let start = std::time::Instant::now();
        let text = match self.state.notes.write().resolve(&p) {
            Ok(text) => text,
            Err(e) => {
                let ms = start.elapsed().as_secs_f64() * 1000.0;
                tracing::warn!(target: "blackbox::tool", tool = "bbox_note_resolve", elapsed_ms = ms, error = %e, "err");
                return Self::err_text(&format!("Error: {e:#}"));
            }
        };

        match self.state.persist_notes_durable().await {
            Ok(()) => {
                let ms = start.elapsed().as_secs_f64() * 1000.0;
                tracing::info!(target: "blackbox::tool", tool = "bbox_note_resolve", elapsed_ms = ms, bytes = text.len(), "ok");
                Self::ok_text(&text)
            }
            Err(e) => {
                let ms = start.elapsed().as_secs_f64() * 1000.0;
                tracing::warn!(target: "blackbox::tool", tool = "bbox_note_resolve", elapsed_ms = ms, error = %e, "err");
                Self::err_text(&format!("Error: {e:#}"))
            }
        }
    }
}
