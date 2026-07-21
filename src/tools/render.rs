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
            if p.project.is_some() {
                let projects = server.state.projects.read().list();
                rescope_render_project(&mut p, &projects);
            }
            let scope_project = p.scope_project.as_deref().or(p.project.as_deref());
            let view = server.session_knowledge_view(scope_project, p.provisional.as_deref())?;
            let rendered = view.knowledge.render(&p)?;
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
        Self::run_blocking("bbox_absorb", move || {
            let out = server.state.kb.write().absorb(&p)?;
            // Central KB persistence is write-behind here: this body runs on
            // the blocking pool where the durable ack can't be awaited.
            server.state.kb_persister.request();
            Ok(out)
        })
        .await
    }

    #[tool(
        name = "bbox_lint",
        description = "Health check for contradictions, stale entries, duplicates."
    )]
    pub(crate) async fn bbox_lint(&self) -> CallToolResult {
        let server = self.clone();
        Self::run_blocking("bbox_lint", move || server.state.kb.read().lint()).await
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
            let target = match p.action.as_deref().unwrap_or("list") {
                "approve" | "reject" => Some(
                    server
                        .prepare_existing_knowledge_mutation(p.id.as_deref().unwrap_or_default())?,
                ),
                _ => None,
            };
            let out = if let Some(target) = &target {
                p.id = Some(target.id.clone());
                server.state.kb.write().review_with_write_dir(
                    &p,
                    target.carrier.as_deref(),
                    target.seed.as_ref(),
                )?
            } else {
                server.state.kb.write().review(&p)?
            };
            server.finish_existing_knowledge_mutation(
                target.as_ref().and_then(|target| target.checkout.as_ref()),
            );
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
            server.state.kb.read().bootstrap(&p)
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::{Path, PathBuf};
    use std::process::Command;

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
}
