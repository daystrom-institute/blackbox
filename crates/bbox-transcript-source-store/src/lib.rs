//! Daemon-owned native transcript snapshots. Receipts follow fsync and atomic
//! pointer replacement; a process restart never turns an admitted write into
//! an acknowledged but missing source. Blob namespace is source AND stream.
//!
//! Chunk files are staging: publication assembles a generation from staged
//! chunks or from verified ranges of the current materialization and then
//! drops the staged chunks it absorbed, so settled content is stored once.
//! The reaper ([`TranscriptSourceStore::reap`]) bounds what remains.
//
// Every operation here is blocking filesystem work by design. Callers run it
// on the blocking pool (ingest routes, the reaper task, doctor) or on the
// index writer thread, never on an async worker, so the workspace's
// blocking-I/O lint does not apply inside this crate.
#![allow(clippy::disallowed_methods)]
use anyhow::{Result, anyhow, ensure};
use bbox_corpus_core::project_catalog::ConnectorScope;
use bbox_transcript_source::*;
use sha2::{Digest, Sha256};
use std::collections::{BTreeSet, HashMap};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

mod admission;
mod lock;
mod reaper;

pub use admission::{
    DEFAULT_RESERVE_FLOOR_BYTES, DEFAULT_RESERVE_PERCENT, FsSpace, METADATA_ALLOWANCE, Reservation,
    ReservePolicy, SpaceProbe, StorageAdmission, StorageExhausted, is_storage_exhausted,
    nearest_existing_ancestor, normalize_storage_error, probe_filesystem,
};
pub use reaper::{
    GenerationPhase, HEALTH_SCHEMA_VERSION, ReapCounters, ReapReport, ReaperPolicy, RemovedCount,
    ScopeReapReport, SourceStorageHealth, StorageAccounting,
};

use lock::{Acquire, StreamLock};

const CURRENT_NAME: &str = "current.json";
const CHUNKS_DIR: &str = "chunks";
const HEALTH_NAME: &str = "health.json";
const CONTACT_LOCK_NAME: &str = "contact.lock";
const READERS_LOCK_NAME: &str = "readers.lock";
/// Prefix of the temporary files `tempfile` creates for atomic writes.
const TEMP_PREFIX: &str = ".tmp";

#[derive(Clone)]
pub struct TranscriptSourceStore {
    root: PathBuf,
    admission: Option<Arc<StorageAdmission>>,
    hooks: Arc<Hooks>,
}
impl TranscriptSourceStore {
    pub fn open(root: impl AsRef<Path>) -> Result<Self> {
        fs::create_dir_all(root.as_ref()).map_err(|error| normalize_storage_error(error.into()))?;
        Ok(Self::for_read(root))
    }
    /// Bind an explicitly configured daemon store root without creating it.
    /// Subsequent reads report missing/corrupt state through their own result.
    pub fn for_read(root: impl AsRef<Path>) -> Self {
        Self {
            root: root.as_ref().to_owned(),
            admission: None,
            hooks: Arc::default(),
        }
    }
    /// Admit chunk installs and publications against the daemon's shared
    /// reservation ledger for this root. Without a ledger nothing is refused
    /// for headroom; exhaustion still surfaces as [`StorageExhausted`].
    pub fn with_admission(mut self, admission: Arc<StorageAdmission>) -> Self {
        self.admission = Some(admission);
        self
    }
    pub fn root(&self) -> &Path {
        &self.root
    }
    fn scope_root(&self, scope: &ConnectorScope) -> PathBuf {
        self.root.join(scope.connector_source_id().as_str())
    }
    fn stream_root(&self, scope: &ConnectorScope, stream: &str) -> Result<PathBuf> {
        validate_hash(stream)?;
        Ok(self.scope_root(scope).join(stream))
    }
    fn acquire(&self, dir: &Path, mode: Acquire) -> Result<Option<StreamLock>> {
        lock::acquire(dir, mode, &self.hooks)
    }
    fn acquire_creating(&self, dir: &Path) -> Result<StreamLock> {
        self.acquire(dir, Acquire::Creating)?
            .ok_or_else(|| anyhow!("creating stream lock acquisition returned no lock"))
    }
    fn admit(&self, bytes: u64) -> Result<Option<Reservation>> {
        self.admission
            .as_ref()
            .map(|admission| admission.reserve(bytes))
            .transpose()
    }
    /// Pins every generation discovered under a source until the lease drops.
    /// Publication may proceed, but cleanup cannot remove a reader's snapshot.
    pub fn reader_lease(&self, scope: &ConnectorScope) -> Result<File> {
        let root = self.scope_root(scope);
        fs::create_dir_all(&root)?;
        let lease = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(root.join(READERS_LOCK_NAME))?;
        fs2::FileExt::lock_shared(&lease)?;
        Ok(lease)
    }

    /// Reads persisted producer evidence; a consumer read never renews it.
    pub fn source_contact(&self, scope: &ConnectorScope) -> Result<Option<SourceContact>> {
        match fs::read(self.scope_root(scope).join("contact.json")) {
            Ok(bytes) => Ok(Some(serde_json::from_slice(&bytes)?)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error.into()),
        }
    }
    pub fn record_contact(&self, request: &ContactRequest, now: &str) -> Result<SourceContact> {
        self.record_contact_inner(request, now)
            .map_err(normalize_storage_error)
    }
    fn record_contact_inner(&self, request: &ContactRequest, now: &str) -> Result<SourceContact> {
        validate_hash(&request.scan_id)?;
        let root = self.scope_root(&request.scope);
        fs::create_dir_all(&root)?;
        let _lock = lock_contact(&root)?;
        let previous = self.source_contact(&request.scope)?;
        let contact = if let Some(summary) = &request.completed {
            let mut previous =
                previous.ok_or_else(|| anyhow::anyhow!("transcript_scan_conflict"))?;
            ensure!(
                previous.scan_id == request.scan_id,
                "transcript_scan_conflict"
            );
            previous.last_contact_at = now.into();
            previous.scan_in_progress = false;
            previous.last_completed_scan_at = Some(now.into());
            previous.last_completed_scan = Some(summary.clone());
            previous
        } else {
            SourceContact {
                last_contact_at: now.into(),
                scan_id: request.scan_id.clone(),
                scan_started_at: now.into(),
                scan_in_progress: true,
                last_completed_scan_at: previous
                    .as_ref()
                    .and_then(|p| p.last_completed_scan_at.clone()),
                last_completed_scan: previous.and_then(|p| p.last_completed_scan),
            }
        };
        self.atomic_write(&root.join("contact.json"), &serde_json::to_vec(&contact)?)?;
        Ok(contact)
    }

    /// The reaper's cached storage measurement for a source. Reading it never
    /// walks the store.
    pub fn source_storage_health(
        &self,
        scope: &ConnectorScope,
    ) -> Result<Option<SourceStorageHealth>> {
        reaper::read_health(&self.scope_root(scope))
    }

    pub fn current(
        &self,
        scope: &ConnectorScope,
        stream: &str,
    ) -> Result<Option<PublishedSnapshot>> {
        let path = self.stream_root(scope, stream)?.join(CURRENT_NAME);
        match fs::read(path) {
            Ok(bytes) => {
                let row: PublishedSnapshot = serde_json::from_slice(&bytes)?;
                ensure!(
                    &row.snapshot.scope == scope && row.snapshot.stream_id == stream,
                    "transcript scope identity mismatch"
                );
                ensure!(
                    row.snapshot.generation()? == row.generation,
                    "transcript generation digest mismatch"
                );
                Ok(Some(row))
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error.into()),
        }
    }
    pub fn status(&self, scope: &ConnectorScope, stream: &str) -> Result<StreamStatus> {
        let row = self.current(scope, stream)?;
        Ok(StreamStatus {
            stream_id: stream.into(),
            generation: row.as_ref().map(|r| r.generation.clone()),
            byte_length: row.as_ref().map_or(0, |r| r.snapshot.byte_length),
            published_at: row.map(|r| r.published_at),
        })
    }
    pub fn install_chunk(
        &self,
        scope: &ConnectorScope,
        stream: &str,
        hash: &str,
        bytes: &[u8],
    ) -> Result<()> {
        self.install_chunk_inner(scope, stream, hash, bytes)
            .map_err(normalize_storage_error)
    }
    fn install_chunk_inner(
        &self,
        scope: &ConnectorScope,
        stream: &str,
        hash: &str,
        bytes: &[u8],
    ) -> Result<()> {
        validate_hash(hash)?;
        ensure!(
            !bytes.is_empty() && bytes.len() <= CHUNK_BYTES && sha256(bytes) == hash,
            "transcript chunk hash or size mismatch"
        );
        let stream_root = self.stream_root(scope, stream)?;
        let root = stream_root.join(CHUNKS_DIR);
        // The temporary file always costs metadata; a new stream directory
        // and a new chunks directory each add one more allowance.
        let new_directories = u64::from(!stream_root.is_dir()) + u64::from(!root.is_dir());
        let _reservation =
            self.admit(bytes.len() as u64 + METADATA_ALLOWANCE * (1 + new_directories))?;
        let _lock = self.acquire_creating(&stream_root)?;
        fs::create_dir_all(&root)?;
        let path = root.join(hash);
        if !chunk_matches(&path, hash, bytes.len() as u64) {
            self.atomic_write(&path, bytes)?;
        }
        Ok(())
    }
    /// Which offered chunk hashes the store cannot supply from verified bytes:
    /// a staged chunk that verifies, or a verified range of the current
    /// materialization. An observation, not a reservation; never creates the
    /// stream.
    pub fn missing_chunks(&self, snapshot: &StreamSnapshot) -> Result<Vec<String>> {
        snapshot.validate()?;
        let root = self.stream_root(&snapshot.scope, &snapshot.stream_id)?;
        let mut offered = BTreeSet::new();
        let unique = snapshot
            .chunks
            .iter()
            .filter(|chunk| offered.insert(chunk.sha256.as_str()))
            .collect::<Vec<_>>();
        let Some(_lock) = self.acquire(&root, Acquire::Existing)? else {
            return Ok(offered.into_iter().map(str::to_owned).collect());
        };
        // An unreadable pointer only withholds range presence; publish
        // reports the pointer error itself.
        let current = self
            .current(&snapshot.scope, &snapshot.stream_id)
            .ok()
            .flatten();
        let mut sources = ChunkSources::new(&root, current.as_ref());
        Ok(unique
            .into_iter()
            .filter(|chunk| !sources.present(&chunk.sha256, chunk.size))
            .map(|chunk| chunk.sha256.clone())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect())
    }
    pub fn data_path(&self, row: &PublishedSnapshot) -> Result<PathBuf> {
        validate_hash(&row.generation)?;
        Ok(self
            .stream_root(&row.snapshot.scope, &row.snapshot.stream_id)?
            .join(format!("{}.jsonl", row.generation)))
    }
    pub fn publish(&self, request: &PublishRequest, now: &str) -> Result<PublishReceipt> {
        self.publish_inner(request, now)
            .map_err(normalize_storage_error)
    }
    fn publish_inner(&self, request: &PublishRequest, now: &str) -> Result<PublishReceipt> {
        let snapshot = &request.snapshot;
        let generation = snapshot.generation()?;
        let root = self.stream_root(&snapshot.scope, &snapshot.stream_id)?;
        let receipt = || PublishReceipt {
            generation: generation.clone(),
            locator: snapshot.locator(&generation),
            byte_length: snapshot.byte_length,
            durable: true,
        };
        let is_current = |current: &Option<PublishedSnapshot>| {
            current
                .as_ref()
                .is_some_and(|row| row.generation == generation)
        };
        // An already-current generation answers before admission, so an
        // idempotent retry needs no headroom.
        let mut held = self.acquire(&root, Acquire::Existing)?;
        let mut current = None;
        if held.is_some() {
            current = self.current(&snapshot.scope, &snapshot.stream_id)?;
            if is_current(&current) {
                return Ok(receipt());
            }
        }
        // The new materialization is written before the old one can go.
        let new_directory = u64::from(held.is_none());
        let _reservation =
            self.admit(snapshot.byte_length + METADATA_ALLOWANCE * (1 + new_directory))?;
        if held.is_none() {
            held = Some(self.acquire_creating(&root)?);
            current = self.current(&snapshot.scope, &snapshot.stream_id)?;
            if is_current(&current) {
                return Ok(receipt());
            }
        }
        let _lock = held;
        ensure!(
            current.as_ref().map(|row| &row.generation) == request.expected_generation.as_ref(),
            "error.transcript_generation_conflict: reread the stream status and retry from current source bytes"
        );
        let mut sources = ChunkSources::new(&root, current.as_ref());
        let mut staged = tempfile::NamedTempFile::new_in(&root)?;
        let mut digest = Sha256::new();
        let mut last = None;
        for chunk in &snapshot.chunks {
            let bytes = sources.read(&chunk.sha256, chunk.size).ok_or_else(|| {
                anyhow!(
                    "missing transcript chunk {}: ask missing again and upload it",
                    chunk.sha256
                )
            })?;
            digest.update(&bytes);
            staged.write_all(&bytes)?;
            last = bytes.last().copied();
        }
        drop(sources);
        ensure!(
            snapshot.byte_length == 0 || last == Some(b'\n'),
            "transcript snapshot has an incomplete final line"
        );
        ensure!(
            format!("{:x}", digest.finalize()) == snapshot.content_sha256,
            "transcript content digest mismatch"
        );
        staged.as_file().sync_all()?;
        let published = PublishedSnapshot {
            snapshot: snapshot.clone(),
            generation: generation.clone(),
            published_at: now.into(),
        };
        self.persist(staged, &self.data_path(&published)?)?;
        File::open(&root)?.sync_all()?;
        // The current pointer is the admission boundary, installed last.
        self.atomic_write(&root.join(CURRENT_NAME), &serde_json::to_vec(&published)?)?;
        // Still holding the stream lock: the admitted snapshot absorbed these
        // staged chunks. Nothing else in chunks/ is ours to remove, since it
        // may belong to another producer's in-flight upload. Best effort: a
        // leftover is redundant and the reaper removes it.
        if !self.hooks.skip_absorb() {
            let mut absorbed = BTreeSet::new();
            for chunk in &snapshot.chunks {
                if absorbed.insert(chunk.sha256.as_str()) {
                    match fs::remove_file(root.join(CHUNKS_DIR).join(&chunk.sha256)) {
                        Ok(()) => {}
                        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                        Err(error) => tracing::warn!(
                            %error,
                            "native transcript publish left an absorbed chunk for the reaper"
                        ),
                    }
                }
            }
        }
        // Cleanup is best effort after durable admission. One source-wide
        // shared lease pins an index pass without opening every session file.
        // If a reader is active, defer cleanup rather than block publication.
        let cleanup_lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(self.scope_root(&snapshot.scope).join(READERS_LOCK_NAME));
        if let Ok(cleanup_lock) = cleanup_lock {
            if fs2::FileExt::try_lock_exclusive(&cleanup_lock).is_ok() {
                if let Ok(entries) = fs::read_dir(&root) {
                    for entry in entries.flatten() {
                        let path = entry.path();
                        if path.extension().is_some_and(|ext| ext == "jsonl")
                            && self.data_path(&published).ok().as_ref() != Some(&path)
                            && current
                                .as_ref()
                                .is_none_or(|old| self.data_path(old).ok().as_ref() != Some(&path))
                        {
                            let _ = fs::remove_file(path);
                        }
                    }
                }
            }
        }
        Ok(receipt())
    }
    pub fn snapshots(&self, scope: &ConnectorScope) -> Result<Vec<PublishedSnapshot>> {
        let entries = match fs::read_dir(self.scope_root(scope)) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(error.into()),
        };
        let mut rows = Vec::new();
        for entry in entries {
            let entry = entry?;
            let name = entry.file_name().to_string_lossy().into_owned();
            if validate_hash(&name).is_ok() && entry.file_type()?.is_dir() {
                if let Some(row) = self.current(scope, &name)? {
                    rows.push(row);
                }
            }
        }
        rows.sort_by(|a, b| a.snapshot.stream_id.cmp(&b.snapshot.stream_id));
        Ok(rows)
    }

    fn atomic_write(&self, path: &Path, bytes: &[u8]) -> Result<()> {
        let parent = path
            .parent()
            .ok_or_else(|| anyhow::anyhow!("missing store parent"))?;
        let mut file = tempfile::NamedTempFile::new_in(parent)?;
        file.write_all(bytes)?;
        file.as_file().sync_all()?;
        self.persist(file, path)?;
        File::open(parent)?.sync_all()?;
        Ok(())
    }
    fn persist(&self, file: tempfile::NamedTempFile, path: &Path) -> Result<()> {
        if let Some(kind) = self.hooks.injected_write_error() {
            return Err(tempfile::PersistError {
                error: std::io::Error::from(kind),
                file,
            }
            .into());
        }
        file.persist(path)?;
        Ok(())
    }
}

fn lock_contact(scope_root: &Path) -> Result<File> {
    let lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(scope_root.join(CONTACT_LOCK_NAME))?;
    fs2::FileExt::lock_exclusive(&lock)?;
    Ok(lock)
}

/// Where a chunk's bytes can come from: a staged chunk file, then the
/// matching range of the current materialization (offset is the sum of the
/// preceding chunk sizes). Every byte returned has been size- and
/// hash-verified.
pub(crate) struct ChunkSources {
    chunks: PathBuf,
    current: Option<File>,
    ranges: HashMap<(String, u64), u64>,
}

impl ChunkSources {
    pub(crate) fn new(stream_root: &Path, current: Option<&PublishedSnapshot>) -> Self {
        let mut ranges = HashMap::new();
        let mut offset = 0;
        for chunk in current.map_or(&[][..], |row| row.snapshot.chunks.as_slice()) {
            ranges
                .entry((chunk.sha256.clone(), chunk.size))
                .or_insert(offset);
            offset += chunk.size;
        }
        let current = current.and_then(|row| {
            validate_hash(&row.generation).ok()?;
            File::open(stream_root.join(format!("{}.jsonl", row.generation))).ok()
        });
        Self {
            chunks: stream_root.join(CHUNKS_DIR),
            current,
            ranges,
        }
    }
    /// Staged bytes first: a damaged current range never shadows a repair.
    pub(crate) fn read(&mut self, hash: &str, size: u64) -> Option<Vec<u8>> {
        read_verified_chunk(&self.chunks.join(hash), hash, size)
            .or_else(|| self.read_current(hash, size))
    }
    pub(crate) fn present(&mut self, hash: &str, size: u64) -> bool {
        chunk_matches(&self.chunks.join(hash), hash, size)
            || self.read_current(hash, size).is_some()
    }
    pub(crate) fn read_current(&mut self, hash: &str, size: u64) -> Option<Vec<u8>> {
        if size == 0 || size > CHUNK_BYTES as u64 {
            return None;
        }
        let offset = *self.ranges.get(&(hash.to_owned(), size))?;
        let file = self.current.as_mut()?;
        file.seek(SeekFrom::Start(offset)).ok()?;
        let mut bytes = vec![0; size as usize];
        file.read_exact(&mut bytes).ok()?;
        (sha256(&bytes) == hash).then_some(bytes)
    }
}

fn read_verified_chunk(path: &Path, hash: &str, size: u64) -> Option<Vec<u8>> {
    let file = File::open(path).ok()?;
    if size > CHUNK_BYTES as u64 || file.metadata().ok()?.len() != size {
        return None;
    }
    let mut bytes = Vec::with_capacity(size as usize);
    file.take(CHUNK_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .ok()?;
    (bytes.len() as u64 == size && sha256(&bytes) == hash).then_some(bytes)
}

fn chunk_matches(path: &Path, hash: &str, size: u64) -> bool {
    read_verified_chunk(path, hash, size).is_some()
}

/// Test seams for interleavings that are otherwise timing-dependent. Every
/// method is a no-op outside tests.
#[derive(Default)]
pub(crate) struct Hooks {
    #[cfg(test)]
    pub(crate) after_lock_open: std::sync::Mutex<Option<Box<dyn FnOnce() + Send>>>,
    #[cfg(test)]
    pub(crate) after_lock_unlink: std::sync::Mutex<Option<Box<dyn FnOnce() + Send>>>,
    #[cfg(test)]
    pub(crate) skip_absorb: std::sync::atomic::AtomicBool,
    #[cfg(test)]
    pub(crate) write_error: std::sync::Mutex<Option<std::io::ErrorKind>>,
}

impl Hooks {
    pub(crate) fn after_lock_open(&self) {
        #[cfg(test)]
        if let Some(hook) = self.after_lock_open.lock().unwrap().take() {
            hook();
        }
    }
    pub(crate) fn after_lock_unlink(&self) {
        #[cfg(test)]
        if let Some(hook) = self.after_lock_unlink.lock().unwrap().take() {
            hook();
        }
    }
    fn skip_absorb(&self) -> bool {
        #[cfg(test)]
        return self.skip_absorb.load(std::sync::atomic::Ordering::SeqCst);
        #[cfg(not(test))]
        false
    }
    fn injected_write_error(&self) -> Option<std::io::ErrorKind> {
        #[cfg(test)]
        return *self.write_error.lock().unwrap();
        #[cfg(not(test))]
        None
    }
}

#[cfg(test)]
mod retention_tests;

#[cfg(test)]
mod tests {
    use super::*;
    use bbox_corpus_core::project_catalog::{ConnectorKind, ConnectorSourceId};
    fn scope() -> ConnectorScope {
        ConnectorScope::new(
            ConnectorSourceId::parse("csrc_0123456789abcdef").unwrap(),
            ConnectorKind::parse("native_transcript").unwrap(),
        )
    }
    fn snapshot(bytes: &[u8]) -> StreamSnapshot {
        StreamSnapshot {
            schema_version: SCHEMA_VERSION,
            scope: scope(),
            stream_id: sha256(b"claude/default/session"),
            source: NativeSource::Claude,
            account: "default".into(),
            session_id: "session-fixture".into(),
            is_subagent: false,
            content_sha256: sha256(bytes),
            byte_length: bytes.len() as u64,
            chunks: bytes
                .chunks(CHUNK_BYTES)
                .map(|chunk| ChunkRef {
                    sha256: sha256(chunk),
                    size: chunk.len() as u64,
                })
                .collect(),
        }
    }
    fn stage(store: &TranscriptSourceStore, bytes: &[u8]) -> StreamSnapshot {
        let snapshot = snapshot(bytes);
        for (chunk, bytes) in snapshot.chunks.iter().zip(bytes.chunks(CHUNK_BYTES)) {
            store
                .install_chunk(&scope(), &snapshot.stream_id, &chunk.sha256, bytes)
                .unwrap();
        }
        snapshot
    }
    #[test]
    fn durable_snapshot_replays_and_rewrites_without_append_cursor_ambiguity() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let store = TranscriptSourceStore::open(&root).unwrap();
        let first = stage(&store, b"first\n");
        let request = PublishRequest {
            snapshot: first.clone(),
            expected_generation: None,
        };
        let receipt = store.publish(&request, "2026-09-01T00:00:00Z").unwrap();
        assert!(receipt.durable);
        let reopened = TranscriptSourceStore::open(&root).unwrap();
        assert_eq!(
            reopened.publish(&request, "later").unwrap().generation,
            receipt.generation
        );
        for bytes in [b"other\n".as_slice(), b"x\n".as_slice()] {
            let previous = reopened
                .current(&scope(), &first.stream_id)
                .unwrap()
                .unwrap();
            let updated = stage(&reopened, bytes);
            assert!(
                reopened
                    .publish(
                        &PublishRequest {
                            snapshot: updated.clone(),
                            expected_generation: None
                        },
                        "later"
                    )
                    .is_err()
            );
            reopened
                .publish(
                    &PublishRequest {
                        snapshot: updated,
                        expected_generation: Some(previous.generation),
                    },
                    "later",
                )
                .unwrap();
            let current = reopened
                .current(&scope(), &first.stream_id)
                .unwrap()
                .unwrap();
            assert_eq!(
                fs::read(reopened.data_path(&current).unwrap()).unwrap(),
                bytes
            );
        }
        assert!(reopened.publish(&request, "stale replay").is_err());
    }
    #[test]
    fn incomplete_or_tampered_snapshots_never_advance_the_published_pointer() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let store = TranscriptSourceStore::open(root).unwrap();
        let torn = stage(&store, b"unfinished");
        assert!(
            store
                .publish(
                    &PublishRequest {
                        snapshot: torn.clone(),
                        expected_generation: None
                    },
                    "now"
                )
                .is_err()
        );
        assert!(store.current(&scope(), &torn.stream_id).unwrap().is_none());
        assert!(
            store
                .install_chunk(
                    &scope(),
                    &torn.stream_id,
                    &sha256(b"different"),
                    b"unfinished"
                )
                .is_err()
        );
        let mut complete = stage(&store, b"complete\n");
        complete.content_sha256 = sha256(b"wrong\n");
        assert!(
            store
                .publish(
                    &PublishRequest {
                        snapshot: complete,
                        expected_generation: None
                    },
                    "now"
                )
                .is_err()
        );
        assert!(store.current(&scope(), &torn.stream_id).unwrap().is_none());
    }
    #[test]
    fn contact_records_distinguish_live_scan_from_last_completed_walk_and_reject_stale_completion()
    {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let store = TranscriptSourceStore::open(&root).unwrap();
        let mut request = ContactRequest {
            scope: scope(),
            scan_id: sha256(b"scan one"),
            completed: None,
        };
        assert!(store.source_contact(&scope()).unwrap().is_none());
        let started = store.record_contact(&request, "start").unwrap();
        assert!(started.scan_in_progress);
        assert!(started.last_completed_scan_at.is_none());
        request.completed = Some(ScanSummary {
            discovered: 2,
            published: 1,
            failed: 1,
            ..Default::default()
        });
        store.record_contact(&request, "complete").unwrap();
        let next = ContactRequest {
            scope: scope(),
            scan_id: sha256(b"scan two"),
            completed: None,
        };
        store.record_contact(&next, "later").unwrap();
        assert!(store.record_contact(&request, "stale").is_err());
        let reopened = TranscriptSourceStore::open(&root).unwrap();
        let contact = reopened.source_contact(&scope()).unwrap().unwrap();
        assert_eq!(contact.last_contact_at, "later");
        assert_eq!(contact.last_completed_scan_at.as_deref(), Some("complete"));
        assert_eq!(contact.last_completed_scan.unwrap().failed, 1);
        assert!(contact.scan_in_progress);
        assert_eq!(
            reopened
                .source_contact(&scope())
                .unwrap()
                .unwrap()
                .last_contact_at,
            "later"
        );
    }

    #[test]
    fn source_reader_lease_pins_a_generation_across_multiple_publications() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let store = TranscriptSourceStore::open(root).unwrap();
        let initial = stage(&store, b"first\n");
        store
            .publish(
                &PublishRequest {
                    snapshot: initial.clone(),
                    expected_generation: None,
                },
                "now",
            )
            .unwrap();
        let lease = store.reader_lease(&scope()).unwrap();
        let pinned = store
            .current(&scope(), &initial.stream_id)
            .unwrap()
            .unwrap();
        for bytes in [
            b"second\n".as_slice(),
            b"third\n".as_slice(),
            b"fourth\n".as_slice(),
        ] {
            let current = store
                .current(&scope(), &initial.stream_id)
                .unwrap()
                .unwrap();
            let next = stage(&store, bytes);
            store
                .publish(
                    &PublishRequest {
                        snapshot: next,
                        expected_generation: Some(current.generation),
                    },
                    "later",
                )
                .unwrap();
        }
        assert_eq!(
            fs::read(store.data_path(&pinned).unwrap()).unwrap(),
            b"first\n"
        );
        drop(lease);
        let current = store
            .current(&scope(), &initial.stream_id)
            .unwrap()
            .unwrap();
        let next = stage(&store, b"fifth\n");
        store
            .publish(
                &PublishRequest {
                    snapshot: next,
                    expected_generation: Some(current.generation),
                },
                "later",
            )
            .unwrap();
        assert!(!store.data_path(&pinned).unwrap().exists());
    }

    #[test]
    fn a_corrupted_cached_chunk_is_reported_missing_and_can_be_repaired() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let store = TranscriptSourceStore::open(root).unwrap();
        let snapshot = stage(&store, b"complete\n");
        let chunk = &snapshot.chunks[0];
        let path = store
            .stream_root(&scope(), &snapshot.stream_id)
            .unwrap()
            .join("chunks")
            .join(&chunk.sha256);
        fs::write(path, b"corrupt!\n").unwrap();
        assert_eq!(
            store.missing_chunks(&snapshot).unwrap(),
            vec![chunk.sha256.clone()]
        );
        store
            .install_chunk(&scope(), &snapshot.stream_id, &chunk.sha256, b"complete\n")
            .unwrap();
        assert!(store.missing_chunks(&snapshot).unwrap().is_empty());
        store
            .publish(
                &PublishRequest {
                    snapshot,
                    expected_generation: None,
                },
                "now",
            )
            .unwrap();
    }

    #[test]
    fn chunk_boundaries_can_cross_a_large_jsonl_event_and_resume_after_restart() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let store = TranscriptSourceStore::open(&root).unwrap();
        let bytes = format!("{}\n", "x".repeat(CHUNK_BYTES + 100));
        let snapshot = snapshot(bytes.as_bytes());
        let first = &snapshot.chunks[0];
        store
            .install_chunk(
                &scope(),
                &snapshot.stream_id,
                &first.sha256,
                &bytes.as_bytes()[..CHUNK_BYTES],
            )
            .unwrap();
        let reopened = TranscriptSourceStore::open(root).unwrap();
        assert_eq!(
            reopened.missing_chunks(&snapshot).unwrap(),
            vec![snapshot.chunks[1].sha256.clone()]
        );
        let final_chunk = &snapshot.chunks[1];
        reopened
            .install_chunk(
                &scope(),
                &snapshot.stream_id,
                &final_chunk.sha256,
                &bytes.as_bytes()[CHUNK_BYTES..],
            )
            .unwrap();
        reopened
            .publish(
                &PublishRequest {
                    snapshot: snapshot.clone(),
                    expected_generation: None,
                },
                "now",
            )
            .unwrap();
        let current = reopened
            .current(&scope(), &snapshot.stream_id)
            .unwrap()
            .unwrap();
        assert_eq!(
            fs::read(reopened.data_path(&current).unwrap()).unwrap(),
            bytes.as_bytes()
        );
    }
}
