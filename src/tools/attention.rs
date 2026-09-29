use crate::pins::PinParams;
use crate::server::BlackboxServer;

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
        description = "Persist scoped ambient context for an active execution lane. Pins survive daemon restarts, are never rendered into repo agent files, and are injected only when the current dispatch matches their session/bro/thread/work-item scope. action=list returns bounded preview pages (200-char bodies); id + full=true is the exact recovery read through a content-bound body cursor."
    )]
    pub(crate) async fn bbox_pin(&self, Parameters(p): Parameters<PinParams>) -> CallToolResult {
        let start = std::time::Instant::now();
        let action = p.action.clone();
        let server = self.clone();
        // Project resolution does fs/git probes — keep it (and the store
        // mutation behind it) off the tokio workers.
        let pin_result = tokio::task::spawn_blocking(move || {
            let mut p = p;
            // Audit A03: action/scope/required-field validation precedes any
            // project resolution, so a typo'd enum never hides behind (or
            // trips over) a locality error.
            validate_pin_request(&p)?;
            // Audit A02: pins are host-owned state. Their project association
            // resolves through the catalog/filter lane — never the checkout
            // write lease that attachment_inactive-fails a published project.
            let diagnostic = rescope_host_owned_project(&server, &mut p);
            if p.action != "list"
                && let Some(error) = diagnostic.as_ref()
            {
                anyhow::bail!("{error}; mutations require a registered project identity");
            }
            let diagnostics: Vec<String> = diagnostic.into_iter().collect();
            let exact = p.full.unwrap_or(false) || p.cursor.is_some() || p.body_limit.is_some();
            if exact {
                // Audit A06: exact recovery read. The content-bound cursor
                // rejects stale continuations and cross-pin selectors.
                let pin = server.state.pins.read().exact(&p)?;
                let selection = format!("pin:{}", pin.id);
                return Ok(serde_json::to_string(&serde_json::json!({
                    "id": pin.id,
                    "body": super::body_page::json_body_page(
                        &selection,
                        &serde_json::to_value(&pin)?,
                        p.cursor.as_deref(),
                        p.body_limit,
                    )?,
                }))?);
            }
            if p.action == "list" {
                return Ok(serde_json::to_string(
                    &server.state.pins.read().list_page(&p, &diagnostics)?,
                )?);
            }
            server.state.pins.write().pin(&p)
        })
        .await
        .map_err(|e| anyhow::anyhow!("pin task failed: {e}"))
        .and_then(std::convert::identity);
        let text = match pin_result {
            Ok(text) => text,
            Err(e) => {
                let ms = start.elapsed().as_secs_f64() * 1000.0;
                tracing::warn!(target: "blackbox::tool", tool = "bbox_pin", elapsed_ms = ms, error = %e, "err");
                return Self::err_text(&format!("Error: {e:#}"));
            }
        };

        if action != "list" {
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
}

/// Audit A03: validate the pin request's shape before any project/locality
/// work runs. Mirrors the store's messages so callers see the same errors
/// they would from the store, just earlier and without resolution side
/// effects.
fn validate_pin_request(p: &PinParams) -> anyhow::Result<()> {
    match p.action.as_str() {
        "set" | "list" | "delete" => {}
        other => anyhow::bail!("unknown pin action: {other} (use set, list, delete)"),
    }
    if let Some(raw) = p.scope.as_deref() {
        std::str::FromStr::from_str(raw)
            .map(|_: crate::pins::PinScope| ())
            .map_err(|_| {
                anyhow::anyhow!("invalid scope: {raw:?} (use session, bro, thread, work_item)")
            })?;
    }
    if p.action != "list"
        && (p.full.is_some()
            || p.cursor.is_some()
            || p.body_limit.is_some()
            || p.limit.is_some()
            || p.offset.is_some())
    {
        anyhow::bail!("full, cursor, body_limit, limit and offset require action=list");
    }
    if p.action != "set" {
        anyhow::ensure!(
            p.content.is_none() && p.title.is_none() && p.expires_at.is_none(),
            "content, title and expires_at require action=set"
        );
    }
    if p.action == "delete" {
        anyhow::ensure!(
            p.id.as_deref().is_some_and(|id| !id.trim().is_empty()),
            "id is required for action=delete"
        );
        anyhow::ensure!(
            p.scope.is_none() && p.target.is_none(),
            "scope and target filter reads or select a set operation; delete selects id"
        );
    }
    if p.full == Some(true) || p.cursor.is_some() || p.body_limit.is_some() {
        anyhow::ensure!(p.id.is_some(), "exact reads require id");
        anyhow::ensure!(
            p.limit.is_none() && p.offset.is_none(),
            "limit and offset apply only to summary lists"
        );
    }
    if p.action == "set" {
        if p.scope.is_none() {
            anyhow::bail!("scope is required for action=set");
        }
        if !p
            .target
            .as_deref()
            .is_some_and(|target| !target.trim().is_empty())
        {
            anyhow::bail!("target is required for action=set");
        }
        if !p
            .content
            .as_deref()
            .is_some_and(|content| !content.trim().is_empty())
        {
            anyhow::bail!("content is required for action=set");
        }
    }
    Ok(())
}

/// Audit A02: host-owned project association for pin reads AND writes.
/// Pins never touch checkout-owned files, so their project link resolves
/// through the catalog/filter lane (the same one knowledge lists use) —
/// never the checkout write lease, which `attachment_inactive`-fails a
/// published-but-detached project. Identity arm + ledger arm + worktree→base
/// rewrite with the checkout alias lane, mirroring `rescope_project_filter`
/// in knowledge.rs. Returns the unresolvable-selector diagnostic so a list
/// can say "named no registered project" instead of rendering an empty
/// result that looks like an empty store.
fn rescope_host_owned_project(
    server: &crate::server::BlackboxServer,
    p: &mut PinParams,
) -> Option<String> {
    use bbox_corpus_core::project_selector::{ProjectResolution, ResolvedAttachment};
    let raw = p
        .project
        .clone()
        .filter(|selector| !selector.trim().is_empty())?;
    let mut diagnostic = None;
    if p.project_id.is_none() {
        match server.project_filter_identity(&raw) {
            Ok(project_id) => p.project_id = Some(project_id),
            Err(text) => diagnostic = Some(text),
        }
    }
    let Some(resolution) = server.resolve_project_filter(&raw) else {
        return diagnostic;
    };
    if let Some(project_id) = resolution.project_id() {
        p.project_ledger_paths = server.ledger_historical_paths(project_id);
    }
    let ProjectResolution::Attached(ctx) = resolution else {
        // Catalog-mode published project: the identity is stamped and the
        // caller's selector kept — the dual-read id arm decides from here.
        return diagnostic;
    };
    let checkout_dir = match &ctx.attachment {
        ResolvedAttachment::V1Compat { checkout_dir, .. } => checkout_dir.clone(),
        ResolvedAttachment::Catalog {
            checkout_project_dir,
            ..
        } => checkout_project_dir.clone(),
    };
    if checkout_dir != ctx.store_key {
        p.project_alias = Some(checkout_dir);
    }
    p.project = Some(ctx.store_key);
    diagnostic
}

#[cfg(test)]
mod tests {
    use crate::server::BlackboxServer;
    use crate::server::state::SharedState;
    use rmcp::handler::server::wrapper::Parameters;
    use std::path::Path;
    use std::sync::Arc;

    fn error_text(result: &rmcp::model::CallToolResult) -> String {
        result
            .content
            .iter()
            .filter_map(|content| content.as_text().map(|text| text.text.clone()))
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn run_git(cwd: &Path, args: &[&str]) {
        let out = std::process::Command::new("git")
            .arg("-C")
            .arg(cwd)
            .args(args)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    /// A pin set from inside an in-tree linked worktree must key to the
    /// registered BASE project (the durable scope) and inject for dispatches
    /// rooted in the base AND in the worktree — exact-match pin scoping was
    /// the sharpest silent failure of the worktree corpus-ops class.
    #[tokio::test]
    async fn bbox_pin_from_worktree_keys_base_and_injects_for_both_cwds() {
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().join("repo");
        std::fs::create_dir_all(&base).unwrap();
        run_git(&base, &["init", "-b", "main"]);
        run_git(&base, &["config", "user.email", "t@example.com"]);
        run_git(&base, &["config", "user.name", "T"]);
        std::fs::write(base.join("README.md"), "base").unwrap();
        run_git(&base, &["add", "."]);
        run_git(&base, &["commit", "-m", "init"]);
        let base_canon = base.canonicalize().unwrap();

        let worktree = base.join(".claude").join("worktrees").join("wt");
        std::fs::create_dir_all(worktree.parent().unwrap()).unwrap();
        run_git(
            &base,
            &[
                "worktree",
                "add",
                "-b",
                "arc/pin",
                worktree.to_str().unwrap(),
                "HEAD",
            ],
        );
        let wt = worktree
            .canonicalize()
            .unwrap()
            .to_string_lossy()
            .into_owned();
        let base_str = base_canon.to_string_lossy().into_owned();

        let server = BlackboxServer::new(Arc::new(SharedState::for_test(tmp.path())));
        server
            .state
            .project_authority
            .bridge_registry()
            .unwrap()
            .write()
            .register_path(&base_canon)
            .unwrap();

        let set = server
            .bbox_pin(Parameters(crate::pins::PinParams {
                action: "set".into(),
                content: Some("WORKTREE_PIN_MARKER guidance".into()),
                title: Some("arc pin".into()),
                scope: Some("bro".into()),
                target: Some("executor".into()),
                project: Some(wt.clone()),
                ..Default::default()
            }))
            .await;
        assert_ne!(set.is_error, Some(true), "pin set failed: {set:?}");

        // Injects for a dispatch rooted at the base…
        let from_base = server
            .ambient_pin_block(Some(&base_str), Some("executor"), None, None, None)
            .expect("base-rooted dispatch should receive the pin");
        assert!(from_base.contains("WORKTREE_PIN_MARKER"));

        // …and for a dispatch rooted in the worktree (cwd resolves to base).
        let from_worktree = server
            .ambient_pin_block(Some(&wt), Some("executor"), None, None, None)
            .expect("worktree-rooted dispatch should receive the pin");
        assert!(from_worktree.contains("WORKTREE_PIN_MARKER"));

        // A different project still doesn't receive it.
        assert!(
            server
                .ambient_pin_block(Some("/repo/other"), Some("executor"), None, None, None)
                .is_none()
        );
    }

    /// Audit A02 live case: pins are host-owned state, so a published project
    /// with NO checkout attachment must still accept pin writes and serve pin
    /// lists. The catalog fixture's broker denies checkout access outright —
    /// any reach for a write lease fails this test loudly.
    #[tokio::test]
    async fn pin_reads_and_writes_resolve_host_owned_identity_without_checkout() {
        use crate::server::state::catalog_fixture::CatalogFixture;

        let fixture = CatalogFixture::new();
        let scope = CatalogFixture::scope(".");
        let project_id = "89bd722f89bd722f89bd722f89bd722f";
        fixture.add_published_project(project_id, &scope);
        let server = fixture.server();

        let set = server
            .bbox_pin(Parameters(crate::pins::PinParams {
                action: "set".into(),
                content: Some("HOST_OWNED_PIN guidance".into()),
                title: Some("host owned".into()),
                scope: Some("bro".into()),
                target: Some("executor".into()),
                project: Some(project_id.into()),
                ..Default::default()
            }))
            .await;
        assert_ne!(set.is_error, Some(true), "pin set failed: {set:?}");
        {
            let pins = server.state.pins.read();
            let pin = pins
                .all()
                .iter()
                .find(|pin| pin.content.contains("HOST_OWNED_PIN"))
                .expect("pin stored");
            assert_eq!(pin.project_id.as_deref(), Some(project_id));
        }

        // The same selector lists it back through the identity arm.
        let listed = server
            .bbox_pin(Parameters(crate::pins::PinParams {
                action: "list".into(),
                project: Some(project_id.into()),
                ..Default::default()
            }))
            .await;
        assert_ne!(listed.is_error, Some(true), "{listed:?}");
        let body = error_text(&listed);
        let page: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(page["total"], 1, "{page}");
        assert_eq!(page["pins"][0]["project_id"], project_id);
        assert!(
            page["pins"][0]["content"]
                .as_str()
                .unwrap()
                .contains("HOST_OWNED_PIN")
        );

        // An unresolvable selector reports itself instead of a bare empty
        // page that reads as "no pins exist".
        let unknown = server
            .bbox_pin(Parameters(crate::pins::PinParams {
                action: "list".into(),
                project: Some("no-such-project".into()),
                ..Default::default()
            }))
            .await;
        let body = error_text(&unknown);
        let page: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(page["total"], 0, "{page}");
        let diagnostics = page["diagnostics"].as_array().expect("diagnostic present");
        assert!(
            diagnostics
                .iter()
                .any(|line| line.as_str().unwrap().contains("no-such-project")),
            "{page}"
        );

        let before_count = server.state.pins.read().all().len();
        let rejected = server
            .bbox_pin(Parameters(crate::pins::PinParams {
                action: "set".into(),
                scope: Some("bro".into()),
                target: Some("executor".into()),
                content: Some("must not persist".into()),
                project: Some("no-such-project".into()),
                ..Default::default()
            }))
            .await;
        assert_eq!(rejected.is_error, Some(true));
        assert_eq!(server.state.pins.read().all().len(), before_count);
        let rejected = server
            .bbox_pin(Parameters(crate::pins::PinParams {
                action: "set".into(),
                scope: Some("bro".into()),
                target: Some("executor".into()),
                content: Some("must not become a read".into()),
                full: Some(true),
                ..Default::default()
            }))
            .await;
        assert_eq!(rejected.is_error, Some(true));
        assert!(error_text(&rejected).contains("require action=list"));
        assert_eq!(server.state.pins.read().all().len(), before_count);

        // Audit A03 ordering: an invalid scope errors even when the project
        // selector is ALSO unknown — validation precedes locality work.
        let invalid = server
            .bbox_pin(Parameters(crate::pins::PinParams {
                action: "list".into(),
                scope: Some("bogus".into()),
                project: Some("no-such-project".into()),
                ..Default::default()
            }))
            .await;
        let body = error_text(&invalid);
        assert!(
            body.contains("invalid scope") && !body.contains("diagnostics"),
            "scope validation must precede locality: {body}"
        );
    }

    /// Audit A02 ledger arm: a path-only pin keyed under a historical path of
    /// a relocated project stays visible when listing by the project's
    /// current identity.
    #[tokio::test]
    async fn pin_list_sees_path_only_rows_through_the_ledger_arm() {
        use crate::server::state::catalog_fixture::CatalogFixture;
        use bbox_corpus_core::project_catalog::{
            LegacyPathBindingId, LegacyPathBindingStatus, LegacyPathLedgerEntry,
            LegacyPathRelationship,
        };

        let fixture = CatalogFixture::new();
        let scope = CatalogFixture::scope(".");
        let project_id = "aaaabbbbccccddddaaaabbbbccccdddd";
        fixture.add_published_project(project_id, &scope);
        let historical = "/tmp/legacy-pin-root";
        let binding_id =
            LegacyPathBindingId::parse("lpb_11111111111111111111111111111111").unwrap();
        let epoch = fixture.store().snapshot().unwrap().epoch();
        fixture
            .store()
            .transact(epoch, |_catalog, attachments| {
                attachments.legacy_path_bindings.insert(
                    binding_id.clone(),
                    LegacyPathLedgerEntry {
                        legacy_path_binding_id: binding_id,
                        historical_path: historical.into(),
                        source_store: "synthetic".into(),
                        source_row_id: "row-1".into(),
                        member_row_count: 1,
                        member_commitment_sha256: "a".repeat(64),
                        inventory_epoch: 1,
                        status: LegacyPathBindingStatus::Mapped {
                            project_id: bbox_corpus_core::project_catalog::ProjectId::parse(
                                project_id,
                            )
                            .unwrap(),
                            relationship: LegacyPathRelationship::Root,
                        },
                    },
                );
                Ok(())
            })
            .unwrap();
        let server = fixture.server();

        // A pin keyed under the historical path, carrying no project id.
        let set = server.state.pins.write().pin(&crate::pins::PinParams {
            action: "set".into(),
            content: Some("LEDGER_ARM_PIN".into()),
            title: Some("legacy".into()),
            scope: Some("bro".into()),
            target: Some("executor".into()),
            project: Some(historical.into()),
            ..Default::default()
        });
        set.unwrap();

        let listed = server
            .bbox_pin(Parameters(crate::pins::PinParams {
                action: "list".into(),
                project: Some(project_id.into()),
                ..Default::default()
            }))
            .await;
        let body = error_text(&listed);
        let page: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(page["total"], 1, "ledger arm must reach the row: {page}");
        assert!(
            page["pins"][0]["content"]
                .as_str()
                .unwrap()
                .contains("LEDGER_ARM_PIN")
        );
    }

    /// Audit A06/A13: exact pin reads page the full row through the
    /// content-bound cursor — full Unicode reconstruction, and stale or
    /// cross-selector continuations are rejected.
    #[tokio::test]
    async fn pin_exact_body_pages_reconstruct_unicode_and_reject_stale_and_cross_cursors() {
        let tmp = tempfile::tempdir().unwrap();
        let server = BlackboxServer::new(Arc::new(SharedState::for_test(tmp.path())));
        let body_a = "指針 🦀 recovery guidance".repeat(600);
        for (target, content) in [("executor", body_a.clone()), ("other", "small body".into())] {
            let set = server
                .bbox_pin(Parameters(crate::pins::PinParams {
                    action: "set".into(),
                    content: Some(content),
                    title: Some("exact".into()),
                    scope: Some("bro".into()),
                    target: Some(target.into()),
                    ..Default::default()
                }))
                .await;
            assert_ne!(set.is_error, Some(true), "{set:?}");
        }
        let (id_a, id_b) = {
            let pins = server.state.pins.read();
            let a = pins
                .all()
                .iter()
                .find(|pin| pin.target == "executor")
                .unwrap();
            let b = pins.all().iter().find(|pin| pin.target == "other").unwrap();
            (a.id.clone(), b.id.clone())
        };
        let pin_a = {
            let pins = server.state.pins.read();
            pins.all()
                .iter()
                .find(|pin| pin.id == id_a)
                .unwrap()
                .clone()
        };

        // Full reconstruction across content-bound pages.
        let mut reconstructed = String::new();
        let mut cursor: Option<String> = None;
        loop {
            let page_call = server
                .bbox_pin(Parameters(crate::pins::PinParams {
                    action: "list".into(),
                    id: Some(id_a.clone()),
                    full: Some(true),
                    cursor,
                    ..Default::default()
                }))
                .await;
            let text = error_text(&page_call);
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
            serde_json::to_value(&pin_a).unwrap(),
            "exact read must reconstruct the full stored row"
        );

        // Cross-selector: pin A's cursor does not continue pin B.
        let first = server
            .bbox_pin(Parameters(crate::pins::PinParams {
                action: "list".into(),
                id: Some(id_a.clone()),
                full: Some(true),
                ..Default::default()
            }))
            .await;
        let text = error_text(&first);
        let cursor =
            serde_json::from_str::<serde_json::Value>(&text).unwrap()["body"]["next_cursor"]
                .as_str()
                .expect("multi-page body")
                .to_string();
        let cross = server
            .bbox_pin(Parameters(crate::pins::PinParams {
                action: "list".into(),
                id: Some(id_b),
                full: Some(true),
                cursor: Some(cursor.clone()),
                ..Default::default()
            }))
            .await;
        let text = error_text(&cross);
        assert!(
            text.contains("Error") && text.contains("changed"),
            "cross-selector cursor must be rejected: {text}"
        );

        // Stale: mutating the pin invalidates the old continuation.
        let update = server
            .bbox_pin(Parameters(crate::pins::PinParams {
                action: "set".into(),
                id: Some(id_a.clone()),
                content: Some("replacement body".into()),
                title: Some("exact".into()),
                scope: Some("bro".into()),
                target: Some("executor".into()),
                ..Default::default()
            }))
            .await;
        assert_ne!(update.is_error, Some(true), "{update:?}");
        let stale = server
            .bbox_pin(Parameters(crate::pins::PinParams {
                action: "list".into(),
                id: Some(id_a),
                full: Some(true),
                cursor: Some(cursor),
                ..Default::default()
            }))
            .await;
        let text = error_text(&stale);
        assert!(
            text.contains("Error") && text.contains("changed"),
            "stale cursor must be rejected: {text}"
        );
    }
}
