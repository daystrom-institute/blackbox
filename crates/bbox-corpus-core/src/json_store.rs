use anyhow::{Context, Result};
use fs2::FileExt;
use std::fs;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

static NONCE: AtomicU64 = AtomicU64::new(0);

/// Acquire an exclusive lock on `<store_path>.json.lock` and execute `f`.
/// The lock is released after `f` returns.
///
/// This is a synchronous storage boundary. Async owners must call it from a
/// store actor or blocking lane so the OS file lock is never held across an
/// async suspension point.
#[allow(clippy::disallowed_methods)]
pub fn with_store_lock<T>(store_path: &Path, f: impl FnOnce() -> Result<T>) -> Result<T> {
    let lock_path = store_path.with_extension("json.lock");
    if let Some(parent) = lock_path.parent() {
        fs::create_dir_all(parent)?;
    }
    let lock_file = fs::OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .open(&lock_path)
        .with_context(|| format!("failed to open lock file {}", lock_path.display()))?;

    lock_file.lock_exclusive().with_context(|| {
        format!(
            "failed to acquire exclusive lock on {}",
            lock_path.display()
        )
    })?;

    let result = f();

    let _ = lock_file.unlock();
    result
}

/// Atomically write `value` to `store_path` using a unique temporary file.
/// This function does NOT acquire the lock; callers should wrap it in `with_store_lock`.
#[allow(clippy::disallowed_methods)] // synchronous locked-store publication boundary
pub fn atomic_write_json_locked<T: serde::Serialize>(store_path: &Path, value: &T) -> Result<()> {
    let pid = std::process::id();
    let nonce = NONCE.fetch_add(1, Ordering::SeqCst);
    let tmp_path = store_path.with_extension(format!("json.{pid}.{nonce}.tmp"));

    if let Some(parent) = tmp_path.parent() {
        fs::create_dir_all(parent)?;
    }

    {
        let mut f = fs::File::create(&tmp_path)
            .with_context(|| format!("failed to create temp file {}", tmp_path.display()))?;
        let bytes = to_vec_pretty_newline(value)?;
        use std::io::Write;
        f.write_all(&bytes)
            .with_context(|| format!("failed to serialize JSON to {}", tmp_path.display()))?;
        f.sync_all()?;
    }

    fs::rename(&tmp_path, store_path).with_context(|| {
        format!(
            "failed to rename {} to {}",
            tmp_path.display(),
            store_path.display()
        )
    })?;

    Ok(())
}

/// Pretty JSON bytes with the repo convention of exactly one trailing newline.
pub fn to_vec_pretty_newline<T: serde::Serialize>(value: &T) -> Result<Vec<u8>> {
    let mut bytes = serde_json::to_vec_pretty(value)?;
    if !bytes.ends_with(b"\n") {
        bytes.push(b'\n');
    }
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;
    use tempfile::tempdir;

    #[test]
    fn json_store_unique_tmp_names_do_not_collide() {
        let dir = tempdir().unwrap();
        let store_path = dir.path().join("store.json");

        let n1 = NONCE.load(Ordering::SeqCst);
        atomic_write_json_locked(&store_path, &serde_json::json!({})).unwrap();
        let n2 = NONCE.load(Ordering::SeqCst);
        assert!(n2 > n1);

        atomic_write_json_locked(&store_path, &serde_json::json!({})).unwrap();
        let n3 = NONCE.load(Ordering::SeqCst);
        assert!(n3 > n2);
    }

    #[test]
    #[allow(clippy::disallowed_methods)] // synchronous filesystem assertion
    fn json_store_writes_trailing_newline() {
        let dir = tempdir().unwrap();
        let store_path = dir.path().join("store.json");

        atomic_write_json_locked(&store_path, &serde_json::json!({"a": 1})).unwrap();

        let text = fs::read_to_string(&store_path).unwrap();
        assert!(text.ends_with('\n'));
        assert!(!text.ends_with("\n\n"));
    }

    #[test]
    #[allow(clippy::disallowed_methods)] // synchronous lock and filesystem stress fixture
    fn json_store_lock_serializes_concurrent_writes() {
        let dir = tempdir().unwrap();
        let store_path = dir.path().join("store.json");

        // Initialize with empty array
        fs::write(&store_path, "[]").unwrap();

        let path_clone = store_path.clone();
        let t1 = thread::spawn(move || {
            for i in 0..10 {
                with_store_lock(&path_clone, || {
                    let text = fs::read_to_string(&path_clone)?;
                    let mut vec: Vec<i32> = serde_json::from_str(&text)?;
                    vec.push(i);
                    atomic_write_json_locked(&path_clone, &vec)?;
                    Ok(())
                })
                .unwrap();
            }
        });

        let path_clone2 = store_path.clone();
        let t2 = thread::spawn(move || {
            for i in 10..20 {
                with_store_lock(&path_clone2, || {
                    let text = fs::read_to_string(&path_clone2)?;
                    let mut vec: Vec<i32> = serde_json::from_str(&text)?;
                    vec.push(i);
                    atomic_write_json_locked(&path_clone2, &vec)?;
                    Ok(())
                })
                .unwrap();
            }
        });

        t1.join().unwrap();
        t2.join().unwrap();

        let text = fs::read_to_string(&store_path).unwrap();
        let vec: Vec<i32> = serde_json::from_str(&text).unwrap();
        assert_eq!(vec.len(), 20);
        for i in 0..20 {
            assert!(vec.contains(&i));
        }
    }
}
