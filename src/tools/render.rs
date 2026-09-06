use crate::knowledge::{
    AbsorbParams, BootstrapParams, PROJECT_RENDER_TRANSPORT_SCOPE,
    PROJECT_RENDER_TRANSPORT_VERSION, ProjectRenderLocalityRequestV1, ProjectRenderPlanV1,
    ProjectRenderViewV1, RenderParams, ReviewParams, Scope,
};
use crate::server::BlackboxServer;

#[cfg(test)]
use crate::knowledge::{ProjectRenderPlanAssemblerV1, ProjectRenderPlanChunkV1};

use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::CallToolResult;
use rmcp::{tool, tool_router};

pub(crate) fn router() -> ToolRouter<BlackboxServer> {
    BlackboxServer::render_tools()
}

pub(crate) const BOUND_WORKSPACE_RENDER_SELECTOR: &str = "$bound-workspace";

fn workspace_project_render_plan(
    server: &BlackboxServer,
    p: &RenderParams,
    render_global: bool,
) -> anyhow::Result<(ProjectRenderPlanV1, Option<String>)> {
    let grant = server
        .authoritative_session_workspace_binding()
        .ok_or_else(|| {
            anyhow::anyhow!(
                "error.render_locality_binding: project render locality requires a bound workspace"
            )
        })?;
    if !grant.is_live_now() {
        anyhow::bail!("error.render_locality_binding: workspace binding has expired");
    }
    let requested = p
        .project
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("project render locality requires a project selector"))?;
    if requested != BOUND_WORKSPACE_RENDER_SELECTOR {
        let project_id = server.validate_project_selection(requested)?;
        if project_id != grant.project_id {
            anyhow::bail!(
                "error.render_locality_scope: project render target differs from the bound workspace"
            );
        }
    }
    let requested_scope = p.scope.as_deref().unwrap_or("both");
    if !matches!(requested_scope, "project" | "both") {
        anyhow::bail!("project render locality requires scope=project or scope=both");
    }

    let view = server.session_knowledge_view(Some(&grant.project_id), p.provisional.as_deref())?;
    let mut entries = view
        .items
        .iter()
        .filter(|item| {
            item.entry.scope == Scope::Project
                && (item.entry.project_id.as_deref() == Some(grant.project_id.as_str())
                    || item.metadata.published_scope.as_ref() == Some(&grant.scope))
        })
        .map(|item| item.entry.clone())
        .collect::<Vec<_>>();
    for entry in &mut entries {
        entry.project = Some(PROJECT_RENDER_TRANSPORT_SCOPE.into());
        entry.project_id = Some(grant.project_id.clone());
    }
    entries.sort_by(|left, right| left.id.cmp(&right.id));
    let plan = ProjectRenderPlanV1 {
        version: PROJECT_RENDER_TRANSPORT_VERSION,
        project_id: grant.project_id.clone(),
        scope: grant.scope.clone(),
        workspace_id: grant.workspace_id.as_str().to_string(),
        provider: p.provider.clone(),
        dry_run: p.dry_run.unwrap_or(false),
        view: ProjectRenderViewV1::parse(p.provisional.as_deref())?,
        requested_scope: requested_scope.to_string(),
        entries,
        diagnostics: view.diagnostics_text(),
    };
    plan.validate()?;
    let global_result = if render_global && plan.requested_scope == "both" {
        Some(view.knowledge.render(&RenderParams {
            provider: plan.provider.clone(),
            project: None,
            scope: Some("global".into()),
            dry_run: Some(plan.dry_run),
            global_plan: None,
            provisional: None,
            scope_project: None,
            locality: None,
        })?)
    } else {
        None
    };
    Ok((plan, global_result))
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
            match p.locality.clone() {
                Some(ProjectRenderLocalityRequestV1::Plan {
                    offset,
                    plan_sha256,
                }) => {
                    let (plan, global_result) =
                        workspace_project_render_plan(&server, &p, offset == 0)?;
                    let chunk = plan.transport_chunk(
                        offset,
                        plan_sha256.as_deref(),
                        global_result,
                    )?;
                    return Ok(serde_json::to_string(&serde_json::json!({
                        "status": "render_locality_plan_chunk",
                        "chunk": chunk,
                    }))?);
                }
                Some(ProjectRenderLocalityRequestV1::Complete {
                    plan_sha256,
                    receipt,
                }) => {
                    let (current, _) = workspace_project_render_plan(&server, &p, false)?;
                    if current.transport_sha256()? != plan_sha256 {
                        anyhow::bail!(
                            "error.render_plan_stale: project render authority changed after the checkout plan was issued"
                        );
                    }
                    receipt.validate_against(&current)?;
                    server
                        .state
                        .render_locality_observations
                        .record_completed(&current, &receipt)?;
                    return Ok(serde_json::to_string_pretty(&serde_json::json!({
                        "status": "render_locality_complete",
                        "diagnostics": current.diagnostics,
                    }))?);
                }
                None if project_render
                    && server.authoritative_session_workspace_binding().is_some() =>
                {
                    anyhow::bail!(
                        "error.render_locality_required: a workspace-bound project render must execute in its checkout owner"
                    );
                }
                None => {}
            }
            let rendered = if project_render {
                let raw = p.project.clone().expect("project render has a target");
                // Project identity is resolved before any render target is
                // opened (plan section 8, P5-E render item 1). The write root
                // below always comes from the acquired lease; no branch
                // re-derives one from a project record path.
                if !server.state.project_authority.is_bridge() {
                    // Catalog selection has stable identity already. Going
                    // through the generic project-mutation resolver would
                    // take an unrelated `repo_mutation` lease before the real
                    // `render_output` gate, so render resolves identity and
                    // acquires its one capability directly.
                    let project_id = server.validate_project_selection(&raw)?;
                    if server
                        .state
                        .render_locality_cutover
                        .transport_governed(&project_id)
                    {
                        anyhow::bail!(
                            "error.render_locality_required: this project's render authority is checkout-local"
                        );
                    }
                    let view = server
                        .session_knowledge_view(Some(&project_id), p.provisional.as_deref())?;
                    p.scope_project = Some(project_id.clone());
                    let broker = &server.state.checkout_access;
                    let lease = crate::server::checkout_access::acquire_catalog_project_lease(
                        &server,
                        broker,
                        &project_id,
                        bbox_indexing::checkout_access::CheckoutAccessKind::RenderFileProvider,
                        bbox_indexing::checkout_access::CheckoutAccessIntent::Write,
                    )?;
                    p.project = Some(lease.project_root().to_string_lossy().into_owned());
                    let rendered = view.knowledge.render(&p);
                    broker.revalidate(&lease).map_err(anyhow::Error::new)?;
                    rendered?
                } else {
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
                        crate::server::checkout_access::with_selected_project_access(
                            &server.state.checkout_access,
                            &project_id,
                            bbox_indexing::checkout_access::CheckoutAccessKind::RenderFileProvider,
                            bbox_indexing::checkout_access::CheckoutAccessIntent::Write,
                            |lease| render(lease.project_root()),
                        )?
                    }
                }
            } else {
                let scope_project = p.scope_project.as_deref().or(p.project.as_deref());
                let view =
                    server.session_knowledge_view(scope_project, p.provisional.as_deref())?;
                let rendered = view.knowledge.render(&p)?;
                if p.global_plan.is_some() {
                    // A host-applied global render plan is a JSON document the
                    // `bro render global` applier decodes; diagnostics ride
                    // inside it rather than as trailing text.
                    let mut plan: bbox_util::global_render::GlobalRenderPlanV1 =
                        serde_json::from_str(&rendered)?;
                    plan.diagnostics = view.diagnostics;
                    return Ok(serde_json::to_string_pretty(&plan)?);
                }
                rendered
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
            if let Some(text) = server.enqueue_review_via_checkout_owner(
                p.action.as_deref().unwrap_or("list"),
                p.id.as_deref().unwrap_or_default(),
            )? {
                return Ok(text);
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
        description = "Retired compatibility operation. Use bbox_hybrid_search for indexed instruction-file discovery and bbox_inspect_entity to expand refs; this operation does not import knowledge or read caller files."
    )]
    pub(crate) async fn bbox_bootstrap(
        &self,
        Parameters(p): Parameters<BootstrapParams>,
    ) -> CallToolResult {
        Self::err_text(&serde_json::json!({
            "error": "error.bootstrap_retired",
            "message": "Bootstrap never imported knowledge; its instruction scan required a local checkout. Discover indexed instruction refs or read files through the checkout owner's file tools, then review proposed knowledge entries before saving them.",
            "replacement": {"tool": "bbox_hybrid_search", "arguments": {
                "project": p.project, "query": "AGENTS.md CLAUDE.md GEMINI.md PROJECT.md instructions",
                "doc_type": "project_file", "limit": 5,
            }},
            "expand": {"tool": "bbox_inspect_entity", "arguments": {"entity_ref": "<returned ref>", "property_mode": "full", "per_type_limit": 0}},
            "coverage": "Search reflects the collected index. Missing instruction refs do not establish that the checkout has no instructions.",
        }).to_string())
    }
}

#[cfg(test)]
#[tokio::test]
async fn bootstrap_mcp_refuses_without_reading_instruction_files() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let server = BlackboxServer::new(std::sync::Arc::new(
        crate::server::state::SharedState::for_test(&root),
    ));
    let result = server
        .bbox_bootstrap(Parameters(BootstrapParams {
            project: root
                .join("unavailable-checkout")
                .to_string_lossy()
                .into_owned(),
        }))
        .await;
    assert_eq!(result.is_error, Some(true));
    let response: serde_json::Value =
        serde_json::from_str(&result.content[0].as_text().unwrap().text).unwrap();
    assert_eq!(response["error"], "error.bootstrap_retired");
    assert_eq!(response["replacement"]["tool"], "bbox_hybrid_search");
    assert_eq!(response["expand"]["tool"], "bbox_inspect_entity");
    assert!(!root.join("unavailable-checkout").exists());
}

#[cfg(test)]
struct FetchedRenderPlanForTest {
    plan: ProjectRenderPlanV1,
    plan_sha256: String,
    page_count: usize,
    max_response_bytes: usize,
}

#[cfg(test)]
async fn fetch_render_plan_for_test(
    server: &BlackboxServer,
    params: RenderParams,
) -> FetchedRenderPlanForTest {
    let mut assembler = ProjectRenderPlanAssemblerV1::default();
    let mut offset = 0;
    let mut plan_sha256 = None::<String>;
    let mut page_count = 0;
    let mut max_response_bytes = 0;
    loop {
        let mut request = params.clone();
        request.locality = Some(ProjectRenderLocalityRequestV1::Plan {
            offset,
            plan_sha256: plan_sha256.clone(),
        });
        let result = server.bbox_render(Parameters(request)).await;
        assert_ne!(result.is_error, Some(true), "{result:?}");
        let wire = serde_json::to_value(result).unwrap();
        let text = wire["content"][0]["text"].as_str().unwrap();
        page_count += 1;
        max_response_bytes = max_response_bytes.max(text.len());
        let value: serde_json::Value = serde_json::from_str(text).unwrap();
        assert_eq!(value["status"], "render_locality_plan_chunk");
        let chunk: ProjectRenderPlanChunkV1 =
            serde_json::from_value(value["chunk"].clone()).unwrap();
        let next_offset = chunk.next_offset;
        plan_sha256 = Some(chunk.plan_sha256.clone());
        if let Some(assembled) = assembler.push(chunk).unwrap() {
            return FetchedRenderPlanForTest {
                plan: assembled.plan,
                plan_sha256: assembled.plan_sha256,
                page_count,
                max_response_bytes,
            };
        }
        offset = next_offset.expect("an incomplete render plan must continue");
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

    #[tokio::test]
    async fn render_locality_plan_preserves_published_own_and_all_views() {
        let temp = tempfile::tempdir().unwrap();
        let fixture = visibility_fixture(&temp);
        let workspace_id = bro_core::WorkspaceId::parse("b".repeat(32)).unwrap();
        assert!(
            fixture
                .server
                .session_workspace_binding
                .set(Some(Arc::new(
                    crate::server::knowledge_source::WorkspaceBindingGrant {
                        task_id: "render-view-task".into(),
                        session_id: "render-view-session".into(),
                        project_id: fixture.own_checkout.project_id.clone(),
                        scope: fixture.scope.clone(),
                        workspace_id,
                        expires_unix_secs: u64::MAX,
                    },
                )))
                .is_ok()
        );

        for (view, expected, absent) in [
            (
                "published",
                vec!["PUBLISHED_REVIEW"],
                vec!["OWN_REVIEW", "PEER_REVIEW"],
            ),
            (
                "own",
                vec!["OWN_REVIEW"],
                vec!["PUBLISHED_REVIEW", "PEER_REVIEW"],
            ),
            (
                "all",
                vec!["PUBLISHED_REVIEW", "OWN_REVIEW", "PEER_REVIEW"],
                vec![],
            ),
        ] {
            let fetched = fetch_render_plan_for_test(
                &fixture.server,
                RenderParams {
                    provider: Some("claude".into()),
                    project: Some(BOUND_WORKSPACE_RENDER_SELECTOR.into()),
                    scope: Some("project".into()),
                    dry_run: Some(true),
                    global_plan: None,
                    provisional: Some(view.into()),
                    scope_project: None,
                    locality: None,
                },
            )
            .await;
            let plan = fetched.plan;
            assert_eq!(plan.view.as_str(), view);
            let content = plan
                .entries
                .iter()
                .map(|entry| entry.content.as_str())
                .collect::<Vec<_>>()
                .join("\n");
            for marker in expected {
                assert!(content.contains(marker), "view={view}: {content}");
            }
            for marker in absent {
                assert!(!content.contains(marker), "view={view}: {content}");
            }
        }
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
    use crate::server::state::catalog_fixture::{COMMIT_ONE, CatalogFixture};
    use bbox_knowledge::knowledge::{Approval, Category, Priority, Scope, Status};
    use rmcp::handler::server::wrapper::Parameters;

    const PROJECT: &str = "p_000000000000000000000000000000a1";

    fn is_error(result: &rmcp::model::CallToolResult) -> bool {
        result.is_error == Some(true)
    }

    fn text(result: &rmcp::model::CallToolResult) -> String {
        let value = serde_json::to_value(result).unwrap();
        value["content"][0]["text"].as_str().unwrap().to_string()
    }

    fn render_entry() -> crate::knowledge::KnowledgeEntry {
        crate::knowledge::KnowledgeEntry {
            id: "render-locality-entry".into(),
            title: "Project render locality".into(),
            content: "DAEMON_RENDER_LOCALITY_MARKER".into(),
            cluster: None,
            variants: Default::default(),
            category: Category::Convention,
            scope: Scope::Project,
            project: None,
            project_id: Some(PROJECT.into()),
            providers: vec![],
            priority: Priority::Standard,
            weight: 100,
            status: Status::Active,
            approval: Approval::UserConfirmed,
            render: true,
            decay: false,
            review_at: None,
            supersedes: None,
            links: vec![],
            rationale: None,
            expires_at: None,
            source: "test".into(),
            created_at: "2026-08-09T00:00:00Z".into(),
            updated_at: "2026-08-09T00:00:00Z".into(),
            recall_count: 0,
            last_recalled: None,
        }
    }

    /// Global render is attachment-free: it writes host-level provider files,
    /// not repository ones. The fixture server installs `DenyCheckoutAccess`,
    /// so any lease this path took would fail the call outright, and the
    /// observation counters would record the attempt.
    #[tokio::test]
    async fn global_render_takes_no_checkout_lease() {
        let fixture = CatalogFixture::new();
        let render_root = fixture.root().join("global-render");
        let mut env = crate::util::TestEnvGuard::new();
        env.set("BLACKBOX_GLOBAL_COMMON_MD", render_root.join("BLACKBOX.md"));
        env.set("BLACKBOX_GLOBAL_CLAUDE_MD", render_root.join("CLAUDE.md"));
        env.set("BLACKBOX_GLOBAL_CODEX_MD", render_root.join("AGENTS.md"));
        env.set("BLACKBOX_GLOBAL_GEMINI_MD", render_root.join("GEMINI.md"));
        env.set("BLACKBOX_BACKUP_DIR", render_root.join("backups"));
        fixture.add_published_project(PROJECT, &CatalogFixture::scope("."));
        let server = fixture.server();

        let result = server
            .bbox_render(Parameters(RenderParams {
                scope: Some("global".into()),
                ..Default::default()
            }))
            .await;

        assert!(!is_error(&result), "{result:?}");
        assert!(
            render_root.join("BLACKBOX.md").is_file(),
            "the global render must stay inside the test fixture"
        );
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

    /// Reproduce the operator-facing failure where a restarted daemon exposed
    /// only the built-in bootstrap rule and an ordinary global render replaced
    /// roughly 30 KiB of standing guidance with that nonempty stub.
    #[tokio::test]
    async fn global_render_refuses_nonempty_stub_over_full_guidance() {
        let fixture = CatalogFixture::new();
        let render_root = fixture.root().join("global-render-shrink");
        std::fs::create_dir_all(&render_root).expect("create render fixture");
        let common_path = render_root.join("BLACKBOX.md");
        let existing_body = "standing operator guidance\n".repeat(1_300);
        let original = format!(
            "{}\n{}{}\n",
            bbox_knowledge::render::MANAGED_START,
            existing_body,
            bbox_knowledge::render::MANAGED_END,
        );
        std::fs::write(&common_path, &original).expect("seed full global guidance");

        let mut env = crate::util::TestEnvGuard::new();
        env.set("BLACKBOX_GLOBAL_COMMON_MD", &common_path);
        env.set("BLACKBOX_GLOBAL_CLAUDE_MD", render_root.join("CLAUDE.md"));
        env.set("BLACKBOX_GLOBAL_CODEX_MD", render_root.join("AGENTS.md"));
        env.set("BLACKBOX_GLOBAL_GEMINI_MD", render_root.join("GEMINI.md"));
        env.set("BLACKBOX_BACKUP_DIR", render_root.join("backups"));
        fixture.add_published_project(PROJECT, &CatalogFixture::scope("."));
        let server = fixture.server();

        let result = server
            .bbox_render(Parameters(RenderParams {
                scope: Some("global".into()),
                ..Default::default()
            }))
            .await;

        assert!(is_error(&result), "{result:?}");
        assert!(
            format!("{result:?}").contains("error.render_destructive_shrink"),
            "{result:?}"
        );
        assert_eq!(
            std::fs::read_to_string(&common_path).unwrap(),
            original,
            "a refused render must preserve the full managed region byte for byte"
        );
        assert!(
            !render_root.join("backups").exists(),
            "a refused render must not claim that it committed a backup"
        );
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

    /// The exact partial-isolation failure: a throwaway daemon has its own
    /// knowledge store but no render overrides, so every target falls back to
    /// the operator's host files. Refusal happens on source authority before
    /// any target is planned or opened.
    #[tokio::test]
    async fn isolated_daemon_cannot_inherit_host_global_render_targets() {
        let fixture = CatalogFixture::new();
        let mut env = crate::util::TestEnvGuard::new();
        for key in [
            "BLACKBOX_GLOBAL_COMMON_MD",
            "BLACKBOX_GLOBAL_CLAUDE_MD",
            "BLACKBOX_GLOBAL_CODEX_MD",
            "BLACKBOX_GLOBAL_GEMINI_MD",
        ] {
            env.remove(key);
        }
        fixture.add_published_project(PROJECT, &CatalogFixture::scope("."));
        let server = fixture.server();

        let result = server
            .bbox_render(Parameters(RenderParams {
                scope: Some("global".into()),
                ..Default::default()
            }))
            .await;

        assert!(is_error(&result), "{result:?}");
        let rendered = format!("{result:?}");
        assert!(
            rendered.contains("error.global_render_authority"),
            "{rendered}"
        );
        // The refusal must name the daemon's real knowledge store. The
        // session view is detached from that store, so an unbound view would
        // report an empty source and refuse even the host-default store.
        let store_path = server.state.kb.read().store_path().display().to_string();
        assert!(!store_path.is_empty());
        assert!(
            rendered.contains(&store_path),
            "refusal must name the durable store {store_path}: {rendered}"
        );
    }

    /// The host-applied lane: a daemon that must not write host guidance
    /// (isolated store, no env bindings, or a remote pod) still serves the
    /// global managed bodies as a plan for the calling host to apply.
    #[tokio::test]
    async fn global_plan_serves_bodies_without_touching_daemon_targets() {
        let fixture = CatalogFixture::new();
        let mut env = crate::util::TestEnvGuard::new();
        for key in [
            "BLACKBOX_GLOBAL_COMMON_MD",
            "BLACKBOX_GLOBAL_CLAUDE_MD",
            "BLACKBOX_GLOBAL_CODEX_MD",
            "BLACKBOX_GLOBAL_GEMINI_MD",
        ] {
            env.remove(key);
        }
        fixture.add_published_project(PROJECT, &CatalogFixture::scope("."));
        let server = fixture.server();
        let host_common = fixture.root().join("host-home/.blackbox/BLACKBOX.md");

        let result = server
            .bbox_render(Parameters(RenderParams {
                scope: Some("global".into()),
                global_plan: Some(bbox_knowledge::knowledge::GlobalRenderPlanRequestV1 {
                    host_common_target: host_common.display().to_string(),
                }),
                ..Default::default()
            }))
            .await;
        assert!(!is_error(&result), "{result:?}");
        let plan: bbox_util::global_render::GlobalRenderPlanV1 =
            serde_json::from_str(&text(&result)).expect("plan JSON");
        plan.validate().expect("checksum");
        assert_eq!(plan.host_common_target, host_common.display().to_string());
        assert!(
            plan.common_body.contains("bbox_gap"),
            "core rules render into the common body"
        );
        let providers: Vec<&str> = plan.providers.iter().map(|p| p.provider.as_str()).collect();
        assert_eq!(providers, ["claude", "agents", "gemini"]);
        for provider in &plan.providers {
            assert!(
                provider.body.contains(&host_common.display().to_string()),
                "{} body must include the host common target: {}",
                provider.provider,
                provider.body
            );
        }
        assert!(
            !host_common.exists(),
            "the daemon must not write the plan's targets itself"
        );

        let rejected = server
            .bbox_render(Parameters(RenderParams {
                scope: Some("project".into()),
                project: Some(PROJECT.into()),
                global_plan: Some(bbox_knowledge::knowledge::GlobalRenderPlanRequestV1 {
                    host_common_target: host_common.display().to_string(),
                }),
                ..Default::default()
            }))
            .await;
        assert!(is_error(&rejected), "{rejected:?}");
    }

    /// The session knowledge view answers for the daemon's durable store:
    /// global render authority compares that store's path against the host
    /// default, so the detached view must carry it rather than the empty
    /// placeholder path.
    #[tokio::test]
    async fn session_knowledge_view_carries_durable_store_authority() {
        let fixture = CatalogFixture::new();
        let server = fixture.server();
        let expected = server.state.kb.read().store_path().to_path_buf();
        assert!(!expected.as_os_str().is_empty());

        let view = server
            .session_knowledge_view(None, None)
            .expect("session knowledge view");
        assert_eq!(view.knowledge.store_path(), expected.as_path());
    }

    /// Authority is preflighted across the complete selected target set. A
    /// safe common target must remain untouched when a provider target is
    /// still an implicit host default.
    #[tokio::test]
    async fn global_render_preflights_every_target_before_writing_common() {
        let fixture = CatalogFixture::new();
        let render_root = fixture.root().join("global-render-mixed-authority");
        let common_path = render_root.join("BLACKBOX.md");
        let mut env = crate::util::TestEnvGuard::new();
        env.set("BLACKBOX_GLOBAL_COMMON_MD", &common_path);
        env.remove("BLACKBOX_GLOBAL_CLAUDE_MD");
        fixture.add_published_project(PROJECT, &CatalogFixture::scope("."));
        let server = fixture.server();

        let result = server
            .bbox_render(Parameters(RenderParams {
                provider: Some("claude".into()),
                scope: Some("global".into()),
                ..Default::default()
            }))
            .await;

        assert!(is_error(&result), "{result:?}");
        assert!(
            format!("{result:?}").contains("error.global_render_authority"),
            "{result:?}"
        );
        assert!(
            !common_path.exists(),
            "global preflight must complete before the common target is written"
        );
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

    #[tokio::test]
    async fn covered_project_render_refuses_before_daemon_checkout_access() {
        let fixture = CatalogFixture::new();
        fixture.add_published_project(PROJECT, &CatalogFixture::scope("."));
        let server = fixture.server_with_render_locality_cutover(PROJECT);
        let before = server.state.checkout_access.health().sequence;

        let result = server
            .bbox_render(Parameters(RenderParams {
                project: Some(PROJECT.into()),
                scope: Some("project".into()),
                provisional: Some("published".into()),
                ..Default::default()
            }))
            .await;

        assert!(is_error(&result), "{}", text(&result));
        assert!(text(&result).contains("error.render_locality_required"));
        assert_eq!(server.state.checkout_access.health().sequence, before);
    }

    #[tokio::test]
    async fn bound_project_render_plan_and_completion_open_no_daemon_checkout() {
        let fixture = CatalogFixture::new();
        let scope = CatalogFixture::scope(".");
        fixture.add_published_project(PROJECT, &scope);
        fixture.install_publication(PROJECT, &scope, COMMIT_ONE, &[render_entry()], &[]);
        let server = fixture.server();
        let workspace_id = bro_core::WorkspaceId::parse("a".repeat(32)).unwrap();
        assert!(
            server
                .session_workspace_binding
                .set(Some(std::sync::Arc::new(
                    crate::server::knowledge_source::WorkspaceBindingGrant {
                        task_id: "render-locality-task".into(),
                        session_id: "render-locality-session".into(),
                        project_id: PROJECT.into(),
                        scope: scope.clone(),
                        workspace_id: workspace_id.clone(),
                        expires_unix_secs: u64::MAX,
                    },
                )))
                .is_ok()
        );
        let before = server.state.checkout_access.health().sequence;
        let legacy = server
            .bbox_render(Parameters(RenderParams {
                project: Some(BOUND_WORKSPACE_RENDER_SELECTOR.into()),
                scope: Some("project".into()),
                provisional: Some("published".into()),
                ..Default::default()
            }))
            .await;
        assert!(is_error(&legacy), "{}", text(&legacy));
        assert!(text(&legacy).contains("error.render_locality_required"));
        assert_eq!(server.state.checkout_access.health().sequence, before);

        let fetched = fetch_render_plan_for_test(
            &server,
            RenderParams {
                provider: Some("claude".into()),
                project: Some(BOUND_WORKSPACE_RENDER_SELECTOR.into()),
                scope: Some("project".into()),
                dry_run: Some(false),
                global_plan: None,
                provisional: Some("published".into()),
                scope_project: None,
                locality: None,
            },
        )
        .await;
        let plan = fetched.plan;
        let plan_sha256 = fetched.plan_sha256;
        assert_eq!(plan.entries.len(), 1);
        assert_eq!(plan.view, ProjectRenderViewV1::Published);
        assert_eq!(
            plan.entries[0].project.as_deref(),
            Some(PROJECT_RENDER_TRANSPORT_SCOPE)
        );
        assert_eq!(server.state.checkout_access.health().sequence, before);

        let local = tempfile::tempdir().unwrap();
        let local_root = local.path().canonicalize().unwrap();
        let execution = bbox_knowledge::knowledge::execute_project_render_plan(
            &plan,
            &local_root,
            &scope,
            workspace_id.as_str(),
        )
        .unwrap();
        assert!(
            std::fs::read_to_string(local_root.join("CLAUDE.md"))
                .unwrap()
                .contains("DAEMON_RENDER_LOCALITY_MARKER")
        );
        let completed = server
            .bbox_render(Parameters(RenderParams {
                provider: Some("claude".into()),
                project: Some(BOUND_WORKSPACE_RENDER_SELECTOR.into()),
                scope: Some("project".into()),
                dry_run: Some(false),
                global_plan: None,
                provisional: Some("published".into()),
                scope_project: None,
                locality: Some(ProjectRenderLocalityRequestV1::Complete {
                    plan_sha256,
                    receipt: execution.receipt,
                }),
            }))
            .await;
        assert!(!is_error(&completed), "{}", text(&completed));
        assert_eq!(server.state.checkout_access.health().sequence, before);
        let observations = server.state.render_locality_observations.snapshot();
        assert_eq!(observations.completions.len(), 1);
        assert_eq!(observations.completions[0].project_id, PROJECT);
        assert_eq!(
            observations.completions[0].view,
            ProjectRenderViewV1::Published
        );
    }

    #[tokio::test]
    async fn oversized_bound_project_render_plan_is_paged_below_the_mcp_cap() {
        let fixture = CatalogFixture::new();
        let scope = CatalogFixture::scope(".");
        fixture.add_published_project(PROJECT, &scope);
        let mut entry = render_entry();
        entry.content = "PAGED_RENDER_PLAN_MARKER".repeat(5_000);
        fixture.install_publication(PROJECT, &scope, COMMIT_ONE, &[entry], &[]);
        let server = fixture.server();
        let workspace_id = bro_core::WorkspaceId::parse("c".repeat(32)).unwrap();
        assert!(
            server
                .session_workspace_binding
                .set(Some(std::sync::Arc::new(
                    crate::server::knowledge_source::WorkspaceBindingGrant {
                        task_id: "render-paging-task".into(),
                        session_id: "render-paging-session".into(),
                        project_id: PROJECT.into(),
                        scope,
                        workspace_id,
                        expires_unix_secs: u64::MAX,
                    },
                )))
                .is_ok()
        );
        let before = server.state.checkout_access.health().sequence;

        let fetched = fetch_render_plan_for_test(
            &server,
            RenderParams {
                provider: Some("claude".into()),
                project: Some(BOUND_WORKSPACE_RENDER_SELECTOR.into()),
                scope: Some("project".into()),
                dry_run: Some(true),
                provisional: Some("published".into()),
                ..Default::default()
            },
        )
        .await;

        assert!(fetched.page_count > 1, "large plan must be paged");
        assert!(
            fetched.max_response_bytes < BlackboxServer::MCP_RESPONSE_CAP_BYTES,
            "largest page was {} bytes",
            fetched.max_response_bytes
        );
        assert!(
            fetched.plan.entries[0]
                .content
                .contains("PAGED_RENDER_PLAN_MARKER")
        );
        assert_eq!(server.state.checkout_access.health().sequence, before);
    }

    #[tokio::test]
    async fn catalog_compat_render_keeps_accepted_project_entries_visible() {
        let fixture = CatalogFixture::new();
        let scope = CatalogFixture::scope(".");
        fixture.add_published_project(PROJECT, &scope);
        fixture.install_publication(PROJECT, &scope, COMMIT_ONE, &[render_entry()], &[]);
        let checkout = fixture.root().join("render-compat-checkout");
        std::fs::create_dir_all(&checkout).unwrap();
        let attachment = "att_00000000000000000000000000000e21";
        fixture.attach_overlay_checkout(
            PROJECT,
            &scope,
            &checkout,
            attachment,
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaae21",
            true,
        );
        fixture.grant_capabilities(
            attachment,
            bbox_corpus_core::project_catalog::AttachmentCapabilities {
                render_output: true,
                ..Default::default()
            },
        );
        let server = fixture.server_with_checkout_authority();

        let result = server
            .bbox_render(Parameters(RenderParams {
                project: Some(PROJECT.into()),
                scope: Some("project".into()),
                provisional: Some("published".into()),
                ..Default::default()
            }))
            .await;
        assert!(!is_error(&result), "{}", text(&result));
        assert!(
            std::fs::read_to_string(checkout.join("CLAUDE.md"))
                .unwrap()
                .contains("DAEMON_RENDER_LOCALITY_MARKER")
        );
    }
}
