use crate::gaps::{GapFileParams, GapListParams, GapResolveParams, GapUpdateParams};
use crate::server::BlackboxServer;
use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::CallToolResult;
use rmcp::{tool, tool_router};

pub(crate) fn router() -> ToolRouter<BlackboxServer> {
    BlackboxServer::gaps_tools()
}

impl BlackboxServer {
    /// Resolve a raw project path/id to its durable gap scope and optional
    /// committed-file write target — for filing and for the resolve/update
    /// rewrites alike. Delegates to the store-shared resolution in
    /// [`BlackboxServer::resolve_project_write_scope`] (`src/tools/scope.rs`).
    fn resolve_gap_project(
        &self,
        raw: &str,
    ) -> anyhow::Result<(
        String,
        Option<String>,
        Option<bbox_corpus_core::project_record::ResolvedCheckoutScope>,
    )> {
        let resolution = self.resolve_project_write(raw)?;
        let durable_scope = resolution.durable_scope;
        let checkout = resolution.checkout_scope;
        let write_dir = checkout
            .as_ref()
            .map(|checkout| checkout.checkout_project_dir.clone())
            .or(resolution.write_dir);
        if self.path_fallback_is_cut() && checkout.is_none() {
            anyhow::bail!(
                "path-scoped project fallback is retired; project gap writes require a registered checkout with recorded repo identity"
            );
        }
        if let Some(checkout) = checkout.as_ref() {
            self.register_dark_knowledge_checkout(checkout)?;
        }
        Ok((durable_scope, write_dir, checkout))
    }

    fn guard_unscoped_gap_mutation(
        &self,
        id: &str,
        has_project_authority: bool,
    ) -> anyhow::Result<()> {
        if self.path_fallback_is_cut()
            && !has_project_authority
            && self
                .state
                .gaps
                .read()
                .all()
                .iter()
                .any(|gap| gap.id == id && gap.project.is_some())
        {
            anyhow::bail!(
                "path-scoped project fallback is retired; project gap mutation requires session checkout authority"
            );
        }
        Ok(())
    }
}

#[tool_router(router = gaps_tools)]
impl BlackboxServer {
    #[tool(
        name = "bbox_gap",
        description = "File a first-class substrate gap note into the repo-owned gap store."
    )]
    pub(crate) async fn bbox_gap(
        &self,
        Parameters(mut p): Parameters<GapFileParams>,
    ) -> CallToolResult {
        // Gap mutations are disk-authoritative: reload + full-store rewrite
        // under a flock. Run on the blocking pool, not a tokio worker.
        let server = self.clone();
        Self::run_blocking("bbox_gap", move || {
            let mut checkout = None;
            let raw_project = p
                .project
                .clone()
                .filter(|s| !s.trim().is_empty())
                .or_else(|| {
                    (p.scope.as_deref() != Some("global"))
                        .then(|| server.authoritative_session_checkout())
                        .flatten()
                        .map(|checkout| checkout.checkout_project_dir.clone())
                });
            if let Some(raw) = raw_project {
                let (project, write_dir, resolved_checkout) = server.resolve_gap_project(&raw)?;
                p.project = Some(project);
                p.write_dir = write_dir;
                checkout = resolved_checkout;
            }
            let (id, created) = server.state.gaps.write().file(&p)?;
            if let Some(checkout) = checkout.as_ref() {
                server.refresh_dark_gap_overlay(checkout);
            }
            if created {
                Ok(format!("Gap {id} filed (dedupe_key={})", p.dedupe_key))
            } else {
                Ok(format!(
                    "Gap already open as {id} (same dedupe_key); pass allow_recurrence=true to tally a recurrence, or reference {id} from a follow-up"
                ))
            }
        })
        .await
    }

    #[tool(
        name = "bbox_gaps",
        description = "List / filter substrate gap notes by typed fields (gap_kind, impact, blocking_level, dedupe_key, resolution, project)."
    )]
    pub(crate) fn bbox_gaps(&self, Parameters(p): Parameters<GapListParams>) -> CallToolResult {
        Self::run_with_structured("bbox_gaps", || {
            let mut p = p;
            let requested_project = p.project.clone();
            let view =
                self.session_gap_view(requested_project.as_deref(), p.provisional.as_deref())?;
            if let Some(base) = requested_project
                .as_deref()
                .and_then(|raw| self.rescope_project_filter_value(raw))
            {
                p.project = Some(base);
            }
            let mut used_stamp_refs = Vec::<String>::new();
            let rows = view
                .gaps
                .query(&p)
                .into_iter()
                .map(|gap| {
                    let metadata = view.gaps.view_metadata(&gap.id);
                    let mut row = serde_json::to_value(gap)?;
                    let object = row
                        .as_object_mut()
                        .expect("serialized gap response row must be an object");
                    if let Some(reference) =
                        metadata.and_then(|metadata| metadata.built_from_ref.as_ref())
                    {
                        object.insert(
                            "built_from_ref".into(),
                            serde_json::Value::String(reference.clone()),
                        );
                        used_stamp_refs.push(reference.clone());
                    }
                    if let Some(lane) =
                        metadata.and_then(|metadata| metadata.compatibility_lane.as_ref())
                    {
                        object.insert(
                            "compatibility_lane".into(),
                            serde_json::Value::String(lane.clone()),
                        );
                    }
                    Ok::<_, anyhow::Error>(row)
                })
                .collect::<anyhow::Result<Vec<_>>>()?;
            let built_from = view.built_from_for_refs(used_stamp_refs.iter().map(String::as_str));
            let structured = serde_json::json!({
                "rows": rows,
                "built_from": &built_from,
                "diagnostics": &view.diagnostics,
            });
            let mut rendered = view.gaps.list_rendered(&p)?;
            if !p.json.unwrap_or(false) {
                if !view.diagnostics.is_empty() {
                    rendered.push_str("\n\nProvisional gap diagnostics:\n- ");
                    rendered.push_str(&view.diagnostics.join("\n- "));
                }
                rendered = view.append_built_from_table(rendered, &built_from);
            }
            Ok((rendered, structured))
        })
    }

    #[tool(
        name = "bbox_gap_resolve",
        description = "Resolve a gap note (acknowledged/addressed); optionally wire a structured supersession link."
    )]
    pub(crate) async fn bbox_gap_resolve(
        &self,
        Parameters(mut p): Parameters<GapResolveParams>,
    ) -> CallToolResult {
        let server = self.clone();
        Self::run_blocking("bbox_gap_resolve", move || {
            // `project` is write-targeting only: resolve it through the same
            // path as filing so a recognized worktree redirects the rewritten
            // repo-owned file into the session's checkout. The gap's durable
            // project scope never changes; absent → today's behavior.
            let mut checkout = None;
            let raw_project = p
                .project
                .clone()
                .filter(|s| !s.trim().is_empty())
                .or_else(|| {
                    server
                        .authoritative_session_checkout()
                        .map(|checkout| checkout.checkout_project_dir.clone())
                });
            server.guard_unscoped_gap_mutation(&p.id, raw_project.is_some())?;
            if let Some(raw) = raw_project {
                let (project, write_dir, resolved_checkout) = server.resolve_gap_project(&raw)?;
                p.project = Some(project);
                p.write_dir = write_dir;
                checkout = resolved_checkout;
            }
            let result = server.state.gaps.write().resolve(&p)?;
            if let Some(checkout) = checkout.as_ref() {
                server.refresh_dark_gap_overlay(checkout);
            }
            Ok(result)
        })
        .await
    }

    #[tool(
        name = "bbox_gap_update",
        description = "Edit an existing gap note's fields in place."
    )]
    pub(crate) async fn bbox_gap_update(
        &self,
        Parameters(mut p): Parameters<GapUpdateParams>,
    ) -> CallToolResult {
        let server = self.clone();
        Self::run_blocking("bbox_gap_update", move || {
            // Same write-targeting resolution as bbox_gap_resolve.
            let mut checkout = None;
            let raw_project = p
                .project
                .clone()
                .filter(|s| !s.trim().is_empty())
                .or_else(|| {
                    server
                        .authoritative_session_checkout()
                        .map(|checkout| checkout.checkout_project_dir.clone())
                });
            server.guard_unscoped_gap_mutation(&p.id, raw_project.is_some())?;
            if let Some(raw) = raw_project {
                let (project, write_dir, resolved_checkout) = server.resolve_gap_project(&raw)?;
                p.project = Some(project);
                p.write_dir = write_dir;
                checkout = resolved_checkout;
            }
            let result = server.state.gaps.write().update(&p)?;
            if let Some(checkout) = checkout.as_ref() {
                server.refresh_dark_gap_overlay(checkout);
            }
            Ok(result)
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::state::SharedState;
    use std::path::Path;
    use std::process::Command;
    use std::sync::Arc;

    fn run_git(cwd: &Path, args: &[&str]) {
        let out = Command::new("git")
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

    fn gap_params(project: String) -> GapFileParams {
        GapFileParams {
            title: "worktree gap".into(),
            gap_kind: "tooling".into(),
            domain: "test-domain".into(),
            wanted_capability: "write into the worktree".into(),
            dedupe_key: "tooling/test-domain/worktree-gap".into(),
            impact: None,
            blocking_level: None,
            missing_primitive: None,
            fallback_used: None,
            evidence: None,
            suggested_owner: None,
            notes: None,
            scope: Some("project".into()),
            project: Some(project),
            write_dir: None,
            task_id: None,
            session_id: None,
            provider: None,
            bro: None,
            thread_id: None,
            allow_recurrence: None,
        }
    }

    /// Init a committed git repo at `dir` and return its canonical path.
    fn init_repo(dir: &Path) -> std::path::PathBuf {
        std::fs::create_dir_all(dir).unwrap();
        run_git(dir, &["init", "-b", "main"]);
        run_git(dir, &["config", "user.email", "t@example.com"]);
        run_git(dir, &["config", "user.name", "T"]);
        std::fs::write(dir.join("README.md"), "base").unwrap();
        run_git(dir, &["add", "."]);
        run_git(dir, &["commit", "-m", "init"]);
        dir.canonicalize().unwrap()
    }

    /// Server with `base` registered, one gap filed at the base, and the base
    /// wired as a loaded gap root (mirrors daemon boot) so mutations reload it.
    async fn server_with_base_gap(
        state_dir: &Path,
        base_canon: &std::path::PathBuf,
    ) -> (BlackboxServer, String) {
        let server = BlackboxServer::new(Arc::new(SharedState::for_test(state_dir)));
        server
            .state
            .projects
            .write()
            .register_path(base_canon)
            .unwrap();
        let filed = server
            .bbox_gap(Parameters(gap_params(
                base_canon.to_string_lossy().into_owned(),
            )))
            .await;
        assert_ne!(filed.is_error, Some(true), "bbox_gap failed: {filed:?}");
        server
            .state
            .gaps
            .write()
            .set_project_roots(vec![base_canon.clone()])
            .unwrap();
        let id = server.state.gaps.read().all().first().unwrap().id.clone();
        (server, id)
    }

    fn gap_file(root: &Path, id: &str) -> std::path::PathBuf {
        root.join(".bbox").join("gaps").join(format!("{id}.json"))
    }

    #[tokio::test]
    async fn path_cut_rejects_unregistered_project_gap_write() {
        let tmp = tempfile::tempdir().unwrap();
        let project = tmp.path().join("unregistered");
        std::fs::create_dir_all(&project).unwrap();
        let server = BlackboxServer::new(Arc::new(SharedState::for_test(tmp.path())));
        server
            .state
            .path_fallback_cut
            .store(true, std::sync::atomic::Ordering::Release);
        let result = server
            .bbox_gap(Parameters(gap_params(
                project.to_string_lossy().into_owned(),
            )))
            .await;
        assert_eq!(result.is_error, Some(true), "{result:?}");
        assert!(format!("{:?}", result.content).contains("project gap writes require"));
    }

    /// (a) resolve with project=<in-tree linked worktree>: rewritten file
    /// lands in the worktree; the base checkout's copy stays untouched.
    #[tokio::test]
    async fn bbox_gap_resolve_from_in_tree_worktree_writes_worktree_keeps_base() {
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().join("repo");
        let base_canon = init_repo(&base);
        let (server, id) = server_with_base_gap(tmp.path(), &base_canon).await;
        let base_before = std::fs::read_to_string(gap_file(&base_canon, &id)).unwrap();

        // In-tree linked worktree (harness shape, arbitrary branch name).
        let wt = base.join(".claude").join("worktrees").join("wt-a");
        std::fs::create_dir_all(wt.parent().unwrap()).unwrap();
        run_git(
            &base,
            &[
                "worktree",
                "add",
                "-b",
                "worktree-wt-a",
                wt.to_str().unwrap(),
                "HEAD",
            ],
        );
        let wt_canon = wt.canonicalize().unwrap();

        let resolved = server
            .bbox_gap_resolve(Parameters(GapResolveParams {
                id: id.clone(),
                resolution: "addressed".into(),
                note: Some("done on branch".into()),
                project: Some(wt_canon.to_string_lossy().into_owned()),
                ..Default::default()
            }))
            .await;
        assert_ne!(
            resolved.is_error,
            Some(true),
            "resolve failed: {resolved:?}"
        );

        let wt_file = gap_file(&wt_canon, &id);
        assert!(wt_file.exists(), "resolve must write into the worktree");
        assert!(
            std::fs::read_to_string(&wt_file)
                .unwrap()
                .contains("addressed")
        );
        assert_eq!(
            std::fs::read_to_string(gap_file(&base_canon, &id)).unwrap(),
            base_before,
            "base checkout copy must be untouched"
        );

        let updated = server
            .bbox_gap_update(Parameters(GapUpdateParams {
                id: id.clone(),
                notes: Some("follow-up in the same checkout".into()),
                project: Some(wt_canon.to_string_lossy().into_owned()),
                ..Default::default()
            }))
            .await;
        assert_ne!(updated.is_error, Some(true), "update failed: {updated:?}");
        let twice_mutated = std::fs::read_to_string(gap_file(&wt_canon, &id)).unwrap();
        assert!(twice_mutated.contains("addressed"));
        assert!(twice_mutated.contains("follow-up in the same checkout"));
    }

    /// (a, update flavor) update with project=<in-tree worktree> redirects too.
    #[tokio::test]
    async fn bbox_gap_update_from_in_tree_worktree_writes_worktree_keeps_base() {
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().join("repo");
        let base_canon = init_repo(&base);
        let (server, id) = server_with_base_gap(tmp.path(), &base_canon).await;
        let base_before = std::fs::read_to_string(gap_file(&base_canon, &id)).unwrap();

        let wt = base.join(".claude").join("worktrees").join("wt-u");
        std::fs::create_dir_all(wt.parent().unwrap()).unwrap();
        run_git(
            &base,
            &[
                "worktree",
                "add",
                "-b",
                "worktree-wt-u",
                wt.to_str().unwrap(),
                "HEAD",
            ],
        );
        let wt_canon = wt.canonicalize().unwrap();

        let updated = server
            .bbox_gap_update(Parameters(GapUpdateParams {
                id: id.clone(),
                notes: Some("amended-from-worktree".into()),
                project: Some(wt_canon.to_string_lossy().into_owned()),
                ..Default::default()
            }))
            .await;
        assert_ne!(updated.is_error, Some(true), "update failed: {updated:?}");

        let wt_file = gap_file(&wt_canon, &id);
        assert!(wt_file.exists(), "update must write into the worktree");
        assert!(
            std::fs::read_to_string(&wt_file)
                .unwrap()
                .contains("amended-from-worktree")
        );
        assert_eq!(
            std::fs::read_to_string(gap_file(&base_canon, &id)).unwrap(),
            base_before,
            "base checkout copy must be untouched"
        );
    }

    /// (b) resolve with project=<out-of-tree bro-fleet worktree>.
    #[tokio::test]
    async fn bbox_gap_resolve_from_fleet_worktree_writes_worktree_keeps_base() {
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().join("repo");
        let base_canon = init_repo(&base);
        let (server, id) = server_with_base_gap(tmp.path(), &base_canon).await;
        let base_before = std::fs::read_to_string(gap_file(&base_canon, &id)).unwrap();

        let wt = tmp.path().join("wt-fleet");
        run_git(
            &base,
            &[
                "worktree",
                "add",
                "-b",
                "bro-fleet/x",
                wt.to_str().unwrap(),
                "HEAD",
            ],
        );
        let wt_canon = wt.canonicalize().unwrap();

        let resolved = server
            .bbox_gap_resolve(Parameters(GapResolveParams {
                id: id.clone(),
                resolution: "addressed".into(),
                project: Some(wt_canon.to_string_lossy().into_owned()),
                ..Default::default()
            }))
            .await;
        assert_ne!(
            resolved.is_error,
            Some(true),
            "resolve failed: {resolved:?}"
        );

        let wt_file = gap_file(&wt_canon, &id);
        assert!(
            wt_file.exists(),
            "resolve must write into the fleet worktree"
        );
        assert!(
            std::fs::read_to_string(&wt_file)
                .unwrap()
                .contains("addressed")
        );
        assert_eq!(
            std::fs::read_to_string(gap_file(&base_canon, &id)).unwrap(),
            base_before,
            "base checkout copy must be untouched"
        );
    }

    /// (c) project absent → today's behavior: the base copy is rewritten.
    /// (d) a plain subdirectory of the root is NEVER worktree-classed: the
    /// rewrite still lands at the base and no `.bbox/` appears in the subdir.
    #[tokio::test]
    async fn bbox_gap_resolve_without_worktree_rewrites_base() {
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().join("repo");
        let base_canon = init_repo(&base);
        let (server, id) = server_with_base_gap(tmp.path(), &base_canon).await;

        // (c) absent project.
        let resolved = server
            .bbox_gap_resolve(Parameters(GapResolveParams {
                id: id.clone(),
                resolution: "acknowledged".into(),
                ..Default::default()
            }))
            .await;
        assert_ne!(
            resolved.is_error,
            Some(true),
            "resolve failed: {resolved:?}"
        );
        assert!(
            std::fs::read_to_string(gap_file(&base_canon, &id))
                .unwrap()
                .contains("acknowledged"),
            "absent project must rewrite the base copy"
        );

        // (d) plain subdirectory passed as project.
        let subdir = base_canon.join("src");
        std::fs::create_dir_all(&subdir).unwrap();
        let resolved = server
            .bbox_gap_resolve(Parameters(GapResolveParams {
                id: id.clone(),
                resolution: "addressed".into(),
                project: Some(subdir.to_string_lossy().into_owned()),
                ..Default::default()
            }))
            .await;
        assert_ne!(
            resolved.is_error,
            Some(true),
            "resolve failed: {resolved:?}"
        );
        assert!(
            std::fs::read_to_string(gap_file(&base_canon, &id))
                .unwrap()
                .contains("addressed"),
            "plain subdir project must still rewrite the base copy"
        );
        assert!(
            !subdir.join(".bbox").exists(),
            "a plain subdirectory must never be worktree-classed"
        );
    }

    /// (e) global-scope gap mutation ignores the project param entirely.
    #[tokio::test]
    async fn bbox_gap_resolve_global_gap_ignores_project_param() {
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().join("repo");
        let base_canon = init_repo(&base);

        let server = BlackboxServer::new(Arc::new(SharedState::for_test(tmp.path())));
        server
            .state
            .projects
            .write()
            .register_path(&base_canon)
            .unwrap();

        let mut p = gap_params(String::new());
        p.scope = Some("global".into());
        // A defaulted project must not project-scope a global filing.
        p.project = Some(base_canon.to_string_lossy().into_owned());
        let filed = server.bbox_gap(Parameters(p)).await;
        assert_ne!(filed.is_error, Some(true), "bbox_gap failed: {filed:?}");
        let id = server.state.gaps.read().all().first().unwrap().id.clone();
        assert!(
            server.state.gaps.read().all()[0].project.is_none(),
            "scope=global must win over a supplied project"
        );

        let wt = base.join(".claude").join("worktrees").join("wt-g");
        std::fs::create_dir_all(wt.parent().unwrap()).unwrap();
        run_git(
            &base,
            &[
                "worktree",
                "add",
                "-b",
                "worktree-wt-g",
                wt.to_str().unwrap(),
                "HEAD",
            ],
        );
        let wt_canon = wt.canonicalize().unwrap();

        let resolved = server
            .bbox_gap_resolve(Parameters(GapResolveParams {
                id: id.clone(),
                resolution: "addressed".into(),
                project: Some(wt_canon.to_string_lossy().into_owned()),
                ..Default::default()
            }))
            .await;
        assert_ne!(
            resolved.is_error,
            Some(true),
            "resolve failed: {resolved:?}"
        );

        assert!(
            !gap_file(&wt_canon, &id).exists(),
            "global gap mutation must not write into the worktree"
        );
        assert!(
            !gap_file(&base_canon, &id).exists(),
            "global gap must stay in the central store"
        );
        let gaps = server.state.gaps.read();
        let gap = gaps.all().iter().find(|g| g.id == id).unwrap();
        assert_eq!(gap.resolution, crate::gaps::GapResolution::Addressed);
        assert!(
            gap.write_dir.is_none(),
            "global gap must ignore write-targeting"
        );
    }

    #[tokio::test]
    async fn bbox_gap_from_worktree_keys_base_writes_worktree_and_list_normalizes() {
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
        crate::config::ensure_recorded_repo_id(&base_canon).unwrap();
        run_git(&base, &["add", ".bbox/config.toml"]);
        run_git(&base, &["commit", "-m", "record identity"]);

        let worktree = tmp.path().join("wt");
        run_git(
            &base,
            &[
                "worktree",
                "add",
                "-b",
                "bro-fleet/x",
                worktree.to_str().unwrap(),
                "HEAD",
            ],
        );
        let worktree_canon = worktree.canonicalize().unwrap();
        let wt = worktree_canon.to_string_lossy().into_owned();

        let server = BlackboxServer::new(Arc::new(SharedState::for_test(tmp.path())));
        server
            .state
            .projects
            .write()
            .register_path(&base_canon)
            .unwrap();

        let filed = server.bbox_gap(Parameters(gap_params(wt.clone()))).await;
        assert_ne!(filed.is_error, Some(true), "bbox_gap failed: {filed:?}");

        assert!(
            server.state.gaps.read().all().is_empty(),
            "checkout gap must not be retained in the central store"
        );
        let row = server
            .state
            .checkout_registry
            .read()
            .rows()
            .iter()
            .find(|row| row.checkout_dir == wt)
            .cloned()
            .expect("worktree registered for gap overlay");
        let scope = row.published_scope().unwrap();
        let snapshot = server
            .state
            .gap_overlays
            .read()
            .get(&scope, &row.checkout_id)
            .cloned()
            .expect("gap overlay published");
        let id = snapshot.values.keys().next().unwrap().clone();
        server.set_session_checkout_for_test(
            scope,
            row.checkout_id.clone(),
            worktree_canon.clone(),
        );
        let gap = {
            let view = server.session_gap_view(Some(&wt), Some("own")).unwrap();
            view.gaps.all().first().expect("one own gap").clone()
        };
        assert_eq!(
            gap.project.as_deref(),
            Some(base_canon.to_string_lossy().as_ref()),
            "logical scope must be the registered base"
        );
        assert!(
            gap.write_dir.is_none(),
            "read view must not expose a host-local redirect"
        );
        assert_eq!(
            gap.provisional_checkout_id.as_deref(),
            Some(row.checkout_id.as_str())
        );
        assert!(
            worktree_canon
                .join(".bbox")
                .join("gaps")
                .join(format!("{id}.json"))
                .exists(),
            "gap should be written into the worktree"
        );
        assert!(
            !base_canon
                .join(".bbox")
                .join("gaps")
                .join(format!("{id}.json"))
                .exists(),
            "gap must not be written into the base checkout"
        );

        let updated = server
            .bbox_gap_update(Parameters(GapUpdateParams {
                id: id.clone(),
                notes: Some("session-authoritative update".into()),
                ..Default::default()
            }))
            .await;
        assert_ne!(updated.is_error, Some(true), "update failed: {updated:?}");
        assert!(
            std::fs::read_to_string(gap_file(&worktree_canon, &id))
                .unwrap()
                .contains("session-authoritative update")
        );

        let published = server.bbox_gaps(Parameters(GapListParams {
            project: Some(wt.clone()),
            provisional: Some("published".into()),
            include_addressed: Some(true),
            ..Default::default()
        }));
        assert_ne!(
            published.is_error,
            Some(true),
            "published list failed: {published:?}"
        );
        assert!(format!("{:?}", published.content).contains("No gaps found"));

        let list = server.bbox_gaps(Parameters(GapListParams {
            project: Some(wt),
            provisional: Some("own".into()),
            include_addressed: Some(true),
            ..Default::default()
        }));
        assert_ne!(list.is_error, Some(true), "bbox_gaps failed: {list:?}");
        let body = format!("{:?}", list.content);
        assert!(
            body.contains("worktree gap"),
            "worktree-scoped list should find the base-keyed gap: {body}"
        );
        assert!(body.contains("built_from=built_from_"), "{body}");
        assert!(body.contains("working_fingerprint="), "{body}");
        let structured = list
            .structured_content
            .expect("bbox_gaps structured response");
        let reference = structured["rows"][0]["built_from_ref"]
            .as_str()
            .expect("row stamp reference");
        assert!(structured["built_from"].get(reference).is_some());
    }
}
