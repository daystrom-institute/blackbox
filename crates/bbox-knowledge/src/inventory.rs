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
use crate::repo_io::{KnowledgeRepoCarrier, KnowledgeRepoRead, KnowledgeRepoWrite};

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
    /// No logical repository carrier was configured for the durable project.
    NoProjectCarrier,
    /// The repository authority refused or could not resolve the carrier.
    RepoAccessUnavailable,
    /// No durable `repo_id` reachable: no override, no recorded id, no aka id,
    /// and no computed bootstrap hash (e.g. the project root is gone).
    NoResolvableRepoId,
    /// The project path is not inside a git repository, so it has no repo
    /// family and no `bbox_root_relpath`.
    NotAGitRepo,
    /// The project root resolved outside its own git root (malformed or moved
    /// state) — the relpath discriminator cannot be computed.
    ProjectRootOutsideGitRoot,
    /// A repo-owned entry or its directory could not be read safely.
    UnreadableRepoEntry,
    /// A repo-owned JSON file could not be parsed as a knowledge entry.
    MalformedRepoEntry,
    /// The durable filename and embedded entry id disagree.
    FilenameIdMismatch,
    /// A repo-owned entry is not project-scoped durable knowledge.
    InvalidRepoEntryScope,
    /// A committed repo-owned entry still embeds host path authority.
    RepoEntryContainsPathAuthority,
    /// A valid repo-owned file was absent from the daemon's loaded scope.
    LoadedStoreMismatch,
}

impl QuarantineReason {
    pub fn as_str(&self) -> &'static str {
        match self {
            QuarantineReason::NoProjectPath => "no_project_path",
            QuarantineReason::NoProjectCarrier => "no_project_carrier",
            QuarantineReason::RepoAccessUnavailable => "repo_access_unavailable",
            QuarantineReason::NoResolvableRepoId => "no_resolvable_repo_id",
            QuarantineReason::NotAGitRepo => "not_a_git_repo",
            QuarantineReason::ProjectRootOutsideGitRoot => "project_root_outside_git_root",
            QuarantineReason::UnreadableRepoEntry => "unreadable_repo_entry",
            QuarantineReason::MalformedRepoEntry => "malformed_repo_entry",
            QuarantineReason::FilenameIdMismatch => "filename_id_mismatch",
            QuarantineReason::InvalidRepoEntryScope => "invalid_repo_entry_scope",
            QuarantineReason::RepoEntryContainsPathAuthority => {
                "repo_entry_contains_path_authority"
            }
            QuarantineReason::LoadedStoreMismatch => "loaded_store_mismatch",
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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QuarantinedKnowledgeFile {
    pub project: String,
    pub path: String,
    pub reason: QuarantineReason,
    pub detail: String,
}

/// Full quarantined bytes, not only an id/reason report. An unresolved legacy
/// entry has no honest repo-owned destination, so the host ledger must retain
/// enough information for operator repair.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QuarantineLedgerStore {
    pub version: u32,
    pub schema_epoch: u32,
    pub entries: Vec<QuarantinedKnowledgeEntry>,
    #[serde(default)]
    pub files: Vec<QuarantinedKnowledgeFile>,
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

pub fn path_fallback_was_cut(state_dir: &Path) -> Result<bool> {
    let path = state_dir.join(PATH_FALLBACK_CUT_MARKER);
    if !path.exists() {
        return Ok(false);
    }
    let raw = std::fs::read(&path).with_context(|| format!("reading {}", path.display()))?;
    let marker: PathFallbackCutMarker =
        serde_json::from_slice(&raw).with_context(|| format!("parsing {}", path.display()))?;
    anyhow::ensure!(
        marker.version == 1,
        "unsupported path-fallback cut marker version {} in {}",
        marker.version,
        path.display()
    );
    anyhow::ensure!(
        marker.schema_epoch == SCHEMA_EPOCH,
        "path-fallback cut marker schema epoch {} does not match {} in {}",
        marker.schema_epoch,
        SCHEMA_EPOCH,
        path.display()
    );
    chrono::DateTime::parse_from_rfc3339(&marker.cut_at)
        .with_context(|| format!("invalid cut_at in {}", path.display()))?;
    Ok(true)
}

/// Persist the cut before enabling it in memory. Existence is the monotonic
/// authority; the JSON body is audit metadata and is never rewritten.
pub fn persist_path_fallback_cut(state_dir: &Path) -> Result<PathBuf> {
    std::fs::create_dir_all(state_dir)
        .with_context(|| format!("creating inventory state dir {}", state_dir.display()))?;
    let path = state_dir.join(PATH_FALLBACK_CUT_MARKER);
    if path.exists() {
        path_fallback_was_cut(state_dir)?;
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
    carriers: &[KnowledgeRepoCarrier],
    repo_read: &dyn KnowledgeRepoRead,
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
        let Some(carrier) = carriers.iter().find(|carrier| carrier.project == project) else {
            inv.quarantined
                .push(row(entry, QuarantineReason::NoProjectCarrier));
            continue;
        };
        let mut resolution = None;
        let access = repo_read.with_read(carrier, &mut |project_root| {
            let inputs = resolve_inputs(project_root);
            let Some(repo_id) = resolve_repo_id(&inputs) else {
                resolution = Some(Err(QuarantineReason::NoResolvableRepoId));
                return Ok(());
            };
            let Some(git_root) = git::git_root_for_path(project_root) else {
                resolution = Some(Err(QuarantineReason::NotAGitRepo));
                return Ok(());
            };
            let Some(relpath) = bbox_root_relpath(&git_root, project_root) else {
                resolution = Some(Err(QuarantineReason::ProjectRootOutsideGitRoot));
                return Ok(());
            };
            resolution = Some(Ok(ResolvedKey {
                repo_id,
                bbox_root_relpath: relpath,
            }));
            Ok(())
        });
        match (access, resolution) {
            (Ok(()), Some(Ok(key))) => {
                inv.resolved.insert(entry.id.clone(), key);
            }
            (Ok(()), Some(Err(reason))) => inv.quarantined.push(row(entry, reason)),
            _ => inv
                .quarantined
                .push(row(entry, QuarantineReason::RepoAccessUnavailable)),
        }
    }
    inv
}

/// Run and persist the local schema-epoch migration products, including the
/// repo-owned marker for every clean scope.
///
/// The quarantine ledger is written before any repo marker. A scope receives
/// its committed marker only when it has recorded/overridden repo authority,
/// owns a `.bbox/knowledge` directory, and every local-store entry associated
/// with that exact project root resolved to the same durable key. Re-running is
/// byte-idempotent and repairs a lost host ledger from source state. Callers
/// must invoke this only from an explicit operator mutation such as project
/// ejection. Background reconciliation must use
/// [`persist_schema_epoch_inventory_read_only`] instead.
pub fn persist_schema_epoch_inventory(
    entries: &[KnowledgeEntry],
    carriers: &[KnowledgeRepoCarrier],
    state_dir: &Path,
    repo_read: &dyn KnowledgeRepoRead,
    repo_write: &dyn KnowledgeRepoWrite,
    resolve_inputs: impl Fn(&Path) -> RepoIdInputs,
) -> Result<PersistedInventoryReport> {
    persist_schema_epoch_inventory_inner(
        entries,
        carriers,
        state_dir,
        repo_read,
        Some(repo_write),
        resolve_inputs,
    )
}

/// Persist host-local inventory and quarantine ledgers without modifying any
/// registered checkout. Existing matching repo markers are reported, but a
/// missing marker remains an explicit operator migration task.
pub fn persist_schema_epoch_inventory_read_only(
    entries: &[KnowledgeEntry],
    carriers: &[KnowledgeRepoCarrier],
    state_dir: &Path,
    repo_read: &dyn KnowledgeRepoRead,
    resolve_inputs: impl Fn(&Path) -> RepoIdInputs,
) -> Result<PersistedInventoryReport> {
    persist_schema_epoch_inventory_inner(
        entries,
        carriers,
        state_dir,
        repo_read,
        None,
        resolve_inputs,
    )
}

fn persist_schema_epoch_inventory_inner(
    entries: &[KnowledgeEntry],
    carriers: &[KnowledgeRepoCarrier],
    state_dir: &Path,
    repo_read: &dyn KnowledgeRepoRead,
    repo_write: Option<&dyn KnowledgeRepoWrite>,
    resolve_inputs: impl Fn(&Path) -> RepoIdInputs,
) -> Result<PersistedInventoryReport> {
    let mut inventory =
        inventory_project_entries(entries, carriers, repo_read, |path| resolve_inputs(path));
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
    let mut quarantined_files = Vec::new();
    let mut marked_scopes = Vec::new();
    for carrier in carriers {
        let mut outcome = None;
        let mut operation = |project_root: &Path| -> Result<()> {
            let knowledge_dir = project_root.join(".bbox").join("knowledge");
            if !knowledge_dir.is_dir() {
                outcome = Some(None);
                return Ok(());
            }
            let inputs = resolve_inputs(project_root);
            let Some(repo_id) = resolve_recorded_repo_id(&inputs) else {
                outcome = Some(None);
                return Ok(());
            };
            let Some(git_root) = git::git_root_for_path(project_root) else {
                outcome = Some(None);
                return Ok(());
            };
            let Some(relpath) = bbox_root_relpath(&git_root, project_root) else {
                outcome = Some(None);
                return Ok(());
            };
            let scope = PublishedScope::try_new(repo_id, relpath)?;
            let scope_file_quarantine =
                inspect_repo_owned_files(project_root, &carrier.project, entries);
            let files_clean = scope_file_quarantine.is_empty();
            let clean = files_clean
                && entries
                    .iter()
                    .filter(|entry| {
                        entry.scope == Scope::Project
                            && entry.project.as_deref() == Some(carrier.project.as_str())
                    })
                    .all(|entry| {
                        inventory.resolved.get(&entry.id)
                            == Some(&ResolvedKey {
                                repo_id: scope.repo_id().to_string(),
                                bbox_root_relpath: scope.bbox_root_relpath().to_string(),
                            })
                    });
            if !clean {
                outcome = Some(Some((scope, scope_file_quarantine, false)));
                return Ok(());
            }
            let marker_path = knowledge_dir.join(SCHEMA_EPOCH_MARKER);
            let marker = SchemaEpochMarker {
                schema_epoch: SCHEMA_EPOCH,
                repo_id: scope.repo_id().to_string(),
                bbox_root_relpath: scope.bbox_root_relpath().to_string(),
            };
            let marker_present = if repo_write.is_some() {
                write_json_if_changed(&marker_path, &marker)?;
                true
            } else {
                std::fs::read(&marker_path)
                    .ok()
                    .and_then(|raw| serde_json::from_slice::<SchemaEpochMarker>(&raw).ok())
                    .is_some_and(|existing| existing == marker)
            };
            outcome = Some(Some((scope, scope_file_quarantine, marker_present)));
            Ok(())
        };
        let access = match repo_write {
            Some(write) => write.with_write(carrier, &mut operation),
            None => repo_read.with_read(carrier, &mut operation),
        };
        drop(operation);
        if let Err(err) = access {
            let file = QuarantinedKnowledgeFile {
                project: carrier.project.clone(),
                path: ".bbox/knowledge".into(),
                reason: QuarantineReason::RepoAccessUnavailable,
                detail: err.to_string(),
            };
            inventory.quarantined.push(QuarantineRow {
                entry_id: "<repo-scope>".into(),
                project: Some(carrier.project.clone()),
                reason: file.reason.clone(),
            });
            quarantined_files.push(file);
            continue;
        }
        let Some(Some((scope, scope_file_quarantine, marker_present))) = outcome else {
            continue;
        };
        for file in &scope_file_quarantine {
            inventory.quarantined.push(QuarantineRow {
                entry_id: Path::new(&file.path)
                    .file_stem()
                    .and_then(|stem| stem.to_str())
                    .unwrap_or("<repo-entry>")
                    .to_string(),
                project: Some(file.project.clone()),
                reason: file.reason.clone(),
            });
        }
        quarantined_files.extend(scope_file_quarantine);
        if marker_present {
            marked_scopes.push(scope);
        }
    }
    marked_scopes.sort();
    marked_scopes.dedup();

    write_json_if_changed(
        &quarantine_path,
        &QuarantineLedgerStore {
            version: 1,
            schema_epoch: SCHEMA_EPOCH,
            entries: quarantine_entries,
            files: quarantined_files,
        },
    )?;

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

fn inspect_repo_owned_files(
    project_root: &Path,
    durable_project: &str,
    loaded_entries: &[KnowledgeEntry],
) -> Vec<QuarantinedKnowledgeFile> {
    let knowledge_dir = project_root.join(".bbox").join("knowledge");
    let project = durable_project.to_string();
    let read_dir = match std::fs::read_dir(&knowledge_dir) {
        Ok(read_dir) => read_dir,
        Err(err) => {
            return vec![QuarantinedKnowledgeFile {
                project,
                path: ".bbox/knowledge".into(),
                reason: QuarantineReason::UnreadableRepoEntry,
                detail: err.to_string(),
            }];
        }
    };
    let mut quarantined = Vec::new();
    for item in read_dir {
        let entry = match item {
            Ok(entry) => entry,
            Err(err) => {
                quarantined.push(QuarantinedKnowledgeFile {
                    project: project.clone(),
                    path: ".bbox/knowledge".into(),
                    reason: QuarantineReason::UnreadableRepoEntry,
                    detail: err.to_string(),
                });
                continue;
            }
        };
        let path = entry.path();
        if path.extension().and_then(|extension| extension.to_str()) != Some("json") {
            continue;
        }
        let path_string = path
            .strip_prefix(project_root)
            .unwrap_or(&path)
            .to_string_lossy()
            .into_owned();
        let metadata = match std::fs::symlink_metadata(&path) {
            Ok(metadata) if !metadata.file_type().is_symlink() => metadata,
            Ok(_) => {
                quarantined.push(QuarantinedKnowledgeFile {
                    project: project.clone(),
                    path: path_string,
                    reason: QuarantineReason::UnreadableRepoEntry,
                    detail: "repo-owned knowledge files must not be symlinks".into(),
                });
                continue;
            }
            Err(err) => {
                quarantined.push(QuarantinedKnowledgeFile {
                    project: project.clone(),
                    path: path_string,
                    reason: QuarantineReason::UnreadableRepoEntry,
                    detail: err.to_string(),
                });
                continue;
            }
        };
        if !metadata.is_file() {
            quarantined.push(QuarantinedKnowledgeFile {
                project: project.clone(),
                path: path_string,
                reason: QuarantineReason::UnreadableRepoEntry,
                detail: "repo-owned knowledge JSON path is not a regular file".into(),
            });
            continue;
        }
        let bytes = match std::fs::read(&path) {
            Ok(bytes) => bytes,
            Err(err) => {
                quarantined.push(QuarantinedKnowledgeFile {
                    project: project.clone(),
                    path: path_string,
                    reason: QuarantineReason::UnreadableRepoEntry,
                    detail: err.to_string(),
                });
                continue;
            }
        };
        let parsed: KnowledgeEntry = match serde_json::from_slice(&bytes) {
            Ok(parsed) => parsed,
            Err(err) => {
                quarantined.push(QuarantinedKnowledgeFile {
                    project: project.clone(),
                    path: path_string,
                    reason: QuarantineReason::MalformedRepoEntry,
                    detail: err.to_string(),
                });
                continue;
            }
        };
        let stem = path.file_stem().and_then(|stem| stem.to_str());
        if stem != Some(parsed.id.as_str()) {
            quarantined.push(QuarantinedKnowledgeFile {
                project: project.clone(),
                path: path_string,
                reason: QuarantineReason::FilenameIdMismatch,
                detail: format!("filename stem {stem:?} does not match id {}", parsed.id),
            });
            continue;
        }
        if parsed.scope != Scope::Project {
            quarantined.push(QuarantinedKnowledgeFile {
                project: project.clone(),
                path: path_string,
                reason: QuarantineReason::InvalidRepoEntryScope,
                detail: format!(
                    "repo-owned entry {} has scope {:?}",
                    parsed.id, parsed.scope
                ),
            });
            continue;
        }
        if parsed.project.is_some() {
            quarantined.push(QuarantinedKnowledgeFile {
                project: project.clone(),
                path: path_string,
                reason: QuarantineReason::RepoEntryContainsPathAuthority,
                detail: format!("repo-owned entry {} embeds a project path", parsed.id),
            });
            continue;
        }
        if !loaded_entries.iter().any(|entry| {
            entry.id == parsed.id
                && entry.scope == Scope::Project
                && entry.project.as_deref() == Some(durable_project)
        }) {
            quarantined.push(QuarantinedKnowledgeFile {
                project: project.clone(),
                path: path_string,
                reason: QuarantineReason::LoadedStoreMismatch,
                detail: format!(
                    "repo-owned entry {} is absent from the loaded project scope",
                    parsed.id
                ),
            });
        }
    }
    quarantined.sort_by(|left, right| left.path.cmp(&right.path));
    quarantined
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
    use crate::repo_io::test_support::TestKnowledgeRepoIo;
    use std::path::{Path, PathBuf};

    fn test_access(roots: &[PathBuf]) -> (TestKnowledgeRepoIo, Vec<KnowledgeRepoCarrier>) {
        let pairs = roots
            .iter()
            .map(|root| {
                let project = root.to_string_lossy().into_owned();
                (
                    KnowledgeRepoCarrier::new(project.clone(), project).unwrap(),
                    root.clone(),
                )
            })
            .collect::<Vec<_>>();
        let io = TestKnowledgeRepoIo::default();
        io.replace(&pairs);
        let carriers = pairs.into_iter().map(|(carrier, _)| carrier).collect();
        (io, carriers)
    }

    fn inventory_for_test(
        entries: &[KnowledgeEntry],
        resolve_inputs: impl Fn(&Path) -> RepoIdInputs,
    ) -> KnowledgeInventory {
        let roots = entries
            .iter()
            .filter_map(|entry| entry.project.as_ref().map(PathBuf::from))
            .collect::<Vec<_>>();
        let (io, carriers) = test_access(&roots);
        inventory_project_entries(entries, &carriers, &io, resolve_inputs)
    }

    fn persist_for_test(
        entries: &[KnowledgeEntry],
        roots: &[PathBuf],
        state_dir: &Path,
        resolve_inputs: impl Fn(&Path) -> RepoIdInputs,
    ) -> Result<PersistedInventoryReport> {
        let (io, carriers) = test_access(roots);
        persist_schema_epoch_inventory(entries, &carriers, state_dir, &io, &io, resolve_inputs)
    }

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
        let inv = inventory_for_test(&entries, |_| recorded("repofam"));
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
        let inv = inventory_for_test(&entries, |_| recorded("repofam"));
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
        let inv = inventory_for_test(&entries, |_| RepoIdInputs::default());
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
        let inv = inventory_for_test(&entries, |_| recorded("repofam"));
        assert_eq!(inv.quarantined.len(), 1);
        assert_eq!(inv.quarantined[0].reason, QuarantineReason::NotAGitRepo);
    }

    #[test]
    fn quarantines_missing_project_path() {
        let entries = vec![project_entry("e1", None)];
        let inv = inventory_for_test(&entries, |_| recorded("repofam"));
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
        let inv = inventory_for_test(&entries, |_| recorded("repofam"));
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
        let inv = inventory_for_test(&entries, |_| RepoIdInputs {
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

        let report = persist_for_test(&entries, std::slice::from_ref(&root), &state, |_| {
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
        assert!(quarantine.files.is_empty());

        let before = std::fs::read(&report.inventory_path).unwrap();
        persist_for_test(&entries, std::slice::from_ref(&root), &state, |_| {
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

        let report = persist_for_test(&entries, std::slice::from_ref(&root), &state, |_| {
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
        assert!(!path_fallback_was_cut(&root).unwrap());
        let path = persist_path_fallback_cut(&root).unwrap();
        assert!(path_fallback_was_cut(&root).unwrap());
        let first = std::fs::read(&path).unwrap();
        assert_eq!(persist_path_fallback_cut(&root).unwrap(), path);
        assert_eq!(std::fs::read(path).unwrap(), first);
    }

    #[test]
    fn malformed_repo_file_blocks_marker_and_is_quarantined() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        git_repo(&root);
        let knowledge_dir = root.join(".bbox/knowledge");
        std::fs::create_dir_all(&knowledge_dir).unwrap();
        std::fs::write(knowledge_dir.join("broken.json"), b"{not json").unwrap();
        let state = root.join("state");

        let report =
            persist_for_test(&[], &[root.clone()], &state, |_| recorded("repofam")).unwrap();

        assert!(!report.inventory.is_covered());
        assert!(report.marked_scopes.is_empty());
        assert!(!knowledge_dir.join(SCHEMA_EPOCH_MARKER).exists());
        let quarantine: QuarantineLedgerStore =
            serde_json::from_slice(&std::fs::read(&report.quarantine_path).unwrap()).unwrap();
        assert_eq!(quarantine.files.len(), 1);
        assert_eq!(
            quarantine.files[0].reason,
            QuarantineReason::MalformedRepoEntry
        );
    }

    #[test]
    fn malformed_cut_marker_fails_closed() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        std::fs::write(root.join(PATH_FALLBACK_CUT_MARKER), b"{}").unwrap();

        assert!(path_fallback_was_cut(&root).is_err());
        assert!(persist_path_fallback_cut(&root).is_err());
    }
}
