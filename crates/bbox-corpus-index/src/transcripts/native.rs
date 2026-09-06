//! Producer-landed native transcripts. Generations participate in the opaque
//! locator, so same-size rewrites never depend on filesystem mtime resolution.
use super::adapters::{TranscriptReadAdapter, TranscriptScanTarget};
use super::types::*;
use bbox_corpus_core::project_catalog::ConnectorScope;
use bbox_transcript_source::NativeSource;
use bbox_transcript_source_store::TranscriptSourceStore;
use std::io::BufRead;
use std::path::Path;

pub struct NativeTranscriptAdapter {
    store: TranscriptSourceStore,
    scopes: Vec<ConnectorScope>,
    source: TranscriptSource,
    reader_leases: std::sync::Mutex<Vec<std::fs::File>>,
}
impl NativeTranscriptAdapter {
    pub fn new(root: &Path, scopes: Vec<ConnectorScope>, source: TranscriptSource) -> Self {
        Self {
            store: TranscriptSourceStore::for_read(root),
            scopes,
            source,
            reader_leases: std::sync::Mutex::new(Vec::new()),
        }
    }
}
impl TranscriptReadAdapter for NativeTranscriptAdapter {
    fn source(&self) -> TranscriptSource {
        self.source
    }
    fn locate(&self, session: &str) -> Result<Option<TranscriptLocation>, TranscriptReadError> {
        Ok(self
            .scan_locations(TranscriptScanTarget::Sessions)?
            .into_iter()
            .find(|location| location.session_id.as_deref() == Some(session)))
    }
    fn scan_locations(
        &self,
        target: TranscriptScanTarget,
    ) -> Result<Vec<TranscriptLocation>, TranscriptReadError> {
        if target == TranscriptScanTarget::History {
            return Ok(Vec::new());
        }
        let mut locations = Vec::new();
        for scope in &self.scopes {
            let lease = self.store.reader_lease(scope).map_err(|error| {
                TranscriptReadError::io(
                    "pin native source",
                    scope.connector_source_id().as_str(),
                    std::io::Error::other(error.to_string()),
                )
            })?;
            self.reader_leases
                .lock()
                .expect("native reader leases poisoned")
                .push(lease);
            let rows = self.store.snapshots(scope).map_err(|error| {
                TranscriptReadError::io(
                    "scan native source",
                    scope.connector_source_id().as_str(),
                    std::io::Error::other(error.to_string()),
                )
            })?;
            for row in rows {
                let source = match row.snapshot.source {
                    NativeSource::Claude => TranscriptSource::Claude,
                    NativeSource::Codex => TranscriptSource::Codex,
                };
                if source != self.source {
                    continue;
                }
                let path = self.store.data_path(&row).map_err(|error| {
                    TranscriptReadError::io(
                        "resolve native snapshot",
                        scope.connector_source_id().as_str(),
                        std::io::Error::other(error.to_string()),
                    )
                })?;
                let logical_key = Some(row.snapshot.locator(&row.generation));
                locations.push(TranscriptLocation {
                    source,
                    storage: TranscriptStorage::LandedRecords,
                    path,
                    account: Some(row.snapshot.account),
                    session_id: Some(row.snapshot.session_id),
                    project: None,
                    cwd: None,
                    is_subagent: row.snapshot.is_subagent,
                    logical_key,
                });
            }
        }
        locations.sort_by_key(TranscriptLocation::locator);
        Ok(locations)
    }
    fn read_since(
        &self,
        location: &TranscriptLocation,
        cursor: Option<&TranscriptCursor>,
    ) -> Result<TranscriptBatch, TranscriptReadError> {
        if location.source != self.source || !location.locator().starts_with("native:") {
            return Err(TranscriptReadError::InvalidLocation {
                source: self.source,
                path: location.path.clone(),
                reason: "not a native landed snapshot",
            });
        }
        let start = match cursor {
            None => 0,
            Some(TranscriptCursor::ByteOffset { offset }) => *offset,
            Some(cursor) => {
                return Err(TranscriptReadError::UnsupportedCursor {
                    source: self.source,
                    cursor: cursor.clone(),
                });
            }
        };
        let file = std::fs::File::open(&location.path).map_err(|error| {
            TranscriptReadError::io("read native snapshot", &location.path, error)
        })?;
        let mut reader = std::io::BufReader::new(file);
        let mut line = String::new();
        let mut offset = 0u64;
        let mut events = Vec::new();
        let mut cwd = None;
        loop {
            line.clear();
            let bytes = reader.read_line(&mut line).map_err(|error| {
                TranscriptReadError::io("read native snapshot line", &location.path, error)
            })?;
            if bytes == 0 {
                break;
            }
            let value: serde_json::Value =
                serde_json::from_str(&line).map_err(|_| TranscriptReadError::InvalidJsonLine {
                    source: self.source,
                    path: location.path.clone(),
                    byte_offset: offset,
                    line_len: bytes,
                })?;
            if let Some(path) = value["payload"]["cwd"]
                .as_str()
                .or_else(|| value["cwd"].as_str())
            {
                cwd = Some(path.to_string());
            }
            if offset >= start {
                let parsed = match self.source {
                    TranscriptSource::Claude => bro_transcript::parse_transcript_line(&line),
                    TranscriptSource::Codex => bro_transcript::parse_codex_line(
                        &line,
                        location.session_id.as_deref().unwrap_or_default(),
                    ),
                    _ => Vec::new(),
                };
                for (i, mut event) in parsed.into_iter().enumerate() {
                    if event.session_id.is_empty() {
                        event.session_id = location.session_id.clone().unwrap_or_default();
                    }
                    if event.cwd.is_none() {
                        event.cwd = cwd.clone();
                    }
                    event.is_subagent |= location.is_subagent;
                    let raw = RawTranscriptRef::jsonl(
                        self.source,
                        TranscriptStorage::LandedRecords,
                        location.locator(),
                        offset,
                        i as u32,
                        bytes,
                    );
                    events.push(NormalizedTranscriptEvent::from_parsed_event(
                        self.source,
                        event,
                        raw,
                    ));
                }
            }
            offset += bytes as u64;
        }
        Ok(TranscriptBatch {
            location: location.clone(),
            cursor: Some(TranscriptCursor::byte_offset(offset)),
            events,
            reached_end: true,
        })
    }
}
