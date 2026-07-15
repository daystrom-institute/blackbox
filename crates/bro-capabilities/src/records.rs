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
}
