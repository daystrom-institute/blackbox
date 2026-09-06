use crate::notes::{NoteListParams, NoteParams, NoteResolveParams};
use crate::server::BlackboxServer;
use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::CallToolResult;
use rmcp::{tool, tool_router};

#[derive(Debug, Default, serde::Deserialize, rmcp::schemars::JsonSchema)]
pub(crate) struct NoteListToolParams {
    #[serde(flatten)]
    pub filters: NoteListParams,
    /// Continue using next_offset; rows order by created_at descending, id ascending.
    #[serde(default)]
    pub offset: Option<usize>,
}

impl From<NoteListParams> for NoteListToolParams {
    fn from(filters: NoteListParams) -> Self {
        Self {
            filters,
            offset: None,
        }
    }
}

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
                let (scope, resolved_project_id, _write_dir) =
                    server.resolve_project_write_scope_with_id(&raw)?;
                p.project_id = resolved_project_id;
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
        description = "List note summary pages (default 20, maximum 100), newest first then id. Continue with next_offset; use id and full=true for complete bodies. Filter by kind, project, session, thread, or resolution."
    )]
    pub(crate) fn bbox_notes(
        &self,
        Parameters(p): Parameters<NoteListToolParams>,
    ) -> CallToolResult {
        Self::run("bbox_notes", || {
            // Worktree filter paths map to the registered base (where notes
            // are keyed); substring filters pass through untouched.
            let offset = p.offset.unwrap_or(0);
            let mut p = p.filters;
            if let Some(raw) = p.project.clone() {
                // Catalog-mode ledger arm (plan §8.2): path-only notes still
                // keyed under one of this project's historical paths stay
                // visible after relocation. Empty in bridge mode.
                p.project_ledger_paths = self.filter_ledger_paths(&raw);
                // Query-side ids come from the resolver (§8.2): a resolving
                // project filter also arms the id arm, so stamped rows match
                // whatever path they were keyed under.
                if p.project_id.is_none() {
                    p.project_id = self
                        .resolve_project_filter(&raw)
                        .and_then(|resolution| resolution.project_id().map(str::to_owned));
                }
                if let Some(base) = self.rescope_project_filter_value(&raw) {
                    p.project = Some(base);
                }
            } else if let Some(raw_id) = p.project_id.clone() {
                // A caller-supplied id is a selector, not an assertion: it
                // resolves through the engine to the canonical id (and the
                // ledger arm), and an unknown id simply matches nothing.
                if let Some(resolution) = self.resolve_project_filter(&raw_id)
                    && let Some(resolved) = resolution.project_id()
                {
                    p.project_id = Some(resolved.to_owned());
                    p.project_ledger_paths = self.ledger_historical_paths(resolved);
                }
            }
            Ok(serde_json::to_string(
                &self.state.notes.read().list_page(&p, offset)?,
            )?)
        })
    }

    #[tool(
        name = "bbox_note_resolve",
        description = "Mark one note, or a batch of notes, acknowledged or addressed."
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
