//! The one-time legacy-state migration.
//!
//! An upgrading daemon finds its state under `~/.claude-shared/` and `~/.bro/`
//! and has to move it into the roots it has claimed. That move is one-time,
//! runs before any store is opened, and must never be performed twice or half
//! performed, so it owns a claim, a set of resolved destinations, and the
//! cross-device fallback that a `$HOME`-to-state-dir move can need.

use std::fs;
use std::path::{Path, PathBuf};

#[derive(Debug, PartialEq, Eq)]
pub enum LegacyMove {
    Moved { old: PathBuf, new: PathBuf },
    SkippedMissing { old: PathBuf },
    SkippedDestinationExists { old: PathBuf, new: PathBuf },
}

/// The staging name a cross-device migration copies into before it renames
/// into place (R33F2).
///
/// The old fallback created the DESTINATION and streamed into it, so an
/// interrupted copy left a short file at the authoritative path: the next
/// startup saw the destination exist, skipped the migration, and the truncated
/// copy became the store. Copying into a sibling temporary and renaming means
/// a partial copy is never named as the authority; it is leftover debris the
/// next attempt overwrites.
pub fn cross_device_temp_path(new: &Path) -> PathBuf {
    let mut name = new
        .file_name()
        .map(std::ffi::OsString::from)
        .unwrap_or_default();
    name.push(".migrating.tmp");
    match new.parent() {
        Some(parent) => parent.join(name),
        None => PathBuf::from(name),
    }
}

/// The EXDEV fallback: copy into a sibling temporary, fsync, then rename it
/// onto the destination. `pub` so the interrupted-copy shape is testable
/// without injecting a real device boundary.
pub fn migrate_legacy_file_across_devices(old: &Path, new: &Path) -> anyhow::Result<()> {
    let temp = cross_device_temp_path(new);
    // Debris from an interrupted earlier attempt is overwritten, never adopted.
    let _ = fs::remove_file(&temp);
    let mut source = fs::File::open(old)?;
    let mut dest = fs::File::create(&temp)?;
    std::io::copy(&mut source, &mut dest)?;
    dest.sync_all()?;
    drop(source);
    drop(dest);
    fs::rename(&temp, new)?;
    fs::remove_file(old)?;
    Ok(())
}

pub fn migrate_legacy_file(old: &Path, new: &Path) -> anyhow::Result<LegacyMove> {
    if !old.exists() {
        return Ok(LegacyMove::SkippedMissing {
            old: old.to_path_buf(),
        });
    }
    if new.exists() {
        return Ok(LegacyMove::SkippedDestinationExists {
            old: old.to_path_buf(),
            new: new.to_path_buf(),
        });
    }

    if let Some(parent) = new.parent() {
        fs::create_dir_all(parent)?;
    }

    // Try atomic rename first
    if let Err(e) = fs::rename(old, new) {
        if e.raw_os_error() == Some(18) {
            // EXDEV: Cross-device link
            migrate_legacy_file_across_devices(old, new).map_err(|error| {
                error.context(format!(
                    "failed to copy {} to {}",
                    old.display(),
                    new.display()
                ))
            })?;
        } else {
            return Err(anyhow::anyhow!(e).context(format!(
                "failed to move {} to {}",
                old.display(),
                new.display()
            )));
        }
    }

    Ok(LegacyMove::Moved {
        old: old.to_path_buf(),
        new: new.to_path_buf(),
    })
}

/// The name of the advisory lock guarding the one-time legacy migration,
/// placed next to the legacy source trees (`~/.claude-shared`, `~/.bro`).
pub const LEGACY_MIGRATION_LOCK_NAME: &str = ".blackbox-legacy-migration.lock";

/// A held claim on the legacy source tree. Dropping it, or exiting the
/// process by any route, releases the advisory lock the kernel holds on the
/// open file description.
#[derive(Debug)]
pub struct LegacyMigrationLock {
    file: fs::File,
    path: PathBuf,
}

impl LegacyMigrationLock {
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for LegacyMigrationLock {
    fn drop(&mut self) {
        let _ = fs2::FileExt::unlock(&self.file);
    }
}

/// The stable lock path for one legacy source tree.
pub fn legacy_migration_lock_path(home: &Path) -> PathBuf {
    home.join(LEGACY_MIGRATION_LOCK_NAME)
}

/// Claim the legacy source tree, without blocking.
///
/// R33F2: the migration's DESTINATIONS are now the daemon's resolved stores,
/// which sit inside roots it has already claimed. The one object two daemons
/// still share is the legacy SOURCE, since it hangs off `$HOME` and no daemon
/// owns `$HOME`. This serializes the one-time move over it: `Ok(None)` means
/// another process holds it, and the right response is to skip the migration
/// entirely — the holder either did it or is doing it, and no legacy sources
/// afterwards is the normal steady state.
pub fn try_lock_legacy_migration(home: &Path) -> anyhow::Result<Option<LegacyMigrationLock>> {
    use fs2::FileExt;

    let path = legacy_migration_lock_path(home);
    let file = bbox_corpus_core::json_store::open_lock_path_nofollow(&path)?;
    if !file.metadata()?.file_type().is_file() {
        anyhow::bail!(
            "legacy migration lock is not a regular file: {}",
            path.display()
        );
    }
    match file.try_lock_exclusive() {
        Ok(()) => Ok(Some(LegacyMigrationLock { file, path })),
        Err(error) if is_lock_contended(&error) => Ok(None),
        Err(error) => Err(anyhow::Error::new(error)
            .context(format!("claiming the legacy source at {}", path.display()))),
    }
}

/// Contention reports as the platform's would-block error rather than a
/// distinct kind, so compare against the value `fs2` documents for it.
fn is_lock_contended(error: &std::io::Error) -> bool {
    let contended = fs2::lock_contended_error();
    error.raw_os_error() == contended.raw_os_error() || error.kind() == contended.kind()
}

/// Where the one-time legacy migration puts each store it moves.
///
/// R33F2: these are the caller's ALREADY-RESOLVED paths, not a second
/// derivation from env vars and `$HOME` defaults. Recomputing them here
/// ignored the config file, so a daemon isolated purely by
/// `[paths].state_dir` renamed shared legacy state into the production
/// default paths it had not claimed, racing the daemon that owns them.
#[derive(Debug, Clone)]
pub struct LegacyMigrationDestinations {
    pub knowledge_path: PathBuf,
    pub threads_path: PathBuf,
    pub notes_path: PathBuf,
    pub index_path: PathBuf,
    pub global_common_md: PathBuf,
    pub bro_home: PathBuf,
}

/// Move the legacy stores into this daemon's RESOLVED destinations, once.
///
/// Returns an empty list when another process holds the legacy source claim:
/// the migration is one-time and idempotent, so deferring to the holder loses
/// nothing.
pub fn migrate_legacy_defaults(
    home: &Path,
    destinations: &LegacyMigrationDestinations,
) -> anyhow::Result<Vec<String>> {
    let Some(_source_claim) = try_lock_legacy_migration(home)? else {
        tracing::info!(
            lock = %legacy_migration_lock_path(home).display(),
            "another process holds the legacy source; skipping the one-time migration"
        );
        return Ok(Vec::new());
    };

    let mut moved = Vec::new();

    for (label, old, new) in [
        (
            "knowledge",
            home.join(".claude-shared").join("blackbox-knowledge.json"),
            destinations.knowledge_path.clone(),
        ),
        (
            "threads",
            home.join(".claude-shared").join("blackbox-threads.json"),
            destinations.threads_path.clone(),
        ),
        (
            "notes",
            home.join(".claude-shared").join("blackbox-notes.json"),
            destinations.notes_path.clone(),
        ),
        (
            "index",
            home.join(".claude-shared").join("transcript-index"),
            destinations.index_path.clone(),
        ),
        (
            "blackbox-md",
            home.join(".claude-shared").join("BLACKBOX.md"),
            destinations.global_common_md.clone(),
        ),
    ] {
        if let LegacyMove::Moved { old, new } = migrate_legacy_file(&old, &new)? {
            moved.push(format!("{label}: {} -> {}", old.display(), new.display()));
        }
    }

    // Task 3: ~/.bro/ migration
    let old_bro = home.join(".bro");
    let new_bro = destinations.bro_home.clone();
    if old_bro.is_dir() {
        for entry in fs::read_dir(&old_bro)? {
            let entry = entry?;
            let old_path = entry.path();
            let name = old_path
                .file_name()
                .ok_or_else(|| anyhow::anyhow!("invalid file name"))?;
            let new_path = new_bro.join(name);
            match migrate_legacy_file(&old_path, &new_path)? {
                LegacyMove::Moved { old, new } => {
                    moved.push(format!("bro: {} -> {}", old.display(), new.display()));
                }
                LegacyMove::SkippedDestinationExists { old, new } => {
                    tracing::warn!(
                        "Skipped migrating {} because {} already exists",
                        old.display(),
                        new.display()
                    );
                }
                _ => {}
            }
        }
        // If empty after migration, try to remove old dir
        if old_bro
            .read_dir()
            .map(|mut d| d.next().is_none())
            .unwrap_or(false)
        {
            let _ = fs::remove_dir(&old_bro);
        }
    }

    Ok(moved)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::util::test_env_lock;
    use tempfile::tempdir;

    /// Every destination below one fixture root. R33F2: the migration takes
    /// its destinations from the caller's resolved configuration, so the test
    /// never has to mutate process environment to steer them.
    fn fixture_destinations(state: &Path, home: &Path) -> LegacyMigrationDestinations {
        LegacyMigrationDestinations {
            knowledge_path: state.join("blackbox-knowledge.json"),
            threads_path: state.join("blackbox-threads.json"),
            notes_path: state.join("blackbox-notes.json"),
            index_path: state.join("index"),
            global_common_md: home.join(".blackbox").join("BLACKBOX.md"),
            bro_home: state.join("bro"),
        }
    }

    fn write_legacy_tree(home: &Path) {
        let old_shared = home.join(".claude-shared");
        let old_bro = home.join(".bro");
        fs::create_dir_all(&old_shared).unwrap();
        fs::create_dir_all(&old_bro).unwrap();
        fs::write(old_shared.join("blackbox-knowledge.json"), "{}").unwrap();
        fs::write(old_shared.join("blackbox-threads.json"), "{}").unwrap();
        fs::write(old_shared.join("blackbox-notes.json"), "{}").unwrap();
        fs::create_dir_all(old_shared.join("transcript-index")).unwrap();
        fs::write(old_shared.join("transcript-index").join("meta"), "x").unwrap();
        fs::write(old_bro.join("tasks.json"), "[]").unwrap();
    }

    #[test]
    fn migrates_legacy_defaults_when_new_targets_absent() {
        let dir = tempdir().unwrap();
        let home = dir.path().canonicalize().unwrap();
        write_legacy_tree(&home);
        let destinations = fixture_destinations(&home.join("state"), &home);

        let moved = migrate_legacy_defaults(&home, &destinations).unwrap();
        // Moved: knowledge, threads, notes, index, bro (tasks.json)
        assert!(
            moved.len() >= 4,
            "expected >=4 moves, got {}: {:?}",
            moved.len(),
            moved
        );
        assert!(destinations.knowledge_path.exists());
        assert!(destinations.threads_path.exists());
        assert!(destinations.notes_path.exists());
        assert!(destinations.index_path.exists());
        assert!(destinations.bro_home.join("tasks.json").exists());
        assert!(
            !home
                .join(".claude-shared")
                .join("blackbox-knowledge.json")
                .exists()
        );
        assert!(!home.join(".bro").exists());
    }

    /// R33F2. Two daemons isolated purely by configuration share one `$HOME`,
    /// so they see the same legacy source tree. The claim on that source is
    /// what stops both from committing to the same one-time move: the loser
    /// migrates nothing, and the sources land in the winner's RESOLVED roots
    /// exactly once. The destinations differ per daemon, which is the whole
    /// point: recomputing them from env vars and `$HOME` sent both daemons at
    /// one production-default path neither had claimed.
    #[test]
    fn two_config_isolated_daemons_sharing_one_home_migrate_the_sources_once() {
        let dir = tempdir().unwrap();
        let home = dir.path().canonicalize().unwrap();
        write_legacy_tree(&home);
        let first = fixture_destinations(&home.join("state-a"), &home);
        let second = fixture_destinations(&home.join("state-b"), &home);

        // The first daemon is inside its migration, holding the source claim.
        let held = try_lock_legacy_migration(&home)
            .unwrap()
            .expect("the first daemon claims the legacy source");

        let skipped = migrate_legacy_defaults(&home, &second).unwrap();
        assert!(
            skipped.is_empty(),
            "the daemon that lost the source claim must migrate nothing: {skipped:?}"
        );
        assert!(!second.knowledge_path.exists());
        assert!(!second.bro_home.exists());
        assert!(
            home.join(".claude-shared")
                .join("blackbox-knowledge.json")
                .exists()
        );

        // The winner finishes and releases; the sources move exactly once,
        // into ITS resolved roots.
        drop(held);
        let moved = migrate_legacy_defaults(&home, &first).unwrap();
        assert!(moved.iter().any(|line| line.starts_with("knowledge:")));
        assert!(first.knowledge_path.exists());
        assert!(first.bro_home.join("tasks.json").exists());

        // The second daemon runs afterwards: the sources are gone, which is
        // the normal steady state, and nothing lands in its roots.
        let after = migrate_legacy_defaults(&home, &second).unwrap();
        assert!(after.is_empty(), "nothing is left to migrate: {after:?}");
        assert!(!second.knowledge_path.exists());
    }

    /// R33F2. An interrupted cross-device copy must never be mistaken for the
    /// migrated authority: the copy lands on a temporary name and is renamed
    /// into place, so debris at the temporary path is overwritten rather than
    /// adopted. Asserted through the naming rather than by injecting a real
    /// device boundary.
    #[test]
    fn a_cross_device_copy_stages_under_a_temporary_name() {
        let dir = tempdir().unwrap();
        let home = dir.path().canonicalize().unwrap();
        let old = home.join("legacy.json");
        let new = home.join("destination.json");
        fs::write(&old, "legacy body").unwrap();

        let temp = cross_device_temp_path(&new);
        assert_ne!(temp, new, "the staging name is never the destination");
        assert_eq!(
            temp.parent(),
            new.parent(),
            "staging stays on the target device"
        );

        // Debris from an interrupted earlier copy, which the destination-exists
        // check above deliberately does not see.
        fs::write(&temp, "truncated debr").unwrap();
        assert!(
            !new.exists(),
            "a partial copy is never named as the authority"
        );

        migrate_legacy_file_across_devices(&old, &new).unwrap();
        assert_eq!(fs::read_to_string(&new).unwrap(), "legacy body");
        assert!(
            !temp.exists(),
            "the staging name does not survive the rename"
        );
        assert!(!old.exists(), "the source is removed only after the rename");
    }

    #[test]
    fn util_migrate_legacy_file_skips_destination_exists() {
        let _guard = test_env_lock();
        let dir = tempdir().unwrap();
        let home = dir.path();
        let old = home.join("old.txt");
        let new = home.join("new.txt");
        fs::write(&old, "old").unwrap();
        fs::write(&new, "new").unwrap();

        let res = migrate_legacy_file(&old, &new).unwrap();
        assert!(matches!(res, LegacyMove::SkippedDestinationExists { .. }));
        assert_eq!(fs::read_to_string(&old).unwrap(), "old");
        assert_eq!(fs::read_to_string(&new).unwrap(), "new");
    }
}
