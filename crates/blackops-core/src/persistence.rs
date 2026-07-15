//! Blocking persistence implementation used behind the blackopsd authority
//! actor. The daemon never invokes it concurrently or from request handlers
//! without first acquiring the single authority owner.
#![allow(clippy::disallowed_methods)]

use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use fs2::FileExt;

use crate::{BlackopsError, BlackopsResult, OperationalRepository, OperationalSnapshot};

#[derive(Debug)]
pub struct MemoryRepository {
    snapshot: Mutex<Option<OperationalSnapshot>>,
}

impl Default for MemoryRepository {
    fn default() -> Self {
        Self {
            snapshot: Mutex::new(None),
        }
    }
}

impl MemoryRepository {
    pub fn with_snapshot(snapshot: OperationalSnapshot) -> Self {
        Self {
            snapshot: Mutex::new(Some(snapshot)),
        }
    }
}

impl OperationalRepository for MemoryRepository {
    fn load(&self) -> BlackopsResult<Option<OperationalSnapshot>> {
        Ok(self
            .snapshot
            .lock()
            .expect("memory repository poisoned")
            .clone())
    }

    fn compare_and_swap(
        &self,
        expected_generation: u64,
        next: &OperationalSnapshot,
    ) -> BlackopsResult<()> {
        let mut guard = self.snapshot.lock().expect("memory repository poisoned");
        let found = guard.as_ref().map_or(0, |snapshot| snapshot.generation);
        if found != expected_generation {
            return Err(BlackopsError::GenerationConflict {
                expected: expected_generation,
                found,
            });
        }
        *guard = Some(next.clone());
        Ok(())
    }
}

#[derive(Debug, Clone)]
pub struct FileRepository {
    state_root: PathBuf,
    snapshot_path: PathBuf,
    lock_path: PathBuf,
}

impl FileRepository {
    pub fn open(state_root: impl Into<PathBuf>) -> BlackopsResult<Self> {
        let state_root = state_root.into();
        prepare_private_directory(&state_root)?;
        Ok(Self {
            snapshot_path: state_root.join("authority.json"),
            lock_path: state_root.join("authority.lock"),
            state_root,
        })
    }

    pub fn state_root(&self) -> &Path {
        &self.state_root
    }

    fn open_lock(&self) -> BlackopsResult<File> {
        reject_symlink_if_present(&self.lock_path)?;
        let mut options = OpenOptions::new();
        options.create(true).read(true).write(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let file = options.open(&self.lock_path)?;
        enforce_private_file(&self.lock_path)?;
        Ok(file)
    }

    fn load_unlocked(&self) -> BlackopsResult<Option<OperationalSnapshot>> {
        reject_symlink_if_present(&self.snapshot_path)?;
        if !self.snapshot_path.exists() {
            return Ok(None);
        }
        enforce_private_file(&self.snapshot_path)?;
        let mut bytes = Vec::new();
        File::open(&self.snapshot_path)?.read_to_end(&mut bytes)?;
        let snapshot: OperationalSnapshot = serde_json::from_slice(&bytes)?;
        snapshot.validate()?;
        Ok(Some(snapshot))
    }

    fn write_atomic(&self, next: &OperationalSnapshot) -> BlackopsResult<()> {
        let bytes = serde_json::to_vec_pretty(next)?;
        let temporary = self
            .state_root
            .join(format!(".authority.{}.tmp", uuid::Uuid::new_v4()));
        let mut options = OpenOptions::new();
        options.create_new(true).write(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&temporary)?;
        let result = (|| -> BlackopsResult<()> {
            file.write_all(&bytes)?;
            file.sync_all()?;
            fs::rename(&temporary, &self.snapshot_path)?;
            File::open(&self.state_root)?.sync_all()?;
            enforce_private_file(&self.snapshot_path)?;
            Ok(())
        })();
        if result.is_err() {
            let _ = fs::remove_file(&temporary);
        }
        result
    }
}

impl OperationalRepository for FileRepository {
    fn load(&self) -> BlackopsResult<Option<OperationalSnapshot>> {
        let lock = self.open_lock()?;
        FileExt::lock_shared(&lock)?;
        let result = self.load_unlocked();
        FileExt::unlock(&lock)?;
        result
    }

    fn compare_and_swap(
        &self,
        expected_generation: u64,
        next: &OperationalSnapshot,
    ) -> BlackopsResult<()> {
        next.validate()?;
        let lock = self.open_lock()?;
        FileExt::lock_exclusive(&lock)?;
        let found = self
            .load_unlocked()?
            .as_ref()
            .map_or(0, |snapshot| snapshot.generation);
        if found != expected_generation {
            FileExt::unlock(&lock)?;
            return Err(BlackopsError::GenerationConflict {
                expected: expected_generation,
                found,
            });
        }
        let result = self.write_atomic(next);
        FileExt::unlock(&lock)?;
        result
    }
}

fn prepare_private_directory(path: &Path) -> BlackopsResult<()> {
    reject_symlink_if_present(path)?;
    fs::create_dir_all(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
        let mode = fs::symlink_metadata(path)?.permissions().mode();
        if mode & 0o077 != 0 {
            return Err(BlackopsError::InvalidState(format!(
                "state directory {} is accessible by group or other users",
                path.display()
            )));
        }
    }
    Ok(())
}

fn reject_symlink_if_present(path: &Path) -> BlackopsResult<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => Err(BlackopsError::InvalidState(
            format!("refusing symlink-backed authority path {}", path.display()),
        )),
        Ok(_) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

fn enforce_private_file(path: &Path) -> BlackopsResult<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let metadata = fs::symlink_metadata(path)?;
        if metadata.file_type().is_symlink() || metadata.permissions().mode() & 0o077 != 0 {
            return Err(BlackopsError::InvalidState(format!(
                "authority file {} is not private",
                path.display()
            )));
        }
    }
    Ok(())
}
