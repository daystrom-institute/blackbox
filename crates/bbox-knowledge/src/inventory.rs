//! Schema-epoch inventory for the durable-key migration (design §3.5).
//!
//! Slice 1c of
//! `design/corpus/knowledge/checkout-identity-and-provisional-knowledge.md`.
//!
//! The identity contract retargets project-scoped durable knowledge from the
//! host-local path key (`entry.project`, an absolute path string) to the
//! traveling `(repo_id, bbox_root_relpath)` key. That cutover is NOT a lazy
//! stamp-on-read: a moved-then-reoccupied path would mis-key repo A's entries
//! onto repo B, and a per-response `built_from` stamp cannot prove coverage
//! across offline hosts and dormant stores. Migration is therefore an explicit
//! **schema epoch**: a one-time inventory resolves every project-scoped entry
//! to `(repo_id, relpath)` by the §3.1 precedence, and QUARANTINES the
//! unresolvable for operator resolution rather than re-keying by current path.
//! Coverage is asserted by the epoch marker plus an empty quarantine.
//!
//! This slice ships the pure, deterministic inventory PASS and its report
//! types. It deliberately does NOT flip the live scope key, persist a gating
//! marker, or run at daemon boot: the scope-key cutover and the path-fallback
//! cut land with the overlay (design §6 steps 3 and 8), which is what consumes
//! this resolution. Building the pass now, tested, is the additive foundation
//! that cutover reads directly.

use std::collections::BTreeMap;
use std::path::Path;

use bbox_corpus_core::git;
use bbox_corpus_core::identity::{RepoIdInputs, bbox_root_relpath, resolve_repo_id};

use crate::knowledge::{KnowledgeEntry, Scope};

/// The current identity schema epoch. Bumped only when the durable-key scheme
/// changes in a way that requires a fresh inventory. Epoch 1 is the
/// `(repo_id, bbox_root_relpath)` scheme this design introduces.
pub const SCHEMA_EPOCH: u32 = 1;

/// The traveling durable key a project-scoped entry resolves to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedKey {
    pub repo_id: String,
    pub bbox_root_relpath: String,
}

/// Why an entry could not be resolved to a durable key and was quarantined for
/// operator resolution instead of being re-keyed by its current path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum QuarantineReason {
    /// A project-scoped entry with no `project` path — nothing to resolve.
    NoProjectPath,
    /// No durable `repo_id` reachable: no override, no recorded id, no aka id,
    /// and no computed bootstrap hash (e.g. the project root is gone).
    NoResolvableRepoId,
    /// The project path is not inside a git repository, so it has no repo
    /// family and no `bbox_root_relpath`.
    NotAGitRepo,
    /// The project root resolved outside its own git root (malformed or moved
    /// state) — the relpath discriminator cannot be computed.
    ProjectRootOutsideGitRoot,
}

impl QuarantineReason {
    pub fn as_str(&self) -> &'static str {
        match self {
            QuarantineReason::NoProjectPath => "no_project_path",
            QuarantineReason::NoResolvableRepoId => "no_resolvable_repo_id",
            QuarantineReason::NotAGitRepo => "not_a_git_repo",
            QuarantineReason::ProjectRootOutsideGitRoot => "project_root_outside_git_root",
        }
    }
}

/// One quarantined entry with the path it was keyed under and the reason.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QuarantineRow {
    pub entry_id: String,
    pub project: Option<String>,
    pub reason: QuarantineReason,
}

/// The result of a schema-epoch inventory pass over durable knowledge.
#[derive(Debug, Clone, Default)]
pub struct KnowledgeInventory {
    pub schema_epoch: u32,
    /// entry id → resolved durable key, for every entry that resolved cleanly.
    pub resolved: BTreeMap<String, ResolvedKey>,
    /// Entries that could not be resolved and await operator resolution.
    pub quarantined: Vec<QuarantineRow>,
    /// Count of non-project (global) entries skipped — not part of the
    /// repo-keyed migration, reported for reconciliation completeness.
    pub skipped_global: usize,
}

impl KnowledgeInventory {
    /// Coverage is proven by the epoch marker plus an EMPTY quarantine
    /// (design §3.5): every project-scoped entry resolved to a durable key.
    /// This is the gate the path-fallback cut (§6 step 8) waits on.
    pub fn is_covered(&self) -> bool {
        self.quarantined.is_empty()
    }
}

/// Run the schema-epoch inventory over `entries`, resolving every
/// project-scoped entry to its durable `(repo_id, bbox_root_relpath)` key.
///
/// `resolve_inputs` supplies the config-derived [`RepoIdInputs`] for a project
/// root (in the daemon this is `bbox_config::read_repo_id_inputs`); it is
/// injected so this crate stays free of a config dependency and the pass stays
/// unit-testable with a fake resolver. Git-root and relpath resolution use the
/// foundation crate directly.
///
/// Deterministic and side-effect-free: it neither mutates entries nor writes
/// any marker. Re-running it on unchanged inputs yields an identical report,
/// which is why the cutover can re-derive coverage rather than trust a stale
/// persisted flag.
pub fn inventory_project_entries(
    entries: &[KnowledgeEntry],
    resolve_inputs: impl Fn(&Path) -> RepoIdInputs,
) -> KnowledgeInventory {
    let mut inv = KnowledgeInventory {
        schema_epoch: SCHEMA_EPOCH,
        ..Default::default()
    };

    for entry in entries {
        if entry.scope != Scope::Project {
            inv.skipped_global += 1;
            continue;
        }
        let Some(project) = entry.project.as_deref() else {
            inv.quarantined.push(QuarantineRow {
                entry_id: entry.id.clone(),
                project: None,
                reason: QuarantineReason::NoProjectPath,
            });
            continue;
        };
        let project_path = Path::new(project);
        let inputs = resolve_inputs(project_path);
        let Some(repo_id) = resolve_repo_id(&inputs) else {
            inv.quarantined.push(row(entry, QuarantineReason::NoResolvableRepoId));
            continue;
        };
        let Some(git_root) = git::git_root_for_path(project_path) else {
            inv.quarantined.push(row(entry, QuarantineReason::NotAGitRepo));
            continue;
        };
        let Some(relpath) = bbox_root_relpath(&git_root, project_path) else {
            inv.quarantined
                .push(row(entry, QuarantineReason::ProjectRootOutsideGitRoot));
            continue;
        };
        inv.resolved.insert(
            entry.id.clone(),
            ResolvedKey {
                repo_id,
                bbox_root_relpath: relpath,
            },
        );
    }
    inv
}

fn row(entry: &KnowledgeEntry, reason: QuarantineReason) -> QuarantineRow {
    QuarantineRow {
        entry_id: entry.id.clone(),
        project: entry.project.clone(),
        reason,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::knowledge::{Approval, Category, Priority, Scope, Status};
    use std::path::Path;

    fn project_entry(id: &str, project: Option<&str>) -> KnowledgeEntry {
        KnowledgeEntry {
            id: id.into(),
            title: "t".into(),
            content: "c".into(),
            cluster: None,
            variants: Default::default(),
            category: Category::Convention,
            scope: Scope::Project,
            project: project.map(str::to_string),
            providers: vec![],
            priority: Priority::Standard,
            weight: 100,
            status: Status::Active,
            approval: Approval::UserConfirmed,
            render: true,
            decay: true,
            review_at: None,
            supersedes: None,
            links: vec![],
            rationale: None,
            expires_at: None,
            source: "test".into(),
            created_at: "2026-01-01".into(),
            updated_at: "2026-01-01".into(),
            recall_count: 0,
            last_recalled: None,
        }
    }

    fn global_entry(id: &str) -> KnowledgeEntry {
        let mut e = project_entry(id, None);
        e.scope = Scope::Global;
        e
    }

    fn git_repo(dir: &Path) {
        let run = |args: &[&str]| {
            assert!(
                std::process::Command::new("git")
                    .arg("-C")
                    .arg(dir)
                    .args(args)
                    .status()
                    .unwrap()
                    .success()
            );
        };
        run(&["init", "-q"]);
        run(&["config", "user.email", "t@example.com"]);
        run(&["config", "user.name", "Test"]);
        std::fs::write(dir.join("f.txt"), "x").unwrap();
        run(&["add", "."]);
        run(&["commit", "-q", "-m", "seed"]);
    }

    fn recorded(repo_id: &str) -> RepoIdInputs {
        RepoIdInputs {
            recorded: Some(repo_id.into()),
            ..Default::default()
        }
    }

    #[test]
    fn resolves_project_entry_at_repo_root() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        git_repo(&root);
        let entries = vec![project_entry("e1", Some(root.to_str().unwrap()))];
        let inv = inventory_project_entries(&entries, |_| recorded("repofam"));
        assert!(inv.is_covered());
        assert_eq!(inv.schema_epoch, SCHEMA_EPOCH);
        let key = inv.resolved.get("e1").unwrap();
        assert_eq!(key.repo_id, "repofam");
        assert_eq!(key.bbox_root_relpath, ".");
    }

    #[test]
    fn resolves_monorepo_subproject_relpath() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        git_repo(&root);
        let sub = root.join("services").join("api");
        std::fs::create_dir_all(&sub).unwrap();
        let entries = vec![project_entry("e1", Some(sub.to_str().unwrap()))];
        let inv = inventory_project_entries(&entries, |_| recorded("repofam"));
        let key = inv.resolved.get("e1").unwrap();
        assert_eq!(key.repo_id, "repofam");
        assert_eq!(key.bbox_root_relpath, "services/api");
    }

    #[test]
    fn quarantines_unresolvable_repo_id() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        git_repo(&root);
        let entries = vec![project_entry("e1", Some(root.to_str().unwrap()))];
        // Empty inputs → no override/recorded/aka/computed → no repo_id.
        let inv = inventory_project_entries(&entries, |_| RepoIdInputs::default());
        assert!(!inv.is_covered());
        assert_eq!(inv.quarantined.len(), 1);
        assert_eq!(
            inv.quarantined[0].reason,
            QuarantineReason::NoResolvableRepoId
        );
        assert!(inv.resolved.is_empty());
    }

    #[test]
    fn quarantines_non_git_project() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        // No git init.
        let entries = vec![project_entry("e1", Some(root.to_str().unwrap()))];
        let inv = inventory_project_entries(&entries, |_| recorded("repofam"));
        assert_eq!(inv.quarantined.len(), 1);
        assert_eq!(inv.quarantined[0].reason, QuarantineReason::NotAGitRepo);
    }

    #[test]
    fn quarantines_missing_project_path() {
        let entries = vec![project_entry("e1", None)];
        let inv = inventory_project_entries(&entries, |_| recorded("repofam"));
        assert_eq!(inv.quarantined.len(), 1);
        assert_eq!(inv.quarantined[0].reason, QuarantineReason::NoProjectPath);
    }

    #[test]
    fn skips_global_entries() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        git_repo(&root);
        let entries = vec![
            global_entry("g1"),
            project_entry("e1", Some(root.to_str().unwrap())),
        ];
        let inv = inventory_project_entries(&entries, |_| recorded("repofam"));
        assert_eq!(inv.skipped_global, 1);
        assert_eq!(inv.resolved.len(), 1);
        assert!(inv.is_covered());
    }

    #[test]
    fn precedence_override_wins_in_inventory() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        git_repo(&root);
        let entries = vec![project_entry("e1", Some(root.to_str().unwrap()))];
        let inv = inventory_project_entries(&entries, |_| RepoIdInputs {
            project_key_override: Some("ovr".into()),
            recorded: Some("rec".into()),
            ..Default::default()
        });
        assert_eq!(inv.resolved.get("e1").unwrap().repo_id, "ovr");
    }
}
