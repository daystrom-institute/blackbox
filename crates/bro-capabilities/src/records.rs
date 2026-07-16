//! Idempotent producer record ingestion into the corpus authority.

use std::collections::BTreeMap;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::CapabilityResult;

/// Durable corpus-side snapshot format for retained producer records.
///
/// The corpus service owns the file and its lifecycle. Keeping the pure data
/// contract here lets the index reconciler restore projections after a full
/// Tantivy rebuild without depending on the service implementation crate.
pub const RECORD_ARCHIVE_SNAPSHOT_VERSION: u16 = 1;
/// Maximum compact-JSON size of one record accepted by the corpus authority.
/// Producers enforce the same limit before assigning a durable stream cursor,
/// so one impossible record cannot wedge every later record in that stream.
pub const MAX_RECORD_BYTES: usize = 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RecordArchiveSnapshot {
    pub version: u16,
    #[serde(default)]
    pub records: BTreeMap<String, RecordEnvelope>,
    #[serde(default)]
    pub producer_cursors: BTreeMap<String, String>,
    #[serde(default)]
    pub transcript_cursors: BTreeMap<String, u64>,
}

impl Default for RecordArchiveSnapshot {
    fn default() -> Self {
        Self {
            version: RECORD_ARCHIVE_SNAPSHOT_VERSION,
            records: BTreeMap::new(),
            producer_cursors: BTreeMap::new(),
            transcript_cursors: BTreeMap::new(),
        }
    }
}

/// One immutable producer record. `record_id` is globally stable and `cursor`
/// is a canonical positive decimal coordinate ordered only within the named
/// producer stream. After a consumer attaches at any coordinate, newly seen
/// records for that producer are contiguous; exact replay remains idempotent.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RecordEnvelope {
    pub record_id: String,
    pub producer: String,
    pub cursor: String,
    pub kind: String,
    pub occurred_at: Option<String>,
    pub subject: Option<String>,
    #[serde(default)]
    pub attributes: BTreeMap<String, String>,
    #[serde(default)]
    pub payload: Value,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RecordIngestRequest {
    #[serde(default)]
    pub records: Vec<RecordEnvelope>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TranscriptRecordTarget {
    pub path: String,
    pub session_id: String,
    pub through_event_seq: u64,
}

/// Validate and coalesce fleet transcript-coordinate records by worker stream.
pub fn transcript_record_targets(
    records: &[RecordEnvelope],
) -> CapabilityResult<BTreeMap<String, TranscriptRecordTarget>> {
    let mut targets = BTreeMap::<String, TranscriptRecordTarget>::new();
    for record in records
        .iter()
        .filter(|record| record.kind == "session.event_committed")
    {
        if record.producer != "fleetd" {
            return Err(invalid_transcript_record(
                "session transcript coordinates are accepted only from fleetd",
            ));
        }
        let worker_id = required_attribute(record, "worker_id")?;
        let session_id = required_attribute(record, "session_id")?;
        let event_seq = required_attribute(record, "event_seq")?
            .parse::<u64>()
            .map_err(|_| invalid_transcript_record("event_seq must be a positive integer"))?;
        if event_seq == 0 {
            return Err(invalid_transcript_record(
                "event_seq must be a positive integer",
            ));
        }
        let payload_seq = record
            .payload
            .get("through_event_seq")
            .and_then(Value::as_u64)
            .ok_or_else(|| invalid_transcript_record("through_event_seq is required"))?;
        if payload_seq != event_seq {
            return Err(invalid_transcript_record(
                "event_seq and through_event_seq must agree",
            ));
        }
        let path = record
            .payload
            .get("transcript_path")
            .and_then(Value::as_str)
            .filter(|path| !path.trim().is_empty())
            .ok_or_else(|| invalid_transcript_record("transcript_path is required"))?;
        let target = TranscriptRecordTarget {
            path: path.to_string(),
            session_id: session_id.to_string(),
            through_event_seq: event_seq,
        };
        match targets.get_mut(worker_id) {
            Some(existing)
                if existing.path == target.path && existing.session_id == target.session_id =>
            {
                existing.through_event_seq = existing.through_event_seq.max(event_seq);
            }
            Some(_) => {
                return Err(invalid_transcript_record(
                    "worker transcript path or session changed within one batch",
                ));
            }
            None => {
                targets.insert(worker_id.to_string(), target);
            }
        }
    }
    Ok(targets)
}

fn required_attribute<'a>(record: &'a RecordEnvelope, key: &str) -> CapabilityResult<&'a str> {
    record
        .attributes
        .get(key)
        .map(String::as_str)
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| invalid_transcript_record(format!("{key} attribute is required")))
}

fn invalid_transcript_record(message: impl Into<String>) -> bro_core::BroError {
    bro_core::BroError::new("record_ingest.invalid_transcript_record", message)
}

/// Record kind for inline-payload transcript increments shipped by satellite
/// collectors. Unlike `session.event_committed`, the committed bytes travel in
/// the record payload because the producer's file is not on the corpus host's
/// disk.
pub const TRANSCRIPT_INCREMENT_KIND: &str = "transcript.increment";
/// Producer prefix for per-host satellite collectors (`collector:<host-id>`).
pub const COLLECTOR_PRODUCER_PREFIX: &str = "collector:";
/// Interactive transcript sources the inline-increment contract accepts today.
/// Append-only JSONL sources only; snapshot-shaped sources (gemini) and the
/// fleet harness lane are deliberately excluded until their shipping semantics
/// are settled.
pub const INLINE_TRANSCRIPT_SOURCES: &[&str] = &["claude", "codex"];

/// One validated inline transcript increment: a contiguous byte range of an
/// append-only transcript file on a satellite host, ending on a complete line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InlineTranscriptIncrement {
    /// Server-derived stream identity: `producer/source/account/relative_path`.
    /// Byte coordinates are properties of one source file, so the stream keys
    /// the file, not the session.
    pub stream: String,
    pub producer: String,
    pub source: String,
    pub account: String,
    pub session_id: String,
    /// Path of the source file relative to its scanned root, validated free of
    /// traversal so it can double as the corpus-side archive layout.
    pub relative_path: String,
    pub byte_start: u64,
    pub byte_end: u64,
    pub bytes: Vec<u8>,
}

/// Deterministic record id for an inline transcript increment, shared by the
/// collector and any verifier so crash-recovery resubmissions dedupe instead
/// of conflicting.
pub fn inline_transcript_record_id(
    producer: &str,
    source: &str,
    account: &str,
    relative_path: &str,
    byte_start: u64,
    byte_end: u64,
) -> String {
    use sha2::Digest as _;
    let mut digest = sha2::Sha256::new();
    for part in [producer, source, account, relative_path] {
        digest.update(part.as_bytes());
        digest.update([0]);
    }
    digest.update(byte_start.to_be_bytes());
    digest.update(byte_end.to_be_bytes());
    let hash = digest.finalize();
    let mut id = String::with_capacity(3 + 32);
    id.push_str("ti-");
    for byte in &hash[..16] {
        id.push_str(&format!("{byte:02x}"));
    }
    id
}

/// Validate inline transcript-increment records. Increments for the same
/// stream must be ascending and contiguous within one batch; cross-batch
/// contiguity is the archive appender's check (byte_start against the
/// archived length).
pub fn inline_transcript_increments(
    records: &[RecordEnvelope],
) -> CapabilityResult<Vec<InlineTranscriptIncrement>> {
    let mut increments = Vec::new();
    let mut batch_tails = BTreeMap::<String, u64>::new();
    for record in records
        .iter()
        .filter(|record| record.kind == TRANSCRIPT_INCREMENT_KIND)
    {
        let host = record
            .producer
            .strip_prefix(COLLECTOR_PRODUCER_PREFIX)
            .ok_or_else(|| {
                invalid_transcript_increment(
                    "inline transcript increments are accepted only from collector:<host-id> producers",
                )
            })?;
        if !is_safe_component(host) {
            return Err(invalid_transcript_increment(
                "collector host id must be a single safe path component",
            ));
        }
        let source = required_attribute(record, "source")?;
        if !INLINE_TRANSCRIPT_SOURCES.contains(&source) {
            return Err(invalid_transcript_increment(format!(
                "unsupported inline transcript source {source}"
            )));
        }
        let account = required_attribute(record, "account")?;
        let session_id = required_attribute(record, "session_id")?;
        let relative_path = required_attribute(record, "relative_path")?;
        if !is_safe_component(account) || !is_safe_component(session_id) {
            return Err(invalid_transcript_increment(
                "account and session_id must be single safe path components",
            ));
        }
        validate_relative_path(relative_path)?;

        let byte_start = required_payload_u64(record, "byte_start")?;
        let byte_end = required_payload_u64(record, "byte_end")?;
        if byte_end <= byte_start {
            return Err(invalid_transcript_increment(
                "byte_end must be greater than byte_start",
            ));
        }
        let encoded = record
            .payload
            .get("bytes_b64")
            .and_then(Value::as_str)
            .ok_or_else(|| invalid_transcript_increment("bytes_b64 is required"))?;
        let bytes = base64_decode(encoded)?;
        if bytes.len() as u64 != byte_end - byte_start {
            return Err(invalid_transcript_increment(
                "decoded byte length must equal byte_end - byte_start",
            ));
        }
        if bytes.last() != Some(&b'\n') {
            return Err(invalid_transcript_increment(
                "increment must end on a complete line",
            ));
        }

        let stream = format!("{}/{source}/{account}/{relative_path}", record.producer);
        if let Some(tail) = batch_tails.get(&stream)
            && *tail != byte_start
        {
            return Err(invalid_transcript_increment(format!(
                "increments for stream {stream} must be ascending and contiguous within a batch"
            )));
        }
        batch_tails.insert(stream.clone(), byte_end);
        increments.push(InlineTranscriptIncrement {
            stream,
            producer: record.producer.clone(),
            source: source.to_string(),
            account: account.to_string(),
            session_id: session_id.to_string(),
            relative_path: relative_path.to_string(),
            byte_start,
            byte_end,
            bytes,
        });
    }
    Ok(increments)
}

fn required_payload_u64(record: &RecordEnvelope, key: &str) -> CapabilityResult<u64> {
    record
        .payload
        .get(key)
        .and_then(Value::as_u64)
        .ok_or_else(|| {
            invalid_transcript_increment(format!("{key} is required and must be a u64"))
        })
}

fn base64_decode(encoded: &str) -> CapabilityResult<Vec<u8>> {
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD
        .decode(encoded)
        .map_err(|_| invalid_transcript_increment("bytes_b64 is not valid base64"))
}

fn is_safe_component(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 256
        && value != "."
        && value != ".."
        && value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
}

fn validate_relative_path(path: &str) -> CapabilityResult<()> {
    if path.len() > 1024 {
        return Err(invalid_transcript_increment("relative_path is too long"));
    }
    let components: Vec<&str> = path.split('/').collect();
    if components.is_empty() || !components.iter().all(|part| is_safe_component(part)) {
        return Err(invalid_transcript_increment(
            "relative_path must be a relative path of safe components",
        ));
    }
    Ok(())
}

fn invalid_transcript_increment(message: impl Into<String>) -> bro_core::BroError {
    bro_core::BroError::new("record_ingest.invalid_transcript_increment", message)
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecordIngestReceipt {
    pub accepted: usize,
    pub deduplicated: usize,
    #[serde(default)]
    pub producer_cursors: BTreeMap<String, String>,
    /// Highest transcript event sequence durably projected into corpus,
    /// keyed by the producer's stable worker/stream identity.
    #[serde(default)]
    pub transcript_cursors: BTreeMap<String, u64>,
}

#[async_trait]
pub trait RecordIngestCapability: Send + Sync {
    async fn ingest_records(
        &self,
        request: RecordIngestRequest,
    ) -> CapabilityResult<RecordIngestReceipt>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn record_contract_round_trips_without_losing_cursor_identity() {
        let record = RecordEnvelope {
            record_id: "record-1".into(),
            producer: "blackopsd".into(),
            cursor: "42".into(),
            kind: "operation.completed".into(),
            occurred_at: None,
            subject: Some("operation-1".into()),
            attributes: BTreeMap::new(),
            payload: serde_json::json!({"status": "completed"}),
        };
        let encoded = serde_json::to_vec(&record).unwrap();
        let decoded: RecordEnvelope = serde_json::from_slice(&encoded).unwrap();
        assert_eq!(decoded, record);
    }

    #[test]
    fn transcript_coordinates_coalesce_by_worker_and_fail_closed_on_drift() {
        let make = |cursor: &str, sequence: u64, path: &str| RecordEnvelope {
            record_id: format!("fleetd:event:worker-1:{sequence}"),
            producer: "fleetd".into(),
            cursor: cursor.into(),
            kind: "session.event_committed".into(),
            occurred_at: None,
            subject: Some("session-1".into()),
            attributes: BTreeMap::from([
                ("worker_id".into(), "worker-1".into()),
                ("session_id".into(), "session-1".into()),
                ("event_seq".into(), sequence.to_string()),
            ]),
            payload: serde_json::json!({
                "transcript_path": path,
                "through_event_seq": sequence
            }),
        };
        let targets = transcript_record_targets(&[
            make("1", 1, "/state/worker-1/events.jsonl"),
            make("2", 2, "/state/worker-1/events.jsonl"),
        ])
        .unwrap();
        assert_eq!(targets["worker-1"].through_event_seq, 2);

        let error = transcript_record_targets(&[
            make("1", 1, "/state/worker-1/events.jsonl"),
            make("2", 2, "/other/events.jsonl"),
        ])
        .unwrap_err();
        assert_eq!(error.code, "record_ingest.invalid_transcript_record");
    }

    fn increment_record(
        cursor: &str,
        relative_path: &str,
        byte_start: u64,
        raw: &[u8],
    ) -> RecordEnvelope {
        use base64::Engine as _;
        let byte_end = byte_start + raw.len() as u64;
        RecordEnvelope {
            record_id: inline_transcript_record_id(
                "collector:host-b1",
                "claude",
                "claude",
                relative_path,
                byte_start,
                byte_end,
            ),
            producer: "collector:host-b1".into(),
            cursor: cursor.into(),
            kind: TRANSCRIPT_INCREMENT_KIND.into(),
            occurred_at: None,
            subject: Some("session-1".into()),
            attributes: BTreeMap::from([
                ("source".into(), "claude".into()),
                ("account".into(), "claude".into()),
                ("session_id".into(), "session-1".into()),
                ("relative_path".into(), relative_path.into()),
            ]),
            payload: serde_json::json!({
                "byte_start": byte_start,
                "byte_end": byte_end,
                "bytes_b64": base64::engine::general_purpose::STANDARD.encode(raw),
            }),
        }
    }

    #[test]
    fn inline_increments_validate_and_stay_contiguous_within_a_batch() {
        let first = increment_record("1", "projects/repo-a/session-1.jsonl", 0, b"{\"a\":1}\n");
        let second = increment_record("2", "projects/repo-a/session-1.jsonl", 8, b"{\"b\":2}\n");
        let increments = inline_transcript_increments(&[first.clone(), second]).unwrap();
        assert_eq!(increments.len(), 2);
        assert_eq!(
            increments[0].stream,
            "collector:host-b1/claude/claude/projects/repo-a/session-1.jsonl"
        );
        assert_eq!(increments[1].byte_start, 8);

        let gapped = increment_record("2", "projects/repo-a/session-1.jsonl", 9, b"{\"b\":2}\n");
        let error = inline_transcript_increments(&[first, gapped]).unwrap_err();
        assert_eq!(error.code, "record_ingest.invalid_transcript_increment");
    }

    #[test]
    fn inline_increments_fail_closed_on_unsafe_input() {
        let traversal = increment_record("1", "projects/../../etc/passwd", 0, b"x\n");
        assert!(inline_transcript_increments(&[traversal]).is_err());

        let mut wrong_producer = increment_record("1", "projects/repo-a/s.jsonl", 0, b"x\n");
        wrong_producer.producer = "fleetd".into();
        assert!(inline_transcript_increments(&[wrong_producer]).is_err());

        let mut unknown_source = increment_record("1", "projects/repo-a/s.jsonl", 0, b"x\n");
        unknown_source
            .attributes
            .insert("source".into(), "gemini".into());
        assert!(inline_transcript_increments(&[unknown_source]).is_err());

        use base64::Engine as _;
        let torn_bytes = base64::engine::general_purpose::STANDARD.encode(b"torn");
        let mut torn_tail = increment_record("1", "projects/repo-a/s.jsonl", 0, b"x\n");
        torn_tail.payload = serde_json::json!({
            "byte_start": 0,
            "byte_end": 4,
            "bytes_b64": torn_bytes,
        });
        assert!(inline_transcript_increments(&[torn_tail]).is_err());
    }

    #[test]
    fn inline_record_ids_are_deterministic_and_range_distinct() {
        let a = inline_transcript_record_id("collector:h", "claude", "acct", "p/s.jsonl", 0, 8);
        let b = inline_transcript_record_id("collector:h", "claude", "acct", "p/s.jsonl", 0, 8);
        let c = inline_transcript_record_id("collector:h", "claude", "acct", "p/s.jsonl", 8, 16);
        assert_eq!(a, b);
        assert_ne!(a, c);
        assert!(a.starts_with("ti-") && a.len() == 35);
    }
}
