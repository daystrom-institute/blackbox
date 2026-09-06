//! Daemon-owned native transcript snapshots. Receipts follow fsync and atomic
//! pointer replacement; a process restart never turns an admitted write into
//! an acknowledged but missing source. Blob namespace is source AND stream.
use anyhow::{Result, ensure};
use bbox_corpus_core::project_catalog::ConnectorScope;
use bbox_transcript_source::*;
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

#[derive(Clone)]
pub struct TranscriptSourceStore {
    root: PathBuf,
}
impl TranscriptSourceStore {
    pub fn open(root: impl AsRef<Path>) -> Result<Self> {
        fs::create_dir_all(root.as_ref())?;
        Ok(Self {
            root: root.as_ref().to_owned(),
        })
    }
    fn scope_root(&self, scope: &ConnectorScope) -> PathBuf {
        self.root.join(scope.connector_source_id().as_str())
    }
    fn stream_root(&self, scope: &ConnectorScope, stream: &str) -> Result<PathBuf> {
        validate_hash(stream)?;
        Ok(self.scope_root(scope).join(stream))
    }
    fn lock(&self, scope: &ConnectorScope, stream: &str) -> Result<File> {
        let root = self.stream_root(scope, stream)?;
        fs::create_dir_all(&root)?;
        let lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(root.join("lock"))?;
        fs2::FileExt::lock_exclusive(&lock)?;
        Ok(lock)
    }
    pub fn current(
        &self,
        scope: &ConnectorScope,
        stream: &str,
    ) -> Result<Option<PublishedSnapshot>> {
        let path = self.stream_root(scope, stream)?.join("current.json");
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
        validate_hash(hash)?;
        ensure!(
            !bytes.is_empty() && bytes.len() <= CHUNK_BYTES && sha256(bytes) == hash,
            "transcript chunk hash or size mismatch"
        );
        let _lock = self.lock(scope, stream)?;
        let root = self.stream_root(scope, stream)?.join("chunks");
        fs::create_dir_all(&root)?;
        let path = root.join(hash);
        if !chunk_matches(&path, hash, bytes.len() as u64) {
            atomic_write(&path, bytes)?;
        }
        Ok(())
    }
    pub fn missing_chunks(&self, snapshot: &StreamSnapshot) -> Result<Vec<String>> {
        snapshot.validate()?;
        let root = self
            .stream_root(&snapshot.scope, &snapshot.stream_id)?
            .join("chunks");
        Ok(snapshot
            .chunks
            .iter()
            .filter(|chunk| !chunk_matches(&root.join(&chunk.sha256), &chunk.sha256, chunk.size))
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
        let snapshot = &request.snapshot;
        let generation = snapshot.generation()?;
        let _lock = self.lock(&snapshot.scope, &snapshot.stream_id)?;
        let current = self.current(&snapshot.scope, &snapshot.stream_id)?;
        let receipt = || PublishReceipt {
            generation: generation.clone(),
            locator: snapshot.locator(&generation),
            byte_length: snapshot.byte_length,
            durable: true,
        };
        if current
            .as_ref()
            .is_some_and(|row| row.generation == generation)
        {
            return Ok(receipt());
        }
        ensure!(
            current.as_ref().map(|row| &row.generation) == request.expected_generation.as_ref(),
            "error.transcript_generation_conflict: reread the stream status and retry from current source bytes"
        );
        let root = self.stream_root(&snapshot.scope, &snapshot.stream_id)?;
        let mut staged = tempfile::NamedTempFile::new_in(&root)?;
        let mut digest = Sha256::new();
        let mut last = None;
        for chunk in &snapshot.chunks {
            let bytes = fs::read(root.join("chunks").join(&chunk.sha256))?;
            ensure!(
                bytes.len() as u64 == chunk.size && sha256(&bytes) == chunk.sha256,
                "transcript chunk content mismatch"
            );
            digest.update(&bytes);
            staged.write_all(&bytes)?;
            last = bytes.last().copied();
        }
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
        staged.persist(self.data_path(&published)?)?;
        File::open(&root)?.sync_all()?;
        // The current pointer is the admission boundary, installed last.
        atomic_write(&root.join("current.json"), &serde_json::to_vec(&published)?)?;
        // Keep the immediately previous materialization for recovery. This is
        // retention, not a reader lease: a discovery snapshot overtaken twice
        // before opening its file must retry discovery.
        // Unreferenced chunks from failed uploads are intentionally untouched:
        // another collector may be assembling its next CAS publication.
        for entry in fs::read_dir(&root)? {
            let path = entry?.path();
            if path.extension().is_some_and(|ext| ext == "jsonl")
                && path != self.data_path(&published)?
                && current
                    .as_ref()
                    .is_none_or(|old| self.data_path(old).ok().as_ref() != Some(&path))
            {
                let _ = fs::remove_file(path);
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
}
fn chunk_matches(path: &Path, hash: &str, size: u64) -> bool {
    let Ok(file) = File::open(path) else {
        return false;
    };
    if size > CHUNK_BYTES as u64
        || file
            .metadata()
            .map_or(true, |metadata| metadata.len() != size)
    {
        return false;
    }
    let mut bytes = Vec::with_capacity(size as usize);
    if file
        .take(CHUNK_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .is_err()
    {
        return false;
    }
    bytes.len() as u64 == size && sha256(&bytes) == hash
}

fn atomic_write(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("missing store parent"))?;
    let mut file = tempfile::NamedTempFile::new_in(parent)?;
    file.write_all(bytes)?;
    file.as_file().sync_all()?;
    file.persist(path)?;
    File::open(parent)?.sync_all()?;
    Ok(())
}

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
