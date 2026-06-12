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
    /// committed-file write target. Recognized worktrees (managed fleet
    /// worktrees AND in-tree linked worktrees like `.claude/worktrees/<name>`)
    /// key to the registered base but write repo-owned files into the worktree
    /// so the branch carries the gap — for filing and for the resolve/update
    /// rewrites alike. Other registered projects resolve through the registry;
    /// unregistered paths fall back to filesystem canonicalization.
    // false positive: called from bbox_gap's run_blocking closure.
    #[allow(clippy::disallowed_methods)]
    fn resolve_gap_project(&self, raw: &str) -> (String, Option<String>) {
        if let Some((base, worktree)) =
            crate::projects::fleet_worktree_scope_and_dir(raw, &self.state.projects.read().list())
        {
            return (base, Some(worktree));
        }
        if let Ok(Some(record)) = self.state.projects.read().resolve(raw) {
            return (record.canonical_path, None);
        }
        let project = std::fs::canonicalize(raw)
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_else(|_| raw.to_string());
        (project, None)
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
            if let Some(raw) = p.project.clone().filter(|s| !s.trim().is_empty()) {
                let (project, write_dir) = server.resolve_gap_project(&raw);
                p.project = Some(project);
                p.write_dir = write_dir;
            }
            let (id, created) = server.state.gaps.write().file(&p)?;
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
        Self::run("bbox_gaps", || {
            let normalized = p.project.as_deref().and_then(|proj| {
                crate::projects::fleet_worktree_scope_and_dir(
                    proj,
                    &self.state.projects.read().list(),
                )
                .map(|(base, _worktree)| base)
            });
            match normalized {
                Some(base) => {
                    let mut p = p;
                    p.project = Some(base);
                    self.state.gaps.read().list_rendered(&p)
                }
                None => self.state.gaps.read().list_rendered(&p),
            }
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
            if let Some(raw) = p.project.clone().filter(|s| !s.trim().is_empty()) {
                let (project, write_dir) = server.resolve_gap_project(&raw);
                p.project = Some(project);
                p.write_dir = write_dir;
            }
            server.state.gaps.write().resolve(&p)
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
            if let Some(raw) = p.project.clone().filter(|s| !s.trim().is_empty()) {
                let (project, write_dir) = server.resolve_gap_project(&raw);
                p.project = Some(project);
                p.write_dir = write_dir;
            }
            server.state.gaps.write().update(&p)
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

        let (id, project, write_dir) = {
            let gaps = server.state.gaps.read();
            let gap = gaps.all().first().expect("one gap").clone();
            (gap.id, gap.project, gap.write_dir)
        };
        assert_eq!(
            project.as_deref(),
            Some(base_canon.to_string_lossy().as_ref()),
            "logical scope must be the registered base"
        );
        assert_eq!(
            write_dir.as_deref(),
            Some(wt.as_str()),
            "committed write target must be the worktree"
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

        let list = server.bbox_gaps(Parameters(GapListParams {
            project: Some(wt),
            include_addressed: Some(true),
            ..Default::default()
        }));
        assert_ne!(list.is_error, Some(true), "bbox_gaps failed: {list:?}");
        let body = format!("{:?}", list.content);
        assert!(
            body.contains("worktree gap"),
            "worktree-scoped list should find the base-keyed gap: {body}"
        );
    }
}
