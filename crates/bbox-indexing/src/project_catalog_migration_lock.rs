use std::fs::File;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use bbox_corpus_core::json_store::open_lock_path_nofollow;
use fs2::FileExt;

const PROJECT_CATALOG_MIGRATION_LOCK_FILE: &str = "project-catalog-migration.lock";

/// A process-lifetime rollout lock for the project catalog migration.
///
/// Version-1 daemon registries hold a shared guard for as long as any
/// writer-capable registry clone remains alive. The offline migration will
/// use the exclusive mode, so it cannot race a compatible daemon writer.
#[derive(Debug)]
pub struct ProjectCatalogMigrationLock {
    _file: File,
}

impl ProjectCatalogMigrationLock {
    /// Acquire the shared lifetime lock used by a compatible daemon or
    /// read-only migration preflight.
    pub fn acquire_shared(projects_path: &Path) -> Result<Self> {
        let (file, lock_path) = open_lock_file(projects_path)?;
        FileExt::lock_shared(&file).with_context(|| {
            format!(
                "failed to acquire shared project catalog migration lock {}",
                lock_path.display()
            )
        })?;
        Ok(Self { _file: file })
    }

    /// Try to acquire the exclusive lifetime lock used by an offline
    /// migration. `Ok(None)` means a compatible daemon or preflight still
    /// holds a shared guard.
    pub fn try_acquire_exclusive(projects_path: &Path) -> Result<Option<Self>> {
        let (file, lock_path) = open_lock_file(projects_path)?;
        match FileExt::try_lock_exclusive(&file) {
            Ok(()) => Ok(Some(Self { _file: file })),
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => Ok(None),
            Err(error) => Err(error).with_context(|| {
                format!(
                    "failed to acquire exclusive project catalog migration lock {}",
                    lock_path.display()
                )
            }),
        }
    }
}

pub fn project_catalog_migration_lock_path(projects_path: &Path) -> PathBuf {
    projects_path
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join(PROJECT_CATALOG_MIGRATION_LOCK_FILE)
}

fn open_lock_file(projects_path: &Path) -> Result<(File, PathBuf)> {
    let lock_path = project_catalog_migration_lock_path(projects_path);
    let file = open_lock_path_nofollow(&lock_path).with_context(|| {
        format!(
            "failed to open project catalog migration lock {}",
            lock_path.display()
        )
    })?;
    Ok((file, lock_path))
}

#[cfg(test)]
mod tests {
    use super::*;
    use bbox_stores::store_persister::StorePersister;
    use parking_lot::RwLock;
    use std::fs;
    use std::sync::Arc;

    #[test]
    fn shared_lifetime_locks_can_coexist() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let projects_path = root.join("projects.json");

        let first = ProjectCatalogMigrationLock::acquire_shared(&projects_path).unwrap();
        let second = ProjectCatalogMigrationLock::acquire_shared(&projects_path).unwrap();

        assert!(
            ProjectCatalogMigrationLock::try_acquire_exclusive(&projects_path)
                .unwrap()
                .is_none()
        );
        drop((first, second));
    }

    #[test]
    fn exclusive_lifetime_lock_refuses_a_live_registry_writer() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let projects_path = root.join("projects.json");
        let registry = Arc::new(RwLock::new(
            crate::projects::ProjectRegistry::open(&projects_path).unwrap(),
        ));
        let persister = StorePersister::spawn(
            "migration-lock-writer",
            registry.clone(),
            projects_path.clone(),
        );
        drop(registry);

        assert!(
            ProjectCatalogMigrationLock::try_acquire_exclusive(&projects_path)
                .unwrap()
                .is_none()
        );

        drop(persister);
    }

    #[test]
    fn exclusive_lifetime_lock_succeeds_after_shared_release() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let projects_path = root.join("projects.json");
        let shared = ProjectCatalogMigrationLock::acquire_shared(&projects_path).unwrap();

        assert!(
            ProjectCatalogMigrationLock::try_acquire_exclusive(&projects_path)
                .unwrap()
                .is_none()
        );
        drop(shared);

        let exclusive = ProjectCatalogMigrationLock::try_acquire_exclusive(&projects_path)
            .unwrap()
            .expect("exclusive migration lock after shared guard release");
        assert_eq!(
            project_catalog_migration_lock_path(&projects_path),
            root.join(PROJECT_CATALOG_MIGRATION_LOCK_FILE)
        );
        drop(exclusive);
    }

    #[cfg(unix)]
    #[test]
    fn lifetime_lock_refuses_symlink() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let projects_path = root.join("projects.json");
        let lock_path = project_catalog_migration_lock_path(&projects_path);
        let target = root.join("target");
        fs::write(&target, b"do not follow").unwrap();
        symlink(&target, &lock_path).unwrap();

        assert!(ProjectCatalogMigrationLock::acquire_shared(&projects_path).is_err());
        assert_eq!(fs::read(&target).unwrap(), b"do not follow");
    }
}
