use crate::server::BlackboxServer;
use crate::threads::{ThreadListParams, ThreadParams};

use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::CallToolResult;
use rmcp::{tool, tool_router};

pub(crate) fn router() -> ToolRouter<BlackboxServer> {
    BlackboxServer::threads_tools()
}

#[tool_router(router = threads_tools)]
impl BlackboxServer {
    #[tool(
        name = "bbox_thread",
        description = "Open / continue / resolve / promote / rename / link a work thread."
    )]
    pub(crate) async fn bbox_thread(
        &self,
        Parameters(p): Parameters<ThreadParams>,
    ) -> CallToolResult {
        let start = std::time::Instant::now();
        let mutation_result: anyhow::Result<_> = {
            // When the agent passes a managed fleet worktree as `project`, key the
            // thread to its registered base (durable scope) but write the committed
            // `.bbox/record/` snapshot into the worktree so it travels with the
            // agent's branch. Resolution lives here (the adapter holds the project
            // registry); the threads store stays registry-free.
            let resolved = p.project.as_deref().and_then(|proj| {
                crate::projects::fleet_worktree_scope_and_dir(
                    proj,
                    &self.state.projects.read().list(),
                )
            });
            let mut threads = self.state.threads.write();
            match resolved {
                Some((base, worktree)) => {
                    let mut p = p.clone();
                    p.project = Some(base);
                    threads.thread_mutation(&p, Some(&worktree))
                }
                None => threads.thread_mutation(&p, None),
            }
        };
        let mutation = match mutation_result {
            Ok(mutation) => mutation,
            Err(e) => {
                let ms = start.elapsed().as_secs_f64() * 1000.0;
                tracing::warn!(target: "blackbox::tool", tool = "bbox_thread", elapsed_ms = ms, error = %e, "err");
                return Self::err_text(&format!("Error: {e:#}"));
            }
        };

        if p.action != "get" {
            if let Err(e) = self.state.persist_threads_durable().await {
                let ms = start.elapsed().as_secs_f64() * 1000.0;
                tracing::warn!(target: "blackbox::tool", tool = "bbox_thread", elapsed_ms = ms, error = %e, "err");
                return Self::err_text(&format!("Error: {e:#}"));
            }
        }

        if let Some(thread) = mutation.changed_thread.as_ref() {
            self.state
                .index_writer
                .enqueue(crate::index::IndexWriteOp::UpsertThread(Box::new(
                    thread.clone(),
                )));
        }
        if mutation.changed_edges {
            // Nudge the watcher thread instead of rebuilding inline: a full
            // rebuild parses the multi-GB sidecar lanes (13s+ in prod) and
            // must not pin a tokio worker. The linked edge appears in the
            // graph once the watcher's rebuild lands (typically seconds).
            self.state.nudge_edge_index_rebuild();
        }
        let ms = start.elapsed().as_secs_f64() * 1000.0;
        tracing::info!(target: "blackbox::tool", tool = "bbox_thread", elapsed_ms = ms, bytes = mutation.message.len(), "ok");
        Self::ok_text(&mutation.message)
    }

    #[tool(
        name = "bbox_thread_list",
        description = "Scan threads by lifecycle status and idle age."
    )]
    pub(crate) fn bbox_thread_list(
        &self,
        Parameters(p): Parameters<ThreadListParams>,
    ) -> CallToolResult {
        Self::run("bbox_thread_list", || {
            // Normalize a managed fleet worktree project filter to its registered
            // base so list-before-open (create etiquette) sees base-keyed threads
            // from inside a worktree and doesn't drive duplicate opens.
            let normalized = p.project.as_deref().and_then(|proj| {
                crate::projects::fleet_worktree_scope_and_dir(
                    proj,
                    &self.state.projects.read().list(),
                )
                .map(|(base, _worktree)| base)
            });
            match normalized {
                Some(base) => {
                    let mut p = p.clone();
                    p.project = Some(base);
                    self.state.threads.read().thread_list(&p)
                }
                None => self.state.threads.read().thread_list(&p),
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::state::SharedState;
    use crate::threads::{ThreadListParams, ThreadParams};
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

    fn tp(action: &str) -> ThreadParams {
        ThreadParams {
            action: action.into(),
            topic: None,
            project: None,
            name: None,
            id: None,
            session_id: None,
            provider: None,
            session_name: None,
            handoff_doc: None,
            note: None,
            target: None,
            target_type: None,
            edge: None,
            promoted_to: None,
            kind: None,
            origin: None,
        }
    }

    fn tlp() -> ThreadListParams {
        ThreadListParams {
            status: None,
            project: None,
            name: None,
            min_idle_days: None,
            include_resolved: Some(true),
            kind: None,
            include_workflows: None,
        }
    }

    /// End-to-end adapter seam: an agent inside a managed fleet worktree passes
    /// the worktree path as `project`. The thread is keyed to the registered
    /// base, the committed record is written into the worktree (travels with the
    /// branch), and list-before-open from the worktree finds the base-keyed thread.
    #[tokio::test]
    async fn bbox_thread_from_worktree_keys_base_writes_worktree_and_list_normalizes() {
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

        // Open from the worktree.
        let open = server
            .bbox_thread(Parameters(ThreadParams {
                topic: Some("audit the dispatch path".into()),
                project: Some(wt.clone()),
                ..tp("open")
            }))
            .await;
        assert_ne!(open.is_error, Some(true), "open failed: {open:?}");

        // Scope keyed to base; committed-record write-dir = worktree.
        let (id, project, record_dir) = {
            let th = server.state.threads.read();
            let t = th.all().first().expect("one thread").clone();
            (t.id, t.project, t.record_dir)
        };
        assert_eq!(
            project,
            base_canon.to_string_lossy(),
            "scope must be the registered base"
        );
        assert_eq!(
            record_dir.as_deref(),
            Some(wt.as_str()),
            "write-dir must be the worktree"
        );

        // Resolve from the worktree → record in worktree, not base.
        let resolve = server
            .bbox_thread(Parameters(ThreadParams {
                id: Some(id.clone()),
                project: Some(wt.clone()),
                note: Some("found it".into()),
                ..tp("resolve")
            }))
            .await;
        assert_ne!(resolve.is_error, Some(true), "resolve failed: {resolve:?}");
        assert!(
            worktree_canon
                .join(".bbox")
                .join("record")
                .join(format!("{id}.json"))
                .exists(),
            "record should be written into the worktree"
        );
        assert!(
            !base_canon
                .join(".bbox")
                .join("record")
                .join(format!("{id}.json"))
                .exists(),
            "record must NOT be written into the base repo"
        );

        // list-before-open from the worktree surfaces the base-keyed thread.
        let list = server.bbox_thread_list(Parameters(ThreadListParams {
            project: Some(wt.clone()),
            ..tlp()
        }));
        assert_ne!(list.is_error, Some(true), "list failed: {list:?}");
        let body = format!("{:?}", list.content);
        assert!(
            body.contains("audit the dispatch path"),
            "worktree-scoped list should surface the base-keyed thread: {body}"
        );
    }
}
