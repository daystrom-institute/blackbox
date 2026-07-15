//! Durable, idempotent intake for records produced by other authority planes.

use std::collections::BTreeMap;
use std::fs::OpenOptions;
use std::io::Write as _;
use std::path::{Path, PathBuf};

use bro_capabilities::{
    CorpusHit, MAX_RECORD_BYTES, RECORD_ARCHIVE_SNAPSHOT_VERSION, RecordArchiveSnapshot,
    RecordEnvelope, RecordIngestReceipt, RecordIngestRequest, TranscriptRecordTarget,
};
use parking_lot::Mutex;
use sha2::{Digest as _, Sha256};

const MAX_INGEST_BATCH: usize = 256;
/// Compact corpus-side projection of immutable producer records.
///
/// Producers remain authoritative for their live streams. This store owns the
/// retained, searchable copy and its per-producer ingestion cursor.
pub struct RecordStore {
    path: PathBuf,
    state: Mutex<RecordArchiveSnapshot>,
}

impl RecordStore {
    // Opening the durable snapshot is an explicitly synchronous service-start
    // boundary; the store is not published until validation and decode finish.
    #[allow(clippy::disallowed_methods)]
    pub fn open(state_root: &Path) -> anyhow::Result<Self> {
        let root = state_root.join("record-ingest");
        create_private_dir(&root)?;
        let path = root.join("records.json");
        let state = if path.exists() {
            validate_private_file(&path)?;
            let snapshot: RecordArchiveSnapshot = serde_json::from_slice(&std::fs::read(&path)?)?;
            if snapshot.version != RECORD_ARCHIVE_SNAPSHOT_VERSION {
                anyhow::bail!(
                    "unsupported record-ingest snapshot version {}; expected {}",
                    snapshot.version,
                    RECORD_ARCHIVE_SNAPSHOT_VERSION
                );
            }
            snapshot
        } else {
            RecordArchiveSnapshot::default()
        };
        Ok(Self {
            path,
            state: Mutex::new(state),
        })
    }

    pub fn ingest(
        &self,
        request: RecordIngestRequest,
    ) -> Result<RecordIngestReceipt, bro_core::BroError> {
        if request.records.len() > MAX_INGEST_BATCH {
            return Err(bro_core::BroError::new(
                "record_ingest.batch_too_large",
                format!("record batch exceeds {MAX_INGEST_BATCH} entries"),
            ));
        }
        for record in &request.records {
            validate_record(record)?;
        }

        let mut state = self.state.lock();
        let mut next = state.clone();
        let mut accepted = 0usize;
        let mut deduplicated = 0usize;
        for record in request.records {
            let cursor = parse_cursor(&record)?;
            match next.records.get(&record.record_id) {
                Some(existing) if existing == &record => {
                    deduplicated = deduplicated.saturating_add(1);
                }
                Some(_) => {
                    return Err(bro_core::BroError::new(
                        "record_ingest.idempotency_conflict",
                        format!(
                            "record id {} was already used for different content",
                            record.record_id
                        ),
                    ));
                }
                None => {
                    validate_next_cursor(&next.producer_cursors, &record.producer, cursor)?;
                    next.producer_cursors
                        .insert(record.producer.clone(), record.cursor.clone());
                    next.records.insert(record.record_id.clone(), record);
                    accepted = accepted.saturating_add(1);
                }
            }
        }

        if accepted > 0 {
            persist_snapshot(&self.path, &next).map_err(|error| {
                bro_core::BroError::new("record_ingest.persistence_failed", error.to_string())
            })?;
            *state = next;
        }
        Ok(RecordIngestReceipt {
            accepted,
            deduplicated,
            producer_cursors: state.producer_cursors.clone(),
            transcript_cursors: state.transcript_cursors.clone(),
        })
    }

    pub fn search(&self, query: &str, limit: usize) -> Vec<CorpusHit> {
        let query = query.trim().to_lowercase();
        if query.is_empty() || limit == 0 {
            return Vec::new();
        }
        self.state
            .lock()
            .records
            .values()
            .filter_map(|record| {
                let text = record_search_text(record);
                text.to_lowercase().contains(&query).then(|| CorpusHit {
                    id: record.record_id.clone(),
                    text,
                })
            })
            .take(limit)
            .collect()
    }

    pub fn producer_cursors(&self) -> BTreeMap<String, String> {
        self.state.lock().producer_cursors.clone()
    }

    pub fn transcript_cursors(&self) -> BTreeMap<String, u64> {
        self.state.lock().transcript_cursors.clone()
    }

    pub fn transcript_archive_root(&self) -> PathBuf {
        self.path
            .parent()
            .expect("record snapshot always has a parent")
            .join("transcript-archive")
    }

    /// Copy pending fleet logs into corpus-owned private storage before their
    /// projection cursor can be acknowledged.
    pub fn archive_transcript_targets(
        &self,
        targets: &BTreeMap<String, TranscriptRecordTarget>,
        source_roots: &[PathBuf],
    ) -> Result<BTreeMap<String, TranscriptRecordTarget>, bro_core::BroError> {
        let archive_root = self.transcript_archive_root();
        create_private_dir(&archive_root).map_err(transcript_archive_error)?;
        let mut archived = BTreeMap::new();
        for (stream, target) in targets {
            let prefix =
                bbox_corpus_index::transcripts::harness_sessions::read_fleet_event_log_prefix(
                    Path::new(&target.path),
                    target.through_event_seq,
                    source_roots,
                )
                .map_err(transcript_archive_error)?;
            let path = archive_path(&archive_root, stream, &target.session_id);
            persist_bytes(&path, &prefix.bytes).map_err(transcript_archive_error)?;
            archived.insert(
                stream.clone(),
                TranscriptRecordTarget {
                    path: path.to_string_lossy().into_owned(),
                    session_id: target.session_id.clone(),
                    through_event_seq: target.through_event_seq,
                },
            );
        }
        Ok(archived)
    }

    /// Resolve stable corpus-owned paths, backfilling archives from original
    /// fleet paths for records created before archive-on-ack was introduced.
    pub fn ensure_archived_transcript_targets(
        &self,
        targets: &BTreeMap<String, TranscriptRecordTarget>,
        source_roots: &[PathBuf],
    ) -> Result<BTreeMap<String, TranscriptRecordTarget>, bro_core::BroError> {
        let archive_root = self.transcript_archive_root();
        let mut ready = BTreeMap::new();
        let mut missing = BTreeMap::new();
        for (stream, target) in targets {
            let path = archive_path(&archive_root, stream, &target.session_id);
            let existing = path.is_file().then(|| {
                bbox_corpus_index::transcripts::harness_sessions::read_fleet_event_log_prefix(
                    &path,
                    target.through_event_seq,
                    std::slice::from_ref(&archive_root),
                )
            });
            if let Some(Ok(prefix)) = existing {
                // Older releases archived through EOF. Rewriting the exact
                // acknowledged prefix here repairs those archives before a
                // full reindex can observe unacknowledged suffix events.
                persist_bytes(&path, &prefix.bytes).map_err(transcript_archive_error)?;
                ready.insert(
                    stream.clone(),
                    TranscriptRecordTarget {
                        path: path.to_string_lossy().into_owned(),
                        session_id: target.session_id.clone(),
                        through_event_seq: target.through_event_seq,
                    },
                );
            } else {
                missing.insert(stream.clone(), target.clone());
            }
        }
        ready.extend(self.archive_transcript_targets(&missing, source_roots)?);
        Ok(ready)
    }

    /// Persist monotonic transcript projection coordinates after the index
    /// commit they describe. Repeating an acknowledgement is idempotent.
    pub fn acknowledge_transcripts(
        &self,
        cursors: &BTreeMap<String, u64>,
    ) -> Result<BTreeMap<String, u64>, bro_core::BroError> {
        let mut state = self.state.lock();
        let mut next = state.clone();
        let mut changed = false;
        for (stream, cursor) in cursors {
            if stream.trim().is_empty() || *cursor == 0 {
                return Err(bro_core::BroError::new(
                    "record_ingest.invalid_transcript_cursor",
                    "transcript cursor requires a nonempty stream and positive sequence",
                ));
            }
            let stored = next.transcript_cursors.entry(stream.clone()).or_default();
            if *cursor > *stored {
                *stored = *cursor;
                changed = true;
            }
        }
        if changed {
            persist_snapshot(&self.path, &next).map_err(|error| {
                bro_core::BroError::new("record_ingest.persistence_failed", error.to_string())
            })?;
            *state = next;
        }
        Ok(state.transcript_cursors.clone())
    }

    /// Snapshot all retained records for idempotent index reconciliation.
    pub fn all_records(&self) -> Vec<RecordEnvelope> {
        self.state.lock().records.values().cloned().collect()
    }
}

fn validate_record(record: &RecordEnvelope) -> Result<(), bro_core::BroError> {
    for (field, value) in [
        ("record_id", record.record_id.as_str()),
        ("producer", record.producer.as_str()),
        ("cursor", record.cursor.as_str()),
        ("kind", record.kind.as_str()),
    ] {
        if value.trim().is_empty() || value.len() > 256 {
            return Err(bro_core::BroError::new(
                "record_ingest.invalid_record",
                format!("{field} must contain 1 through 256 bytes"),
            ));
        }
    }
    let size = serde_json::to_vec(record)
        .map_err(|error| {
            bro_core::BroError::new("record_ingest.invalid_record", error.to_string())
        })?
        .len();
    if size > MAX_RECORD_BYTES {
        return Err(bro_core::BroError::new(
            "record_ingest.record_too_large",
            format!("record exceeds {MAX_RECORD_BYTES} bytes"),
        ));
    }
    Ok(())
}

fn parse_cursor(record: &RecordEnvelope) -> Result<u64, bro_core::BroError> {
    let cursor = record.cursor.parse::<u64>().map_err(|_| {
        bro_core::BroError::new(
            "record_ingest.invalid_cursor",
            format!(
                "producer {} cursor must be a canonical positive decimal integer",
                record.producer
            ),
        )
    })?;
    if cursor == 0 || cursor.to_string() != record.cursor {
        return Err(bro_core::BroError::new(
            "record_ingest.invalid_cursor",
            format!(
                "producer {} cursor must be a canonical positive decimal integer",
                record.producer
            ),
        ));
    }
    Ok(cursor)
}

fn validate_next_cursor(
    producer_cursors: &BTreeMap<String, String>,
    producer: &str,
    cursor: u64,
) -> Result<(), bro_core::BroError> {
    let Some(previous) = producer_cursors.get(producer) else {
        // A newly attached producer may have compacted an older prefix, so its
        // first observed coordinate need not be one. Once attached, catch-up
        // is strictly contiguous and can neither regress nor skip evidence.
        return Ok(());
    };
    let previous = previous.parse::<u64>().map_err(|_| {
        bro_core::BroError::new(
            "record_ingest.invalid_state",
            format!("stored cursor for producer {producer} is malformed"),
        )
    })?;
    let expected = previous.checked_add(1).ok_or_else(|| {
        bro_core::BroError::new(
            "record_ingest.cursor_exhausted",
            format!("cursor space for producer {producer} is exhausted"),
        )
    })?;
    if cursor != expected {
        return Err(bro_core::BroError::new(
            "record_ingest.cursor_gap",
            format!(
                "producer {producer} submitted cursor {cursor}; expected contiguous cursor {expected}"
            ),
        ));
    }
    Ok(())
}

pub(crate) fn record_search_text(record: &RecordEnvelope) -> String {
    let mut parts = vec![record.kind.clone(), record.producer.clone()];
    if let Some(subject) = &record.subject {
        parts.push(subject.clone());
    }
    parts.extend(
        record
            .attributes
            .iter()
            .map(|(key, value)| format!("{key}: {value}")),
    );
    parts.push(record.payload.to_string());
    parts.join("\n")
}

// This is the store's synchronous durability boundary: fsync and atomic rename
// must complete before the in-memory snapshot and receipt can advance.
#[allow(clippy::disallowed_methods)]
fn persist_snapshot(path: &Path, snapshot: &RecordArchiveSnapshot) -> anyhow::Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("record snapshot path has no parent"))?;
    create_private_dir(parent)?;
    let temporary = parent.join(format!(".records.{}.tmp", uuid::Uuid::new_v4().simple()));
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    let result = (|| -> anyhow::Result<()> {
        let mut file = options.open(&temporary)?;
        serde_json::to_writer(&mut file, snapshot)?;
        file.write_all(b"\n")?;
        file.sync_all()?;
        std::fs::rename(&temporary, path)?;
        std::fs::File::open(parent)?.sync_all()?;
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    result
}

fn archive_path(root: &Path, stream: &str, session_id: &str) -> PathBuf {
    let mut digest = Sha256::new();
    digest.update(stream.as_bytes());
    digest.update([0]);
    digest.update(session_id.as_bytes());
    root.join(format!("{}.events.jsonl", hex::encode(digest.finalize())))
}

// Transcript coordinates are acknowledged only after this synchronous atomic
// archive replacement and parent-directory fsync complete.
#[allow(clippy::disallowed_methods)]
fn persist_bytes(path: &Path, bytes: &[u8]) -> anyhow::Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("transcript archive path has no parent"))?;
    create_private_dir(parent)?;
    let temporary = parent.join(format!(".transcript.{}.tmp", uuid::Uuid::new_v4().simple()));
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    let result = (|| -> anyhow::Result<()> {
        let mut file = options.open(&temporary)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        std::fs::rename(&temporary, path)?;
        std::fs::File::open(parent)?.sync_all()?;
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    result
}

fn transcript_archive_error(error: impl std::fmt::Display) -> bro_core::BroError {
    bro_core::BroError::new("record_ingest.transcript_archive_failed", error.to_string())
}

// Private state roots are synchronously established before any durable record
// or archive file is opened beneath them.
#[allow(clippy::disallowed_methods)]
fn create_private_dir(path: &Path) -> anyhow::Result<()> {
    if path.exists() {
        let metadata = std::fs::symlink_metadata(path)?;
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            anyhow::bail!("record-ingest state root is not a real directory");
        }
    } else {
        std::fs::create_dir_all(path)?;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

fn validate_private_file(path: &Path) -> anyhow::Result<()> {
    let metadata = std::fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        anyhow::bail!("record-ingest snapshot is not a regular file");
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        if metadata.permissions().mode() & 0o077 != 0 {
            anyhow::bail!("record-ingest snapshot permissions are too broad");
        }
    }
    Ok(())
}

#[cfg(test)]
// Record-store fixtures intentionally construct and inspect private temporary
// snapshots and transcript archives on disk.
#[allow(clippy::disallowed_methods)]
mod tests {
    use super::*;

    fn transcript_line(sequence: u64, text: &str) -> String {
        format!(
            "{}\n",
            serde_json::json!({
                "ts": "2026-07-15T01:00:00.000Z",
                "event_seq": sequence,
                "event": {
                    "type": "user",
                    "session_id": "session-1",
                    "message": {
                        "role": "user",
                        "content": [{"type": "text", "text": text}],
                    },
                },
            })
        )
    }

    fn record(id: &str, cursor: &str, payload: serde_json::Value) -> RecordEnvelope {
        RecordEnvelope {
            record_id: id.into(),
            producer: "blackopsd".into(),
            cursor: cursor.into(),
            kind: "operation.completed".into(),
            occurred_at: None,
            subject: Some("operation-1".into()),
            attributes: BTreeMap::from([("outcome".into(), "success".into())]),
            payload,
        }
    }

    #[test]
    fn ingest_is_idempotent_across_restart() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let store = RecordStore::open(&root).unwrap();
        let request = RecordIngestRequest {
            records: vec![record(
                "record-1",
                "1",
                serde_json::json!({"result": "needle"}),
            )],
        };
        let first = store.ingest(request.clone()).unwrap();
        assert_eq!((first.accepted, first.deduplicated), (1, 0));
        drop(store);

        let reopened = RecordStore::open(&root).unwrap();
        let second = reopened.ingest(request).unwrap();
        assert_eq!((second.accepted, second.deduplicated), (0, 1));
        assert_eq!(reopened.search("needle", 10).len(), 1);
        assert_eq!(
            reopened.producer_cursors().get("blackopsd"),
            Some(&"1".to_string())
        );
    }

    #[test]
    fn conflicting_batch_is_refused_atomically() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let store = RecordStore::open(&root).unwrap();
        store
            .ingest(RecordIngestRequest {
                records: vec![record("record-1", "1", serde_json::json!({"value": 1}))],
            })
            .unwrap();
        let error = store
            .ingest(RecordIngestRequest {
                records: vec![
                    record("record-2", "2", serde_json::json!({"new": true})),
                    record("record-1", "3", serde_json::json!({"value": 2})),
                ],
            })
            .unwrap_err();
        assert_eq!(error.code, "record_ingest.idempotency_conflict");
        assert!(store.search("new", 10).is_empty());
        assert_eq!(store.search("value", 10).len(), 1);
    }

    #[test]
    fn producer_cursor_cannot_regress_or_skip() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let store = RecordStore::open(&root).unwrap();
        store
            .ingest(RecordIngestRequest {
                records: vec![record("record-10", "10", serde_json::json!({"value": 10}))],
            })
            .unwrap();

        for (id, cursor) in [("record-9", "9"), ("record-12", "12")] {
            let error = store
                .ingest(RecordIngestRequest {
                    records: vec![record(id, cursor, serde_json::json!({"value": cursor}))],
                })
                .unwrap_err();
            assert_eq!(error.code, "record_ingest.cursor_gap");
        }
        assert_eq!(store.producer_cursors().get("blackopsd").unwrap(), "10");

        store
            .ingest(RecordIngestRequest {
                records: vec![record("record-11", "11", serde_json::json!({"value": 11}))],
            })
            .unwrap();
        assert_eq!(store.producer_cursors().get("blackopsd").unwrap(), "11");
    }

    #[test]
    fn partial_replay_deduplicates_prefix_before_contiguous_suffix() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let store = RecordStore::open(&root).unwrap();
        let first = record("record-1", "1", serde_json::json!({"value": 1}));
        store
            .ingest(RecordIngestRequest {
                records: vec![first.clone()],
            })
            .unwrap();
        let receipt = store
            .ingest(RecordIngestRequest {
                records: vec![
                    first,
                    record("record-2", "2", serde_json::json!({"value": 2})),
                ],
            })
            .unwrap();
        assert_eq!((receipt.accepted, receipt.deduplicated), (1, 1));
        assert_eq!(receipt.producer_cursors.get("blackopsd").unwrap(), "2");
    }

    #[test]
    fn transcript_projection_cursor_is_monotonic_and_durable() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let store = RecordStore::open(&root).unwrap();
        assert_eq!(
            store
                .acknowledge_transcripts(&BTreeMap::from([("worker-1".into(), 7)]))
                .unwrap()
                .get("worker-1"),
            Some(&7)
        );
        assert_eq!(
            store
                .acknowledge_transcripts(&BTreeMap::from([("worker-1".into(), 4)]))
                .unwrap()
                .get("worker-1"),
            Some(&7)
        );
        drop(store);

        let reopened = RecordStore::open(&root).unwrap();
        assert_eq!(reopened.transcript_cursors().get("worker-1"), Some(&7));
        let receipt = reopened
            .ingest(RecordIngestRequest {
                records: Vec::new(),
            })
            .unwrap();
        assert_eq!(receipt.transcript_cursors.get("worker-1"), Some(&7));
    }

    #[test]
    fn transcript_archive_persists_and_repairs_the_exact_acknowledged_prefix() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let source_root = root.join("fleet");
        std::fs::create_dir_all(&source_root).unwrap();
        let source_root = source_root.canonicalize().unwrap();
        let source = source_root.join("session-1.events.jsonl");
        let first = transcript_line(1, "acknowledged");
        let second = transcript_line(2, "future");
        std::fs::write(&source, format!("{first}{second}")).unwrap();
        let store = RecordStore::open(&root.join("corpus")).unwrap();
        let targets = BTreeMap::from([(
            "worker-1".to_string(),
            TranscriptRecordTarget {
                path: source.to_string_lossy().into_owned(),
                session_id: "session-1".into(),
                through_event_seq: 1,
            },
        )]);

        let archived = store
            .archive_transcript_targets(&targets, std::slice::from_ref(&source_root))
            .unwrap();
        let archive = PathBuf::from(&archived["worker-1"].path);
        assert_eq!(std::fs::read_to_string(&archive).unwrap(), first);

        // Simulate an archive written by the former through-EOF behavior.
        std::fs::write(&archive, format!("{first}{second}")).unwrap();
        let repaired = store
            .ensure_archived_transcript_targets(&targets, std::slice::from_ref(&source_root))
            .unwrap();
        assert_eq!(repaired["worker-1"].through_event_seq, 1);
        assert_eq!(std::fs::read_to_string(archive).unwrap(), first);
    }

    #[cfg(unix)]
    #[test]
    fn snapshot_is_private() {
        use std::os::unix::fs::PermissionsExt as _;

        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let store = RecordStore::open(&root).unwrap();
        store
            .ingest(RecordIngestRequest {
                records: vec![record("record-1", "1", serde_json::json!({"ok": true}))],
            })
            .unwrap();
        let mode = std::fs::metadata(root.join("record-ingest/records.json"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o077, 0);
    }
}
