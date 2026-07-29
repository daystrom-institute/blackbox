//! The daemon's cross-process instance lock.
//!
//! R31F1. Every durable store below the state root is written by exactly one
//! daemon, but nothing in the startup chain enforced that. The listener bind
//! is the only exclusivity the process had, and it happens far too late:
//! `open_shared_state` opens the corpus index, runs local-activation
//! recovery, and takes the coordinator-held pin clear before `run` ever tries
//! to claim the port. A duplicate or leaked daemon therefore reached
//! reclamation paths that assume single-writer semantics, and the live
//! daemon's in-flight publication was the collateral: the second process
//! unlinked the temporary between the first's `openat` and its `renameat`,
//! and the first's reindex went down with an `ENOENT`.
//!
//! The fix is a root-scoped advisory lock claimed BEFORE any shared store
//! opens and held for the process lifetime. A second daemon on the same state
//! root now fails fast, at the lock, having mutated nothing.
//!
//! # Scope: daemon only, not the offline CLI
//!
//! `src/bin/blackbox.rs` deliberately does NOT take this lock. It never opens
//! the corpus index or the edge sidecar, so it cannot reach the reclamation
//! this lock exists to serialize, and the durable catalog state it does
//! mutate is already guarded at the right granularity by the per-store
//! advisory locks (`acquire_store_lock_nofollow`, the catalog migration
//! lock). Taking a process-lifetime exclusive root lock there would refuse
//! every offline `project-catalog list`/`get` against a live daemon, which
//! trades a real capability for no additional safety.

use bbox_corpus_core::json_store::open_lock_path_nofollow;
use fs2::FileExt;
use std::fs::File;
use std::path::{Path, PathBuf};

/// The stable lock path for one daemon state root.
pub fn instance_lock_path(state_dir: &Path) -> PathBuf {
    state_dir.join("instance.lock")
}

/// Why a daemon could not claim its state root.
#[derive(Debug)]
pub enum InstanceLockError {
    /// Another live process holds the lock. The overwhelmingly likely cause
    /// is a second daemon on the same state root.
    AlreadyHeld { path: PathBuf },
    /// The lock file itself could not be opened or locked.
    Unavailable {
        path: PathBuf,
        source: anyhow::Error,
    },
}

impl InstanceLockError {
    /// The stable code, so callers and tests can discriminate without
    /// matching on prose.
    pub fn code(&self) -> &'static str {
        match self {
            Self::AlreadyHeld { .. } => "error.daemon_instance_locked",
            Self::Unavailable { .. } => "error.daemon_instance_lock_unavailable",
        }
    }

    pub fn path(&self) -> &Path {
        match self {
            Self::AlreadyHeld { path } | Self::Unavailable { path, .. } => path,
        }
    }
}

impl std::fmt::Display for InstanceLockError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::AlreadyHeld { path } => write!(
                formatter,
                "another blackboxd already holds the daemon instance lock at {}. \
                 One state root serves exactly one daemon: stop the running instance, \
                 or point this one at a different BLACKBOX_STATE_DIR, before starting it",
                path.display()
            ),
            Self::Unavailable { path, source } => write!(
                formatter,
                "failed to claim the daemon instance lock at {}: {source:#}",
                path.display()
            ),
        }
    }
}

impl std::error::Error for InstanceLockError {}

/// A held instance lock. Dropping it, or exiting the process by any route,
/// releases the advisory lock the kernel holds on the open file description.
#[derive(Debug)]
pub struct InstanceLockGuard {
    file: File,
    path: PathBuf,
}

impl InstanceLockGuard {
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for InstanceLockGuard {
    fn drop(&mut self) {
        let _ = FileExt::unlock(&self.file);
    }
}

/// Claim `<state_dir>/instance.lock` for this process, without blocking.
///
/// Non-blocking is the point: a duplicate daemon must refuse loudly and
/// immediately rather than queue behind a healthy one and then proceed into
/// shared-state mutation whenever that one exits.
pub fn acquire_instance_lock(state_dir: &Path) -> Result<InstanceLockGuard, InstanceLockError> {
    let path = instance_lock_path(state_dir);
    let unavailable = |source: anyhow::Error| InstanceLockError::Unavailable {
        path: path.clone(),
        source,
    };

    let file = open_lock_path_nofollow(&path).map_err(&unavailable)?;
    let metadata = file
        .metadata()
        .map_err(|error| unavailable(anyhow::Error::new(error).context("inspect lock file")))?;
    if !metadata.file_type().is_file() {
        return Err(unavailable(anyhow::anyhow!(
            "instance lock is not a regular file"
        )));
    }

    match file.try_lock_exclusive() {
        Ok(()) => Ok(InstanceLockGuard { file, path }),
        Err(error) if is_lock_contended(&error) => Err(InstanceLockError::AlreadyHeld { path }),
        Err(error) => Err(unavailable(anyhow::Error::new(error))),
    }
}

/// Contention reports as the platform's would-block error rather than a
/// distinct kind, so compare against the value `fs2` documents for it.
fn is_lock_contended(error: &std::io::Error) -> bool {
    let contended = fs2::lock_contended_error();
    error.raw_os_error() == contended.raw_os_error() || error.kind() == contended.kind()
}

#[cfg(test)]
mod tests {
    use super::*;
    use bbox_edge_sidecar::snapshot::{
        clear_pending_local_activation_pins, pending_local_activation_pins_dir,
    };

    fn state_root(dir: &tempfile::TempDir) -> PathBuf {
        dir.path().canonicalize().expect("canonicalize state root")
    }

    /// The shape the atomic pin writer mints between its create and its
    /// `renameat`: `.<leaf>.<pid>.<sequence>.tmp`, carrying a foreign pid.
    fn peer_temporary(pins_dir: &Path, project_id: &str, pid: u32) -> PathBuf {
        let path = pins_dir.join(format!(".{project_id}.json.{pid}.7.tmp"));
        std::fs::create_dir_all(pins_dir).expect("create the pin directory");
        std::fs::write(&path, b"{}").expect("mint the in-flight temporary");
        path
    }

    #[test]
    fn second_acquirer_refuses_while_the_first_holds_the_root() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = state_root(&dir);

        let first = acquire_instance_lock(&root).expect("first acquirer claims the root");
        assert_eq!(first.path(), instance_lock_path(&root));

        let error = acquire_instance_lock(&root).expect_err("second acquirer must refuse");
        assert_eq!(error.code(), "error.daemon_instance_locked");
        assert_eq!(error.path(), instance_lock_path(&root));
        let rendered = error.to_string();
        assert!(
            rendered.contains(&instance_lock_path(&root).display().to_string()),
            "the refusal must name the lock path: {rendered}"
        );
        assert!(
            rendered.contains("blackboxd"),
            "the refusal must name the likely duplicate-daemon cause: {rendered}"
        );
    }

    #[test]
    fn releasing_the_lock_lets_the_next_daemon_claim_the_root() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = state_root(&dir);

        let first = acquire_instance_lock(&root).expect("first acquirer claims the root");
        drop(first);
        let second = acquire_instance_lock(&root).expect("a released root is claimable again");
        assert_eq!(second.path(), instance_lock_path(&root));
    }

    /// The R31F1 regression, driven through the public surfaces.
    ///
    /// A live daemon is mid-publication: its temporary exists in the pin
    /// directory and its `renameat` has not run yet. A duplicate daemon boots
    /// on the same state root and would run local-activation recovery, whose
    /// clear takes the coordinator-held enumeration and unlinks every
    /// temporary it walks past. With the instance lock in front of the
    /// startup chain the duplicate never reaches that clear, so the peer's
    /// temporary survives and its rename still completes.
    #[test]
    fn a_duplicate_daemon_cannot_reclaim_a_peers_in_flight_pin_temporary() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = state_root(&dir);
        let edges_dir = crate::edge_index::edges_dir_from_bro_store(&root.join("bro"));
        let pins_dir = pending_local_activation_pins_dir(&edges_dir);

        // The live daemon owns the root and is mid-publication: its temporary
        // is on disk and its rename has not run.
        let live = acquire_instance_lock(&root).expect("the live daemon claims the root");
        let temporary = peer_temporary(&pins_dir, "peer-project", 424_242);

        // The duplicate daemon's startup. The instance lock refuses before
        // any shared store opens, so startup recovery and its coordinator-held
        // clear never run.
        let error = acquire_instance_lock(&root).expect_err("the duplicate daemon must refuse");
        assert_eq!(error.code(), "error.daemon_instance_locked");
        assert!(
            temporary.exists(),
            "the refused duplicate must leave the peer's in-flight temporary alone"
        );

        // The peer's rename completes, which an unlinked temporary would have
        // turned into an ENOENT failure that takes its reindex down.
        let published = pins_dir.join("peer-project.json");
        std::fs::rename(&temporary, &published).expect("the peer's rename must still succeed");
        assert!(published.exists());

        // The counterfactual, so the guard is not vacuous: reaching the clear
        // IS what destroys a foreign temporary. Only the process holding the
        // root may run it.
        let doomed = peer_temporary(&pins_dir, "second-project", 424_243);
        clear_pending_local_activation_pins(&edges_dir).expect("clear under the held root");
        assert!(
            !doomed.exists(),
            "the coordinator-held clear reclaims every temporary it walks past"
        );
        assert!(!published.exists(), "the clear also retracts the pin set");

        drop(live);
        acquire_instance_lock(&root).expect("a released root is claimable again");
    }
}
