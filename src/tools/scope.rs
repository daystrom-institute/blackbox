//! Shared project write-scope resolution for store tool adapters.
//!
//! Stores that key durable state by project path (knowledge, gaps, and —
//! progressively — pins/notes/whiteboards/roadmap) must agree on what a
//! caller-supplied `project` value means when the caller works inside a
//! worktree: the durable scope is the registered BASE project, while
//! repo-owned committed files belong in the WORKTREE checkout so they travel
//! with the agent's branch. Centralizing the resolution here keeps every
//! store's interpretation identical (gap-de82a74d: bbox_learn and bbox_render
//! disagreeing on scope made worktree-written entries unrenderable).

use crate::server::BlackboxServer;

impl BlackboxServer {
    /// Resolve a raw `project` path/id to `(durable_scope, write_dir)`.
    ///
    /// - Recognized worktrees (managed fleet worktrees AND in-tree linked
    ///   worktrees like `.claude/worktrees/<name>`) key to the registered
    ///   base; `write_dir = Some(worktree)` redirects repo-owned committed
    ///   files into the worktree checkout.
    /// - Other registered projects resolve through the registry to their
    ///   canonical path (`write_dir = None`).
    /// - Unregistered paths fall back to filesystem canonicalization;
    ///   non-path values (registry misses) pass through untouched.
    // Blocking fs (canonicalize/git probes): call from run_blocking /
    // spawn_blocking closures only, like the store mutations it scopes.
    #[allow(clippy::disallowed_methods)]
    pub(crate) fn resolve_project_write_scope(&self, raw: &str) -> (String, Option<String>) {
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

    /// Filter-side companion to [`Self::resolve_project_write_scope`]: map a
    /// project FILTER value to its registered base only when it is a
    /// recognized worktree path (`None` otherwise — caller keeps the raw
    /// value). Substring filters and other non-path values pass through
    /// untouched, preserving each store's existing match semantics.
    pub(crate) fn rescope_project_filter_value(&self, raw: &str) -> Option<String> {
        crate::projects::fleet_worktree_scope_and_dir(raw, &self.state.projects.read().list())
            .map(|(base, _worktree)| base)
    }
}

#[cfg(test)]
mod tests {
    use crate::server::BlackboxServer;
    use crate::server::state::SharedState;
    use rmcp::handler::server::wrapper::Parameters;
    use std::path::Path;
    use std::sync::Arc;

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

    /// Cross-store contract for the shared write-scope resolver: notes,
    /// roadmap items, and whiteboards authored from an in-tree linked
    /// worktree all key to the registered BASE project, and the notes list
    /// filter maps a worktree path back to that scope.
    #[tokio::test]
    async fn worktree_callers_key_notes_roadmap_and_whiteboards_to_base() {
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
        let base_str = base_canon.to_string_lossy().into_owned();

        let worktree = base.join(".claude").join("worktrees").join("wt");
        std::fs::create_dir_all(worktree.parent().unwrap()).unwrap();
        run_git(
            &base,
            &["worktree", "add", "-b", "arc/scope", worktree.to_str().unwrap(), "HEAD"],
        );
        let wt = worktree
            .canonicalize()
            .unwrap()
            .to_string_lossy()
            .into_owned();

        let server = BlackboxServer::new(Arc::new(SharedState::for_test(tmp.path())));
        server
            .state
            .projects
            .write()
            .register_path(&base_canon)
            .unwrap();

        // Notes: write keys base; a worktree-path list filter still finds it.
        let note = server
            .bbox_note(Parameters(crate::notes::NoteParams {
                kind: "learned".into(),
                body: "WORKTREE_NOTE_MARKER observation".into(),
                task_id: None,
                session_id: None,
                project: Some(wt.clone()),
                thread_id: None,
                provider: None,
                bro: None,
            }))
            .await;
        assert_ne!(note.is_error, Some(true), "note failed: {note:?}");
        {
            let notes = server.state.notes.read();
            let stored = notes
                .all()
                .iter()
                .find(|n| n.body.contains("WORKTREE_NOTE_MARKER"))
                .expect("note stored");
            assert_eq!(stored.project.as_deref(), Some(base_str.as_str()));
        }
        let listed = server.bbox_notes(Parameters(crate::notes::NoteListParams {
            project: Some(wt.clone()),
            ..Default::default()
        }));
        assert!(
            format!("{:?}", listed.content).contains("WORKTREE_NOTE_MARKER"),
            "worktree-path note filter should map to the base scope: {listed:?}"
        );

        // Roadmap: item created from the worktree keys base.
        let created = server
            .roadmap_create(super::super::roadmap::RoadmapCreateParams {
                title: "worktree-authored item".into(),
                body: "scoping check".into(),
                category: "feature".into(),
                priority: None,
                scope: Some("project".into()),
                project: Some(wt.clone()),
            })
            .await
            .expect("roadmap create");
        assert!(created.contains("\"id\""), "create response: {created}");
        {
            let rm = server.state.roadmap.read();
            let item = rm
                .find_by_title("worktree-authored item")
                .expect("item stored");
            assert_eq!(item.project.as_deref(), Some(base_str.as_str()));
        }

        // Whiteboards: board opened from the worktree keys base.
        let opened = server
            .whiteboard_open(Parameters(
                crate::tools::bro_runtime_params::WhiteboardOpenParams {
                    board_id: "scope-test-board".into(),
                    topic: "worktree scoping".into(),
                    project: Some(wt.clone()),
                    arc_thread_id: None,
                    opened_by: "tester".into(),
                    domain: None,
                },
            ))
            .await;
        assert_ne!(opened.is_error, Some(true), "open failed: {opened:?}");
        let board = server
            .state
            .whiteboards
            .get("scope-test-board")
            .expect("board registered");
        assert_eq!(board.read().project, base_str);
    }
}
