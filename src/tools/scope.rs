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

use std::path::{Path, PathBuf};

use bbox_corpus_core::identity::{PublishedScope, bbox_root_relpath, resolve_recorded_repo_id};
use bbox_corpus_core::project_record::{ProjectRecord, ResolvedCheckoutScope};

use crate::server::BlackboxServer;

pub(crate) struct ProjectWriteResolution {
    pub(crate) durable_scope: String,
    pub(crate) write_dir: Option<String>,
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
    /// - Unregistered paths fall back to filesystem canonicalization;
    ///   non-path values (registry misses) pass through untouched.
    // Blocking fs (canonicalize/git probes): call from run_blocking /
    // spawn_blocking closures only, like the store mutations it scopes.
    #[allow(clippy::disallowed_methods)]
    pub(crate) fn resolve_project_write_scope(&self, raw: &str) -> (String, Option<String>) {
        let projects = self.state.projects.read().list();
        if let Some(context) = crate::projects::resolve_project_context(
            raw,
            &projects,
            crate::projects::ResolveIntent::Write,
        ) {
            let record = select_scope_record(raw, &context, &projects).unwrap_or_else(|| {
                projects
                    .iter()
                    .find(|project| project.project_id == context.project_id)
                    .expect("resolved project context must name a registered project")
            });
            let write_dir = context.checkout.and_then(|checkout| {
                let base_root = Path::new(&record.canonical_path);
                let base_git_root = bbox_corpus_core::git::git_root_for_path(base_root)?;
                let relpath = bbox_root_relpath(&base_git_root, base_root)?;
                Some(
                    join_repo_relpath(Path::new(&checkout.checkout_dir), &relpath)
                        .to_string_lossy()
                        .into_owned(),
                )
            });
            return (record.canonical_path.clone(), write_dir);
        }
        let project = std::fs::canonicalize(raw)
            .map(|path| path.to_string_lossy().into_owned())
            .unwrap_or_else(|_| raw.to_string());
        (project, None)
    }

    /// Rich write resolution used by the dark provisional overlay. Existing
    /// store callers can keep the tuple wrapper above until their own overlay
    /// migration lands.
    #[allow(clippy::disallowed_methods)]
    pub(crate) fn resolve_project_write(
        &self,
        raw: &str,
    ) -> anyhow::Result<ProjectWriteResolution> {
        let projects = self.state.projects.read().list();
        if let Some(ctx) = crate::projects::resolve_project_context(
            raw,
            &projects,
            crate::projects::ResolveIntent::Write,
        ) {
            let record = select_scope_record(raw, &ctx, &projects).unwrap_or_else(|| {
                projects
                    .iter()
                    .find(|p| p.project_id == ctx.project_id)
                    .unwrap()
            });
            let base_project = Path::new(&record.canonical_path);
            let base_git_root = bbox_corpus_core::git::git_root_for_path(base_project);
            let checkout_dir = ctx
                .checkout
                .as_ref()
                .map(|checkout| PathBuf::from(&checkout.checkout_dir))
                .or_else(|| base_git_root.clone());
            let relpath = base_git_root
                .as_deref()
                .and_then(|git_root| bbox_root_relpath(git_root, base_project));
            let checkout_project_dir = checkout_dir.as_ref().map(|checkout| {
                relpath
                    .as_deref()
                    .map(|relpath| join_repo_relpath(checkout, relpath))
                    .unwrap_or_else(|| checkout.clone())
            });
            let write_dir = ctx.checkout.as_ref().and_then(|_| {
                checkout_project_dir
                    .as_ref()
                    .map(|path| path.to_string_lossy().into_owned())
            });

            let checkout_scope = match (base_git_root, checkout_dir, checkout_project_dir, relpath)
            {
                (Some(_), Some(checkout_dir), Some(checkout_project_dir), Some(relpath)) => {
                    let inputs = crate::config::read_repo_id_inputs(base_project);
                    let Some(repo_id) = resolve_recorded_repo_id(&inputs) else {
                        return Ok(ProjectWriteResolution {
                            durable_scope: record.canonical_path.clone(),
                            write_dir,
                            checkout_scope: None,
                        });
                    };
                    let checkout_id =
                        bbox_corpus_core::identity::ensure_checkout_id(&checkout_dir)?;
                    let branch_ref = bbox_corpus_core::git::current_branch(&checkout_dir)
                        .map(|branch| format!("refs/heads/{branch}"));
                    Some(ResolvedCheckoutScope {
                        project_id: record.project_id.clone(),
                        published_scope: PublishedScope {
                            repo_id,
                            bbox_root_relpath: relpath,
                        },
                        checkout_id,
                        checkout_dir: checkout_dir.to_string_lossy().into_owned(),
                        checkout_project_dir: checkout_project_dir.to_string_lossy().into_owned(),
                        branch_ref,
                    })
                }
                _ => None,
            };
            return Ok(ProjectWriteResolution {
                durable_scope: record.canonical_path.clone(),
                write_dir,
                checkout_scope,
            });
        }
        let project = std::fs::canonicalize(raw)
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_else(|_| raw.to_string());
        Ok(ProjectWriteResolution {
            durable_scope: project,
            write_dir: None,
            checkout_scope: None,
        })
    }

    /// Filter-side companion to [`Self::resolve_project_write_scope`]: map a
    /// project FILTER value to its registered base only when it is a
    /// recognized worktree path (`None` otherwise — caller keeps the raw
    /// value). Substring filters and other non-path values pass through
    /// untouched, preserving each store's existing match semantics.
    pub(crate) fn rescope_project_filter_value(&self, raw: &str) -> Option<String> {
        crate::projects::resolve_project_context(
            raw,
            &self.state.projects.read().list(),
            crate::projects::ResolveIntent::Write,
        )
        .filter(|ctx| ctx.checkout.is_some())
        .map(|ctx| ctx.host_root)
    }
}

fn join_repo_relpath(checkout_dir: &Path, relpath: &str) -> PathBuf {
    if relpath == "." {
        checkout_dir.to_path_buf()
    } else {
        relpath
            .split('/')
            .fold(checkout_dir.to_path_buf(), |path, component| {
                path.join(component)
            })
    }
}

/// Correct the shared-common-dir resolver's first-match ambiguity for a
/// monorepo by selecting the deepest registered bbox root containing the raw
/// path inside this checkout.
// Called only by resolve_project_write, whose contract requires a blocking
// pool because it also performs git subprocess and filesystem probes.
#[allow(clippy::disallowed_methods)]
fn select_scope_record<'a>(
    raw: &str,
    context: &bbox_corpus_core::project_record::ProjectContext,
    projects: &'a [ProjectRecord],
) -> Option<&'a ProjectRecord> {
    let checkout = context.checkout.as_ref()?;
    let checkout_dir = Path::new(&checkout.checkout_dir);
    let raw = std::fs::canonicalize(raw).ok()?;
    let raw_rel = raw.strip_prefix(checkout_dir).ok()?;
    let common = bbox_corpus_core::git::git_common_dir(checkout_dir)?;
    let mut matches = projects
        .iter()
        .filter_map(|project| {
            let project_root = Path::new(&project.canonical_path);
            let project_git_root = bbox_corpus_core::git::git_root_for_path(project_root)?;
            if bbox_corpus_core::git::git_common_dir(&project_git_root).as_ref() != Some(&common) {
                return None;
            }
            let relpath = bbox_root_relpath(&project_git_root, project_root)?;
            let rel = (relpath != ".").then(|| PathBuf::from(&relpath));
            if rel.as_ref().is_some_and(|rel| !raw_rel.starts_with(rel)) {
                return None;
            }
            let depth = rel
                .as_ref()
                .map(|rel| rel.components().count())
                .unwrap_or(0);
            Some((depth, project))
        })
        .collect::<Vec<_>>();
    matches.sort_by_key(|(depth, _)| *depth);
    let (depth, selected) = matches.pop()?;
    if matches
        .last()
        .is_some_and(|(other_depth, _)| *other_depth == depth)
    {
        return None;
    }
    Some(selected)
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
