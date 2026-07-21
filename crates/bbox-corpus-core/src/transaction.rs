//! Crash-consistent host-local transactions for repo-owned corpus files.
//!
//! Git commit is the traveling transaction. This module only protects the
//! daemon's uncommitted multi-file apply window inside one checkout. The
//! pointer is both the exclusive claim and the loader-visible pending marker;
//! old and new bytes remain available until the apply reaches a terminal state.

use std::collections::BTreeSet;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use crate::json_store::{
    atomic_write_bytes_from_dir_locked, atomic_write_json_locked, to_vec_pretty_newline,
};
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

const TRANSACTION_VERSION: u32 = 1;
// These knowledge-era names are part of the durable v1 layout and manifest
// identity. Gaps share the lane without rewriting existing closeout proofs.
const TRANSACTION_KIND: &str = "knowledge_transaction_v1";
const TRANSACTION_ROOT: &str = "knowledge-transactions";
const PENDING_FILE: &str = "pending.json";
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
    let checkout_dir = checkout_dir
        .canonicalize()
        .with_context(|| format!("canonicalizing checkout {}", checkout_dir.display()))?;
    let root = transaction_root(&checkout_dir);
    fs::create_dir_all(root.join("completed"))
        .with_context(|| format!("creating transaction root {}", root.display()))?;
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
                clear_pointer(&pointer_path)?;
                let _ = fs::remove_dir_all(&transaction_dir);
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
/// without a complete manifest touched no canonical files and are discarded.
/// Any complete manifest rolls forward idempotently, then records the closeout
/// proof and clears the pointer.
pub fn recover_pending_transaction(checkout_dir: &Path) -> Result<Option<RepoTransactionManifest>> {
    let pointer_path = pending_path(checkout_dir);
    if !pointer_path.exists() {
        return Ok(None);
    }
    let checkout_dir = checkout_dir
        .canonicalize()
        .with_context(|| format!("canonicalizing checkout {}", checkout_dir.display()))?;
    let root = transaction_root(&checkout_dir);
    let pointer: TransactionPointer = serde_json::from_slice(
        &fs::read(&pointer_path)
            .with_context(|| format!("reading pending pointer {}", pointer_path.display()))?,
    )
    .with_context(|| format!("parsing pending pointer {}", pointer_path.display()))?;
    if pointer.version != TRANSACTION_VERSION {
        anyhow::bail!(
            "unsupported knowledge transaction pointer version {}",
            pointer.version
        );
    }
    validate_transaction_id(&pointer.transaction_id)?;
    if matches!(pointer.state, TransactionState::Closeout) {
        clear_pointer(&pointer_path)?;
        return Ok(None);
    }
    let transaction_dir = root.join(&pointer.transaction_id);
    let manifest_path = transaction_dir.join("manifest.json");
    if !manifest_path.exists() && matches!(pointer.state, TransactionState::Preparing) {
        let _ = fs::remove_dir_all(&transaction_dir);
        clear_pointer(&pointer_path)?;
        return Ok(None);
    }
    let manifest: RepoTransactionManifest =
        serde_json::from_slice(&fs::read(&manifest_path).with_context(|| {
            format!("reading transaction manifest {}", manifest_path.display())
        })?)
        .with_context(|| format!("parsing transaction manifest {}", manifest_path.display()))?;
    validate_manifest(&manifest, &pointer.transaction_id)?;
    apply_manifest(&checkout_dir, &transaction_dir, &manifest)?;
    complete_transaction(&root, &transaction_dir, &pointer_path, &manifest)?;
    Ok(Some(manifest))
}

fn prepare_manifest(
    checkout_dir: &Path,
    transaction_dir: &Path,
    transaction_id: &str,
    writes: Vec<TransactionWrite>,
) -> Result<RepoTransactionManifest> {
    fs::create_dir_all(transaction_dir.join("old"))?;
    fs::create_dir_all(transaction_dir.join("new"))?;
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
        let relative_path = relative.to_string_lossy().replace('\\', "/");
        let old_bytes = match fs::read(&write.target) {
            Ok(bytes) => Some(bytes),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => None,
            Err(err) => {
                return Err(err).with_context(|| format!("reading {}", write.target.display()));
            }
        };
        let old_ref = old_bytes.as_ref().map(|bytes| {
            let name = format!("old/{index}");
            (name, bytes)
        });
        if let Some((name, bytes)) = &old_ref {
            write_sync(&transaction_dir.join(name), bytes)?;
        }
        let new_ref = write.new_bytes.as_ref().map(|bytes| {
            let name = format!("new/{index}");
            (name, bytes)
        });
        if let Some((name, bytes)) = &new_ref {
            write_sync(&transaction_dir.join(name), bytes)?;
        }
        files.push(RepoTransactionFile {
            relative_path,
            old_ref: old_ref.as_ref().map(|(name, _)| name.clone()),
            new_ref: new_ref.as_ref().map(|(name, _)| name.clone()),
            old_sha256: old_bytes.as_deref().map(sha256),
            new_sha256: write.new_bytes.as_deref().map(sha256),
        });
    }
    sync_dir(&transaction_dir.join("old"))?;
    sync_dir(&transaction_dir.join("new"))?;
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
        let target = checkout_dir.join(&file.relative_path);
        if let Some(new_ref) = &file.new_ref {
            let bytes = fs::read(transaction_dir.join(new_ref))?;
            if sha256(&bytes) != file.new_sha256.as_deref().unwrap_or_default() {
                anyhow::bail!(
                    "staged new bytes failed checksum for {}",
                    file.relative_path
                );
            }
            if let Some(parent) = target.parent() {
                fs::create_dir_all(parent)?;
            }
            atomic_write_bytes_from_dir_locked(&target, transaction_dir, &bytes)?;
            if let Some(parent) = target.parent() {
                sync_dir(parent)?;
            }
        } else if target.exists() {
            fs::remove_file(&target)?;
            if let Some(parent) = target.parent() {
                sync_dir(parent)?;
            }
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
        let target = checkout_dir.join(&file.relative_path);
        if let Some(old_ref) = &file.old_ref {
            let bytes = fs::read(transaction_dir.join(old_ref))?;
            if sha256(&bytes) != file.old_sha256.as_deref().unwrap_or_default() {
                anyhow::bail!(
                    "staged old bytes failed checksum for {}",
                    file.relative_path
                );
            }
            if let Some(parent) = target.parent() {
                fs::create_dir_all(parent)?;
            }
            atomic_write_bytes_from_dir_locked(&target, transaction_dir, &bytes)?;
            if let Some(parent) = target.parent() {
                sync_dir(parent)?;
            }
        } else if target.exists() {
            fs::remove_file(&target)?;
            if let Some(parent) = target.parent() {
                sync_dir(parent)?;
            }
        }
    }
    Ok(())
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
    Ok(())
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

fn validate_transaction_id(transaction_id: &str) -> Result<()> {
    if transaction_id.is_empty()
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
    let mut file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(path)
        .with_context(|| {
            format!(
                "claiming knowledge transaction at {}; recover or finish the existing transaction before retrying",
                path.display()
            )
        })?;
    file.write_all(&bytes)?;
    file.sync_all()?;
    sync_dir(path.parent().unwrap_or_else(|| Path::new(".")))
}

fn clear_pointer(path: &Path) -> Result<()> {
    match fs::remove_file(path) {
        Ok(()) => sync_dir(path.parent().unwrap_or_else(|| Path::new("."))),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err).with_context(|| format!("removing {}", path.display())),
    }
}

fn write_sync(path: &Path, bytes: &[u8]) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut file = File::create(path)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    Ok(())
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
