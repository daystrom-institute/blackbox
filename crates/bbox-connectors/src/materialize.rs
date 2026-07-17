//! Atomic materialization writes and the mount-root ownership marker.
//!
//! Every write here is a synchronous filesystem boundary
//! (`#[allow(clippy::disallowed_methods)]`, matching the
//! `bbox-corpus-core::json_store` / `bbox-indexing::projects` convention) -
//! stage 1 is library-only, so callers run these inline; the daemon
//! (stage 2) is expected to pool them onto a blocking lane.

use std::path::Path;

use anyhow::{Context, Result};

/// Marker file name stamped into a materialization root at creation.
/// Destructive operations (mount removal with `remove_files`, full-resync
/// pruning) must refuse to run against a root missing this file, so a
/// misconfigured path can never aim deletion at operator data. Never
/// recorded in the manifest and never walked by the indexer.
pub const OWNERSHIP_MARKER_FILE: &str = ".bbox-mount-owner";

const OWNERSHIP_MARKER_BODY: &str = "blackbox remote-source-connector materialization root\n";

/// Ensure `root` exists and carries the ownership marker. Idempotent - safe
/// to call at the start of every sync pass.
#[allow(clippy::disallowed_methods)]
pub fn ensure_ownership_marker(root: &Path) -> Result<()> {
    std::fs::create_dir_all(root)
        .with_context(|| format!("creating materialization root {}", root.display()))?;
    let marker = root.join(OWNERSHIP_MARKER_FILE);
    if !marker.exists() {
        std::fs::write(&marker, OWNERSHIP_MARKER_BODY)
            .with_context(|| format!("writing ownership marker {}", marker.display()))?;
    }
    Ok(())
}

/// Verify `root` carries the ownership marker. Callers performing
/// destructive operations (removal, full-resync pruning) must call this
/// and refuse to proceed on `Err`.
#[allow(clippy::disallowed_methods)]
pub fn verify_ownership_marker(root: &Path) -> Result<()> {
    let marker = root.join(OWNERSHIP_MARKER_FILE);
    if !marker.exists() {
        anyhow::bail!(
            "materialization root {} is missing its ownership marker \
             ({OWNERSHIP_MARKER_FILE}); refusing a destructive operation \
             against a root blackbox may not own",
            root.display()
        );
    }
    Ok(())
}

/// Write `bytes` to `path` atomically (temp file in the same directory,
/// `fsync`, then rename). Creates parent directories as needed. Skips the
/// write entirely (returns `Ok(false)`) when the destination already holds
/// identical content, per the design's "set file mtime only when content
/// actually changed" freshness rule - callers use the returned bool to
/// decide whether to bump `SyncSummary` counters.
#[allow(clippy::disallowed_methods)]
pub fn atomic_write(path: &Path, bytes: &[u8]) -> Result<bool> {
    if let Ok(existing) = std::fs::read(path)
        && existing == bytes
    {
        return Ok(false);
    }

    let parent = path
        .parent()
        .with_context(|| format!("materialized path {} has no parent", path.display()))?;
    std::fs::create_dir_all(parent)
        .with_context(|| format!("creating materialization directory {}", parent.display()))?;

    let pid = std::process::id();
    let nonce = TMP_NONCE.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    let file_name = path
        .file_name()
        .with_context(|| format!("materialized path {} has no file name", path.display()))?
        .to_string_lossy()
        .into_owned();
    let tmp_path = parent.join(format!(".{file_name}.{pid}.{nonce}.tmp"));

    {
        use std::io::Write;
        let mut f = std::fs::File::create(&tmp_path)
            .with_context(|| format!("creating temp file {}", tmp_path.display()))?;
        f.write_all(bytes)
            .with_context(|| format!("writing temp file {}", tmp_path.display()))?;
        f.sync_all()?;
    }

    std::fs::rename(&tmp_path, path)
        .with_context(|| format!("renaming {} to {}", tmp_path.display(), path.display()))?;
    Ok(true)
}

static TMP_NONCE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Remove a materialized file (a remote deletion or a policy-rejection
/// cleanup). Tolerates an already-missing file - callers may race a
/// partial prior pass.
#[allow(clippy::disallowed_methods)]
pub fn remove_materialized_file(path: &Path) -> Result<()> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e).with_context(|| format!("removing {}", path.display())),
    }
}

/// Rename a materialized file within the root (a reconciled remote
/// rename). Creates the destination's parent directory as needed.
#[allow(clippy::disallowed_methods)]
pub fn rename_materialized_file(from: &Path, to: &Path) -> Result<()> {
    if let Some(parent) = to.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating materialization directory {}", parent.display()))?;
    }
    std::fs::rename(from, to)
        .with_context(|| format!("renaming {} to {}", from.display(), to.display()))
}

/// Size in bytes of an already-materialized file. Used by the driver's
/// bulk-materialize path to apply per-file/total byte policy after a
/// connector's own checkout mechanics have written bytes to disk.
#[allow(clippy::disallowed_methods)]
pub fn file_size(path: &Path) -> Result<u64> {
    Ok(std::fs::metadata(path)
        .with_context(|| format!("statting {}", path.display()))?
        .len())
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hex::encode(hasher.finalize())
}

#[cfg(test)]
// Assertions read materialized bytes back directly via plain std::fs -
// unrelated to the sync-boundary functions under test, which already
// carry their own reasoned allows.
#[allow(clippy::disallowed_methods)]
mod tests {
    use super::*;

    #[test]
    fn ownership_marker_is_idempotent_and_verifiable() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap().join("mount-root");
        assert!(verify_ownership_marker(&root).is_err());

        ensure_ownership_marker(&root).unwrap();
        verify_ownership_marker(&root).unwrap();
        // Idempotent: calling again does not error or duplicate content.
        ensure_ownership_marker(&root).unwrap();
        let body = std::fs::read_to_string(root.join(OWNERSHIP_MARKER_FILE)).unwrap();
        assert_eq!(body, OWNERSHIP_MARKER_BODY);
    }

    #[test]
    fn atomic_write_creates_parents_and_is_readable() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let path = root.join("nested").join("dir").join("file.txt");

        let changed = atomic_write(&path, b"hello").unwrap();
        assert!(changed);
        assert_eq!(std::fs::read(&path).unwrap(), b"hello");
        // No leftover temp files.
        let siblings: Vec<_> = std::fs::read_dir(path.parent().unwrap())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(siblings.len(), 1);
    }

    #[test]
    fn atomic_write_is_a_no_op_when_content_is_unchanged() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let path = root.join("file.txt");

        assert!(atomic_write(&path, b"same").unwrap());
        assert!(!atomic_write(&path, b"same").unwrap());
        assert!(atomic_write(&path, b"different").unwrap());
    }

    #[test]
    fn remove_materialized_file_tolerates_missing() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let path = root.join("gone.txt");
        remove_materialized_file(&path).unwrap();
        atomic_write(&path, b"x").unwrap();
        remove_materialized_file(&path).unwrap();
        assert!(!path.exists());
    }

    #[test]
    fn rename_materialized_file_moves_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let from = root.join("old.txt");
        let to = root.join("nested").join("new.txt");
        atomic_write(&from, b"content").unwrap();

        rename_materialized_file(&from, &to).unwrap();
        assert!(!from.exists());
        assert_eq!(std::fs::read(&to).unwrap(), b"content");
    }

    #[test]
    fn sha256_hex_is_stable_and_distinct() {
        let a = sha256_hex(b"a");
        let b = sha256_hex(b"b");
        assert_ne!(a, b);
        assert_eq!(a.len(), 64);
        assert_eq!(a, sha256_hex(b"a"));
    }
}
