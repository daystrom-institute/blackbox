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
//! The deterministic inventory pass, host ledgers, repo epoch marker, and
//! monotonic local cut marker now live together here. Daemon lifecycle code
//! runs the inventory and enables the cut only after verifying the repo marker
//! on each pinned committed publisher ref, an empty quarantine, and no central
//! path-scoped records.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use bbox_corpus_core::git;
use bbox_corpus_core::identity::{
    PublishedScope, RepoIdInputs, bbox_root_relpath, resolve_recorded_repo_id, resolve_repo_id,
};
use bbox_corpus_core::json_store::atomic_write_json_locked;
use serde::{Deserialize, Serialize};

use crate::knowledge::{KnowledgeEntry, Scope};

/// The current identity schema epoch. Bumped only when the durable-key scheme
/// changes in a way that requires a fresh inventory. Epoch 1 is the
/// `(repo_id, bbox_root_relpath)` scheme this design introduces.
pub const SCHEMA_EPOCH: u32 = 1;

/// The traveling durable key a project-scoped entry resolves to.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResolvedKey {
    pub repo_id: String,
    pub bbox_root_relpath: String,
}

/// Why an entry could not be resolved to a durable key and was quarantined for
/// operator resolution instead of being re-keyed by its current path.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
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
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QuarantineRow {
    pub entry_id: String,
    pub project: Option<String>,
    pub reason: QuarantineReason,
}

/// The result of a schema-epoch inventory pass over durable knowledge.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
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

pub const SCHEMA_EPOCH_MARKER: &str = ".schema-epoch";
pub const INVENTORY_LEDGER: &str = "knowledge-schema-epoch.json";
pub const QUARANTINE_LEDGER: &str = "knowledge-quarantine.json";
pub const PATH_FALLBACK_CUT_MARKER: &str = "knowledge-path-fallback-cut.json";

/// Committed marker carried by one clean repo-owned knowledge scope.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SchemaEpochMarker {
    pub schema_epoch: u32,
    pub repo_id: String,
    pub bbox_root_relpath: String,
}

/// Monotonic host-local proof that this daemon store retired path-keyed
/// project authority. Once present, runtime reads and writes never reopen the
/// fallback, even if later inventory finds new legacy debris.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PathFallbackCutMarker {
    pub version: u32,
    pub schema_epoch: u32,
    pub cut_at: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InventoryResolvedRow {
    pub entry_id: String,
    pub project: String,
    pub key: ResolvedKey,
}

/// Host-local proof of what this daemon store resolved during the current
/// schema-epoch pass. It never claims coverage for another host's store.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InventoryLedgerStore {
    pub version: u32,
    pub schema_epoch: u32,
    pub resolved: Vec<InventoryResolvedRow>,
    pub skipped_global: usize,
    pub marked_scopes: Vec<PublishedScope>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QuarantinedKnowledgeEntry {
    pub entry: KnowledgeEntry,
    pub reason: QuarantineReason,
}

/// Full quarantined bytes, not only an id/reason report. An unresolved legacy
/// entry has no honest repo-owned destination, so the host ledger must retain
/// enough information for operator repair.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QuarantineLedgerStore {
    pub version: u32,
    pub schema_epoch: u32,
    pub entries: Vec<QuarantinedKnowledgeEntry>,
}

#[derive(Debug, Clone)]
pub struct PersistedInventoryReport {
    pub inventory: KnowledgeInventory,
    pub marked_scopes: Vec<PublishedScope>,
    pub inventory_path: PathBuf,
    pub quarantine_path: PathBuf,
}

impl KnowledgeInventory {
    /// Coverage is proven by the epoch marker plus an EMPTY quarantine
    /// (design §3.5): every project-scoped entry resolved to a durable key.
    /// This is the gate the path-fallback cut (§6 step 8) waits on.
    pub fn is_covered(&self) -> bool {
        self.quarantined.is_empty()
    }
}

pub fn path_fallback_was_cut(state_dir: &Path) -> bool {
    state_dir.join(PATH_FALLBACK_CUT_MARKER).is_file()
}

/// Persist the cut before enabling it in memory. Existence is the monotonic
/// authority; the JSON body is audit metadata and is never rewritten.
pub fn persist_path_fallback_cut(state_dir: &Path) -> Result<PathBuf> {
    std::fs::create_dir_all(state_dir)
        .with_context(|| format!("creating inventory state dir {}", state_dir.display()))?;
    let path = state_dir.join(PATH_FALLBACK_CUT_MARKER);
    if path.is_file() {
        return Ok(path);
    }
    atomic_write_json_locked(
        &path,
        &PathFallbackCutMarker {
            version: 1,
            schema_epoch: SCHEMA_EPOCH,
            cut_at: chrono::Utc::now().to_rfc3339(),
        },
    )?;
    Ok(path)
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
            inv.quarantined
                .push(row(entry, QuarantineReason::NoResolvableRepoId));
            continue;
        };
        let Some(git_root) = git::git_root_for_path(project_path) else {
            inv.quarantined
                .push(row(entry, QuarantineReason::NotAGitRepo));
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

/// Run and persist the local schema-epoch migration products.
///
/// The quarantine ledger is written before any repo marker. A scope receives
/// its committed marker only when it has recorded/overridden repo authority,
/// owns a `.bbox/knowledge` directory, and every local-store entry associated
/// with that exact project root resolved to the same durable key. Re-running is
/// byte-idempotent and repairs a lost host ledger from source state.
pub fn persist_schema_epoch_inventory(
    entries: &[KnowledgeEntry],
    project_roots: &[PathBuf],
    state_dir: &Path,
    resolve_inputs: impl Fn(&Path) -> RepoIdInputs,
) -> Result<PersistedInventoryReport> {
    let inventory = inventory_project_entries(entries, |path| resolve_inputs(path));
    let quarantine_entries = inventory
        .quarantined
        .iter()
        .filter_map(|row| {
            entries
                .iter()
                .find(|entry| entry.id == row.entry_id)
                .cloned()
                .map(|entry| QuarantinedKnowledgeEntry {
                    entry,
                    reason: row.reason.clone(),
                })
        })
        .collect::<Vec<_>>();

    std::fs::create_dir_all(state_dir)
        .with_context(|| format!("creating inventory state dir {}", state_dir.display()))?;
    let quarantine_path = state_dir.join(QUARANTINE_LEDGER);
    write_json_if_changed(
        &quarantine_path,
        &QuarantineLedgerStore {
            version: 1,
            schema_epoch: SCHEMA_EPOCH,
            entries: quarantine_entries,
        },
    )?;

    let mut marked_scopes = Vec::new();
    for project_root in project_roots {
        let project_root = project_root
            .canonicalize()
            .unwrap_or_else(|_| project_root.clone());
        let knowledge_dir = project_root.join(".bbox").join("knowledge");
        if !knowledge_dir.is_dir() {
            continue;
        }
        let inputs = resolve_inputs(&project_root);
        let Some(repo_id) = resolve_recorded_repo_id(&inputs) else {
            continue;
        };
        let Some(git_root) = git::git_root_for_path(&project_root) else {
            continue;
        };
        let Some(relpath) = bbox_root_relpath(&git_root, &project_root) else {
            continue;
        };
        let scope = PublishedScope {
            repo_id,
            bbox_root_relpath: relpath,
        };
        let scoped_entries = entries.iter().filter(|entry| {
            entry.scope == Scope::Project
                && entry
                    .project
                    .as_deref()
                    .is_some_and(|project| project_path_matches(project, &project_root))
        });
        let clean = scoped_entries.into_iter().all(|entry| {
            inventory.resolved.get(&entry.id)
                == Some(&ResolvedKey {
                    repo_id: scope.repo_id.clone(),
                    bbox_root_relpath: scope.bbox_root_relpath.clone(),
                })
        });
        if !clean {
            continue;
        }
        write_json_if_changed(
            &knowledge_dir.join(SCHEMA_EPOCH_MARKER),
            &SchemaEpochMarker {
                schema_epoch: SCHEMA_EPOCH,
                repo_id: scope.repo_id.clone(),
                bbox_root_relpath: scope.bbox_root_relpath.clone(),
            },
        )?;
        marked_scopes.push(scope);
    }
    marked_scopes.sort();
    marked_scopes.dedup();

    let mut resolved = inventory
        .resolved
        .iter()
        .filter_map(|(entry_id, key)| {
            let project = entries
                .iter()
                .find(|entry| entry.id == *entry_id)?
                .project
                .clone()?;
            Some(InventoryResolvedRow {
                entry_id: entry_id.clone(),
                project,
                key: key.clone(),
            })
        })
        .collect::<Vec<_>>();
    resolved.sort_by(|a, b| a.entry_id.cmp(&b.entry_id));
    let inventory_path = state_dir.join(INVENTORY_LEDGER);
    write_json_if_changed(
        &inventory_path,
        &InventoryLedgerStore {
            version: 1,
            schema_epoch: SCHEMA_EPOCH,
            resolved,
            skipped_global: inventory.skipped_global,
            marked_scopes: marked_scopes.clone(),
        },
    )?;

    Ok(PersistedInventoryReport {
        inventory,
        marked_scopes,
        inventory_path,
        quarantine_path,
    })
}

fn project_path_matches(raw: &str, project_root: &Path) -> bool {
    let raw = Path::new(raw);
    raw == project_root
        || raw
            .canonicalize()
            .is_ok_and(|canonical| canonical == project_root)
}

fn write_json_if_changed(path: &Path, value: &impl Serialize) -> Result<()> {
    let mut expected = serde_json::to_vec_pretty(value)?;
    expected.push(b'\n');
    if std::fs::read(path).ok().as_deref() == Some(expected.as_slice()) {
        return Ok(());
    }
    atomic_write_json_locked(path, value)
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

    #[test]
    fn persisted_inventory_writes_clean_marker_and_host_ledgers() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        git_repo(&root);
        std::fs::create_dir_all(root.join(".bbox/knowledge")).unwrap();
        let state = root.join("state");
        let entries = vec![
            global_entry("global"),
            project_entry("project", Some(root.to_str().unwrap())),
        ];

        let report =
            persist_schema_epoch_inventory(&entries, std::slice::from_ref(&root), &state, |_| {
                recorded("repofam")
            })
            .unwrap();
        assert_eq!(report.marked_scopes.len(), 1);
        let marker: SchemaEpochMarker = serde_json::from_slice(
            &std::fs::read(root.join(".bbox/knowledge/.schema-epoch")).unwrap(),
        )
        .unwrap();
        assert_eq!(marker.schema_epoch, SCHEMA_EPOCH);
        assert_eq!(marker.repo_id, "repofam");
        assert_eq!(marker.bbox_root_relpath, ".");

        let ledger: InventoryLedgerStore =
            serde_json::from_slice(&std::fs::read(&report.inventory_path).unwrap()).unwrap();
        assert_eq!(ledger.resolved.len(), 1);
        assert_eq!(ledger.resolved[0].entry_id, "project");
        assert_eq!(ledger.skipped_global, 1);
        let quarantine: QuarantineLedgerStore =
            serde_json::from_slice(&std::fs::read(&report.quarantine_path).unwrap()).unwrap();
        assert!(quarantine.entries.is_empty());

        let before = std::fs::read(&report.inventory_path).unwrap();
        persist_schema_epoch_inventory(&entries, std::slice::from_ref(&root), &state, |_| {
            recorded("repofam")
        })
        .unwrap();
        assert_eq!(std::fs::read(&report.inventory_path).unwrap(), before);
    }

    #[test]
    fn persisted_inventory_quarantines_full_entry_and_withholds_marker() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        git_repo(&root);
        std::fs::create_dir_all(root.join(".bbox/knowledge")).unwrap();
        let state = root.join("state");
        let entries = vec![project_entry("orphan", Some(root.to_str().unwrap()))];

        let report =
            persist_schema_epoch_inventory(&entries, std::slice::from_ref(&root), &state, |_| {
                RepoIdInputs::default()
            })
            .unwrap();
        assert!(report.marked_scopes.is_empty());
        assert!(!root.join(".bbox/knowledge/.schema-epoch").exists());
        let quarantine: QuarantineLedgerStore =
            serde_json::from_slice(&std::fs::read(&report.quarantine_path).unwrap()).unwrap();
        assert_eq!(quarantine.entries.len(), 1);
        assert_eq!(quarantine.entries[0].entry.id, "orphan");
        assert_eq!(
            quarantine.entries[0].reason,
            QuarantineReason::NoResolvableRepoId
        );
    }

    #[test]
    fn path_fallback_cut_marker_is_persistent_and_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        assert!(!path_fallback_was_cut(&root));
        let path = persist_path_fallback_cut(&root).unwrap();
        assert!(path_fallback_was_cut(&root));
        let first = std::fs::read(&path).unwrap();
        assert_eq!(persist_path_fallback_cut(&root).unwrap(), path);
        assert_eq!(std::fs::read(path).unwrap(), first);
    }
}
