//! Per-stream lock with open-lock-verify identity. Stream directories are
//! reclaimable, so holding a lock only counts once the path `stream/lock`
//! still names the locked file. A holder that lost that race retries from
//! the open; a deleter removes the directory only while holding a verified
//! lock and never resumes after its empty-only removal fails.
use crate::Hooks;
use anyhow::{Result, bail};
use std::fs::{self, File, OpenOptions};
use std::io::{self, ErrorKind};
use std::path::{Path, PathBuf};

pub(crate) const LOCK_NAME: &str = "lock";
const LOCK_ATTEMPTS: usize = 64;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Acquire {
    /// May create the stream directory and `lock` (chunk install, publish).
    Creating,
    /// Never creates anything; an absent stream yields no lock.
    Existing,
}

/// A verified exclusive stream lock. Dropping it releases the lock.
pub(crate) struct StreamLock {
    file: File,
    dir: PathBuf,
}

/// Bytes and files removed with a stream directory.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct RemovedStream {
    pub(crate) files: u64,
    pub(crate) bytes: u64,
    /// False when the empty-only directory removal found the directory not
    /// empty or gone: a producer re-created the stream, and the attempt ends.
    pub(crate) directory_removed: bool,
}

pub(crate) fn acquire(dir: &Path, mode: Acquire, hooks: &Hooks) -> Result<Option<StreamLock>> {
    let path = dir.join(LOCK_NAME);
    for _ in 0..LOCK_ATTEMPTS {
        let opened = match mode {
            Acquire::Creating => {
                fs::create_dir_all(dir)?;
                OpenOptions::new()
                    .create(true)
                    .truncate(false)
                    .read(true)
                    .write(true)
                    .open(&path)
            }
            Acquire::Existing => OpenOptions::new().read(true).write(true).open(&path),
        };
        let file = match opened {
            Ok(file) => file,
            // A deleter removed the directory between our create and open.
            Err(error) if error.kind() == ErrorKind::NotFound && mode == Acquire::Creating => {
                continue;
            }
            Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        hooks.after_lock_open();
        fs2::FileExt::lock_exclusive(&file)?;
        if names_same_file(&file, &path)? {
            return Ok(Some(StreamLock {
                file,
                dir: dir.to_path_buf(),
            }));
        }
        // The locked inode was unlinked or replaced while we waited. Drop it
        // and start over from the open, which sees the current state.
    }
    bail!("transcript stream lock kept changing identity; retry the request")
}

fn names_same_file(file: &File, path: &Path) -> io::Result<bool> {
    let named = match fs::symlink_metadata(path) {
        Ok(named) => named,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error),
    };
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let held = file.metadata()?;
        Ok(held.dev() == named.dev() && held.ino() == named.ino())
    }
    #[cfg(not(unix))]
    {
        let _ = file;
        Ok(named.file_type().is_file())
    }
}

impl StreamLock {
    pub(crate) fn dir(&self) -> &Path {
        &self.dir
    }

    /// Remove the whole stream directory: its contents, then `lock`, then
    /// the directory itself with an empty-only removal. A failure while
    /// removing contents leaves `lock` in place for a later sweep. After
    /// `lock` is unlinked a producer may re-create the stream; the empty-only
    /// removal then fails and this attempt ends without touching its files.
    pub(crate) fn remove_stream_dir(self, hooks: &Hooks) -> Result<RemovedStream> {
        let mut removed = RemovedStream::default();
        for entry in fs::read_dir(&self.dir)? {
            let entry = entry?;
            if entry.file_name() == LOCK_NAME {
                continue;
            }
            let path = entry.path();
            let metadata = fs::symlink_metadata(&path)?;
            if metadata.is_dir() {
                for inner in fs::read_dir(&path)? {
                    let inner = inner?.path();
                    let inner_metadata = fs::symlink_metadata(&inner)?;
                    fs::remove_file(&inner)?;
                    removed.files += 1;
                    removed.bytes += inner_metadata.len();
                }
                fs::remove_dir(&path)?;
            } else {
                fs::remove_file(&path)?;
                removed.files += 1;
                removed.bytes += metadata.len();
            }
        }
        let lock_path = self.dir.join(LOCK_NAME);
        let lock_bytes = fs::symlink_metadata(&lock_path).map_or(0, |metadata| metadata.len());
        fs::remove_file(&lock_path)?;
        removed.files += 1;
        removed.bytes += lock_bytes;
        hooks.after_lock_unlink();
        removed.directory_removed = fs::remove_dir(&self.dir).is_ok();
        drop(self.file);
        Ok(removed)
    }
}
