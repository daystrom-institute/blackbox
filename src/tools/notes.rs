use crate::notes::{NoteListParams, NoteParams, NoteResolveParams};
use crate::server::BlackboxServer;
use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::CallToolResult;
use rmcp::schemars;
use rmcp::{tool, tool_router};

#[derive(Debug, Default, serde::Deserialize, rmcp::schemars::JsonSchema)]
pub(crate) struct NoteListToolParams {
    #[serde(flatten)]
    pub filters: NoteListParams,
    /// Continue using next_offset; rows order by created_at descending, id ascending.
    #[serde(default)]
    pub offset: Option<usize>,
    /// Continue an exact (`full=true`) body page using body.next_cursor.
    #[serde(default)]
    pub cursor: Option<String>,
    /// Maximum bytes per exact body page (default 4096, clamped).
    #[serde(default)]
    pub body_limit: Option<usize>,
}

impl From<NoteListParams> for NoteListToolParams {
    fn from(filters: NoteListParams) -> Self {
        Self {
            filters,
            offset: None,
            cursor: None,
            body_limit: None,
        }
    }
}

/// Audit A02: notes are host-owned state — they never write checkout files,
/// so a note's project association resolves through the catalog/filter lane,
/// never the checkout write lease that `attachment_inactive`-fails a
/// published-but-detached project. Identity is stamped from the resolver and
/// a worktree selector keys to its registered base so orchestrator filters
/// see worktree-authored notes.
fn rescope_host_owned_note_project(server: &crate::server::BlackboxServer, p: &mut NoteParams) {
    use bbox_corpus_core::project_selector::ProjectResolution;
    let Some(raw) = p
        .project
        .clone()
        .filter(|selector| !selector.trim().is_empty())
    else {
        return;
    };
    if p.project_id.is_none() {
        if let Ok(project_id) = server.project_filter_identity(&raw) {
            p.project_id = Some(project_id);
        }
    }
    if let Some(ProjectResolution::Attached(ctx)) = server.resolve_project_filter(&raw) {
        p.project = Some(ctx.store_key);
    }
    // Catalog-mode published project: identity stamped, selector kept — the
    // dual-read id arm decides from here. Unresolvable literals keep their
    // bridge-compatible path semantics.
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
            rescope_host_owned_note_project(&server, &mut p);
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
        description = "List note summary pages (default 20, maximum 100), newest first then id. Continue with next_offset; use id and full=true for complete bodies through a content-bound cursor. Filter by kind, project, session, thread, or resolution."
    )]
    pub(crate) fn bbox_notes(
        &self,
        Parameters(p): Parameters<NoteListToolParams>,
    ) -> CallToolResult {
        Self::run("bbox_notes", || {
            // Audit A06: exact read lane. `full=true` (or a body cursor)
            // targets one note by id and pages the complete stored row
            // through the content-bound cursor, which rejects stale
            // continuations and cross-note selectors.
            let exact =
                p.filters.full.unwrap_or(false) || p.cursor.is_some() || p.body_limit.is_some();
            if exact {
                anyhow::ensure!(
                    p.offset.is_none() && p.filters.limit.is_none(),
                    "exact note reads do not accept offset or limit; use body_limit"
                );
            }
            // Worktree filter paths map to the registered base (where notes
            // are keyed); substring filters pass through untouched.
            let offset = p.offset.unwrap_or(0);
            let cursor = p.cursor;
            let body_limit = p.body_limit;
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
            if exact {
                let note = self.state.notes.read().exact(&p)?;
                let selection = format!("note:{}", note.id);
                return Ok(serde_json::to_string(&serde_json::json!({
                    "id": note.id,
                    "body": super::body_page::json_body_page(
                        &selection,
                        &serde_json::to_value(&note)?,
                        cursor.as_deref(),
                        body_limit,
                    )?,
                }))?);
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::state::SharedState;
    use std::sync::Arc;

    fn error_text(result: &rmcp::model::CallToolResult) -> String {
        result
            .content
            .iter()
            .filter_map(|content| content.as_text().map(|text| text.text.clone()))
            .collect::<Vec<_>>()
            .join("\n")
    }

    async fn create_note(server: &BlackboxServer, body: &str) -> String {
        let created = server
            .bbox_note(Parameters(NoteParams {
                kind: "learned".into(),
                body: body.into(),
                task_id: None,
                session_id: None,
                project: None,
                project_id: None,
                thread_id: None,
                provider: None,
                bro: None,
            }))
            .await;
        assert_ne!(created.is_error, Some(true), "{}", error_text(&created));
        let text = error_text(&created);
        text.split_whitespace()
            .find(|word| word.starts_with("note-"))
            .expect("note id in response")
            .to_string()
    }

    /// Audit A02 source-only case: note creation is host-owned state — a
    /// published project with no checkout attachment accepts the write and
    /// stamps identity. The catalog fixture's broker denies checkout access,
    /// so any reach for a write lease fails this test loudly.
    #[tokio::test]
    async fn note_create_resolves_host_owned_identity_without_checkout() {
        use crate::server::state::catalog_fixture::CatalogFixture;

        let fixture = CatalogFixture::new();
        let scope = CatalogFixture::scope(".");
        let project_id = "89bd722f89bd722f89bd722f89bd722f";
        fixture.add_published_project(project_id, &scope);
        let server = fixture.server();

        let created = server
            .bbox_note(Parameters(NoteParams {
                kind: "learned".into(),
                body: "HOST_OWNED_NOTE observation".into(),
                project: Some(project_id.into()),
                task_id: None,
                session_id: None,
                project_id: None,
                thread_id: None,
                provider: None,
                bro: None,
            }))
            .await;
        assert_ne!(created.is_error, Some(true), "{}", error_text(&created));
        {
            let notes = server.state.notes.read();
            let note = notes
                .all()
                .iter()
                .find(|note| note.body.contains("HOST_OWNED_NOTE"))
                .expect("note stored");
            assert_eq!(note.project_id.as_deref(), Some(project_id));
        }

        let listed = server.bbox_notes(Parameters(
            NoteListParams {
                project: Some(project_id.into()),
                ..Default::default()
            }
            .into(),
        ));
        let text = error_text(&listed);
        let page: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(page["total"], 1, "{page}");
        let mut historical: NoteParams = serde_json::from_value(serde_json::json!({
            "kind":"learned", "body":"unstamped historical path note", "project":"former-project-path", "project_id":project_id
        })).unwrap();
        assert!(
            historical.project_id.is_none(),
            "wire input cannot assert resolver-owned identity"
        );
        server.state.notes.write().create(&historical).unwrap();
        // Seed the identity through the internal resolver-owned field. A
        // deserialized project_id is intentionally ignored, and a path-only
        // historical note needs an explicit ledger mapping to be visible.
        historical.project_id = Some(server.project_filter_identity(project_id).unwrap());
        historical.body = "historical path note".into();
        server.state.notes.write().create(&historical).unwrap();
        let note = server
            .state
            .notes
            .read()
            .all()
            .iter()
            .find(|note| note.body == "historical path note")
            .unwrap()
            .clone();
        assert_eq!(note.project_id.as_deref(), Some(project_id));
        assert_eq!(note.project.as_deref(), Some("former-project-path"));
        assert!(server.filter_ledger_paths(project_id).is_empty());
        let before = serde_json::to_value(server.state.notes.read().all()).unwrap();
        let listed = server.bbox_notes(Parameters(
            NoteListParams {
                project: Some(project_id.into()),
                ..Default::default()
            }
            .into(),
        ));
        assert_ne!(listed.is_error, Some(true), "{listed:?}");
        let listed: serde_json::Value = serde_json::from_str(&error_text(&listed)).unwrap();
        assert_eq!(
            listed["total"], 2,
            "unstamped historical paths require ledger mapping"
        );
        assert!(
            listed["notes"]
                .as_array()
                .unwrap()
                .iter()
                .any(|row| row["id"] == note.id)
        );
        // The stored path spelling can differ from the resolving selector;
        // exact recovery must use the same identity arm as discovery.
        let exact = server.bbox_notes(Parameters(
            NoteListParams {
                id: Some(note.id.clone()),
                project: Some(project_id.into()),
                full: Some(true),
                ..Default::default()
            }
            .into(),
        ));
        assert_ne!(exact.is_error, Some(true), "{exact:?}");
        let exact: serde_json::Value = serde_json::from_str(&error_text(&exact)).unwrap();
        let recovered: serde_json::Value =
            serde_json::from_str(exact["body"]["text"].as_str().unwrap()).unwrap();
        assert_eq!(recovered, serde_json::to_value(&note).unwrap());
        let unstamped_id = server
            .state
            .notes
            .read()
            .all()
            .iter()
            .find(|note| note.body == "unstamped historical path note")
            .unwrap()
            .id
            .clone();
        for (id, selector) in [
            (unstamped_id, project_id),
            (note.id.clone(), "unknown-project-selector"),
        ] {
            let refused = server.bbox_notes(Parameters(
                NoteListParams {
                    id: Some(id),
                    project: Some(selector.into()),
                    full: Some(true),
                    ..Default::default()
                }
                .into(),
            ));
            assert_eq!(refused.is_error, Some(true), "{refused:?}");
        }
        let invalid = server.bbox_notes(Parameters(NoteListToolParams {
            filters: NoteListParams {
                id: Some(note.id),
                full: Some(true),
                ..Default::default()
            },
            offset: Some(0),
            ..Default::default()
        }));
        assert_eq!(invalid.is_error, Some(true));
        assert_eq!(
            serde_json::to_value(server.state.notes.read().all()).unwrap(),
            before
        );
    }

    /// Audit A06/A13: exact note reads page the complete stored row through
    /// the content-bound cursor — Unicode reconstruction works, and stale
    /// (note mutated between pages) and cross-note continuations are
    /// rejected.
    #[tokio::test]
    async fn note_exact_read_pages_unicode_and_rejects_stale_and_cross_cursors() {
        let tmp = tempfile::tempdir().unwrap();
        let server = BlackboxServer::new(Arc::new(SharedState::for_test(tmp.path())));
        let body_a = "前提 🦀 仮定 recovery".repeat(500);
        let id_a = create_note(&server, &body_a).await;
        let id_b = create_note(&server, "small body").await;
        let note_a = {
            let notes = server.state.notes.read();
            notes.all().iter().find(|n| n.id == id_a).unwrap().clone()
        };

        // Full reconstruction across content-bound pages.
        let mut reconstructed = String::new();
        let mut cursor: Option<String> = None;
        loop {
            let page = server.bbox_notes(Parameters(NoteListToolParams {
                filters: NoteListParams {
                    id: Some(id_a.clone()),
                    full: Some(true),
                    ..Default::default()
                },
                cursor,
                ..Default::default()
            }));
            let text = error_text(&page);
            let envelope: serde_json::Value = serde_json::from_str(&text).unwrap();
            let body = &envelope["body"];
            assert!(serde_json::to_vec(body).unwrap().len() <= 4096, "{body}");
            reconstructed.push_str(body["text"].as_str().unwrap());
            cursor = body["next_cursor"].as_str().map(str::to_string);
            if cursor.is_none() {
                break;
            }
        }
        assert!(
            reconstructed.len() > 4096,
            "must have paged: {reconstructed:?}"
        );
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&reconstructed).unwrap(),
            serde_json::to_value(&note_a).unwrap(),
            "exact read must reconstruct the full stored row"
        );

        let first_cursor = server.bbox_notes(Parameters(NoteListToolParams {
            filters: NoteListParams {
                id: Some(id_a.clone()),
                full: Some(true),
                ..Default::default()
            },
            ..Default::default()
        }));
        let cursor = serde_json::from_str::<serde_json::Value>(&error_text(&first_cursor)).unwrap()
            ["body"]["next_cursor"]
            .as_str()
            .expect("multi-page body")
            .to_string();

        // Cross-note: note A's cursor does not continue note B.
        let cross = server.bbox_notes(Parameters(NoteListToolParams {
            filters: NoteListParams {
                id: Some(id_b),
                full: Some(true),
                ..Default::default()
            },
            cursor: Some(cursor.clone()),
            ..Default::default()
        }));
        let text = error_text(&cross);
        assert!(
            text.contains("Error") && text.contains("changed"),
            "cross-note cursor must be rejected: {text}"
        );

        // Stale: resolving the note mutates the row (resolution_note), so
        // the old continuation no longer matches the content revision.
        let resolved = server
            .bbox_note_resolve(Parameters(NoteResolveParams {
                id: Some(id_a.clone()),
                ids: Vec::new(),
                resolution: "acknowledged".into(),
                note: Some("mid-pagination mutation".into()),
                notes: std::collections::BTreeMap::new(),
            }))
            .await;
        assert_ne!(resolved.is_error, Some(true), "{}", error_text(&resolved));
        let stale = server.bbox_notes(Parameters(NoteListToolParams {
            filters: NoteListParams {
                id: Some(id_a),
                full: Some(true),
                ..Default::default()
            },
            cursor: Some(cursor),
            ..Default::default()
        }));
        let text = error_text(&stale);
        assert!(
            text.contains("Error") && text.contains("changed"),
            "stale cursor must be rejected: {text}"
        );
    }
}
