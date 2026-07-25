//! Crash-consistent host-local transactions for repo-owned corpus files.
//!
//! Git commit is the traveling transaction. This module only protects the
//! daemon's uncommitted multi-file apply window inside one checkout. The
//! pointer is both the exclusive claim and the loader-visible pending marker;
//! old and new bytes remain available until the apply reaches a terminal state.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use crate::json_store::{NofollowDirectory, atomic_write_json_locked, to_vec_pretty_newline};
use anyhow::{Context, Result};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

const TRANSACTION_VERSION: u32 = 1;
// These knowledge-era names are part of the durable v1 layout and manifest
// identity. Gaps share the lane without rewriting existing closeout proofs.
const TRANSACTION_KIND: &str = "knowledge_transaction_v1";
const TRANSACTION_ROOT: &str = "knowledge-transactions";
const PENDING_FILE: &str = "pending.json";
const MAX_COMPLETED_MANIFESTS: usize = 64;
static TRANSACTION_NONCE: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone)]
pub struct TransactionWrite {
    pub target: PathBuf,
    pub new_bytes: Option<Vec<u8>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct TransactionPointer {
    version: u32,
    transaction_id: String,
    state: TransactionState,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum TransactionState {
    Preparing,
    Applying,
    Blocked,
    Closeout,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RepoTransactionManifest {
    pub version: u32,
    pub kind: String,
    pub transaction_id: String,
    pub created_at: String,
    pub files: Vec<RepoTransactionFile>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RepoTransactionFile {
    pub relative_path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub old_ref: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub new_ref: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub old_sha256: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub new_sha256: Option<String>,
}

/// Compatibility names for callers and persisted tests from the original
/// knowledge-only transaction lane. The wire format remains version 1.
pub type KnowledgeTransactionManifest = RepoTransactionManifest;
pub type KnowledgeTransactionFile = RepoTransactionFile;

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct CompletedTransactionOrderKey {
    unix_seconds: i64,
    nanoseconds: u32,
    transaction_id: String,
}

/// Parse the durable manifest timestamp before ordering completed transactions.
/// RFC3339's variable-width fractional seconds are not lexicographically sortable.
pub fn completed_transaction_order_key(
    created_at: &str,
    transaction_id: &str,
) -> Result<CompletedTransactionOrderKey> {
    validate_transaction_id(transaction_id)?;
    let timestamp = chrono::DateTime::parse_from_rfc3339(created_at)
        .with_context(|| format!("invalid transaction created_at {created_at:?}"))?;
    Ok(CompletedTransactionOrderKey {
        unix_seconds: timestamp.timestamp(),
        nanoseconds: timestamp.timestamp_subsec_nanos(),
        transaction_id: transaction_id.to_string(),
    })
}

/// True while a daemon repo-file transaction owns this checkout.
pub fn has_pending_transaction(checkout_dir: &Path) -> bool {
    pending_path(checkout_dir).is_file()
}

/// Apply all writes atomically from the loader's perspective. The canonical
/// files may be replaced one at a time, but the pending pointer remains set for
/// the entire window and recovery retains both directions until completion.
pub fn apply_transaction(
    checkout_dir: &Path,
    writes: Vec<TransactionWrite>,
) -> Result<Option<RepoTransactionManifest>> {
    if writes.is_empty() {
        return Ok(None);
    }
    apply_planned_transaction(checkout_dir, || Ok(writes))
}

/// Plan and apply one transaction while holding the checkout transaction lane.
///
/// Bulk read-modify-write callers must use this entry point so directory
/// enumeration, generation-purge decisions, and old-byte capture cannot race a
/// second daemon writer that owns the same checkout. The returned writes still
/// flow through the ordinary crash-consistent manifest and recovery protocol.
pub fn apply_planned_transaction(
    checkout_dir: &Path,
    plan: impl FnOnce() -> Result<Vec<TransactionWrite>>,
) -> Result<Option<RepoTransactionManifest>> {
    let checkout_dir = checkout_dir
        .canonicalize()
        .with_context(|| format!("canonicalizing checkout {}", checkout_dir.display()))?;
    let root = transaction_root(&checkout_dir);
    reject_symlink_components(
        &checkout_dir,
        Path::new(".bbox/local/knowledge-transactions"),
    )?;
    fs::create_dir_all(root.join("completed"))
        .with_context(|| format!("creating transaction root {}", root.display()))?;
    reject_symlink_components(&root, Path::new("completed"))?;
    let _lane_lock = acquire_transaction_lane(&root, false)?.with_context(|| {
        format!(
            "knowledge transaction lane at {} is active; retry after the current writer or closeout finishes",
            root.display()
        )
    })?;
    let writes = plan()?;
    if writes.is_empty() {
        return Ok(None);
    }
    ensure_unique_targets(&writes)?;

    let transaction_id = transaction_id();
    let pointer_path = root.join(PENDING_FILE);
    let pointer = TransactionPointer {
        version: TRANSACTION_VERSION,
        transaction_id: transaction_id.clone(),
        state: TransactionState::Preparing,
    };
    create_claim(&pointer_path, &pointer)?;

    let transaction_dir = root.join(&transaction_id);
    let prepared = prepare_manifest(&checkout_dir, &transaction_dir, &transaction_id, writes);
    let manifest = match prepared {
        Ok(manifest) => manifest,
        Err(err) => {
            let _ = fs::remove_dir_all(&transaction_dir);
            let _ = clear_pointer(&pointer_path);
            return Err(err);
        }
    };
    atomic_write_json_locked(&transaction_dir.join("manifest.json"), &manifest)?;
    sync_dir(&transaction_dir)?;
    atomic_write_json_locked(
        &pointer_path,
        &TransactionPointer {
            state: TransactionState::Applying,
            ..pointer
        },
    )?;
    sync_dir(&root)?;

    if let Err(apply_err) = apply_manifest(&checkout_dir, &transaction_dir, &manifest) {
        match rollback_manifest(&checkout_dir, &transaction_dir, &manifest) {
            Ok(()) => {
                finish_terminal_rollback(&root, &transaction_dir, &pointer_path)?;
                return Err(
                    apply_err.context("knowledge transaction applied partially and rolled back")
                );
            }
            Err(rollback_err) => {
                return Err(apply_err.context(format!(
                    "knowledge transaction apply failed and rollback also failed: {rollback_err:#}; pending recovery retained"
                )));
            }
        }
    }

    complete_transaction(&root, &transaction_dir, &pointer_path, &manifest)?;
    Ok(Some(manifest))
}

/// Recover the one pending transaction for a checkout. Preparing transactions
/// are always discarded because the durable Applying pointer is the commit
/// point. An Applying manifest rolls forward idempotently when its staged new
/// bytes are intact. If roll-forward cannot verify those bytes, recovery
/// restores the checksummed old direction and clears the pointer at that
/// terminal state.
pub fn recover_pending_transaction(checkout_dir: &Path) -> Result<Option<RepoTransactionManifest>> {
    recover_pending_transaction_with_lock(checkout_dir, true)
}

/// Recover a pending transaction only when no live writer or closeout still
/// owns the checkout lane. The directory advisory lock is released by the OS
/// if its process unwinds or exits, so periodic reconciliation can distinguish
/// an abandoned pointer from an in-flight transaction without a timeout.
pub fn recover_abandoned_pending_transaction(
    checkout_dir: &Path,
) -> Result<Option<RepoTransactionManifest>> {
    recover_pending_transaction_with_lock(checkout_dir, false)
}

/// Probe whether a pending transaction is still owned by a live lane without
/// waiting or mutating recovery state. Callers can use this cheap read phase
/// before acquiring broader repository-mutation authority.
pub fn pending_transaction_lane_is_busy(checkout_dir: &Path) -> Result<bool> {
    let pointer_path = pending_path(checkout_dir);
    if !pointer_path.exists() {
        return Ok(false);
    }
    let checkout_dir = checkout_dir
        .canonicalize()
        .with_context(|| format!("canonicalizing checkout {}", checkout_dir.display()))?;
    let root = transaction_root(&checkout_dir);
    reject_symlink_components(
        &checkout_dir,
        Path::new(".bbox/local/knowledge-transactions/pending.json"),
    )?;
    Ok(acquire_transaction_lane(&root, false)?.is_none())
}

fn recover_pending_transaction_with_lock(
    checkout_dir: &Path,
    wait_for_lane: bool,
) -> Result<Option<RepoTransactionManifest>> {
    let pointer_path = pending_path(checkout_dir);
    if !pointer_path.exists() {
        return Ok(None);
    }
    let checkout_dir = checkout_dir
        .canonicalize()
        .with_context(|| format!("canonicalizing checkout {}", checkout_dir.display()))?;
    let root = transaction_root(&checkout_dir);
    reject_symlink_components(
        &checkout_dir,
        Path::new(".bbox/local/knowledge-transactions/pending.json"),
    )?;
    let Some(_lane_lock) = acquire_transaction_lane(&root, wait_for_lane)? else {
        return Ok(None);
    };
    if !pointer_path.exists() {
        return Ok(None);
    }
    let pointer_bytes = fs::read(&pointer_path)
        .with_context(|| format!("reading pending pointer {}", pointer_path.display()))?;
    let pointer: TransactionPointer = match serde_json::from_slice(&pointer_bytes) {
        Ok(pointer) => pointer,
        Err(err) => {
            // The pre-atomic v1 claim writer could crash after create_new but
            // before the pointer bytes were complete. Canonical files were not
            // touched while the pointer was in that preparing window, so an
            // unparseable LEGACY pointer is safe to clear and must not wedge
            // the checkout forever. The clear is gated on the era
            // discriminator the bytes themselves cannot supply: a
            // current-era transaction always has a staging directory under
            // the transaction root, so unparseable bytes BESIDE a staging
            // directory could be a corrupted `Applying` pointer with
            // half-applied canonical files and must route to the blocked
            // path, never a silent clear.
            if root_holds_transaction_dirs(&root)? {
                anyhow::bail!(
                    "error.repo_transaction_recovery_blocked: pending pointer is \
                     unreadable beside staged transaction state; operator repair \
                     required ({err})"
                );
            }
            clear_pointer(&pointer_path).with_context(|| {
                format!(
                    "clearing unparseable pending pointer {} after parse error: {err}",
                    pointer_path.display()
                )
            })?;
            cleanup_terminal_debris_best_effort(&root);
            return Ok(None);
        }
    };
    if pointer.version != TRANSACTION_VERSION {
        anyhow::bail!(
            "unsupported knowledge transaction pointer version {}",
            pointer.version
        );
    }
    validate_transaction_id(&pointer.transaction_id)?;
    if matches!(pointer.state, TransactionState::Closeout) {
        clear_pointer(&pointer_path)?;
        cleanup_terminal_debris_best_effort(&root);
        return Ok(None);
    }
    if matches!(pointer.state, TransactionState::Blocked) {
        anyhow::bail!(
            "error.repo_transaction_recovery_blocked: pending transaction requires operator repair"
        );
    }
    let transaction_dir = root.join(&pointer.transaction_id);
    reject_symlink_components(&root, Path::new(&pointer.transaction_id))?;
    let manifest_path = transaction_dir.join("manifest.json");
    if matches!(pointer.state, TransactionState::Preparing) {
        let _ = fs::remove_dir_all(&transaction_dir);
        clear_pointer(&pointer_path)?;
        cleanup_terminal_debris_best_effort(&root);
        return Ok(None);
    }
    let manifest = (|| {
        let manifest: RepoTransactionManifest =
            serde_json::from_slice(&fs::read(&manifest_path).with_context(|| {
                format!("reading transaction manifest {}", manifest_path.display())
            })?)
            .with_context(|| format!("parsing transaction manifest {}", manifest_path.display()))?;
        validate_manifest(&manifest, &pointer.transaction_id)?;
        Ok::<_, anyhow::Error>(manifest)
    })()
    .or_else(|error| {
        atomic_write_json_locked(
            &pointer_path,
            &TransactionPointer {
                state: TransactionState::Blocked,
                ..pointer.clone()
            },
        )?;
        sync_dir(&root)?;
        Err(error.context(
            "error.repo_transaction_recovery_blocked: applying transaction manifest is unavailable",
        ))
    })?;
    if let Err(apply_err) = apply_manifest(&checkout_dir, &transaction_dir, &manifest) {
        match rollback_manifest(&checkout_dir, &transaction_dir, &manifest) {
            Ok(()) => {
                finish_terminal_rollback(&root, &transaction_dir, &pointer_path)?;
                tracing::warn!(
                    transaction_id = %manifest.transaction_id,
                    error = %apply_err,
                    "knowledge transaction recovery rolled back after roll-forward failed"
                );
                return Ok(Some(manifest));
            }
            Err(rollback_err) => {
                return Err(apply_err.context(format!(
                    "knowledge transaction recovery could neither roll forward nor roll back: {rollback_err:#}; pending recovery retained"
                )));
            }
        }
    }
    complete_transaction(&root, &transaction_dir, &pointer_path, &manifest)?;
    Ok(Some(manifest))
}

fn acquire_transaction_lane(root: &Path, wait: bool) -> Result<Option<File>> {
    let lane = File::open(root)
        .with_context(|| format!("opening knowledge transaction lane {}", root.display()))?;
    if wait {
        lane.lock_exclusive()
            .with_context(|| format!("locking knowledge transaction lane {}", root.display()))?;
        return Ok(Some(lane));
    }
    match lane.try_lock_exclusive() {
        Ok(()) => Ok(Some(lane)),
        Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => Ok(None),
        Err(err) => Err(err)
            .with_context(|| format!("locking knowledge transaction lane {}", root.display())),
    }
}

fn prepare_manifest(
    checkout_dir: &Path,
    transaction_dir: &Path,
    transaction_id: &str,
    writes: Vec<TransactionWrite>,
) -> Result<RepoTransactionManifest> {
    let old_directory = NofollowDirectory::open_or_create(&transaction_dir.join("old"))?;
    let new_directory = NofollowDirectory::open_or_create(&transaction_dir.join("new"))?;
    let mut files = Vec::with_capacity(writes.len());
    for (index, write) in writes.into_iter().enumerate() {
        let relative = write.target.strip_prefix(checkout_dir).with_context(|| {
            format!(
                "transaction target {} is outside checkout {}",
                write.target.display(),
                checkout_dir.display()
            )
        })?;
        validate_relative_path(relative)?;
        reject_symlink_components(checkout_dir, relative)?;
        let relative_path = relative.to_string_lossy().replace('\\', "/");
        let old_bytes =
            read_relative_regular_nofollow(checkout_dir, relative, "transaction target")?;
        let old_ref = old_bytes.as_ref().map(|bytes| {
            let name = format!("old/{index}");
            (name, bytes)
        });
        if let Some((name, bytes)) = &old_ref {
            old_directory.atomic_replace(
                Path::new(name)
                    .file_name()
                    .and_then(|name| name.to_str())
                    .context("staged old filename is not UTF-8")?,
                bytes,
            )?;
        }
        let new_ref = write.new_bytes.as_ref().map(|bytes| {
            let name = format!("new/{index}");
            (name, bytes)
        });
        if let Some((name, bytes)) = &new_ref {
            new_directory.atomic_replace(
                Path::new(name)
                    .file_name()
                    .and_then(|name| name.to_str())
                    .context("staged new filename is not UTF-8")?,
                bytes,
            )?;
        }
        files.push(RepoTransactionFile {
            relative_path,
            old_ref: old_ref.as_ref().map(|(name, _)| name.clone()),
            new_ref: new_ref.as_ref().map(|(name, _)| name.clone()),
            old_sha256: old_bytes.as_deref().map(sha256),
            new_sha256: write.new_bytes.as_deref().map(sha256),
        });
    }
    old_directory.ensure_still_current()?;
    new_directory.ensure_still_current()?;
    Ok(RepoTransactionManifest {
        version: TRANSACTION_VERSION,
        kind: TRANSACTION_KIND.to_string(),
        transaction_id: transaction_id.to_string(),
        created_at: chrono::Utc::now().to_rfc3339(),
        files,
    })
}

fn apply_manifest(
    checkout_dir: &Path,
    transaction_dir: &Path,
    manifest: &RepoTransactionManifest,
) -> Result<()> {
    validate_manifest(manifest, &manifest.transaction_id)?;
    for file in &manifest.files {
        let relative = Path::new(&file.relative_path);
        if let Some(new_ref) = &file.new_ref {
            let bytes =
                read_relative_regular_nofollow(transaction_dir, Path::new(new_ref), "staged new")?
                    .context("staged new bytes are missing")?;
            if sha256(&bytes) != file.new_sha256.as_deref().unwrap_or_default() {
                anyhow::bail!(
                    "staged new bytes failed checksum for {}",
                    file.relative_path
                );
            }
            replace_relative_regular_nofollow(checkout_dir, relative, transaction_dir, &bytes)?;
        } else {
            remove_relative_regular_nofollow(checkout_dir, relative)?;
        }
    }
    Ok(())
}

fn rollback_manifest(
    checkout_dir: &Path,
    transaction_dir: &Path,
    manifest: &RepoTransactionManifest,
) -> Result<()> {
    for file in &manifest.files {
        let relative = Path::new(&file.relative_path);
        if let Some(old_ref) = &file.old_ref {
            let bytes =
                read_relative_regular_nofollow(transaction_dir, Path::new(old_ref), "staged old")?
                    .context("staged old bytes are missing")?;
            if sha256(&bytes) != file.old_sha256.as_deref().unwrap_or_default() {
                anyhow::bail!(
                    "staged old bytes failed checksum for {}",
                    file.relative_path
                );
            }
            replace_relative_regular_nofollow(checkout_dir, relative, transaction_dir, &bytes)?;
        } else {
            remove_relative_regular_nofollow(checkout_dir, relative)?;
        }
    }
    Ok(())
}

fn read_relative_regular_nofollow(
    base: &Path,
    relative: &Path,
    label: &str,
) -> Result<Option<Vec<u8>>> {
    validate_relative_path(relative)?;
    let parent_relative = relative.parent().unwrap_or_else(|| Path::new("."));
    let Some(parent) = NofollowDirectory::open_existing(&base.join(parent_relative))? else {
        return Ok(None);
    };
    let name = relative
        .file_name()
        .and_then(|name| name.to_str())
        .context("transaction filename is not UTF-8")?;
    let bytes = parent.read_regular(name, usize::MAX - 1, label)?;
    parent.ensure_still_current()?;
    Ok(bytes)
}

fn replace_relative_regular_nofollow(
    checkout_dir: &Path,
    relative: &Path,
    transaction_dir: &Path,
    bytes: &[u8],
) -> Result<()> {
    validate_relative_path(relative)?;
    let parent_relative = relative.parent().unwrap_or_else(|| Path::new("."));
    let parent = NofollowDirectory::open_or_create(&checkout_dir.join(parent_relative))?;
    let staging = NofollowDirectory::open_existing(transaction_dir)?
        .context("transaction staging directory is missing")?;
    let name = relative
        .file_name()
        .and_then(|name| name.to_str())
        .context("transaction filename is not UTF-8")?;
    parent.atomic_replace_from(name, &staging, bytes)?;
    parent.ensure_still_current()?;
    staging.ensure_still_current()
}

fn remove_relative_regular_nofollow(checkout_dir: &Path, relative: &Path) -> Result<()> {
    validate_relative_path(relative)?;
    let parent_relative = relative.parent().unwrap_or_else(|| Path::new("."));
    let Some(parent) = NofollowDirectory::open_existing(&checkout_dir.join(parent_relative))?
    else {
        return Ok(());
    };
    let name = relative
        .file_name()
        .and_then(|name| name.to_str())
        .context("transaction filename is not UTF-8")?;
    parent.remove_regular(name, "transaction target")?;
    parent.ensure_still_current()
}

fn complete_transaction(
    root: &Path,
    transaction_dir: &Path,
    pointer_path: &Path,
    manifest: &RepoTransactionManifest,
) -> Result<()> {
    let completed = root
        .join("completed")
        .join(format!("{}.json", manifest.transaction_id));
    atomic_write_json_locked(&completed, manifest)?;
    sync_dir(completed.parent().unwrap_or(root))?;
    clear_pointer(pointer_path)?;
    let _ = fs::remove_dir_all(transaction_dir);
    sync_dir(root)?;
    cleanup_terminal_debris_best_effort(root);
    Ok(())
}

fn finish_terminal_rollback(
    root: &Path,
    transaction_dir: &Path,
    pointer_path: &Path,
) -> Result<()> {
    clear_pointer(pointer_path)?;
    if let Err(err) = fs::remove_dir_all(transaction_dir)
        && err.kind() != std::io::ErrorKind::NotFound
    {
        tracing::warn!(
            transaction_dir = %transaction_dir.display(),
            error = %err,
            "failed to remove rolled-back transaction staging"
        );
    }
    sync_dir(root)?;
    cleanup_terminal_debris_best_effort(root);
    Ok(())
}

fn cleanup_terminal_debris_best_effort(root: &Path) {
    if let Err(err) = cleanup_terminal_debris(root) {
        tracing::warn!(
            transaction_root = %root.display(),
            error = %err,
            "failed to clean terminal knowledge transaction debris"
        );
    }
}

/// Remove debris only after the pending pointer reached a terminal state while
/// the caller still owns the transaction lane. Completed manifests are closeout
/// proofs, so they are compacted rather than discarded.
fn cleanup_terminal_debris(root: &Path) -> Result<()> {
    if pending_path_for_root(root).exists() {
        return Ok(());
    }
    for entry in fs::read_dir(root).with_context(|| format!("reading {}", root.display()))? {
        let entry = entry?;
        let name = entry.file_name();
        if name == "completed" {
            continue;
        }
        let path = entry.path();
        if entry.file_type()?.is_dir() {
            fs::remove_dir_all(&path).with_context(|| {
                format!("removing orphan transaction staging {}", path.display())
            })?;
        } else {
            fs::remove_file(&path)
                .with_context(|| format!("removing transaction debris {}", path.display()))?;
        }
    }
    compact_completed_manifests(root)?;
    sync_dir(root)
}

fn compact_completed_manifests(root: &Path) -> Result<()> {
    let completed = root.join("completed");
    if !completed.is_dir() {
        return Ok(());
    }
    let mut paths = fs::read_dir(&completed)
        .with_context(|| format!("reading completed transactions {}", completed.display()))?
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .filter(|path| path.extension().and_then(|ext| ext.to_str()) == Some("json"))
        .collect::<Vec<_>>();
    if paths.len() <= MAX_COMPLETED_MANIFESTS {
        return Ok(());
    }

    let mut manifests = Vec::with_capacity(paths.len());
    for path in &paths {
        let manifest: RepoTransactionManifest = serde_json::from_slice(
            &fs::read(path).with_context(|| format!("reading {}", path.display()))?,
        )
        .with_context(|| format!("parsing {}", path.display()))?;
        validate_manifest(&manifest, &manifest.transaction_id)?;
        let order_key =
            completed_transaction_order_key(&manifest.created_at, &manifest.transaction_id)?;
        manifests.push((order_key, manifest));
    }
    manifests.sort_by(|(left, _), (right, _)| left.cmp(right));
    let mut latest = BTreeMap::new();
    for (_, manifest) in manifests {
        for file in manifest.files {
            latest.insert(file.relative_path.clone(), file);
        }
    }
    let transaction_id = format!("compacted-{}", transaction_id());
    let compacted = RepoTransactionManifest {
        version: TRANSACTION_VERSION,
        kind: TRANSACTION_KIND.to_string(),
        transaction_id: transaction_id.clone(),
        created_at: chrono::Utc::now().to_rfc3339(),
        files: latest.into_values().collect(),
    };
    let compacted_path = completed.join(format!("{transaction_id}.json"));
    atomic_write_json_locked(&compacted_path, &compacted)?;
    sync_dir(&completed)?;
    paths.retain(|path| path != &compacted_path);
    for path in paths {
        fs::remove_file(&path)
            .with_context(|| format!("removing superseded closeout proof {}", path.display()))?;
    }
    sync_dir(&completed)
}

fn pending_path_for_root(root: &Path) -> PathBuf {
    root.join(PENDING_FILE)
}

fn validate_manifest(manifest: &RepoTransactionManifest, transaction_id: &str) -> Result<()> {
    validate_transaction_id(transaction_id)?;
    if manifest.version != TRANSACTION_VERSION
        || manifest.kind != TRANSACTION_KIND
        || manifest.transaction_id != transaction_id
    {
        anyhow::bail!("invalid knowledge transaction manifest identity");
    }
    let mut paths = BTreeSet::new();
    for file in &manifest.files {
        let path = Path::new(&file.relative_path);
        validate_relative_path(path)?;
        if !paths.insert(file.relative_path.as_str()) {
            anyhow::bail!("duplicate transaction target {}", file.relative_path);
        }
        if file.new_ref.is_some() != file.new_sha256.is_some()
            || file.old_ref.is_some() != file.old_sha256.is_some()
        {
            anyhow::bail!(
                "incomplete transaction checksum metadata for {}",
                file.relative_path
            );
        }
        if let Some(old_ref) = file.old_ref.as_deref() {
            validate_staged_ref(old_ref, "old")?;
        }
        if let Some(new_ref) = file.new_ref.as_deref() {
            validate_staged_ref(new_ref, "new")?;
        }
    }
    Ok(())
}

/// Era discriminator for the unparseable-pointer clear: `true` when the
/// transaction root holds at least one staged transaction directory. Lock
/// and pointer files do not count; any directory does, because a
/// current-era writer creates its staging directory before any canonical
/// mutation and an unknown directory beside a corrupt pointer is exactly
/// the state that must not be cleared blind.
fn root_holds_transaction_dirs(root: &Path) -> Result<bool> {
    let entries = match fs::read_dir(root) {
        Ok(entries) => entries,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(err) => {
            return Err(anyhow::Error::new(err)
                .context(format!("reading transaction root {}", root.display())));
        }
    };
    for entry in entries {
        let entry = entry?;
        // `completed/` holds compacted closeout proofs of TERMINAL
        // transactions; it is structural, never staged state, and its
        // presence must not block the legacy clear.
        if entry.file_name() == "completed" {
            continue;
        }
        if entry.file_type()?.is_dir() {
            return Ok(true);
        }
    }
    Ok(false)
}

fn validate_transaction_id(transaction_id: &str) -> Result<()> {
    if transaction_id.is_empty()
        || matches!(transaction_id, "." | "..")
        || !transaction_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
    {
        anyhow::bail!("unsafe knowledge transaction id {transaction_id:?}");
    }
    Ok(())
}

fn validate_staged_ref(reference: &str, expected_dir: &str) -> Result<()> {
    let path = Path::new(reference);
    validate_relative_path(path)?;
    if path
        .components()
        .next()
        .and_then(|component| match component {
            std::path::Component::Normal(value) => value.to_str(),
            _ => None,
        })
        != Some(expected_dir)
    {
        anyhow::bail!("invalid staged knowledge transaction ref {reference}");
    }
    Ok(())
}

fn ensure_unique_targets(writes: &[TransactionWrite]) -> Result<()> {
    let mut targets = BTreeSet::new();
    for write in writes {
        if !targets.insert(&write.target) {
            anyhow::bail!(
                "duplicate knowledge transaction target {}",
                write.target.display()
            );
        }
    }
    Ok(())
}

fn validate_relative_path(path: &Path) -> Result<()> {
    if path.as_os_str().is_empty()
        || path.is_absolute()
        || path.components().any(|component| {
            matches!(
                component,
                std::path::Component::ParentDir | std::path::Component::Prefix(_)
            )
        })
    {
        anyhow::bail!("unsafe knowledge transaction path {}", path.display());
    }
    Ok(())
}

fn create_claim(path: &Path, pointer: &TransactionPointer) -> Result<()> {
    let bytes = to_vec_pretty_newline(pointer)?;
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let temp = parent.join(format!(
        ".pending-{}-{}.tmp",
        pointer.transaction_id,
        TRANSACTION_NONCE.fetch_add(1, Ordering::SeqCst)
    ));
    let mut file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&temp)
        .with_context(|| format!("staging knowledge transaction claim at {}", temp.display()))?;
    file.write_all(&bytes)?;
    file.sync_all()?;
    drop(file);
    if let Err(err) = fs::hard_link(&temp, path) {
        let _ = fs::remove_file(&temp);
        return Err(err).with_context(|| {
            format!(
                "claiming knowledge transaction at {}; recover or finish the existing transaction before retrying",
                path.display()
            )
        });
    }
    sync_dir(parent)?;
    fs::remove_file(&temp)
        .with_context(|| format!("removing staged transaction claim {}", temp.display()))?;
    sync_dir(parent)
}

fn reject_symlink_components(base: &Path, relative: &Path) -> Result<()> {
    validate_relative_path(relative)?;
    let mut current = base.to_path_buf();
    for component in relative.components() {
        let std::path::Component::Normal(component) = component else {
            continue;
        };
        current.push(component);
        match fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                anyhow::bail!(
                    "knowledge transaction path traverses symlink {}",
                    current.display()
                );
            }
            Ok(_) => {}
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => break,
            Err(err) => {
                return Err(err)
                    .with_context(|| format!("inspecting transaction path {}", current.display()));
            }
        }
    }
    Ok(())
}

fn clear_pointer(path: &Path) -> Result<()> {
    match fs::remove_file(path) {
        Ok(()) => sync_dir(path.parent().unwrap_or_else(|| Path::new("."))),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err).with_context(|| format!("removing {}", path.display())),
    }
}

fn sync_dir(path: &Path) -> Result<()> {
    File::open(path)
        .with_context(|| format!("opening directory {} for fsync", path.display()))?
        .sync_all()
        .with_context(|| format!("fsync directory {}", path.display()))
}

fn transaction_root(checkout_dir: &Path) -> PathBuf {
    checkout_dir.join(".bbox/local").join(TRANSACTION_ROOT)
}

fn pending_path(checkout_dir: &Path) -> PathBuf {
    transaction_root(checkout_dir).join(PENDING_FILE)
}

fn transaction_id() -> String {
    let nonce = TRANSACTION_NONCE.fetch_add(1, Ordering::SeqCst);
    format!(
        "{}-{}-{nonce}",
        chrono::Utc::now().format("%Y%m%dT%H%M%S%.9fZ"),
        std::process::id()
    )
}

fn sha256(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    format!("{:x}", hasher.finalize())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transaction_applies_multiple_files_and_records_closeout_proof() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        fs::create_dir_all(root.join(".bbox/local")).unwrap();
        let first = root.join(".bbox/knowledge/first.json");
        let second = root.join(".bbox/knowledge/second.json");
        fs::create_dir_all(first.parent().unwrap()).unwrap();
        fs::write(&first, b"old").unwrap();

        let manifest = apply_transaction(
            &root,
            vec![
                TransactionWrite {
                    target: first.clone(),
                    new_bytes: Some(b"new-first".to_vec()),
                },
                TransactionWrite {
                    target: second.clone(),
                    new_bytes: Some(b"new-second".to_vec()),
                },
            ],
        )
        .unwrap()
        .unwrap();
        assert_eq!(fs::read(&first).unwrap(), b"new-first");
        assert_eq!(fs::read(&second).unwrap(), b"new-second");
        assert!(!has_pending_transaction(&root));
        assert!(
            transaction_root(&root)
                .join("completed")
                .join(format!("{}.json", manifest.transaction_id))
                .is_file()
        );
    }

    #[test]
    fn recovery_rolls_forward_an_applying_manifest() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        fs::create_dir_all(root.join(".bbox/local")).unwrap();
        let first = root.join(".bbox/knowledge/first.json");
        let second = root.join(".bbox/knowledge/second.json");
        fs::create_dir_all(first.parent().unwrap()).unwrap();
        fs::write(&first, b"old-first").unwrap();
        fs::write(&second, b"old-second").unwrap();

        let tx_id = "recovery-fixture";
        let root_dir = transaction_root(&root);
        let tx_dir = root_dir.join(tx_id);
        fs::create_dir_all(root_dir.join("completed")).unwrap();
        create_claim(
            &root_dir.join(PENDING_FILE),
            &TransactionPointer {
                version: TRANSACTION_VERSION,
                transaction_id: tx_id.into(),
                state: TransactionState::Preparing,
            },
        )
        .unwrap();
        let manifest = prepare_manifest(
            &root,
            &tx_dir,
            tx_id,
            vec![
                TransactionWrite {
                    target: first.clone(),
                    new_bytes: Some(b"new-first".to_vec()),
                },
                TransactionWrite {
                    target: second.clone(),
                    new_bytes: Some(b"new-second".to_vec()),
                },
            ],
        )
        .unwrap();
        atomic_write_json_locked(&tx_dir.join("manifest.json"), &manifest).unwrap();
        atomic_write_json_locked(
            &root_dir.join(PENDING_FILE),
            &TransactionPointer {
                version: TRANSACTION_VERSION,
                transaction_id: tx_id.into(),
                state: TransactionState::Applying,
            },
        )
        .unwrap();
        fs::write(&first, b"new-first").unwrap();

        recover_pending_transaction(&root).unwrap().unwrap();
        assert_eq!(fs::read(&first).unwrap(), b"new-first");
        assert_eq!(fs::read(&second).unwrap(), b"new-second");
        assert!(!has_pending_transaction(&root));
    }

    #[test]
    fn recovery_rolls_back_when_staged_new_bytes_are_corrupt() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let first = root.join(".bbox/knowledge/first.json");
        let created = root.join(".bbox/knowledge/created.json");
        fs::create_dir_all(first.parent().unwrap()).unwrap();
        fs::write(&first, b"old-first").unwrap();

        let tx_id = "rollback-recovery-fixture";
        let root_dir = transaction_root(&root);
        let tx_dir = root_dir.join(tx_id);
        fs::create_dir_all(root_dir.join("completed")).unwrap();
        let manifest = prepare_manifest(
            &root,
            &tx_dir,
            tx_id,
            vec![
                TransactionWrite {
                    target: created.clone(),
                    new_bytes: Some(b"created-new".to_vec()),
                },
                TransactionWrite {
                    target: first.clone(),
                    new_bytes: Some(b"new-first".to_vec()),
                },
            ],
        )
        .unwrap();
        atomic_write_json_locked(&tx_dir.join("manifest.json"), &manifest).unwrap();
        atomic_write_json_locked(
            &root_dir.join(PENDING_FILE),
            &TransactionPointer {
                version: TRANSACTION_VERSION,
                transaction_id: tx_id.into(),
                state: TransactionState::Applying,
            },
        )
        .unwrap();

        // Simulate a crash after the first canonical replacement, followed by
        // corruption of a later staged new file.
        fs::write(&created, b"created-new").unwrap();
        fs::write(tx_dir.join("new/1"), b"corrupt").unwrap();

        assert!(recover_pending_transaction(&root).unwrap().is_some());
        assert!(
            !created.exists(),
            "rollback must remove newly created files"
        );
        assert_eq!(fs::read(&first).unwrap(), b"old-first");
        assert!(!has_pending_transaction(&root));
        assert!(!tx_dir.exists());
        assert!(
            fs::read_dir(root_dir.join("completed"))
                .unwrap()
                .next()
                .is_none(),
            "a rolled-back transaction must not become a closeout proof"
        );
    }

    #[test]
    fn transaction_applies_deletion_and_records_it_in_closeout_proof() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let target = root.join(".bbox/knowledge/deleted.json");
        fs::create_dir_all(target.parent().unwrap()).unwrap();
        fs::write(&target, b"old").unwrap();

        let manifest = apply_transaction(
            &root,
            vec![TransactionWrite {
                target: target.clone(),
                new_bytes: None,
            }],
        )
        .unwrap()
        .unwrap();

        assert!(!target.exists());
        assert_eq!(manifest.files.len(), 1);
        assert!(manifest.files[0].old_ref.is_some());
        assert!(manifest.files[0].new_ref.is_none());
        assert!(manifest.files[0].new_sha256.is_none());
    }

    #[test]
    fn terminal_cleanup_removes_orphan_staging_and_temp_debris() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let transaction_root = transaction_root(&root);
        fs::create_dir_all(transaction_root.join("orphan/new")).unwrap();
        fs::write(transaction_root.join("orphan/new/0"), b"orphan").unwrap();
        fs::write(transaction_root.join(".pending-orphan.tmp"), b"temp").unwrap();

        apply_transaction(
            &root,
            vec![TransactionWrite {
                target: root.join(".bbox/knowledge/entry.json"),
                new_bytes: Some(b"new".to_vec()),
            }],
        )
        .unwrap();

        assert!(!transaction_root.join("orphan").exists());
        assert!(!transaction_root.join(".pending-orphan.tmp").exists());
    }

    #[test]
    fn completed_closeout_proofs_compact_without_losing_terminal_paths() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let transaction_root = transaction_root(&root);
        let completed = transaction_root.join("completed");
        fs::create_dir_all(&completed).unwrap();
        for index in 0..=MAX_COMPLETED_MANIFESTS {
            let transaction_id = format!("fixture-{index:03}");
            let manifest = RepoTransactionManifest {
                version: TRANSACTION_VERSION,
                kind: TRANSACTION_KIND.to_string(),
                transaction_id: transaction_id.clone(),
                created_at: format!("2026-07-22T00:{:02}:{:02}Z", index / 60, index % 60),
                files: vec![RepoTransactionFile {
                    relative_path: format!(".bbox/knowledge/{index}.json"),
                    old_ref: None,
                    new_ref: Some(format!("new/{index}")),
                    old_sha256: None,
                    new_sha256: Some(sha256(format!("value-{index}").as_bytes())),
                }],
            };
            atomic_write_json_locked(&completed.join(format!("{transaction_id}.json")), &manifest)
                .unwrap();
        }

        compact_completed_manifests(&transaction_root).unwrap();

        let paths = fs::read_dir(&completed)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .collect::<Vec<_>>();
        assert_eq!(paths.len(), 1);
        let compacted: RepoTransactionManifest =
            serde_json::from_slice(&fs::read(&paths[0]).unwrap()).unwrap();
        assert_eq!(compacted.files.len(), MAX_COMPLETED_MANIFESTS + 1);
    }

    #[test]
    fn preparing_pointer_without_manifest_clears_safely() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let root_dir = transaction_root(&root);
        fs::create_dir_all(&root_dir).unwrap();
        create_claim(
            &root_dir.join(PENDING_FILE),
            &TransactionPointer {
                version: TRANSACTION_VERSION,
                transaction_id: "unfinished".into(),
                state: TransactionState::Preparing,
            },
        )
        .unwrap();

        assert!(recover_pending_transaction(&root).unwrap().is_none());
        assert!(!has_pending_transaction(&root));
    }

    #[test]
    fn preparing_pointer_with_manifest_still_discards_before_commit_point() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let target = root.join(".bbox/knowledge/entry.json");
        fs::create_dir_all(target.parent().unwrap()).unwrap();
        fs::write(&target, b"old").unwrap();
        let root_dir = transaction_root(&root);
        let transaction_id = "preparing-with-manifest";
        let transaction_dir = root_dir.join(transaction_id);
        fs::create_dir_all(root_dir.join("completed")).unwrap();
        let manifest = prepare_manifest(
            &root,
            &transaction_dir,
            transaction_id,
            vec![TransactionWrite {
                target: target.clone(),
                new_bytes: Some(b"new".to_vec()),
            }],
        )
        .unwrap();
        atomic_write_json_locked(&transaction_dir.join("manifest.json"), &manifest).unwrap();
        create_claim(
            &root_dir.join(PENDING_FILE),
            &TransactionPointer {
                version: TRANSACTION_VERSION,
                transaction_id: transaction_id.into(),
                state: TransactionState::Preparing,
            },
        )
        .unwrap();

        assert!(recover_pending_transaction(&root).unwrap().is_none());
        assert_eq!(fs::read(&target).unwrap(), b"old");
        assert!(!transaction_dir.exists());
        assert!(!has_pending_transaction(&root));
    }

    #[test]
    fn applying_pointer_without_manifest_becomes_typed_blocked_state() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let root_dir = transaction_root(&root);
        fs::create_dir_all(&root_dir).unwrap();
        let pointer_path = root_dir.join(PENDING_FILE);
        create_claim(
            &pointer_path,
            &TransactionPointer {
                version: TRANSACTION_VERSION,
                transaction_id: "missing-applying-manifest".into(),
                state: TransactionState::Applying,
            },
        )
        .unwrap();

        let first = recover_pending_transaction(&root).unwrap_err().to_string();
        assert!(first.contains("error.repo_transaction_recovery_blocked"));
        let pointer: TransactionPointer =
            serde_json::from_slice(&fs::read(&pointer_path).unwrap()).unwrap();
        assert!(matches!(pointer.state, TransactionState::Blocked));
        let second = recover_pending_transaction(&root).unwrap_err().to_string();
        assert!(second.contains("error.repo_transaction_recovery_blocked"));
        assert!(has_pending_transaction(&root));
    }

    #[test]
    fn malformed_legacy_pointer_clears_without_wedging_checkout() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let root_dir = transaction_root(&root);
        fs::create_dir_all(&root_dir).unwrap();
        fs::write(root_dir.join(PENDING_FILE), b"{\"version\":").unwrap();

        assert!(recover_pending_transaction(&root).unwrap().is_none());
        assert!(!has_pending_transaction(&root));

        let target = root.join(".bbox/knowledge/entry.json");
        apply_transaction(
            &root,
            vec![TransactionWrite {
                target: target.clone(),
                new_bytes: Some(b"new".to_vec()),
            }],
        )
        .unwrap();
        assert_eq!(fs::read(target).unwrap(), b"new");
    }

    #[test]
    fn periodic_recovery_waits_for_a_live_lane_and_repairs_it_after_release() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let root_dir = transaction_root(&root);
        fs::create_dir_all(root_dir.join("completed")).unwrap();
        let lane = acquire_transaction_lane(&root_dir, false)
            .unwrap()
            .expect("fixture owns the live transaction lane");
        fs::write(root_dir.join(PENDING_FILE), b"{\"version\":").unwrap();

        assert!(
            recover_abandoned_pending_transaction(&root)
                .unwrap()
                .is_none()
        );
        assert!(
            root_dir.join(PENDING_FILE).exists(),
            "periodic recovery must not clear a live owner's pointer"
        );

        drop(lane);
        assert!(
            recover_abandoned_pending_transaction(&root)
                .unwrap()
                .is_none()
        );
        assert!(
            !root_dir.join(PENDING_FILE).exists(),
            "the abandoned pointer must self-heal without daemon restart"
        );
    }

    /// The unparseable-pointer clear is era-gated (review round 2): corrupt
    /// bytes BESIDE a staged transaction directory could be a torn
    /// current-era `Applying` pointer with half-applied canonical files,
    /// so recovery must block for operator repair instead of clearing.
    #[test]
    fn unparseable_pointer_beside_staged_state_blocks_instead_of_clearing() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let root_dir = transaction_root(&root);
        fs::create_dir_all(root_dir.join("tx-current-era")).unwrap();
        fs::write(root_dir.join(PENDING_FILE), b"{\"version\":").unwrap();

        let err = recover_abandoned_pending_transaction(&root).unwrap_err();
        assert!(
            err.to_string()
                .contains("error.repo_transaction_recovery_blocked"),
            "{err:#}"
        );
        assert!(
            root_dir.join(PENDING_FILE).exists(),
            "the corrupt pointer must survive for operator repair"
        );
    }

    #[test]
    fn transaction_ids_reject_dot_segments() {
        assert!(validate_transaction_id(".").is_err());
        assert!(validate_transaction_id("..").is_err());
        assert!(validate_transaction_id("valid-id_1.2").is_ok());
    }

    #[test]
    fn completed_transaction_order_parses_variable_width_rfc3339_fractions() {
        let whole = completed_transaction_order_key(
            "2026-07-21T12:00:05Z",
            "20260721T120005.000000000Z-1-0",
        )
        .unwrap();
        let fractional = completed_transaction_order_key(
            "2026-07-21T12:00:05.500000Z",
            "20260721T120005.500000000Z-1-1",
        )
        .unwrap();
        assert!(whole < fractional);
    }

    #[cfg(unix)]
    #[test]
    fn transaction_rejects_symlinked_repo_owned_parent() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let outside = tempfile::tempdir().unwrap();
        fs::create_dir_all(root.join(".bbox")).unwrap();
        symlink(outside.path(), root.join(".bbox/knowledge")).unwrap();
        let target = root.join(".bbox/knowledge/escaped.json");

        let err = apply_transaction(
            &root,
            vec![TransactionWrite {
                target,
                new_bytes: Some(b"escaped".to_vec()),
            }],
        )
        .unwrap_err();
        assert!(err.to_string().contains("traverses symlink"));
        assert!(!outside.path().join("escaped.json").exists());
    }

    #[test]
    fn closeout_claim_excludes_writes_and_restart_recovery_clears_stale_claim() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let root_dir = transaction_root(&root);
        fs::create_dir_all(&root_dir).unwrap();
        create_claim(
            &root_dir.join(PENDING_FILE),
            &TransactionPointer {
                version: TRANSACTION_VERSION,
                transaction_id: "closeout-fixture".into(),
                state: TransactionState::Closeout,
            },
        )
        .unwrap();
        let target = root.join(".bbox/knowledge/entry.json");
        let err = apply_transaction(
            &root,
            vec![TransactionWrite {
                target: target.clone(),
                new_bytes: Some(b"new".to_vec()),
            }],
        )
        .unwrap_err();
        assert!(err.to_string().contains("claiming knowledge transaction"));
        assert!(!target.exists());
        assert!(recover_pending_transaction(&root).unwrap().is_none());
        assert!(!has_pending_transaction(&root));
    }
}
