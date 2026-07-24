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

use std::path::Path;

use bbox_corpus_core::project_record::ResolvedCheckoutScope;
use bbox_indexing::checkout_access::{
    CheckoutAccessErrorCode, CheckoutAccessIntent, CheckoutAccessKind, CheckoutAccessRequest,
    CheckoutAccessSourceLane, CheckoutAttachmentSelector,
};

use crate::server::BlackboxServer;

pub(crate) struct ProjectWriteResolution {
    pub(crate) durable_scope: String,
    pub(crate) checkout_scope: Option<ResolvedCheckoutScope>,
}

impl BlackboxServer {
    /// Resolve a raw `project` path/id to `(durable_scope, write_dir)`.
    ///
    /// - Recognized worktrees (managed fleet worktrees AND in-tree linked
    ///   worktrees like `.claude/worktrees/<name>`) key to the registered
    ///   base; `write_dir = Some(worktree)` redirects repo-owned committed
    ///   files into the worktree checkout.
    /// - Other registered projects resolve through the registry to their
    ///   canonical path (`write_dir = None`).
    /// - Unregistered selectors pass through untouched without filesystem
    ///   probing outside checkout authority.
    pub(crate) fn resolve_project_write_scope(
        &self,
        raw: &str,
    ) -> anyhow::Result<(String, Option<String>)> {
        self.resolve_project_write(raw).map(|resolution| {
            let write_dir = resolution
                .checkout_scope
                .as_ref()
                .map(|checkout| checkout.checkout_project_dir.clone());
            (resolution.durable_scope, write_dir)
        })
    }

    /// Rich write resolution used by the dark provisional overlay. Existing
    /// store callers can keep the tuple wrapper above until their own overlay
    /// migration lands.
    pub(crate) fn resolve_project_write(
        &self,
        raw: &str,
    ) -> anyhow::Result<ProjectWriteResolution> {
        let lease = match self.state.checkout_access.acquire(CheckoutAccessRequest {
            project_id: String::new(),
            attachment: CheckoutAttachmentSelector::LegacyPath(raw.to_owned()),
            expected_scope: None,
            kind: CheckoutAccessKind::RepositoryMutation,
            intent: CheckoutAccessIntent::Write,
            source_lane: CheckoutAccessSourceLane::LegacyPathResolver,
        }) {
            Ok(lease) => lease,
            Err(error) if error.code == CheckoutAccessErrorCode::AttachmentNotFound => {
                return Ok(ProjectWriteResolution {
                    durable_scope: raw.to_owned(),
                    checkout_scope: None,
                });
            }
            Err(error) => return Err(anyhow::Error::new(error)),
        };
        let record = self
            .state
            .records_provider
            .records_snapshot()
            .records
            .iter()
            .cloned()
            .find(|project| project.project_id == lease.project_id())
            .ok_or_else(|| {
                anyhow::anyhow!("validated checkout project disappeared from registry")
            })?;
        let checkout_scope =
            lease
                .published_scope()
                .cloned()
                .map(|published_scope| ResolvedCheckoutScope {
                    project_id: lease.project_id().to_owned(),
                    published_scope,
                    checkout_id: lease.checkout_id().to_owned(),
                    checkout_dir: lease.checkout_root().to_string_lossy().into_owned(),
                    checkout_project_dir: lease.project_root().to_string_lossy().into_owned(),
                    branch_ref: lease.branch_ref().map(str::to_owned),
                });
        self.state
            .checkout_access
            .revalidate(&lease)
            .map_err(anyhow::Error::new)?;
        if let Some(checkout) = checkout_scope.as_ref() {
            // The write lease's mutation pin excludes detach/relocation while
            // this row is materialized. Registering before the lease drops
            // prevents callers from carrying an unfenced path descriptor into
            // a later lifecycle mutation.
            self.state.checkout_registry.write().register(
                bbox_indexing::checkout_registry::CheckoutRow {
                    project_id: Some(checkout.project_id.clone()),
                    checkout_id: checkout.checkout_id.clone(),
                    checkout_dir: checkout.checkout_dir.clone(),
                    repo_id: Some(checkout.published_scope.repo_id().to_string()),
                    bbox_root_relpath: Some(
                        checkout.published_scope.bbox_root_relpath().to_string(),
                    ),
                    branch_ref: checkout.branch_ref.clone(),
                },
            )?;
        }
        Ok(ProjectWriteResolution {
            durable_scope: record.canonical_path,
            checkout_scope,
        })
    }

    /// Filter-side companion to [`Self::resolve_project_write_scope`]: map a
    /// project FILTER value to its registered base only when it is a
    /// recognized worktree path (`None` otherwise — caller keeps the raw
    /// value). Substring filters and other non-path values pass through
    /// untouched, preserving each store's existing match semantics.
    pub(crate) fn rescope_project_filter_value(&self, raw: &str) -> Option<String> {
        let lease = self
            .state
            .checkout_access
            .acquire(CheckoutAccessRequest {
                project_id: String::new(),
                attachment: CheckoutAttachmentSelector::LegacyPath(raw.to_owned()),
                expected_scope: None,
                kind: CheckoutAccessKind::KnowledgeGapOverlayRead,
                intent: CheckoutAccessIntent::Read,
                source_lane: CheckoutAccessSourceLane::LegacyPathResolver,
            })
            .ok()?;
        let record = self
            .state
            .records_provider
            .records_snapshot()
            .records
            .iter()
            .cloned()
            .find(|project| project.project_id == lease.project_id())?;
        let is_checkout_alias = lease.project_root() != Path::new(&record.canonical_path);
        self.state.checkout_access.revalidate(&lease).ok()?;
        is_checkout_alias.then_some(record.canonical_path)
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
            &[
                "worktree",
                "add",
                "-b",
                "arc/scope",
                worktree.to_str().unwrap(),
                "HEAD",
            ],
        );
        let wt = worktree
            .canonicalize()
            .unwrap()
            .to_string_lossy()
            .into_owned();

        let server = BlackboxServer::new(Arc::new(SharedState::for_test(tmp.path())));
        server
            .state
            .project_authority
            .bridge_registry()
            .unwrap()
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
