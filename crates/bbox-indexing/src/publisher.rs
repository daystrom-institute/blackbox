//! Publisher election for a published knowledge scope (design §4.1).
//!
//! Slice 3.1 of
//! `design/corpus/knowledge/checkout-identity-and-provisional-knowledge.md`.
//!
//! Published truth for a scope is the COMMITTED tree of exactly ONE registered
//! clone. The project registry permits multiple registered paths with the same
//! repo identity, and if two clones of one `(repo_id, bbox_root_relpath)` have
//! divergent HEADs, today's id-based load overwrites by directory scan order
//! (`knowledge.rs`), a silent data-dependent-on-scan-order bug.
//!
//! In the operator's topology this exception is rare: one registered clone per
//! repo plus many worktrees is typical, and a worktree (or a marker-carrying
//! lane clone) resolves TO the base rather than registering as its own base
//! project, so it contributes an overlay and never publishes. The single
//! publisher is therefore the norm, trivially. The exception, two explicitly
//! registered base clones of one repo, FAILS CLOSED here: the caller surfaces
//! the duplicate rather than silently reading one clone's divergent HEAD.
//!
//! This slice ships the election as a pure function; the committed-tree read
//! that consumes it is slice 3.2. The `repo_id` resolution inputs are injected
//! (bbox-indexing has no bbox-config dependency) exactly like the slice-1c
//! inventory, so the daemon supplies `bbox_config::read_repo_id_inputs` and this
//! stays unit-testable with a fake resolver.

use std::path::Path;

use bbox_corpus_core::identity::{RepoIdInputs, bbox_root_relpath, resolve_repo_id};
use bbox_corpus_core::project_record::ProjectRecord;
use bbox_corpus_core::git;

/// The durable key of a published knowledge scope: repo-family id plus the
/// monorepo discriminator.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct PublishedScope {
    pub repo_id: String,
    pub bbox_root_relpath: String,
}

/// Resolve the published scope a registered project publishes into, if any.
///
/// `None` when the project has no resolvable `repo_id` (no override, recorded
/// id, aka, or computed hint) or its root is not in a git repo — such a project
/// cannot be a durable-scope publisher. `resolve_inputs` supplies the
/// config-derived [`RepoIdInputs`] for the project root.
pub fn project_published_scope(
    project: &ProjectRecord,
    resolve_inputs: impl Fn(&Path) -> RepoIdInputs,
) -> Option<PublishedScope> {
    let root = Path::new(&project.canonical_path);
    let repo_id = resolve_repo_id(&resolve_inputs(root))?;
    let git_root = git::git_root_for_path(root)?;
    let relpath = bbox_root_relpath(&git_root, root)?;
    Some(PublishedScope {
        repo_id,
        bbox_root_relpath: relpath,
    })
}

/// The outcome of electing a publisher for a scope.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PublisherResolution {
    /// No registered clone publishes this scope.
    None,
    /// Exactly one registered clone publishes it — its canonical path.
    One(String),
    /// Two or more registered clones claim this scope. Reads must FAIL CLOSED
    /// and surface these paths, never pick one by scan order.
    Duplicate(Vec<String>),
}

/// Elect the single publisher for `scope` among the registered `projects`.
///
/// Classifies by how many registered clones resolve to `scope`:
/// zero → [`PublisherResolution::None`], one → [`PublisherResolution::One`],
/// two or more → [`PublisherResolution::Duplicate`] (fail closed). Paths are
/// returned sorted for deterministic surfacing.
pub fn elect_publisher(
    projects: &[ProjectRecord],
    scope: &PublishedScope,
    resolve_inputs: impl Fn(&Path) -> RepoIdInputs,
) -> PublisherResolution {
    let mut matches: Vec<String> = projects
        .iter()
        .filter(|p| project_published_scope(p, &resolve_inputs).as_ref() == Some(scope))
        .map(|p| p.canonical_path.clone())
        .collect();
    matches.sort();
    match matches.len() {
        0 => PublisherResolution::None,
        1 => PublisherResolution::One(matches.pop().unwrap()),
        _ => PublisherResolution::Duplicate(matches),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn project(id: &str, path: &Path) -> ProjectRecord {
        ProjectRecord {
            project_id: id.into(),
            repo_id: None,
            canonical_path: path.to_string_lossy().into_owned(),
            registered_at: "2026-01-01".into(),
            is_git_repo: true,
            languages: Default::default(),
            aliases: Default::default(),
        }
    }

    fn init_repo(root: &Path) {
        let run = |args: &[&str]| {
            assert!(
                std::process::Command::new("git")
                    .arg("-C")
                    .arg(root)
                    .args(args)
                    .status()
                    .unwrap()
                    .success()
            );
        };
        run(&["init", "-q"]);
        run(&["config", "user.email", "t@example.com"]);
        run(&["config", "user.name", "Test"]);
        std::fs::write(root.join("f.txt"), "x").unwrap();
        run(&["add", "."]);
        run(&["commit", "-q", "-m", "seed"]);
    }

    fn recorded(id: &str) -> RepoIdInputs {
        RepoIdInputs {
            recorded: Some(id.into()),
            ..Default::default()
        }
    }

    #[test]
    fn scope_resolves_at_repo_root() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        init_repo(&root);
        let p = project("p1", &root);
        let scope = project_published_scope(&p, |_| recorded("fam")).unwrap();
        assert_eq!(scope.repo_id, "fam");
        assert_eq!(scope.bbox_root_relpath, ".");
    }

    #[test]
    fn scope_none_without_repo_id() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        init_repo(&root);
        let p = project("p1", &root);
        assert!(project_published_scope(&p, |_| RepoIdInputs::default()).is_none());
    }

    #[test]
    fn scope_none_for_non_git() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let p = project("p1", &root);
        assert!(project_published_scope(&p, |_| recorded("fam")).is_none());
    }

    #[test]
    fn elects_single_publisher() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        init_repo(&root);
        let projects = vec![project("p1", &root)];
        let scope = PublishedScope {
            repo_id: "fam".into(),
            bbox_root_relpath: ".".into(),
        };
        assert_eq!(
            elect_publisher(&projects, &scope, |_| recorded("fam")),
            PublisherResolution::One(root.to_string_lossy().into_owned())
        );
    }

    #[test]
    fn no_publisher_when_scope_unmatched() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        init_repo(&root);
        let projects = vec![project("p1", &root)];
        let scope = PublishedScope {
            repo_id: "other".into(),
            bbox_root_relpath: ".".into(),
        };
        assert_eq!(
            elect_publisher(&projects, &scope, |_| recorded("fam")),
            PublisherResolution::None
        );
    }

    #[test]
    fn duplicate_publishers_fail_closed() {
        // Two registered clones of the same repo family: the exception that
        // must fail closed instead of silent scan-order selection.
        let d1 = tempfile::tempdir().unwrap();
        let r1 = d1.path().canonicalize().unwrap();
        init_repo(&r1);
        let d2 = tempfile::tempdir().unwrap();
        let r2 = d2.path().canonicalize().unwrap();
        init_repo(&r2);
        let projects = vec![project("p1", &r1), project("p2", &r2)];
        let scope = PublishedScope {
            repo_id: "fam".into(),
            bbox_root_relpath: ".".into(),
        };
        // Both resolve to repo_id "fam" (override wins regardless of path).
        let res = elect_publisher(&projects, &scope, |_| recorded("fam"));
        match res {
            PublisherResolution::Duplicate(paths) => {
                assert_eq!(paths.len(), 2);
                // Sorted for deterministic surfacing.
                let mut expect = vec![
                    r1.to_string_lossy().into_owned(),
                    r2.to_string_lossy().into_owned(),
                ];
                expect.sort();
                assert_eq!(paths, expect);
            }
            other => panic!("expected Duplicate, got {other:?}"),
        }
    }
}
