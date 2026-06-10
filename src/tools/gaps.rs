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
    /// committed-file write target. Managed fleet worktrees key to the
    /// registered base but write repo-owned files into the worktree so the
    /// branch carries the gap. Other registered projects resolve through the
    /// registry; unregistered paths fall back to filesystem canonicalization.
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
        Parameters(p): Parameters<GapResolveParams>,
    ) -> CallToolResult {
        let server = self.clone();
        Self::run_blocking("bbox_gap_resolve", move || {
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
        Parameters(p): Parameters<GapUpdateParams>,
    ) -> CallToolResult {
        let server = self.clone();
        Self::run_blocking("bbox_gap_update", move || {
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
