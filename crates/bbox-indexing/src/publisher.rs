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

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use bbox_corpus_core::git;
use bbox_corpus_core::identity::{RepoIdInputs, bbox_root_relpath, resolve_recorded_repo_id};
use bbox_corpus_core::json_store::atomic_write_json_locked;
use bbox_corpus_core::project_record::ProjectRecord;

pub use bbox_corpus_core::identity::PublishedScope;

/// Resolve the published scope a registered project publishes into, if any.
///
/// `None` when the project has no operator-supplied or recorded `repo_id`, or
/// its root is not in a git repo. Migration aliases and computed bootstrap
/// hints are deliberately insufficient for live publisher admission.
/// `resolve_inputs` supplies the config-derived [`RepoIdInputs`] for the
/// project root.
pub fn project_published_scope(
    project: &ProjectRecord,
    resolve_inputs: impl Fn(&Path) -> RepoIdInputs,
) -> Option<PublishedScope> {
    let root = Path::new(&project.canonical_path);
    let repo_id = resolve_recorded_repo_id(&resolve_inputs(root))?;
    let git_root = git::git_root_for_path(root)?;
    let relpath = bbox_root_relpath(&git_root, root)?;
    PublishedScope::try_new(repo_id, relpath).ok()
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

/// Host-local pinned branch ref for one published scope. The path claiming the
/// scope may move or be re-registered; the symbolic branch ref remains the
/// definition of published truth until an explicit repin.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PublisherRefRow {
    pub scope: PublishedScope,
    pub branch_ref: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PublisherRefData {
    version: u32,
    #[serde(default)]
    refs: Vec<PublisherRefRow>,
}

impl Default for PublisherRefData {
    fn default() -> Self {
        Self {
            version: 1,
            refs: Vec::new(),
        }
    }
}

/// Durable host state for symbolic publisher refs.
pub struct PublisherRefStore {
    path: PathBuf,
    data: PublisherRefData,
}

impl PublisherRefStore {
    pub fn open(path: impl Into<PathBuf>) -> Result<Self> {
        let path = path.into();
        let data = if path.exists() {
            let raw = std::fs::read_to_string(&path)
                .with_context(|| format!("reading {}", path.display()))?;
            serde_json::from_str(&raw).with_context(|| format!("parsing {}", path.display()))?
        } else {
            PublisherRefData::default()
        };
        Ok(Self { path, data })
    }

    pub fn pinned(&self, scope: &PublishedScope) -> Option<&PublisherRefRow> {
        self.data.refs.iter().find(|row| &row.scope == scope)
    }

    /// Seed a pin from the publisher's current symbolic branch. Existing pins
    /// are immutable here, so switching checkout branches cannot redefine
    /// published truth.
    pub fn ensure_pinned(
        &mut self,
        scope: &PublishedScope,
        publisher_root: &Path,
    ) -> Result<PublisherRefRow> {
        let row = self.pin_candidate(scope, publisher_root)?;
        self.persist_pin_candidate(&row)?;
        Ok(row)
    }

    /// Compute the immutable pin a caller would establish without mutating
    /// durable state. Reindex uses this during preparation and persists it
    /// only inside the checkout publication fence.
    pub fn pin_candidate(
        &self,
        scope: &PublishedScope,
        publisher_root: &Path,
    ) -> Result<PublisherRefRow> {
        if let Some(existing) = self.pinned(scope) {
            return Ok(existing.clone());
        }
        let branch = git::current_branch(publisher_root).with_context(|| {
            format!(
                "publisher {} is detached; an explicit full branch ref is required",
                publisher_root.display()
            )
        })?;
        let row = PublisherRefRow {
            scope: scope.clone(),
            branch_ref: format!("refs/heads/{branch}"),
        };
        Ok(row)
    }

    /// Persist a previously prepared immutable pin. A competing different pin
    /// fails closed instead of redefining published truth.
    pub fn persist_pin_candidate(&mut self, row: &PublisherRefRow) -> Result<()> {
        if let Some(existing) = self.pinned(&row.scope) {
            if existing == row {
                return Ok(());
            }
            anyhow::bail!("publisher pin changed before prepared publication");
        }
        let mut next = self.data.clone();
        next.refs.push(row.clone());
        next.refs.sort_by(|a, b| a.scope.cmp(&b.scope));
        self.replace_data(next)?;
        Ok(())
    }

    /// Explicit operator-authority primitive. Tool/API exposure is a later
    /// slice; callers must supply a full local branch ref.
    pub fn repin(&mut self, scope: &PublishedScope, branch_ref: &str) -> Result<PublisherRefRow> {
        if !branch_ref.starts_with("refs/heads/") {
            anyhow::bail!("publisher ref must be a full refs/heads/... name");
        }
        let row = PublisherRefRow {
            scope: scope.clone(),
            branch_ref: branch_ref.to_string(),
        };
        let mut next = self.data.clone();
        if let Some(existing) = next.refs.iter_mut().find(|item| &item.scope == scope) {
            *existing = row.clone();
        } else {
            next.refs.push(row.clone());
            next.refs.sort_by(|a, b| a.scope.cmp(&b.scope));
        }
        self.replace_data(next)?;
        Ok(row)
    }

    fn replace_data(&mut self, next: PublisherRefData) -> Result<()> {
        atomic_write_json_locked(&self.path, &next)?;
        self.data = next;
        Ok(())
    }
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
        assert_eq!(scope.repo_id(), "fam");
        assert_eq!(scope.bbox_root_relpath(), ".");
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
        let scope = PublishedScope::try_new("fam", ".").unwrap();
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
        let scope = PublishedScope::try_new("other", ".").unwrap();
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
        let scope = PublishedScope::try_new("fam", ".").unwrap();
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

    #[test]
    fn publisher_ref_pin_survives_checkout_branch_switch() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        init_repo(&root);
        let state = tempfile::tempdir().unwrap();
        let scope = PublishedScope::try_new("fam", ".").unwrap();
        let mut store = PublisherRefStore::open(state.path().join("publisher-refs.json")).unwrap();
        let first = store.ensure_pinned(&scope, &root).unwrap();
        assert!(first.branch_ref.starts_with("refs/heads/"));

        let status = std::process::Command::new("git")
            .arg("-C")
            .arg(&root)
            .args(["switch", "-q", "-c", "other"])
            .status()
            .unwrap();
        assert!(status.success());
        let second = store.ensure_pinned(&scope, &root).unwrap();
        assert_eq!(second.branch_ref, first.branch_ref);

        drop(store);
        let reopened = PublisherRefStore::open(state.path().join("publisher-refs.json")).unwrap();
        assert_eq!(reopened.pinned(&scope), Some(&first));
    }

    #[test]
    fn detached_publisher_cannot_seed_pin() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        init_repo(&root);
        let head = git::current_head(&root).unwrap();
        let status = std::process::Command::new("git")
            .arg("-C")
            .arg(&root)
            .args(["checkout", "-q", "--detach", &head])
            .status()
            .unwrap();
        assert!(status.success());
        let scope = PublishedScope::try_new("fam", ".").unwrap();
        let state = tempfile::tempdir().unwrap();
        let mut store = PublisherRefStore::open(state.path().join("publisher-refs.json")).unwrap();

        assert!(store.ensure_pinned(&scope, &root).is_err());
        assert!(store.pinned(&scope).is_none());
    }

    #[test]
    fn failed_pin_save_does_not_change_in_memory_store() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        init_repo(&root);
        let state = tempfile::tempdir().unwrap();
        let blocked_parent = state.path().join("not-a-directory");
        let path = blocked_parent.join("publisher-refs.json");
        let mut store = PublisherRefStore::open(&path).unwrap();
        std::fs::write(&blocked_parent, "blocked").unwrap();
        let scope = PublishedScope::try_new("fam", ".").unwrap();

        assert!(store.ensure_pinned(&scope, &root).is_err());
        assert!(store.pinned(&scope).is_none());
    }

    #[test]
    fn corrupt_publisher_ref_store_fails_closed() {
        let state = tempfile::tempdir().unwrap();
        let path = state.path().join("publisher-refs.json");
        std::fs::write(&path, b"{\"version\":").unwrap();

        let error = PublisherRefStore::open(&path).err().unwrap();

        assert!(error.to_string().contains("parsing"));
    }

    #[test]
    fn repin_requires_full_branch_ref() {
        let state = tempfile::tempdir().unwrap();
        let scope = PublishedScope::try_new("fam", ".").unwrap();
        let mut store = PublisherRefStore::open(state.path().join("publisher-refs.json")).unwrap();
        assert!(store.repin(&scope, "main").is_err());
        let row = store.repin(&scope, "refs/heads/main").unwrap();
        assert_eq!(row.branch_ref, "refs/heads/main");
    }
}
