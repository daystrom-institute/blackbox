// This module is the synchronous repository implementation. Async service
// hosts must call it through their blocking-I/O lane or persistence actor.
#![allow(clippy::disallowed_methods)]

use std::fs::{self, File, OpenOptions};
use std::io::{BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use fs2::FileExt;
#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;

use crate::error::{FleetError, FleetResult};
use crate::model::FleetSnapshot;
use crate::ports::FleetRepository;

#[derive(Debug, Clone)]
pub struct FileFleetRepository {
    path: PathBuf,
}

impl FileFleetRepository {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    fn load_from_path(path: &Path) -> FleetResult<Option<FleetSnapshot>> {
        let file = match File::open(path) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        let snapshot: FleetSnapshot = serde_json::from_reader(BufReader::new(file))?;
        snapshot.validate()?;
        Ok(Some(snapshot))
    }

    fn write_atomic(&self, snapshot: &FleetSnapshot) -> FleetResult<()> {
        let parent = self.path.parent().unwrap_or_else(|| Path::new("."));
        fs::create_dir_all(parent)?;
        let file_name = self
            .path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("fleet-state.json");
        let temp = parent.join(format!(".{file_name}.{}.tmp", uuid::Uuid::new_v4()));

        let result = (|| -> FleetResult<()> {
            let mut options = OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            options.mode(0o600);
            let file = options.open(&temp)?;
            let mut writer = BufWriter::new(file);
            serde_json::to_writer_pretty(&mut writer, snapshot)?;
            writer.write_all(b"\n")?;
            writer.flush()?;
            writer.get_ref().sync_all()?;
            drop(writer);
            fs::rename(&temp, &self.path)?;
            File::open(parent)?.sync_all()?;
            Ok(())
        })();

        if result.is_err() {
            let _ = fs::remove_file(&temp);
        }
        result
    }

    fn open_lock_file(&self) -> FleetResult<File> {
        let parent = self.path.parent().unwrap_or_else(|| Path::new("."));
        fs::create_dir_all(parent)?;
        let file_name = self
            .path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("fleet-state.json");
        let mut options = OpenOptions::new();
        options.read(true).write(true).create(true);
        #[cfg(unix)]
        options.mode(0o600);
        Ok(options.open(parent.join(format!(".{file_name}.lock")))?)
    }
}

impl FleetRepository for FileFleetRepository {
    fn load(&self) -> FleetResult<Option<FleetSnapshot>> {
        Self::load_from_path(&self.path)
    }

    fn compare_and_swap(&self, expected_generation: u64, next: &FleetSnapshot) -> FleetResult<()> {
        next.validate()?;
        let required_generation = expected_generation
            .checked_add(1)
            .ok_or(FleetError::CounterExhausted("snapshot generation"))?;
        if next.generation != required_generation {
            return Err(FleetError::InvalidState(format!(
                "next snapshot generation {} does not follow expected generation {expected_generation}",
                next.generation
            )));
        }
        let lock = self.open_lock_file()?;
        lock.lock_exclusive()?;
        let actual = Self::load_from_path(&self.path)?
            .map(|snapshot| snapshot.generation)
            .unwrap_or(0);
        if actual != expected_generation {
            return Err(FleetError::SnapshotGenerationConflict {
                expected: expected_generation,
                actual,
            });
        }
        self.write_atomic(next)
    }
}

#[derive(Debug, Default)]
pub struct MemoryFleetRepository {
    state: Mutex<Option<FleetSnapshot>>,
}

impl MemoryFleetRepository {
    pub fn with_snapshot(snapshot: FleetSnapshot) -> Self {
        Self {
            state: Mutex::new(Some(snapshot)),
        }
    }
}

impl FleetRepository for MemoryFleetRepository {
    fn load(&self) -> FleetResult<Option<FleetSnapshot>> {
        self.state
            .lock()
            .map_err(|_| FleetError::LockPoisoned)
            .map(|guard| guard.clone())
    }

    fn compare_and_swap(&self, expected_generation: u64, next: &FleetSnapshot) -> FleetResult<()> {
        next.validate()?;
        let mut state = self.state.lock().map_err(|_| FleetError::LockPoisoned)?;
        let actual = state
            .as_ref()
            .map(|snapshot| snapshot.generation)
            .unwrap_or(0);
        if actual != expected_generation {
            return Err(FleetError::SnapshotGenerationConflict {
                expected: expected_generation,
                actual,
            });
        }
        let required_generation = expected_generation
            .checked_add(1)
            .ok_or(FleetError::CounterExhausted("snapshot generation"))?;
        if next.generation != required_generation {
            return Err(FleetError::InvalidState(format!(
                "next snapshot generation {} does not follow expected generation {expected_generation}",
                next.generation
            )));
        }
        *state = Some(next.clone());
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::sync::{Arc, Barrier};

    use super::*;

    #[test]
    fn file_repository_is_atomic_and_generation_fenced() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let repository = FileFleetRepository::new(root.join("fleet/state.json"));
        let first = FleetSnapshot {
            generation: 1,
            ..FleetSnapshot::default()
        };
        repository.compare_and_swap(0, &first).unwrap();
        assert_eq!(repository.load().unwrap(), Some(first.clone()));

        let mut stale = first.clone();
        stale.generation = 2;
        repository.compare_and_swap(1, &stale).unwrap();
        let mut conflict = stale.clone();
        conflict.generation = 2;
        let error = repository.compare_and_swap(1, &conflict).unwrap_err();
        assert!(matches!(
            error,
            FleetError::SnapshotGenerationConflict {
                expected: 1,
                actual: 2
            }
        ));
        assert_eq!(repository.load().unwrap(), Some(stale));
        let temporary_files = fs::read_dir(root.join("fleet"))
            .unwrap()
            .filter_map(Result::ok)
            .filter(|entry| entry.file_name().to_string_lossy().ends_with(".tmp"))
            .count();
        assert_eq!(temporary_files, 0);
    }

    #[cfg(unix)]
    #[test]
    fn file_repository_creates_private_state_file() {
        use std::os::unix::fs::PermissionsExt;

        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let path = root.join("state.json");
        let repository = FileFleetRepository::new(&path);
        let snapshot = FleetSnapshot {
            generation: 1,
            ..FleetSnapshot::default()
        };
        repository.compare_and_swap(0, &snapshot).unwrap();
        assert_eq!(
            fs::metadata(path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }

    #[test]
    fn malformed_and_unsupported_snapshots_fail_closed() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let path = root.join("state.json");
        fs::write(&path, b"not json").unwrap();
        let repository = FileFleetRepository::new(&path);
        assert!(matches!(
            repository.load(),
            Err(FleetError::Serialization(_))
        ));

        fs::write(
            &path,
            serde_json::to_vec(&serde_json::json!({
                "format_version": 99,
                "generation": 1,
                "roster_generation": 0
            }))
            .unwrap(),
        )
        .unwrap();
        assert!(matches!(
            repository.load(),
            Err(FleetError::UnsupportedSnapshotFormat { found: 99, .. })
        ));
    }

    #[test]
    fn file_compare_and_swap_serializes_competing_writers() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let repository = FileFleetRepository::new(root.join("state.json"));
        let barrier = Arc::new(Barrier::new(3));
        let mut handles = Vec::new();
        for _ in 0..2 {
            let repository = repository.clone();
            let barrier = barrier.clone();
            handles.push(std::thread::spawn(move || {
                let snapshot = FleetSnapshot {
                    generation: 1,
                    ..FleetSnapshot::default()
                };
                barrier.wait();
                repository.compare_and_swap(0, &snapshot)
            }));
        }
        barrier.wait();
        let results: Vec<_> = handles
            .into_iter()
            .map(|handle| handle.join().unwrap())
            .collect();
        assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
        assert_eq!(
            results
                .iter()
                .filter(|result| matches!(
                    result,
                    Err(FleetError::SnapshotGenerationConflict { .. })
                ))
                .count(),
            1
        );
    }
}
