//! The one-time legacy-state migration.
//!
//! An upgrading daemon finds its state under `~/.claude-shared/` and `~/.bro/`
//! and has to move it into the roots it has claimed. That move is one-time,
//! runs before any store is opened, and must never be performed twice or half
//! performed, so it owns a claim, a set of resolved destinations, and the
//! cross-device fallback that a `$HOME`-to-state-dir move can need.

// Sanctioned blocking context (concurrency-model §5, invariant I2): the
// migration runs on the startup thread, before the daemon binds a listener or
// spawns any runtime work, so none of these filesystem calls can land on a
// tokio worker. It is also inherently synchronous — the whole point is that
// nothing else runs until the move is durable.
#![allow(clippy::disallowed_methods)]

use anyhow::Context;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

/// `EACCES`, injected by tests. The value is identical on every platform this
/// daemon runs on, and the migration only ever compares errnos.
pub const INJECTED_EACCES: i32 = 13;
/// `EIO`, injected by tests.
pub const INJECTED_EIO: i32 = 5;
/// `EXDEV`. The cross-device rename refusal the staged fallback exists for.
const EXDEV: i32 = 18;

/// The points the one-time migration crosses that a test can fail on demand.
///
/// R34F1/R34F2: both findings are about what the migration does when an
/// inspection or a durability step fails, and neither is reachable from a
/// test that can only supply well-behaved files. Faults are injected rather
/// than provoked with real permissions so the tests are deterministic and do
/// not depend on the test process being unprivileged.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LegacyMigrationFault {
    /// Inspecting a legacy source entry.
    InspectSource,
    /// Inspecting the destination the entry would move to.
    InspectDestination,
    /// Inspecting the legacy `~/.bro` directory.
    InspectBroSource,
    /// Probing whether `~/.bro` is empty after its entries moved.
    ProbeBroEmptiness,
    /// Force every publish onto the cross-device staging path, as if the
    /// destination lived on another filesystem.
    RenameCrossDevice,
    /// After the staged copy is durable, before it is published.
    AfterStage,
    /// After the destination name is durable, before publication is recorded.
    AfterPublish,
    /// After publication is recorded, before the source is deleted.
    AfterPublishRecorded,
    /// After the source deletion is durable, before the journal is cleared.
    AfterSourceRemoved,
}

static FAULTS_ARMED: AtomicBool = AtomicBool::new(false);
static FAULTS: Mutex<Vec<(LegacyMigrationFault, i32)>> = Mutex::new(Vec::new());
/// Serializes fault-armed tests. Deliberately NOT `test_env_lock`: tests that
/// arm faults also mutate environment, and the two locks must be takeable
/// together.
static FAULT_LOCK: Mutex<()> = Mutex::new(());

/// Holds the armed faults for one test; disarms on drop, including on panic.
pub struct LegacyMigrationFaultGuard {
    _lock: std::sync::MutexGuard<'static, ()>,
}

impl Drop for LegacyMigrationFaultGuard {
    fn drop(&mut self) {
        FAULTS_ARMED.store(false, Ordering::SeqCst);
        FAULTS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clear();
    }
}

/// Arm the listed faults for the lifetime of the returned guard.
///
/// A test seam, not an API: production never calls this, and the armed check
/// is one relaxed atomic load per checkpoint.
pub fn arm_legacy_migration_faults(
    faults: &[(LegacyMigrationFault, i32)],
) -> LegacyMigrationFaultGuard {
    let lock = FAULT_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    {
        let mut armed = FAULTS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        armed.clear();
        armed.extend_from_slice(faults);
    }
    FAULTS_ARMED.store(true, Ordering::SeqCst);
    LegacyMigrationFaultGuard { _lock: lock }
}

fn injected_errno(point: LegacyMigrationFault) -> Option<i32> {
    if !FAULTS_ARMED.load(Ordering::Relaxed) {
        return None;
    }
    FAULTS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .iter()
        .find(|(armed, _)| *armed == point)
        .map(|(_, errno)| *errno)
}

/// Fail at `point` when a test has armed it.
fn checkpoint(point: LegacyMigrationFault, subject: &Path) -> anyhow::Result<()> {
    match injected_errno(point) {
        None => Ok(()),
        Some(errno) => Err(anyhow::Error::new(std::io::Error::from_raw_os_error(errno))
            .context(format!("injected {point:?} fault at {}", subject.display()))),
    }
}

/// Whether an error is the kernel's cross-filesystem rename refusal.
fn is_cross_device(error: &std::io::Error) -> bool {
    error.raw_os_error() == Some(EXDEV)
}

/// Inspect one migration entry without collapsing inspection failures into
/// absence.
///
/// R34F1: `Path::exists()` and `Path::is_dir()` map EVERY error — `EACCES` on
/// a parent component, `EIO` from a failing device — onto `false`. A transient
/// inspection failure therefore read as "the legacy source is not there", the
/// migration reported it skipped, and startup went on to create a fresh
/// destination. The next startup then took the destination-exists branch and
/// stranded the legacy source permanently. Only `NotFound` is absence here;
/// every other error propagates and refuses the startup before any destination
/// is created.
fn inspect(path: &Path, point: LegacyMigrationFault) -> anyhow::Result<Option<fs::Metadata>> {
    checkpoint(point, path)?;
    match fs::metadata(path) {
        Ok(metadata) => Ok(Some(metadata)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(anyhow::Error::new(error).context(format!(
            "inspecting the legacy migration entry {}",
            path.display()
        ))),
    }
}

/// Whether `path` holds no entries, refusing rather than guessing when the
/// directory cannot be read.
///
/// R34F1: the old probe was `read_dir(...).map(...).unwrap_or(false)`, which
/// treated an unreadable directory as non-empty. That direction happens to be
/// the safe one, but it hid the failure; the caller now sees it.
fn directory_is_empty(path: &Path) -> anyhow::Result<bool> {
    checkpoint(LegacyMigrationFault::ProbeBroEmptiness, path)?;
    let mut entries = fs::read_dir(path)
        .with_context(|| format!("reading the legacy directory {}", path.display()))?;
    match entries.next() {
        None => Ok(true),
        Some(Ok(_)) => Ok(false),
        Some(Err(error)) => Err(anyhow::Error::new(error)
            .context(format!("reading the legacy directory {}", path.display()))),
    }
}

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

/// The durable record of the migration in flight, kept next to the legacy
/// source claim (R34F2).
///
/// A cross-filesystem move is not one atomic operation: it stages a copy,
/// publishes it, and only then deletes the source. Every boundary between
/// those steps is a place a crash can land, and the old fallback recorded
/// none of them — a crash after the destination rename but before the source
/// was removed left BOTH names, and the next daemon (possibly one with
/// entirely different roots) migrated the stale source a second time. The
/// journal lives with the SOURCE because the source is the one object every
/// daemon shares, so whoever next holds the claim finds and finishes the
/// interrupted transaction.
pub const LEGACY_MIGRATION_JOURNAL_NAME: &str = ".blackbox-legacy-migration.journal";

/// The stable journal path for one legacy source tree.
pub fn legacy_migration_journal_path(home: &Path) -> PathBuf {
    home.join(LEGACY_MIGRATION_JOURNAL_NAME)
}

const MIGRATION_RECORD_VERSION: u32 = 1;

/// How far the transaction got. The distinction that matters is whether the
/// destination is durably published: before it is, the source is still the
/// authority and the transaction rolls back; after it is, the destination is
/// the authority and the transaction rolls forward.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
enum MigrationPhase {
    Prepared,
    Published,
}

/// Whether the publish is a same-filesystem rename or a staged copy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
enum MigrationMode {
    Rename,
    Stage,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct MigrationRecord {
    version: u32,
    source: PathBuf,
    destination: PathBuf,
    stage: PathBuf,
    mode: MigrationMode,
    phase: MigrationPhase,
}

/// fsync one directory so the names it holds survive a crash.
fn sync_dir(path: &Path) -> anyhow::Result<()> {
    fs::File::open(path)
        .with_context(|| format!("opening {} for fsync", path.display()))?
        .sync_all()
        .with_context(|| format!("fsync {}", path.display()))
}

/// fsync one directory, tolerating its absence. Recovery reaches for parents
/// that a rolled-back transaction may never have created.
fn sync_dir_if_present(path: &Path) -> anyhow::Result<()> {
    match fs::File::open(path) {
        Ok(directory) => directory
            .sync_all()
            .with_context(|| format!("fsync {}", path.display())),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => {
            Err(anyhow::Error::new(error).context(format!("opening {} for fsync", path.display())))
        }
    }
}

/// Replace the journal with `record` and make the replacement durable.
fn write_record(home: &Path, record: &MigrationRecord) -> anyhow::Result<()> {
    use std::io::{Seek, SeekFrom, Write};

    let path = legacy_migration_journal_path(home);
    let bytes = serde_json::to_vec_pretty(record)
        .with_context(|| format!("encoding the migration journal {}", path.display()))?;
    // Same no-follow open as the claim beside it: the journal hangs off
    // `$HOME`, which no daemon owns.
    let mut file = bbox_corpus_core::json_store::open_lock_path_nofollow(&path)
        .with_context(|| format!("opening the migration journal {}", path.display()))?;
    if !file.metadata()?.file_type().is_file() {
        anyhow::bail!(
            "the legacy migration journal is not a regular file: {}",
            path.display()
        );
    }
    file.set_len(0)
        .with_context(|| format!("truncating the migration journal {}", path.display()))?;
    file.seek(SeekFrom::Start(0))
        .with_context(|| format!("rewinding the migration journal {}", path.display()))?;
    file.write_all(&bytes)
        .with_context(|| format!("writing the migration journal {}", path.display()))?;
    file.sync_all()
        .with_context(|| format!("fsync the migration journal {}", path.display()))?;
    drop(file);
    sync_dir(home)
}

/// Read the pending transaction, if there is one.
///
/// A journal that cannot be read or parsed is a refusal, never an assumed
/// absence: the whole point of the record is that its absence means "nothing
/// is in flight".
fn read_record(home: &Path) -> anyhow::Result<Option<MigrationRecord>> {
    let path = legacy_migration_journal_path(home);
    let bytes = match fs::read(&path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(anyhow::Error::new(error)
                .context(format!("reading the migration journal {}", path.display())));
        }
    };
    // An empty journal is the file the claim's own open can leave behind.
    if bytes.iter().all(|byte| byte.is_ascii_whitespace()) {
        return Ok(None);
    }
    let record: MigrationRecord = serde_json::from_slice(&bytes)
        .with_context(|| format!("parsing the migration journal {}", path.display()))?;
    if record.version != MIGRATION_RECORD_VERSION {
        anyhow::bail!(
            "the migration journal {} has unsupported version {}",
            path.display(),
            record.version
        );
    }
    Ok(Some(record))
}

/// Drop the journal; the transaction it described is complete.
fn clear_record(home: &Path) -> anyhow::Result<()> {
    let path = legacy_migration_journal_path(home);
    match fs::remove_file(&path) {
        Ok(()) => sync_dir(home),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(anyhow::Error::new(error)
            .context(format!("removing the migration journal {}", path.display()))),
    }
}

/// Remove a file, a symlink, or a whole tree; absence is success.
fn remove_any(path: &Path) -> anyhow::Result<()> {
    match fs::symlink_metadata(path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => {
            Err(anyhow::Error::new(error)
                .context(format!("inspecting {} for removal", path.display())))
        }
        Ok(metadata) if metadata.file_type().is_dir() => fs::remove_dir_all(path)
            .with_context(|| format!("removing the directory {}", path.display())),
        Ok(_) => fs::remove_file(path).with_context(|| format!("removing {}", path.display())),
    }
}

/// Copy one legacy entry onto the destination filesystem under a staging
/// name, fsyncing every file and every directory it creates.
///
/// R34F2: the old fallback was file-only, so a legacy DIRECTORY (the
/// transcript index, anything beneath `~/.bro`) could not cross a filesystem
/// boundary at all and the upgrade refused on every boot. It also synced only
/// the copied file, never the directories holding the new names, so a crash
/// could lose a name the rename had already published.
fn stage_entry(source: &Path, stage: &Path) -> anyhow::Result<()> {
    let metadata = fs::symlink_metadata(source)
        .with_context(|| format!("inspecting {} for staging", source.display()))?;
    let file_type = metadata.file_type();
    if file_type.is_dir() {
        fs::create_dir(stage)
            .with_context(|| format!("creating the staging directory {}", stage.display()))?;
        for entry in fs::read_dir(source)
            .with_context(|| format!("reading the legacy directory {}", source.display()))?
        {
            let entry = entry
                .with_context(|| format!("reading the legacy directory {}", source.display()))?;
            stage_entry(&entry.path(), &stage.join(entry.file_name()))?;
        }
        fs::set_permissions(stage, metadata.permissions())
            .with_context(|| format!("setting permissions on the staged {}", stage.display()))?;
        // The children are durable before the parent name is published.
        sync_dir(stage)?;
    } else if file_type.is_file() {
        let mut reader = fs::File::open(source)
            .with_context(|| format!("opening the legacy file {}", source.display()))?;
        let mut writer = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(stage)
            .with_context(|| format!("creating the staged file {}", stage.display()))?;
        std::io::copy(&mut reader, &mut writer)
            .with_context(|| format!("copying {} to {}", source.display(), stage.display()))?;
        writer
            .set_permissions(metadata.permissions())
            .with_context(|| format!("setting permissions on the staged {}", stage.display()))?;
        writer
            .sync_all()
            .with_context(|| format!("fsync the staged {}", stage.display()))?;
    } else if file_type.is_symlink() {
        stage_symlink(source, stage)?;
    } else {
        anyhow::bail!(
            "cannot migrate {}: it is neither a file, a directory, nor a symlink",
            source.display()
        );
    }
    Ok(())
}

#[cfg(unix)]
fn stage_symlink(source: &Path, stage: &Path) -> anyhow::Result<()> {
    let target = fs::read_link(source)
        .with_context(|| format!("reading the legacy symlink {}", source.display()))?;
    std::os::unix::fs::symlink(&target, stage)
        .with_context(|| format!("staging the symlink {}", stage.display()))
}

#[cfg(not(unix))]
fn stage_symlink(source: &Path, _stage: &Path) -> anyhow::Result<()> {
    anyhow::bail!(
        "cannot migrate the symlink {} across filesystems on this platform",
        source.display()
    )
}

/// Move one legacy entry into its destination as a recoverable transaction.
///
/// The ordering is the contract (R34F2). The journal records the intent
/// before either name is touched; the staged copy is durable before it is
/// published; the destination name is durable before publication is recorded;
/// publication is recorded before the source is deleted; the source parent is
/// durable before the journal is cleared. Every boundary between those steps
/// is recoverable by [`recover_legacy_migration`].
pub fn migrate_legacy_entry(home: &Path, old: &Path, new: &Path) -> anyhow::Result<LegacyMove> {
    if inspect(old, LegacyMigrationFault::InspectSource)?.is_none() {
        return Ok(LegacyMove::SkippedMissing {
            old: old.to_path_buf(),
        });
    }
    if inspect(new, LegacyMigrationFault::InspectDestination)?.is_some() {
        return Ok(LegacyMove::SkippedDestinationExists {
            old: old.to_path_buf(),
            new: new.to_path_buf(),
        });
    }

    let destination_parent = new
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));
    let source_parent = old
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));
    fs::create_dir_all(&destination_parent).with_context(|| {
        format!(
            "creating the migration destination directory {}",
            destination_parent.display()
        )
    })?;

    let mut record = MigrationRecord {
        version: MIGRATION_RECORD_VERSION,
        source: old.to_path_buf(),
        destination: new.to_path_buf(),
        stage: cross_device_temp_path(new),
        mode: MigrationMode::Rename,
        phase: MigrationPhase::Prepared,
    };
    // Durable BEFORE either name is touched: a crash from here on leaves a
    // record the next holder of the claim can finish or roll back.
    write_record(home, &record)?;

    // A same-filesystem rename publishes and closes out the source in one
    // atomic step; anything else has to stage.
    let published = if injected_errno(LegacyMigrationFault::RenameCrossDevice).is_some() {
        Err(std::io::Error::from_raw_os_error(EXDEV))
    } else {
        fs::rename(old, new)
    };
    match published {
        Ok(()) => {}
        Err(error) if is_cross_device(&error) => {
            record.mode = MigrationMode::Stage;
            write_record(home, &record)?;
            // Debris from an interrupted earlier attempt is overwritten, never
            // adopted.
            remove_any(&record.stage)?;
            stage_entry(old, &record.stage)
                .with_context(|| format!("staging {} for {}", old.display(), new.display()))?;
            // The staged tree is durable under its own name before publishing.
            sync_dir(&destination_parent)?;
            checkpoint(LegacyMigrationFault::AfterStage, &record.stage)?;
            fs::rename(&record.stage, new).with_context(|| {
                format!("publishing {} as {}", record.stage.display(), new.display())
            })?;
        }
        Err(error) => {
            return Err(anyhow::Error::new(error).context(format!(
                "failed to move {} to {}",
                old.display(),
                new.display()
            )));
        }
    }
    // The destination name is durable.
    sync_dir(&destination_parent)?;
    checkpoint(LegacyMigrationFault::AfterPublish, new)?;

    // Publication is recorded BEFORE the source is deleted, so a crash in the
    // window below can only roll forward.
    record.phase = MigrationPhase::Published;
    write_record(home, &record)?;
    checkpoint(LegacyMigrationFault::AfterPublishRecorded, new)?;

    // A rename already consumed the source; a staged publish has not.
    remove_any(old)?;
    sync_dir_if_present(&source_parent)?;
    checkpoint(LegacyMigrationFault::AfterSourceRemoved, old)?;

    clear_record(home)?;
    Ok(LegacyMove::Moved {
        old: old.to_path_buf(),
        new: new.to_path_buf(),
    })
}

/// Finish or roll back the transaction an earlier run left in flight.
///
/// Runs under the legacy source claim, before any fresh migration, so exactly
/// one process is ever deciding the fate of a pending record.
pub fn recover_legacy_migration(home: &Path) -> anyhow::Result<Vec<String>> {
    let Some(mut record) = read_record(home)? else {
        return Ok(Vec::new());
    };

    let destination_parent = record
        .destination
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));

    if record.phase == MigrationPhase::Prepared {
        // The destination can only exist here because THIS transaction
        // published it: the entry is only recorded once its destination has
        // been inspected and found absent.
        if inspect(
            &record.destination,
            LegacyMigrationFault::InspectDestination,
        )?
        .is_some()
        {
            record.phase = MigrationPhase::Published;
            write_record(home, &record)?;
        } else {
            // Nothing was published; the source is still the authority. Drop
            // the staged debris and let the ordinary pass migrate it again.
            remove_any(&record.stage)?;
            sync_dir_if_present(&destination_parent)?;
            clear_record(home)?;
            return Ok(vec![format!(
                "recovered: rolled back an unpublished migration of {} to {}",
                record.source.display(),
                record.destination.display()
            )]);
        }
    }

    // Published: the destination is the authority, so roll forward. This is
    // what stops a second, differently-rooted daemon from finding a stale
    // source next to a committed destination and migrating it again.
    remove_any(&record.stage)?;
    sync_dir_if_present(&destination_parent)?;
    remove_any(&record.source)?;
    let source_parent = record
        .source
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));
    sync_dir_if_present(&source_parent)?;
    clear_record(home)?;
    Ok(vec![format!(
        "recovered: {} -> {} (finished an interrupted migration)",
        record.source.display(),
        record.destination.display()
    )])
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

    // R34F2: finish whatever an interrupted earlier run left in flight before
    // starting anything new. Recovery runs under the same claim, so exactly
    // one process ever decides a pending record's fate.
    let mut moved = recover_legacy_migration(home)?;

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
        if let LegacyMove::Moved { old, new } = migrate_legacy_entry(home, &old, &new)? {
            moved.push(format!("{label}: {} -> {}", old.display(), new.display()));
        }
    }

    // Task 3: ~/.bro/ migration
    let old_bro = home.join(".bro");
    let new_bro = destinations.bro_home.clone();
    let old_bro_is_dir = inspect(&old_bro, LegacyMigrationFault::InspectBroSource)?
        .is_some_and(|metadata| metadata.is_dir());
    if old_bro_is_dir {
        // Collect first: each migration below removes its source, and reading
        // a directory while deleting from it has unspecified results.
        let mut entries = Vec::new();
        for entry in fs::read_dir(&old_bro)
            .with_context(|| format!("reading the legacy directory {}", old_bro.display()))?
        {
            let entry = entry
                .with_context(|| format!("reading the legacy directory {}", old_bro.display()))?;
            entries.push(entry.path());
        }
        for old_path in entries {
            let name = old_path
                .file_name()
                .ok_or_else(|| anyhow::anyhow!("invalid file name"))?;
            let new_path = new_bro.join(name);
            match migrate_legacy_entry(home, &old_path, &new_path)? {
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
        // If empty after migration, try to remove old dir. The PROBE refuses
        // on an inspection failure (R34F1); the removal itself stays
        // best-effort, because an empty legacy directory strands nothing and
        // the next startup simply finds it empty again.
        if directory_is_empty(&old_bro)?
            && let Err(error) = fs::remove_dir(&old_bro)
            && error.kind() != std::io::ErrorKind::NotFound
        {
            tracing::warn!(
                "could not remove the emptied legacy directory {}: {error}",
                old_bro.display()
            );
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
        // No faults, but hold the seam's lock so a concurrently-armed test in a
        // single-process `cargo test` run cannot leak a fault into this one.
        let _faults = arm_legacy_migration_faults(&[]);
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
        let _faults = arm_legacy_migration_faults(&[]);
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
    /// adopted.
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

        let _faults =
            arm_legacy_migration_faults(&[(LegacyMigrationFault::RenameCrossDevice, INJECTED_EIO)]);
        migrate_legacy_entry(&home, &old, &new).unwrap();
        assert_eq!(fs::read_to_string(&new).unwrap(), "legacy body");
        assert!(
            !temp.exists(),
            "the staging name does not survive the rename"
        );
        assert!(!old.exists(), "the source is removed only after the rename");
        assert!(
            !legacy_migration_journal_path(&home).exists(),
            "a completed transaction clears its journal"
        );
    }

    /// A legacy tree holding only the transcript index, so a fault lands on a
    /// DIRECTORY migration rather than on one of the JSON files ahead of it.
    fn write_legacy_index_only(home: &Path) {
        let old_shared = home.join(".claude-shared");
        let index = old_shared.join("transcript-index");
        fs::create_dir_all(index.join("segments")).unwrap();
        fs::write(index.join("meta.json"), "{\"generation\":7}").unwrap();
        fs::write(index.join("segments").join("0.store"), "segment bytes").unwrap();
    }

    fn assert_index_arrived(index: &Path) {
        assert_eq!(
            fs::read_to_string(index.join("meta.json")).unwrap(),
            "{\"generation\":7}"
        );
        assert_eq!(
            fs::read_to_string(index.join("segments").join("0.store")).unwrap(),
            "segment bytes"
        );
    }

    /// R34F2. The old fallback was file-only, so a legacy DIRECTORY could not
    /// cross a filesystem boundary at all: the EXDEV branch opened the
    /// directory as a file and the upgrade refused on every single boot. The
    /// staged path has to carry whole trees.
    #[test]
    fn a_cross_device_directory_migration_publishes_the_whole_tree() {
        let dir = tempdir().unwrap();
        let home = dir.path().canonicalize().unwrap();
        write_legacy_index_only(&home);
        let state = home.join("state");
        let destinations = fixture_destinations(&state, &home);

        let _faults =
            arm_legacy_migration_faults(&[(LegacyMigrationFault::RenameCrossDevice, INJECTED_EIO)]);
        let moved = migrate_legacy_defaults(&home, &destinations).unwrap();
        assert!(
            moved.iter().any(|line| line.starts_with("index:")),
            "the directory migrated: {moved:?}"
        );

        assert_index_arrived(&destinations.index_path);
        assert!(
            !home
                .join(".claude-shared")
                .join("transcript-index")
                .exists(),
            "the legacy tree is removed once the destination is durable"
        );
        assert!(
            !cross_device_temp_path(&destinations.index_path).exists(),
            "no staging debris survives"
        );
        assert!(!legacy_migration_journal_path(&home).exists());
    }

    /// R34F2. A crash BEFORE publication leaves the source authoritative. The
    /// next startup must roll the staged debris back and migrate again, not
    /// adopt a partial tree.
    #[test]
    fn an_interruption_before_publication_rolls_back_and_migrates_on_the_next_run() {
        let dir = tempdir().unwrap();
        let home = dir.path().canonicalize().unwrap();
        write_legacy_index_only(&home);
        let state = home.join("state");
        let destinations = fixture_destinations(&state, &home);
        let stage = cross_device_temp_path(&destinations.index_path);

        let faults = arm_legacy_migration_faults(&[
            (LegacyMigrationFault::RenameCrossDevice, INJECTED_EIO),
            (LegacyMigrationFault::AfterStage, INJECTED_EIO),
        ]);
        migrate_legacy_defaults(&home, &destinations)
            .expect_err("the interrupted migration must refuse");
        assert!(
            !destinations.index_path.exists(),
            "nothing was published, so the destination name does not exist"
        );
        assert!(stage.exists(), "the staged tree is the interruption debris");
        assert!(
            legacy_migration_journal_path(&home).exists(),
            "the pending transaction is recorded"
        );
        assert_index_arrived(&home.join(".claude-shared").join("transcript-index"));
        drop(faults);

        let _retry =
            arm_legacy_migration_faults(&[(LegacyMigrationFault::RenameCrossDevice, INJECTED_EIO)]);
        let moved = migrate_legacy_defaults(&home, &destinations).unwrap();
        assert!(
            moved.iter().any(|line| line.starts_with("recovered:")),
            "the interrupted transaction is identified: {moved:?}"
        );
        assert!(
            moved.iter().any(|line| line.starts_with("index:")),
            "and the migration completes: {moved:?}"
        );
        assert_index_arrived(&destinations.index_path);
        assert!(!stage.exists(), "the rolled-back debris is gone");
        assert!(
            !home
                .join(".claude-shared")
                .join("transcript-index")
                .exists()
        );
        assert!(!legacy_migration_journal_path(&home).exists());
    }

    /// R34F2. A crash AFTER the destination rename but before the source was
    /// removed used to leave both names, so a second, differently-rooted
    /// daemon could migrate the stale source a second time and break
    /// exactly-once destination selection. Recovery must roll FORWARD from a
    /// durable destination, whichever side of the phase record the crash
    /// landed on.
    #[test]
    fn an_interruption_after_publication_finishes_the_source_closeout() {
        for (point, source_survives) in [
            (LegacyMigrationFault::AfterPublish, true),
            (LegacyMigrationFault::AfterPublishRecorded, true),
            (LegacyMigrationFault::AfterSourceRemoved, false),
        ] {
            let dir = tempdir().unwrap();
            let home = dir.path().canonicalize().unwrap();
            write_legacy_index_only(&home);
            let destinations = fixture_destinations(&home.join("state"), &home);
            let legacy_index = home.join(".claude-shared").join("transcript-index");

            let faults = arm_legacy_migration_faults(&[
                (LegacyMigrationFault::RenameCrossDevice, INJECTED_EIO),
                (point, INJECTED_EIO),
            ]);
            assert!(
                migrate_legacy_defaults(&home, &destinations).is_err(),
                "the interruption at {point:?} must refuse"
            );
            assert_index_arrived(&destinations.index_path);
            assert_eq!(
                legacy_index.exists(),
                source_survives,
                "the source state at {point:?} is what the interruption implies"
            );
            assert!(
                legacy_migration_journal_path(&home).exists(),
                "the interrupted transaction at {point:?} is recorded"
            );
            drop(faults);

            // The next startup finds the record and rolls FORWARD: the
            // destination is already the authority.
            let moved = migrate_legacy_defaults(&home, &destinations).unwrap();
            assert!(
                moved.iter().any(|line| line.starts_with("recovered:")),
                "{point:?} is finished by recovery: {moved:?}"
            );
            assert!(
                !moved.iter().any(|line| line.starts_with("index:")),
                "{point:?} must not migrate a second time: {moved:?}"
            );
            assert_index_arrived(&destinations.index_path);
            assert!(
                !legacy_index.exists(),
                "{point:?} leaves no stale source behind"
            );
            assert!(
                !cross_device_temp_path(&destinations.index_path).exists(),
                "{point:?} leaves no staging debris"
            );
            assert!(
                !legacy_migration_journal_path(&home).exists(),
                "{point:?} clears the journal once the transaction is complete"
            );
        }
    }

    /// The second daemon in the R33F2 scenario, now crossing an interrupted
    /// transaction: a stale source next to a committed destination must be
    /// closed out by recovery, never migrated a second time into the other
    /// daemon's roots.
    #[test]
    fn a_second_daemon_closes_out_an_interrupted_migration_instead_of_repeating_it() {
        let dir = tempdir().unwrap();
        let home = dir.path().canonicalize().unwrap();
        write_legacy_index_only(&home);
        let first = fixture_destinations(&home.join("state-a"), &home);
        let second = fixture_destinations(&home.join("state-b"), &home);
        let legacy_index = home.join(".claude-shared").join("transcript-index");

        let faults = arm_legacy_migration_faults(&[
            (LegacyMigrationFault::RenameCrossDevice, INJECTED_EIO),
            (LegacyMigrationFault::AfterPublish, INJECTED_EIO),
        ]);
        migrate_legacy_defaults(&home, &first).expect_err("the interrupted migration must refuse");
        assert!(first.index_path.exists(), "the destination is published");
        assert!(legacy_index.exists(), "the source is not yet closed out");
        drop(faults);

        // A differently-rooted daemon starts next. It must finish the FIRST
        // daemon's transaction, not adopt the stale source.
        let moved = migrate_legacy_defaults(&home, &second).unwrap();
        assert!(
            moved.iter().any(|line| line.starts_with("recovered:")),
            "the interrupted transaction is finished: {moved:?}"
        );
        assert!(
            !moved.iter().any(|line| line.starts_with("index:")),
            "the stale source is never migrated a second time: {moved:?}"
        );
        assert_index_arrived(&first.index_path);
        assert!(!legacy_index.exists(), "the stale source is closed out");
        assert!(
            !second.index_path.exists(),
            "nothing landed in the second daemon's roots"
        );
        assert!(!legacy_migration_journal_path(&home).exists());
    }

    /// R34F1. `Path::exists()` collapses `EACCES` into `false`, so a legacy
    /// source the daemon merely could not read reported as already migrated.
    /// The migration must refuse instead, and it must refuse before it creates
    /// anything at the destination: a created destination is what makes the
    /// next startup take the destination-exists branch and strand the source
    /// permanently.
    #[test]
    fn an_unreadable_legacy_source_refuses_instead_of_reporting_it_absent() {
        let dir = tempdir().unwrap();
        let home = dir.path().canonicalize().unwrap();
        write_legacy_tree(&home);
        let state = home.join("state");
        let destinations = fixture_destinations(&state, &home);

        let _faults =
            arm_legacy_migration_faults(&[(LegacyMigrationFault::InspectSource, INJECTED_EACCES)]);
        let error = migrate_legacy_defaults(&home, &destinations)
            .expect_err("an unreadable legacy source must refuse the startup");
        assert!(
            format!("{error:#}").contains("InspectSource"),
            "the refusal names the inspection that failed: {error:#}"
        );

        assert!(
            !state.exists(),
            "the refusal must not create any destination"
        );
        assert!(
            home.join(".claude-shared")
                .join("blackbox-knowledge.json")
                .exists(),
            "the legacy source is untouched"
        );
    }

    /// R34F1. The destination probe fails open the same way: an `EIO` while
    /// inspecting the destination read as "the destination is not there" and
    /// the migration went on to rename onto a path it had not really
    /// inspected.
    #[test]
    fn an_unreadable_destination_refuses_before_creating_it() {
        let dir = tempdir().unwrap();
        let home = dir.path().canonicalize().unwrap();
        write_legacy_tree(&home);
        let state = home.join("state");
        let destinations = fixture_destinations(&state, &home);

        let _faults = arm_legacy_migration_faults(&[(
            LegacyMigrationFault::InspectDestination,
            INJECTED_EIO,
        )]);
        let error = migrate_legacy_defaults(&home, &destinations)
            .expect_err("an uninspectable destination must refuse the startup");
        assert!(
            format!("{error:#}").contains("InspectDestination"),
            "the refusal names the inspection that failed: {error:#}"
        );

        assert!(
            !state.exists(),
            "the refusal must not create any destination"
        );
        assert!(
            home.join(".claude-shared")
                .join("blackbox-knowledge.json")
                .exists()
        );
    }

    /// R34F1. `old_bro.is_dir()` was the same fail-open shape: an unreadable
    /// `~/.bro` reported as "not a directory" and the whole orchestration
    /// state was silently left behind.
    #[test]
    fn an_unreadable_bro_directory_refuses_instead_of_reporting_it_absent() {
        let dir = tempdir().unwrap();
        let home = dir.path().canonicalize().unwrap();
        write_legacy_tree(&home);
        let state = home.join("state");
        let destinations = fixture_destinations(&state, &home);

        let _faults = arm_legacy_migration_faults(&[(
            LegacyMigrationFault::InspectBroSource,
            INJECTED_EACCES,
        )]);
        let error = migrate_legacy_defaults(&home, &destinations)
            .expect_err("an unreadable legacy bro home must refuse the startup");
        assert!(
            format!("{error:#}").contains("InspectBroSource"),
            "the refusal names the inspection that failed: {error:#}"
        );
        assert!(
            home.join(".bro").join("tasks.json").exists(),
            "the legacy orchestration state is untouched"
        );
        assert!(
            !destinations.bro_home.exists(),
            "no bro destination was created"
        );
    }

    /// R34F1. The post-migration emptiness probe swallowed its errors too.
    #[test]
    fn a_failed_bro_emptiness_probe_refuses() {
        let dir = tempdir().unwrap();
        let home = dir.path().canonicalize().unwrap();
        write_legacy_tree(&home);
        let destinations = fixture_destinations(&home.join("state"), &home);

        let _faults =
            arm_legacy_migration_faults(&[(LegacyMigrationFault::ProbeBroEmptiness, INJECTED_EIO)]);
        let error = migrate_legacy_defaults(&home, &destinations)
            .expect_err("an unreadable legacy bro home must refuse the startup");
        assert!(
            format!("{error:#}").contains("ProbeBroEmptiness"),
            "the refusal names the probe that failed: {error:#}"
        );
        assert!(
            home.join(".bro").exists(),
            "the emptied legacy directory is left for the next startup"
        );
    }

    #[test]
    fn util_migrate_legacy_file_skips_destination_exists() {
        let _guard = test_env_lock();
        let _faults = arm_legacy_migration_faults(&[]);
        let dir = tempdir().unwrap();
        let home = dir.path();
        let old = home.join("old.txt");
        let new = home.join("new.txt");
        fs::write(&old, "old").unwrap();
        fs::write(&new, "new").unwrap();

        let res = migrate_legacy_entry(home, &old, &new).unwrap();
        assert!(matches!(res, LegacyMove::SkippedDestinationExists { .. }));
        assert_eq!(fs::read_to_string(&old).unwrap(), "old");
        assert_eq!(fs::read_to_string(&new).unwrap(), "new");
    }
}
