use crate::knowledge::{AbsorbParams, BootstrapParams, RenderParams, ReviewParams};
use crate::server::BlackboxServer;

use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::CallToolResult;
use rmcp::{tool, tool_router};

pub(crate) fn router() -> ToolRouter<BlackboxServer> {
    BlackboxServer::render_tools()
}

/// Rescope a project render request through worktree→base project resolution.
/// Project-scoped entries live under the registered base canonical path, so a
/// managed/linked worktree path (or a subdirectory) would filter to nothing
/// and render bare provider files. After this:
///   - plain subdirectory of a registered root → render into the ROOT
///     (provider files belong at the top of the checkout), filter by root;
///   - worktree of a registered repo (in-tree or out-of-tree) → render into
///     the WORKTREE checkout root, filter by the registered base path
///     (`scope_project`);
///   - unregistered paths and non-path values → untouched.
#[cfg(test)]
fn rescope_render_project(p: &mut RenderParams, projects: &[crate::projects::ProjectRecord]) {
    let Some(raw) = p.project.as_deref().filter(|raw| raw.starts_with('/')) else {
        return;
    };
    let Some((scope, checkout)) = crate::projects::resolve_scope_and_checkout_dir(raw, projects)
    else {
        return;
    };
    if checkout != scope {
        p.scope_project = Some(scope);
        p.project = Some(checkout);
    } else {
        p.project = Some(scope);
    }
}

#[tool_router(router = render_tools)]
impl BlackboxServer {
    pub(crate) fn lint_session_knowledge(&self) -> anyhow::Result<String> {
        let view = self.session_knowledge_view(None, None)?;
        let output = view.knowledge.lint()?;
        Ok(view.append_diagnostics(output))
    }

    pub(crate) fn review_session_knowledge(&self, p: &ReviewParams) -> anyhow::Result<String> {
        let mut view = self.session_knowledge_view(None, None)?;
        let output = view.knowledge.review(p)?;
        Ok(view.append_diagnostics(output))
    }

    pub(crate) fn absorb_session_knowledge(&self, p: &AbsorbParams) -> anyhow::Result<String> {
        let requested_project = (p.scope.as_deref().unwrap_or("project") == "project")
            .then_some(p.project.as_deref())
            .flatten();
        let mut view = self.session_knowledge_view(requested_project, None)?;
        let output = view.knowledge.absorb(p)?;
        Ok(view.append_diagnostics(output))
    }

    pub(crate) fn bootstrap_session_knowledge(
        &self,
        p: &BootstrapParams,
    ) -> anyhow::Result<String> {
        // Filter-class engine resolution (phase-2 §9.2): bootstrap tolerates
        // an unrecognized selector by proceeding unscoped, exactly as before.
        let scope_project = self
            .resolve_project_filter(&p.project)
            .and_then(|resolution| resolution.store_key().map(str::to_owned));
        let view = self.session_knowledge_view(Some(&p.project), None)?;
        let output = view
            .knowledge
            .bootstrap_with_scope(p, scope_project.as_deref())?;
        Ok(view.append_diagnostics(output))
    }

    #[tool(
        name = "bbox_render",
        description = "Render entries into CLAUDE.md / AGENTS.md / GEMINI.md."
    )]
    pub(crate) async fn bbox_render(
        &self,
        Parameters(p): Parameters<RenderParams>,
    ) -> CallToolResult {
        let server = self.clone();
        Self::run_blocking("bbox_render", move || {
            let mut p = p;
            let project_render = p.project.is_some()
                && matches!(p.scope.as_deref().unwrap_or("both"), "project" | "both");
            let rendered = if project_render {
                let raw = p.project.clone().expect("project render has a target");
                // Project identity is resolved before any render target is
                // opened (plan section 8, P5-E render item 1). The write root
                // below always comes from the acquired lease; no branch
                // re-derives one from a project record path.
                let resolution = server.resolve_project_write(&raw)?;
                let durable_scope = resolution.durable_scope;
                let resolved_project_id = resolution.project_id;
                let checkout = resolution.checkout_scope;
                if let Some(checkout) = checkout.as_ref() {
                    server.register_dark_knowledge_checkout(checkout)?;
                }
                let view = server.session_knowledge_view(
                    Some(&durable_scope),
                    p.provisional.as_deref(),
                )?;
                let mut render = |root: &std::path::Path| {
                    p.project = Some(root.to_string_lossy().into_owned());
                    p.scope_project = Some(durable_scope.clone());
                    view.knowledge.render(&p)
                };
                if let Some(checkout) = checkout.as_ref() {
                    crate::server::checkout_access::with_resolved_checkout_access(
                        &server.state.checkout_access,
                        checkout,
                        bbox_indexing::checkout_access::CheckoutAccessKind::RenderFileProvider,
                        bbox_indexing::checkout_access::CheckoutAccessIntent::Write,
                        |lease| render(lease.project_root()),
                    )?
                } else {
                    let project_id = resolved_project_id.ok_or_else(|| {
                        anyhow::anyhow!(
                            "error.attachment_required: project render target is not a registered attachment"
                        )
                    })?;
                    if server.state.project_authority.is_bridge() {
                        crate::server::checkout_access::with_selected_project_access(
                            &server.state.checkout_access,
                            &project_id,
                            bbox_indexing::checkout_access::CheckoutAccessKind::RenderFileProvider,
                            bbox_indexing::checkout_access::CheckoutAccessIntent::Write,
                            |lease| render(lease.project_root()),
                        )?
                    } else {
                        // Catalog render gates on `render_output` alone. The
                        // bridge helper first takes a `PublisherConfigTreeRead`
                        // lease to discover scope, which rides `repo_knowledge`
                        // (D-032) and would deny a render-capable attachment
                        // for lacking an unrelated capability; the catalog row
                        // already carries the scope.
                        let broker = &server.state.checkout_access;
                        let lease = crate::server::checkout_access::acquire_catalog_project_lease(
                            &server,
                            broker,
                            &project_id,
                            bbox_indexing::checkout_access::CheckoutAccessKind::RenderFileProvider,
                            bbox_indexing::checkout_access::CheckoutAccessIntent::Write,
                        )?;
                        let rendered = render(lease.project_root());
                        broker.revalidate(&lease).map_err(anyhow::Error::new)?;
                        rendered?
                    }
                }
            } else {
                let scope_project = p.scope_project.as_deref().or(p.project.as_deref());
                let view =
                    server.session_knowledge_view(scope_project, p.provisional.as_deref())?;
                view.knowledge.render(&p)?
            };
            let view = server.session_knowledge_view(
                p.scope_project.as_deref().or(p.project.as_deref()),
                p.provisional.as_deref(),
            )?;
            match view.diagnostics_text() {
                Some(diagnostics) => Ok(format!("{rendered}\n{diagnostics}")),
                None => Ok(rendered),
            }
        })
        .await
    }

    #[tool(
        name = "bbox_absorb",
        description = "Compatibility no-op for the old rendered-file import path."
    )]
    pub(crate) async fn bbox_absorb(
        &self,
        Parameters(p): Parameters<AbsorbParams>,
    ) -> CallToolResult {
        let server = self.clone();
        Self::run_blocking("bbox_absorb", move || server.absorb_session_knowledge(&p)).await
    }

    #[tool(
        name = "bbox_lint",
        description = "Health check for contradictions, stale entries, duplicates."
    )]
    pub(crate) async fn bbox_lint(&self) -> CallToolResult {
        let server = self.clone();
        Self::run_blocking("bbox_lint", move || server.lint_session_knowledge()).await
    }

    #[tool(
        name = "bbox_review",
        description = "Approve or reject entries awaiting review."
    )]
    pub(crate) async fn bbox_review(
        &self,
        Parameters(p): Parameters<ReviewParams>,
    ) -> CallToolResult {
        let server = self.clone();
        Self::run_blocking("bbox_review", move || {
            let mut p = p;
            if !matches!(p.action.as_deref().unwrap_or("list"), "approve" | "reject") {
                return server.review_session_knowledge(&p);
            }
            let target =
                server.prepare_existing_knowledge_mutation(p.id.as_deref().unwrap_or_default())?;
            p.id = Some(target.id.clone());
            let out = server.state.kb.write().review_with_write_dir(
                &p,
                target
                    .carrier
                    .as_ref()
                    .map(|carrier| carrier.carrier_id.as_str()),
                target.seed.as_ref(),
            )?;
            server.finish_existing_knowledge_mutation(target.checkout.as_ref());
            // Central KB persistence is write-behind here: this body runs on
            // the blocking pool where the durable ack can't be awaited.
            server.state.kb_persister.request();
            Ok(out)
        })
        .await
    }

    #[tool(
        name = "bbox_bootstrap",
        description = "Onboard a new repo into the blackbox knowledge system."
    )]
    pub(crate) async fn bbox_bootstrap(
        &self,
        Parameters(p): Parameters<BootstrapParams>,
    ) -> CallToolResult {
        let server = self.clone();
        Self::run_blocking("bbox_bootstrap", move || {
            server.bootstrap_session_knowledge(&p)
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bbox_corpus_core::identity::PublishedScope;
    use bbox_corpus_core::project_record::ResolvedCheckoutScope;
    use bbox_knowledge::knowledge::{Approval, Category, KnowledgeEntry, Priority, Scope, Status};
    use bbox_knowledge::overlay::{
        OverlayKey, OverlaySnapshot, OverlayStatus, OverlayValue, provisional_entity_ref,
    };
    use std::collections::{BTreeMap, HashMap};
    use std::path::{Path, PathBuf};
    use std::process::Command;
    use std::sync::Arc;

    fn git_ok(dir: &Path, args: &[&str]) {
        let out = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    fn init_repo_with_worktree(tmp: &Path) -> (PathBuf, PathBuf) {
        let base = tmp.join("repo");
        std::fs::create_dir_all(&base).unwrap();
        git_ok(&base, &["init"]);
        git_ok(
            &base,
            &[
                "-c",
                "user.name=Blackbox Test",
                "-c",
                "user.email=blackbox@example.invalid",
                "commit",
                "--allow-empty",
                "-m",
                "initial",
            ],
        );
        let worktree = tmp.join("wt");
        git_ok(
            &base,
            &[
                "worktree",
                "add",
                "-b",
                "arc/render",
                worktree.to_str().unwrap(),
                "HEAD",
            ],
        );
        (
            base.canonicalize().unwrap(),
            worktree.canonicalize().unwrap(),
        )
    }

    fn record_for(path: &Path) -> crate::projects::ProjectRecord {
        crate::projects::ProjectRecord {
            project_id: "feedbeef".into(),
            repo_id: None,
            canonical_path: path.to_string_lossy().into_owned(),
            registered_at: "2026-01-01T00:00:00Z".into(),
            is_git_repo: true,
            languages: Default::default(),
            aliases: Default::default(),
        }
    }

    fn knowledge_entry(id: &str, content: String) -> KnowledgeEntry {
        KnowledgeEntry {
            id: id.into(),
            title: id.into(),
            content,
            cluster: None,
            variants: HashMap::new(),
            category: Category::Memory,
            scope: Scope::Project,
            project: None,
            project_id: None,
            providers: Vec::new(),
            priority: Priority::Standard,
            weight: 100,
            status: Status::Active,
            approval: Approval::Imported,
            render: true,
            decay: false,
            review_at: None,
            supersedes: None,
            links: Vec::new(),
            rationale: None,
            expires_at: None,
            source: "test".into(),
            created_at: "2026-01-01T00:00:00Z".into(),
            updated_at: "2026-01-01T00:00:00Z".into(),
            recall_count: 0,
            last_recalled: None,
        }
    }

    fn write_knowledge_entry(root: &Path, entry: &KnowledgeEntry) {
        let dir = root.join(".bbox/knowledge");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join(format!("{}.json", entry.id)),
            serde_json::to_vec_pretty(entry).unwrap(),
        )
        .unwrap();
    }

    struct VisibilityFixture {
        server: BlackboxServer,
        state: Arc<crate::server::SharedState>,
        project: PathBuf,
        scope: PublishedScope,
        own_checkout: ResolvedCheckoutScope,
        own_ref: String,
        peer_ref: String,
        source_file: PathBuf,
    }

    fn visibility_fixture(temp: &tempfile::TempDir) -> VisibilityFixture {
        let root = temp.path().canonicalize().unwrap();
        let project = root.join("repo");
        std::fs::create_dir_all(&project).unwrap();
        git_ok(&project, &["init", "-q", "-b", "main"]);
        git_ok(&project, &["config", "user.email", "test@example.com"]);
        git_ok(&project, &["config", "user.name", "Test"]);
        let source_file = project.join("src/lib.rs");
        std::fs::create_dir_all(source_file.parent().unwrap()).unwrap();
        std::fs::write(&source_file, "pub fn visible() {}\n").unwrap();
        git_ok(&project, &["add", "src/lib.rs"]);
        git_ok(&project, &["commit", "-q", "-m", "seed"]);
        let repo_id = crate::config::ensure_recorded_repo_id(&project).unwrap();
        write_knowledge_entry(
            &project,
            &knowledge_entry("visible", "PUBLISHED_REVIEW".into()),
        );
        git_ok(&project, &["add", ".bbox"]);
        git_ok(&project, &["commit", "-q", "-m", "published knowledge"]);
        let project = project.canonicalize().unwrap();
        let source_file = source_file.canonicalize().unwrap();

        let state_dir = root.join("state");
        std::fs::create_dir_all(&state_dir).unwrap();
        let state = Arc::new(crate::server::SharedState::for_test(&state_dir));
        let record = state
            .project_authority
            .bridge_registry()
            .unwrap()
            .write()
            .register_path(&project)
            .unwrap();
        let scope = PublishedScope::try_new(repo_id.repo_id, ".").unwrap();
        let own_id = "own-visibility";
        let peer_id = "peer-visibility";
        let mut own_values = BTreeMap::new();
        own_values.insert(
            "visible".into(),
            OverlayValue::Upsert {
                entry: Box::new(knowledge_entry(
                    "visible",
                    format!("OWN_REVIEW {}", source_file.display()),
                )),
                content_hash: "own-hash".into(),
            },
        );
        let mut peer_values = BTreeMap::new();
        peer_values.insert(
            "visible".into(),
            OverlayValue::Upsert {
                entry: Box::new(knowledge_entry(
                    "visible",
                    format!("PEER_REVIEW {}", source_file.display()),
                )),
                content_hash: "peer-hash".into(),
            },
        );
        for (checkout_id, values) in [(own_id, own_values), (peer_id, peer_values)] {
            state.knowledge_overlays.write().publish(OverlaySnapshot {
                snapshot_id: format!("snapshot-{checkout_id}"),
                key: OverlayKey {
                    published_scope: scope.clone(),
                    checkout_id: checkout_id.into(),
                },
                stamp: None,
                status: OverlayStatus::Valid,
                values,
                diagnostics: Vec::new(),
            });
        }
        let own_checkout = ResolvedCheckoutScope {
            project_id: record.project_id,
            published_scope: scope.clone(),
            checkout_id: own_id.into(),
            checkout_dir: project.to_string_lossy().into_owned(),
            checkout_project_dir: project.to_string_lossy().into_owned(),
            branch_ref: Some("refs/heads/main".into()),
        };
        let server = BlackboxServer::new(state.clone());
        server
            .session_checkout
            .set(Some(Arc::new(own_checkout.clone())))
            .unwrap();
        VisibilityFixture {
            server,
            state,
            project,
            own_ref: provisional_entity_ref(&scope, own_id, "visible"),
            peer_ref: provisional_entity_ref(&scope, peer_id, "visible"),
            source_file,
            scope,
            own_checkout,
        }
    }

    #[test]
    fn rescope_render_project_splits_worktree_into_scope_and_write_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let tmp_root = tmp.path().canonicalize().unwrap();
        let (base, worktree) = init_repo_with_worktree(&tmp_root);
        let projects = vec![record_for(&base)];

        // Worktree: write into the worktree, filter by the base scope.
        let mut p = RenderParams {
            project: Some(worktree.to_string_lossy().into_owned()),
            ..Default::default()
        };
        rescope_render_project(&mut p, &projects);
        assert_eq!(p.project.as_deref(), Some(worktree.to_str().unwrap()));
        assert_eq!(p.scope_project.as_deref(), Some(base.to_str().unwrap()));

        // Plain subdirectory: collapse entirely to the registered root —
        // provider files belong at the top of the checkout.
        let subdir = base.join("src");
        std::fs::create_dir_all(&subdir).unwrap();
        let mut p = RenderParams {
            project: Some(subdir.to_string_lossy().into_owned()),
            ..Default::default()
        };
        rescope_render_project(&mut p, &projects);
        assert_eq!(p.project.as_deref(), Some(base.to_str().unwrap()));
        assert_eq!(p.scope_project, None);

        // Unregistered path: untouched.
        let stranger = tmp_root.join("stranger");
        std::fs::create_dir_all(&stranger).unwrap();
        let mut p = RenderParams {
            project: Some(stranger.to_string_lossy().into_owned()),
            ..Default::default()
        };
        rescope_render_project(&mut p, &projects);
        assert_eq!(p.project.as_deref(), Some(stranger.to_str().unwrap()));
        assert_eq!(p.scope_project, None);
    }

    #[test]
    fn read_only_knowledge_consumers_share_session_visibility() {
        let temp = tempfile::tempdir().unwrap();
        let fixture = visibility_fixture(&temp);
        let list = ReviewParams {
            action: Some("list".into()),
            id: None,
        };

        let lint = fixture.server.lint_session_knowledge().unwrap();
        assert!(lint.contains("1 unverified"), "{lint}");
        assert!(!lint.contains("2 unverified"), "{lint}");

        let review = fixture.server.review_session_knowledge(&list).unwrap();
        assert!(review.contains(&fixture.own_ref), "{review}");
        assert!(review.contains("OWN_REVIEW"), "{review}");
        assert!(!review.contains(&fixture.peer_ref), "{review}");
        assert!(!review.contains("PEER_REVIEW"), "{review}");

        let absorb = fixture
            .server
            .absorb_session_knowledge(&AbsorbParams {
                project: Some(fixture.project.to_string_lossy().into_owned()),
                scope: Some("project".into()),
            })
            .unwrap();
        assert!(absorb.contains("no-op"), "{absorb}");

        let bootstrap = fixture
            .server
            .bootstrap_session_knowledge(&BootstrapParams {
                project: fixture.project.to_string_lossy().into_owned(),
            })
            .unwrap();
        assert!(
            bootstrap.contains("1 active project-scoped entries"),
            "{bootstrap}"
        );

        let smart_read = crate::tools::workspace::impl_work_smart_read(
            &fixture.server,
            &crate::tools::workspace::WorkSmartReadParams {
                file_path: fixture.source_file.to_string_lossy().into_owned(),
                enrich: Some(true),
                offset: None,
                limit: None,
            },
        )
        .unwrap();
        assert!(smart_read.contains(&fixture.own_ref), "{smart_read}");
        assert!(smart_read.contains("OWN_REVIEW"), "{smart_read}");
        assert!(!smart_read.contains(&fixture.peer_ref), "{smart_read}");
        assert!(!smart_read.contains("PEER_REVIEW"), "{smart_read}");

        let published_server = BlackboxServer::new(fixture.state.clone());
        let published = published_server.review_session_knowledge(&list).unwrap();
        assert!(published.contains("[visible]"), "{published}");
        assert!(published.contains("PUBLISHED_REVIEW"), "{published}");
        assert!(!published.contains("OWN_REVIEW"), "{published}");
        assert!(!published.contains("PEER_REVIEW"), "{published}");
    }

    #[test]
    fn read_only_knowledge_consumers_fail_consistently_on_invalid_own_overlay() {
        let temp = tempfile::tempdir().unwrap();
        let fixture = visibility_fixture(&temp);
        fixture
            .state
            .knowledge_overlays
            .write()
            .publish(OverlaySnapshot {
                snapshot_id: "invalid-own-snapshot".into(),
                key: OverlayKey {
                    published_scope: fixture.scope.clone(),
                    checkout_id: fixture.own_checkout.checkout_id.clone(),
                },
                stamp: None,
                status: OverlayStatus::Invalid,
                values: BTreeMap::new(),
                diagnostics: vec!["malformed own entry".into()],
            });
        let project = fixture.project.to_string_lossy().into_owned();
        let errors = vec![
            fixture.server.lint_session_knowledge().unwrap_err(),
            fixture
                .server
                .review_session_knowledge(&ReviewParams {
                    action: Some("list".into()),
                    id: None,
                })
                .unwrap_err(),
            fixture
                .server
                .absorb_session_knowledge(&AbsorbParams {
                    project: Some(project.clone()),
                    scope: Some("project".into()),
                })
                .unwrap_err(),
            fixture
                .server
                .bootstrap_session_knowledge(&BootstrapParams {
                    project: project.clone(),
                })
                .unwrap_err(),
            crate::tools::workspace::impl_work_smart_read(
                &fixture.server,
                &crate::tools::workspace::WorkSmartReadParams {
                    file_path: fixture.source_file.to_string_lossy().into_owned(),
                    enrich: Some(true),
                    offset: None,
                    limit: None,
                },
            )
            .unwrap_err(),
        ];
        for error in errors {
            assert!(
                error
                    .to_string()
                    .contains("own checkout overlay is invalid"),
                "{error:#}"
            );
        }
    }
}

/// Catalog-mode render tests (plan section 13.5).
#[cfg(test)]
mod catalog_render_tests {
    use super::*;
    use crate::server::state::catalog_fixture::CatalogFixture;
    use rmcp::handler::server::wrapper::Parameters;

    const PROJECT: &str = "p_000000000000000000000000000000a1";

    fn is_error(result: &rmcp::model::CallToolResult) -> bool {
        result.is_error == Some(true)
    }

    /// Global render is attachment-free: it writes host-level provider files,
    /// not repository ones. The fixture server installs `DenyCheckoutAccess`,
    /// so any lease this path took would fail the call outright, and the
    /// observation counters would record the attempt.
    #[tokio::test]
    async fn global_render_takes_no_checkout_lease() {
        let fixture = CatalogFixture::new();
        fixture.add_published_project(PROJECT, &CatalogFixture::scope("."));
        let server = fixture.server();

        let result = server
            .bbox_render(Parameters(RenderParams {
                scope: Some("global".into()),
                ..Default::default()
            }))
            .await;

        assert!(!is_error(&result), "{result:?}");
        let attempted: u64 = server
            .state
            .checkout_access
            .health()
            .operations
            .into_iter()
            .map(|operation| operation.granted + operation.denied)
            .sum();
        assert_eq!(attempted, 0, "global render acquired checkout authority");
    }

    /// A project render against a catalog project with no attachment reports
    /// the attachment requirement rather than reaching a checkout. The
    /// refusal is the resolver's, so no record path lookup stands between the
    /// caller and the answer.
    #[tokio::test]
    async fn remote_only_project_render_refuses_without_reaching_a_checkout() {
        let fixture = CatalogFixture::new();
        fixture.add_published_project(PROJECT, &CatalogFixture::scope("."));
        let server = fixture.server();

        let result = server
            .bbox_render(Parameters(RenderParams {
                project: Some(PROJECT.into()),
                scope: Some("project".into()),
                ..Default::default()
            }))
            .await;

        assert!(is_error(&result), "{result:?}");
        let granted: u64 = server
            .state
            .checkout_access
            .health()
            .operations
            .into_iter()
            .map(|operation| operation.granted)
            .sum();
        assert_eq!(granted, 0, "a refused render must open no checkout");
    }
}
