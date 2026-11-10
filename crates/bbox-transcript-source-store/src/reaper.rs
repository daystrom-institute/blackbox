//! Source-level reclamation. The staging phase needs only stream locks and
//! runs while readers hold the source lease; the generation phase needs the
//! exclusive reader lease and is skipped, never waited for, when a reader
//! holds it. Each run caches its measurement in the source health record so
//! diagnostics never walk the store.
use crate::lock::Acquire;
use crate::{
    CHUNKS_DIR, CURRENT_NAME, ChunkSources, HEALTH_NAME, READERS_LOCK_NAME, TEMP_PREFIX,
    TranscriptSourceStore, lock_contact,
};
use anyhow::Result;
use bbox_corpus_core::project_catalog::ConnectorSourceId;
use bbox_transcript_source::{PublishedSnapshot, validate_hash};
use serde::{Deserialize, Serialize};
use std::fs::{self, OpenOptions};
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

pub const HEALTH_SCHEMA_VERSION: u32 = 1;
/// Errors kept per scope and run; the rest are counted, not listed.
const MAX_RECORDED_ERRORS: usize = 8;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReaperPolicy {
    /// Unadmitted staging older than this is reclaimed.
    pub staging_expiry: Duration,
    /// Per-source staged chunk bytes the staging phase evicts down to.
    pub staging_budget_bytes: u64,
}

impl Default for ReaperPolicy {
    fn default() -> Self {
        Self {
            staging_expiry: Duration::from_secs(24 * 60 * 60),
            staging_budget_bytes: 2 * 1024 * 1024 * 1024,
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemovedCount {
    pub files: u64,
    pub bytes: u64,
}

impl RemovedCount {
    fn add(&mut self, bytes: u64) {
        self.files += 1;
        self.bytes += bytes;
    }
    fn merge(&mut self, other: &Self) {
        self.files += other.files;
        self.bytes += other.bytes;
    }
}

/// What one run removed, per category.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReapCounters {
    pub streams_visited: u64,
    /// Staged chunks whose bytes verify in the current materialization.
    pub absorbed_chunks: RemovedCount,
    /// Staged chunks older than the staging expiry.
    pub expired_chunks: RemovedCount,
    /// Expired temporary files inside stream directories.
    pub temporary_files: RemovedCount,
    /// Staged chunks evicted, oldest first, to meet the staging budget.
    pub budget_evicted_chunks: RemovedCount,
    /// Unpublished stream directories removed; `files` counts directories.
    pub unpublished_streams: RemovedCount,
    /// Non-current generation materializations.
    pub non_current_generations: RemovedCount,
    /// Expired scope-level temporaries of contact and health writes.
    pub contact_temporaries: RemovedCount,
}

impl ReapCounters {
    pub fn merge(&mut self, other: &Self) {
        self.streams_visited += other.streams_visited;
        self.absorbed_chunks.merge(&other.absorbed_chunks);
        self.expired_chunks.merge(&other.expired_chunks);
        self.temporary_files.merge(&other.temporary_files);
        self.budget_evicted_chunks
            .merge(&other.budget_evicted_chunks);
        self.unpublished_streams.merge(&other.unpublished_streams);
        self.non_current_generations
            .merge(&other.non_current_generations);
        self.contact_temporaries.merge(&other.contact_temporaries);
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GenerationPhase {
    Ran,
    LeaseHeld,
}

/// Bytes a source holds, measured after a run's removals.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct StorageAccounting {
    /// Sum of current generation byte lengths.
    pub live_bytes: u64,
    /// Every file under the source directory.
    pub stored_bytes: u64,
    pub staged_chunk_bytes: u64,
    pub staged_chunks: u64,
    pub non_current_generation_bytes: u64,
    pub non_current_generations: u64,
    pub temporary_bytes: u64,
    pub published_streams: u64,
    pub unpublished_streams: u64,
}

/// The cached result of the latest reaper attempt on one source.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceStorageHealth {
    pub schema_version: u32,
    pub last_attempt_at_unix: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_attempt_error: Option<String>,
    /// When `accounting` was measured; kept from an earlier run when the
    /// latest attempt could not measure.
    #[serde(default)]
    pub measured_at_unix: Option<u64>,
    #[serde(default)]
    pub accounting: Option<StorageAccounting>,
    /// Whether the measuring run's generation phase ran or found the reader
    /// lease held.
    #[serde(default)]
    pub generation_phase: Option<GenerationPhase>,
    #[serde(default)]
    pub last_counters: Option<ReapCounters>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScopeReapReport {
    pub connector_source_id: String,
    pub counters: ReapCounters,
    pub accounting: Option<StorageAccounting>,
    pub generation_phase: Option<GenerationPhase>,
    pub errors: Vec<String>,
    pub error_count: u64,
    /// The health record this run cached for the source.
    pub health: SourceStorageHealth,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ReapReport {
    pub scopes: Vec<ScopeReapReport>,
}

impl ReapReport {
    pub fn totals(&self) -> ReapCounters {
        let mut totals = ReapCounters::default();
        for scope in &self.scopes {
            totals.merge(&scope.counters);
        }
        totals
    }
    pub fn generation_phases(&self, phase: GenerationPhase) -> u64 {
        self.scopes
            .iter()
            .filter(|scope| scope.generation_phase == Some(phase))
            .count() as u64
    }
    pub fn error_count(&self) -> u64 {
        self.scopes.iter().map(|scope| scope.error_count).sum()
    }
}

struct StagedCandidate {
    modified: SystemTime,
    bytes: u64,
    stream: PathBuf,
    path: PathBuf,
}

struct ScopeSweep<'a> {
    policy: &'a ReaperPolicy,
    now: SystemTime,
    counters: ReapCounters,
    candidates: Vec<StagedCandidate>,
    errors: Vec<String>,
    error_count: u64,
}

impl ScopeSweep<'_> {
    fn expired(&self, modified: SystemTime) -> bool {
        self.now
            .duration_since(modified)
            .is_ok_and(|age| age >= self.policy.staging_expiry)
    }
    fn error(&mut self, error: anyhow::Error) {
        self.error_count += 1;
        if self.errors.len() < MAX_RECORDED_ERRORS {
            self.errors.push(format!("{error:#}"));
        }
    }
}

fn remove_counted(path: &Path, bytes: u64, bucket: &mut RemovedCount) -> std::io::Result<()> {
    match fs::remove_file(path) {
        Ok(()) => {
            bucket.add(bytes);
            Ok(())
        }
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

fn is_temporary(name: &str) -> bool {
    name.starts_with(TEMP_PREFIX)
}

fn unix_secs(time: SystemTime) -> u64 {
    time.duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs())
}

/// `current.json` presence, conservatively: an unreadable entry counts as
/// present so its directory is never treated as unpublished.
fn has_pointer(stream: &Path) -> bool {
    !matches!(
        fs::symlink_metadata(stream.join(CURRENT_NAME)),
        Err(error) if error.kind() == ErrorKind::NotFound
    )
}

/// The verified current pointer of a stream directory, if it parses and
/// names this stream.
fn read_pointer(stream: &Path) -> Option<PublishedSnapshot> {
    let name = stream.file_name()?.to_str()?;
    let row: PublishedSnapshot =
        serde_json::from_slice(&fs::read(stream.join(CURRENT_NAME)).ok()?).ok()?;
    (row.snapshot.stream_id == name
        && validate_hash(&row.generation).is_ok()
        && row.snapshot.generation().ok()? == row.generation)
        .then_some(row)
}

fn list_streams(scope_root: &Path) -> Result<Vec<PathBuf>> {
    let mut streams = Vec::new();
    for entry in fs::read_dir(scope_root)? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if validate_hash(&name).is_ok() && entry.file_type()?.is_dir() {
            streams.push(entry.path());
        }
    }
    streams.sort();
    Ok(streams)
}

pub(crate) fn read_health(scope_root: &Path) -> Result<Option<SourceStorageHealth>> {
    match fs::read(scope_root.join(HEALTH_NAME)) {
        Ok(bytes) => Ok(Some(serde_json::from_slice(&bytes)?)),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

impl TranscriptSourceStore {
    /// One reclamation run over every source directory under the store root,
    /// including sources that no longer have a producer grant. An absent root
    /// is an empty run. Failures are recorded per source and never stop the
    /// remaining sources.
    pub fn reap(&self, policy: &ReaperPolicy, now: SystemTime) -> Result<ReapReport> {
        let entries = match fs::read_dir(&self.root) {
            Ok(entries) => entries,
            Err(error) if error.kind() == ErrorKind::NotFound => return Ok(ReapReport::default()),
            Err(error) => return Err(error.into()),
        };
        let mut scopes = Vec::new();
        for entry in entries {
            let entry = entry?;
            let name = entry.file_name().to_string_lossy().into_owned();
            if ConnectorSourceId::parse(name.as_str()).is_ok() && entry.file_type()?.is_dir() {
                scopes.push(name);
            }
        }
        scopes.sort();
        Ok(ReapReport {
            scopes: scopes
                .into_iter()
                .map(|name| self.reap_scope(&self.root.join(&name), name, policy, now))
                .collect(),
        })
    }

    fn reap_scope(
        &self,
        scope_root: &Path,
        connector_source_id: String,
        policy: &ReaperPolicy,
        now: SystemTime,
    ) -> ScopeReapReport {
        let mut sweep = ScopeSweep {
            policy,
            now,
            counters: ReapCounters::default(),
            candidates: Vec::new(),
            errors: Vec::new(),
            error_count: 0,
        };
        let streams = list_streams(scope_root).unwrap_or_else(|error| {
            sweep.error(error);
            Vec::new()
        });
        for stream in &streams {
            sweep.counters.streams_visited += 1;
            if let Err(error) = self.sweep_stream_staging(stream, &mut sweep) {
                sweep.error(error);
            }
        }
        if let Err(error) = self.evict_over_budget(&mut sweep) {
            sweep.error(error);
        }
        let generation_phase = self
            .generation_phase(scope_root, &streams, &mut sweep)
            .map_err(|error| sweep.error(error))
            .ok();
        if let Err(error) = sweep_contact_temporaries(scope_root, &mut sweep) {
            sweep.error(error);
        }
        let accounting = measure(scope_root).map_err(|error| sweep.error(error)).ok();
        let previous = read_health(scope_root).ok().flatten();
        let measured = accounting.is_some();
        let health = SourceStorageHealth {
            schema_version: HEALTH_SCHEMA_VERSION,
            last_attempt_at_unix: unix_secs(now),
            last_attempt_error: sweep.errors.first().cloned(),
            measured_at_unix: if measured {
                Some(unix_secs(now))
            } else {
                previous.as_ref().and_then(|row| row.measured_at_unix)
            },
            accounting: accounting
                .clone()
                .or_else(|| previous.as_ref().and_then(|row| row.accounting.clone())),
            generation_phase: if measured {
                generation_phase
            } else {
                previous.as_ref().and_then(|row| row.generation_phase)
            },
            last_counters: Some(sweep.counters.clone()),
        };
        if let Err(error) = self.write_health(scope_root, &health) {
            sweep.error(error);
        }
        ScopeReapReport {
            connector_source_id,
            counters: sweep.counters,
            accounting,
            generation_phase,
            errors: sweep.errors,
            error_count: sweep.error_count,
            health,
        }
    }

    fn write_health(&self, scope_root: &Path, health: &SourceStorageHealth) -> Result<()> {
        let _lock = lock_contact(scope_root)?;
        self.atomic_write(&scope_root.join(HEALTH_NAME), &serde_json::to_vec(health)?)
    }

    /// Staging phase for one stream, under its verified lock.
    fn sweep_stream_staging(&self, stream: &Path, sweep: &mut ScopeSweep<'_>) -> Result<()> {
        let Some(lock) = self.acquire(stream, Acquire::Existing)? else {
            remove_expired_lockless(stream, sweep);
            return Ok(());
        };
        let published = has_pointer(stream);
        let current = published.then(|| read_pointer(stream)).flatten();
        let mut sources = ChunkSources::new(stream, current.as_ref());
        match fs::read_dir(stream.join(CHUNKS_DIR)) {
            Ok(entries) => {
                for entry in entries {
                    let entry = entry?;
                    let path = entry.path();
                    let metadata = fs::symlink_metadata(&path)?;
                    if !metadata.is_file() {
                        continue;
                    }
                    let name = entry.file_name().to_string_lossy().into_owned();
                    let modified = metadata.modified().unwrap_or(sweep.now);
                    let bytes = metadata.len();
                    if is_temporary(&name) {
                        if sweep.expired(modified) {
                            remove_counted(&path, bytes, &mut sweep.counters.temporary_files)?;
                        }
                        continue;
                    }
                    // Redundant regardless of age: the bytes verify in the
                    // current materialization.
                    if validate_hash(&name).is_ok() && sources.read_current(&name, bytes).is_some()
                    {
                        remove_counted(&path, bytes, &mut sweep.counters.absorbed_chunks)?;
                        continue;
                    }
                    if sweep.expired(modified) {
                        remove_counted(&path, bytes, &mut sweep.counters.expired_chunks)?;
                        continue;
                    }
                    sweep.candidates.push(StagedCandidate {
                        modified,
                        bytes,
                        stream: stream.to_path_buf(),
                        path,
                    });
                }
            }
            Err(error) if error.kind() == ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        drop(sources);
        for entry in fs::read_dir(stream)? {
            let entry = entry?;
            let name = entry.file_name().to_string_lossy().into_owned();
            let metadata = fs::symlink_metadata(entry.path())?;
            if metadata.is_file()
                && is_temporary(&name)
                && sweep.expired(metadata.modified().unwrap_or(sweep.now))
            {
                remove_counted(
                    &entry.path(),
                    metadata.len(),
                    &mut sweep.counters.temporary_files,
                )?;
            }
        }
        if !published && all_entries_expired(stream, sweep)? {
            let removed = lock.remove_stream_dir(&self.hooks)?;
            if removed.directory_removed {
                sweep.counters.unpublished_streams.files += 1;
            }
            sweep.counters.unpublished_streams.bytes += removed.bytes;
            sweep
                .candidates
                .retain(|candidate| candidate.stream != stream);
        }
        Ok(())
    }

    /// Evict the source's oldest staged chunks until it is within budget.
    /// Each eviction takes that stream's lock again and skips a chunk that
    /// changed since the sweep saw it.
    fn evict_over_budget(&self, sweep: &mut ScopeSweep<'_>) -> Result<()> {
        let mut staged: u64 = sweep
            .candidates
            .iter()
            .map(|candidate| candidate.bytes)
            .sum();
        if staged <= sweep.policy.staging_budget_bytes {
            return Ok(());
        }
        let mut candidates = std::mem::take(&mut sweep.candidates);
        candidates.sort_by(|a, b| a.modified.cmp(&b.modified).then(a.path.cmp(&b.path)));
        for candidate in candidates {
            if staged <= sweep.policy.staging_budget_bytes {
                break;
            }
            staged -= candidate.bytes;
            let Some(_lock) = self.acquire(&candidate.stream, Acquire::Existing)? else {
                continue;
            };
            match fs::symlink_metadata(&candidate.path) {
                Ok(metadata)
                    if metadata.len() == candidate.bytes
                        && metadata.modified().ok() == Some(candidate.modified) =>
                {
                    remove_counted(
                        &candidate.path,
                        candidate.bytes,
                        &mut sweep.counters.budget_evicted_chunks,
                    )?;
                }
                Ok(_) => {}
                Err(error) if error.kind() == ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
        }
        Ok(())
    }

    /// Generation phase: only with the exclusive reader lease, taken without
    /// waiting. Removes every materialization other than the current one;
    /// a stream whose pointer is unreadable keeps everything.
    fn generation_phase(
        &self,
        scope_root: &Path,
        streams: &[PathBuf],
        sweep: &mut ScopeSweep<'_>,
    ) -> Result<GenerationPhase> {
        let readers = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(scope_root.join(READERS_LOCK_NAME))?;
        if fs2::FileExt::try_lock_exclusive(&readers).is_err() {
            return Ok(GenerationPhase::LeaseHeld);
        }
        for stream in streams {
            let Some(_lock) = self.acquire(stream, Acquire::Existing)? else {
                continue;
            };
            let current = read_pointer(stream);
            if has_pointer(stream) && current.is_none() {
                continue;
            }
            let keep = current.map(|row| format!("{}.jsonl", row.generation));
            for entry in fs::read_dir(stream)? {
                let entry = entry?;
                let name = entry.file_name().to_string_lossy().into_owned();
                let metadata = fs::symlink_metadata(entry.path())?;
                if metadata.is_file()
                    && name.ends_with(".jsonl")
                    && keep.as_deref() != Some(name.as_str())
                {
                    remove_counted(
                        &entry.path(),
                        metadata.len(),
                        &mut sweep.counters.non_current_generations,
                    )?;
                }
            }
        }
        drop(readers);
        Ok(GenerationPhase::Ran)
    }
}

/// A crash between creating a stream directory and its `lock`, or between
/// unlinking `lock` and removing the directory, leaves a directory no lock
/// can be verified for. Nothing can hold it, so the only safe step is an
/// empty-only removal once the directory itself is older than the staging
/// expiry. A producer re-creating it races benignly: either its `lock` makes
/// the directory non-empty, or its open fails and it re-creates the
/// directory. Contents are never removed without a verified lock.
fn remove_expired_lockless(stream: &Path, sweep: &mut ScopeSweep<'_>) {
    let Ok(metadata) = fs::symlink_metadata(stream) else {
        return;
    };
    if metadata.is_dir()
        && !has_pointer(stream)
        && sweep.expired(metadata.modified().unwrap_or(sweep.now))
        && fs::remove_dir(stream).is_ok()
    {
        sweep.counters.unpublished_streams.files += 1;
    }
}

/// True when every file of an unpublished stream (its `lock`, staged chunks,
/// temporaries, orphaned materializations) is older than the staging expiry.
fn all_entries_expired(stream: &Path, sweep: &ScopeSweep<'_>) -> Result<bool> {
    for entry in fs::read_dir(stream)? {
        let entry = entry?;
        let metadata = fs::symlink_metadata(entry.path())?;
        if metadata.is_dir() {
            for inner in fs::read_dir(entry.path())? {
                let inner = fs::symlink_metadata(inner?.path())?;
                if !sweep.expired(inner.modified().unwrap_or(sweep.now)) {
                    return Ok(false);
                }
            }
        } else if !sweep.expired(metadata.modified().unwrap_or(sweep.now)) {
            return Ok(false);
        }
    }
    Ok(true)
}

/// Scope-level temporaries of interrupted `contact.json` and health writes,
/// removed under the lock those writes take. Named control files are kept.
fn sweep_contact_temporaries(scope_root: &Path, sweep: &mut ScopeSweep<'_>) -> Result<()> {
    let _lock = lock_contact(scope_root)?;
    for entry in fs::read_dir(scope_root)? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        let metadata = fs::symlink_metadata(entry.path())?;
        if metadata.is_file()
            && is_temporary(&name)
            && sweep.expired(metadata.modified().unwrap_or(sweep.now))
        {
            remove_counted(
                &entry.path(),
                metadata.len(),
                &mut sweep.counters.contact_temporaries,
            )?;
        }
    }
    Ok(())
}

/// Measure a source by stat alone, after the run's removals.
fn measure(scope_root: &Path) -> Result<StorageAccounting> {
    let mut accounting = StorageAccounting::default();
    for entry in fs::read_dir(scope_root)? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        let metadata = match fs::symlink_metadata(entry.path()) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == ErrorKind::NotFound => continue,
            Err(error) => return Err(error.into()),
        };
        if metadata.is_file() {
            accounting.stored_bytes += metadata.len();
            if is_temporary(&name) {
                accounting.temporary_bytes += metadata.len();
            }
        } else if metadata.is_dir() && validate_hash(&name).is_ok() {
            measure_stream(&entry.path(), &mut accounting)?;
        }
    }
    Ok(accounting)
}

fn measure_stream(stream: &Path, accounting: &mut StorageAccounting) -> Result<()> {
    let current = read_pointer(stream);
    if has_pointer(stream) {
        accounting.published_streams += 1;
    } else {
        accounting.unpublished_streams += 1;
    }
    let keep = current.as_ref().map(|row| {
        accounting.live_bytes += row.snapshot.byte_length;
        format!("{}.jsonl", row.generation)
    });
    let entries = match fs::read_dir(stream) {
        Ok(entries) => entries,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    for entry in entries {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        let Ok(metadata) = fs::symlink_metadata(entry.path()) else {
            continue;
        };
        if metadata.is_dir() {
            if name == CHUNKS_DIR {
                for inner in fs::read_dir(entry.path())?.flatten() {
                    let inner_name = inner.file_name().to_string_lossy().into_owned();
                    let Ok(inner_metadata) = fs::symlink_metadata(inner.path()) else {
                        continue;
                    };
                    accounting.stored_bytes += inner_metadata.len();
                    if is_temporary(&inner_name) {
                        accounting.temporary_bytes += inner_metadata.len();
                    } else {
                        accounting.staged_chunk_bytes += inner_metadata.len();
                        accounting.staged_chunks += 1;
                    }
                }
            }
            continue;
        }
        accounting.stored_bytes += metadata.len();
        if is_temporary(&name) {
            accounting.temporary_bytes += metadata.len();
        } else if name.ends_with(".jsonl") && keep.as_deref() != Some(name.as_str()) {
            accounting.non_current_generation_bytes += metadata.len();
            accounting.non_current_generations += 1;
        }
    }
    Ok(())
}
