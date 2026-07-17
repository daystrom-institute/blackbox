//! Increment record construction: turn a complete-line byte buffer into one or
//! more `RecordEnvelope`s that satisfy the wire contract in
//! `bro_capabilities::records`.
//!
//! Invariants enforced here (the server validates all of them, so we must not
//! emit a record it will reject):
//! - every increment ends on `\n` (chunk boundaries fall on line boundaries);
//! - each record's compact JSON stays under `MAX_RECORD_BYTES` (we target
//!   `TARGET_RAW_CHUNK` raw bytes so base64 + envelope overhead stays clear);
//! - per-stream increments are ascending and contiguous (chunks tile the input
//!   buffer left to right with no gaps or overlaps);
//! - the deterministic `record_id` comes from
//!   `inline_transcript_record_id(...)` so replays dedupe.

use std::collections::BTreeMap;
use std::path::Path;

use base64::Engine as _;
use bro_capabilities::{
    MAX_RECORD_BYTES, RecordEnvelope, TRANSCRIPT_INCREMENT_KIND, inline_transcript_record_id,
};

use crate::config::is_safe_component;

/// Target raw bytes per record. Base64 inflates by ~4/3, so ~512 KiB raw ->
/// ~683 KiB encoded, comfortably under the 1 MiB `MAX_RECORD_BYTES` even with
/// the JSON envelope. A single line larger than this still becomes one record
/// (we never split a line), guarded by an explicit size check.
pub const TARGET_RAW_CHUNK: usize = 512 * 1024;

/// Identity of one shippable stream (a single source file).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StreamKey {
    /// `"claude"` or `"codex"`.
    pub source: String,
    pub account: String,
    pub session_id: String,
    /// Forward-slash path relative to the account root, traversal-free.
    pub relative_path: String,
}

impl StreamKey {
    /// Server-side stream identity: `producer/source/account/relative_path`.
    pub fn stream_id(&self, producer: &str) -> String {
        format!(
            "{producer}/{}/{}/{}",
            self.source, self.account, self.relative_path
        )
    }

    /// Validate every component against the wire contract's safe-component
    /// rules. Returns the offending piece on failure so the shipper can log a
    /// precise skip reason.
    pub fn validate(&self) -> Result<(), String> {
        if !crate::config::is_safe_component(&self.source) {
            return Err(format!("unsafe source {:?}", self.source));
        }
        if !is_safe_component(&self.account) {
            return Err(format!("unsafe account {:?}", self.account));
        }
        if !is_safe_component(&self.session_id) {
            return Err(format!("unsafe session_id {:?}", self.session_id));
        }
        validate_relative_path(&self.relative_path)
    }
}

/// Split `data` (which MUST end on `\n`) into a contiguous ascending sequence
/// of increment records starting at file offset `base_offset`. Oversized
/// single lines split into continuation chunks (`line_complete=false` on all
/// but the last), so building never fails.
///
/// `next_cursor` allocates the monotonic per-producer record cursor stamped
/// into each record's `cursor` field.
pub fn build_increment_records(
    producer: &str,
    key: &StreamKey,
    base_offset: u64,
    data: &[u8],
    mut next_cursor: impl FnMut() -> u64,
) -> Vec<RecordEnvelope> {
    debug_assert!(
        data.last() == Some(&b'\n'),
        "build_increment_records requires a complete-line buffer"
    );
    let mut records = Vec::new();
    let mut chunk_start = 0usize; // offset within `data`

    while chunk_start < data.len() {
        let chunk_end = next_chunk_end(data, chunk_start);
        let chunk = &data[chunk_start..chunk_end];
        let byte_start = base_offset + chunk_start as u64;
        let byte_end = base_offset + chunk_end as u64;

        let record = increment_record(producer, key, byte_start, byte_end, chunk, next_cursor());
        if compact_len(&record) > MAX_RECORD_BYTES {
            // A single line too large for one record (only possible when the
            // chunk is exactly one line; next_chunk_end would have split
            // earlier otherwise). Ship it as byte-contiguous sub-chunks where
            // every chunk but the last declares line_complete=false; the
            // server relaxes the ends-on-newline check for exactly those.
            // Real transcripts hit this on large tool results (>1MB lines),
            // and skipping would wedge the stream at this line forever.
            let mut sub_start = chunk_start;
            while sub_start < chunk_end {
                let sub_end = (sub_start + TARGET_RAW_CHUNK).min(chunk_end);
                let mut record = increment_record(
                    producer,
                    key,
                    base_offset + sub_start as u64,
                    base_offset + sub_end as u64,
                    &data[sub_start..sub_end],
                    next_cursor(),
                );
                if sub_end < chunk_end {
                    let payload = record
                        .payload
                        .as_object_mut()
                        .expect("increment payload is always an object");
                    payload.insert("line_complete".into(), serde_json::Value::Bool(false));
                }
                records.push(record);
                sub_start = sub_end;
            }
        } else {
            records.push(record);
        }
        chunk_start = chunk_end;
    }
    records
}

/// Find the end offset of the next chunk: accumulate whole lines until adding
/// the next line would exceed `TARGET_RAW_CHUNK`, but always take at least one
/// line so progress is guaranteed even for an oversized line.
fn next_chunk_end(data: &[u8], start: usize) -> usize {
    let mut cursor = start;
    let mut chunk_end = start;
    let mut took_a_line = false;
    while cursor < data.len() {
        // `data` ends on '\n', so every rposition-from-cursor find succeeds.
        let newline = data[cursor..]
            .iter()
            .position(|byte| *byte == b'\n')
            .map(|idx| cursor + idx + 1)
            .unwrap_or(data.len());
        let candidate_len = newline - start;
        if took_a_line && candidate_len > TARGET_RAW_CHUNK {
            break;
        }
        chunk_end = newline;
        took_a_line = true;
        cursor = newline;
        if candidate_len >= TARGET_RAW_CHUNK {
            break;
        }
    }
    chunk_end
}

fn increment_record(
    producer: &str,
    key: &StreamKey,
    byte_start: u64,
    byte_end: u64,
    bytes: &[u8],
    cursor: u64,
) -> RecordEnvelope {
    let record_id = inline_transcript_record_id(
        producer,
        &key.source,
        &key.account,
        &key.relative_path,
        byte_start,
        byte_end,
    );
    let mut attributes = BTreeMap::new();
    attributes.insert("source".to_string(), key.source.clone());
    attributes.insert("account".to_string(), key.account.clone());
    attributes.insert("session_id".to_string(), key.session_id.clone());
    attributes.insert("relative_path".to_string(), key.relative_path.clone());

    RecordEnvelope {
        record_id,
        producer: producer.to_string(),
        cursor: cursor.to_string(),
        kind: TRANSCRIPT_INCREMENT_KIND.to_string(),
        occurred_at: None,
        subject: Some(key.session_id.clone()),
        attributes,
        payload: serde_json::json!({
            "byte_start": byte_start,
            "byte_end": byte_end,
            "bytes_b64": base64::engine::general_purpose::STANDARD.encode(bytes),
        }),
    }
}

fn compact_len(record: &RecordEnvelope) -> usize {
    serde_json::to_vec(record)
        .map(|v| v.len())
        .unwrap_or(usize::MAX)
}

fn validate_relative_path(path: &str) -> Result<(), String> {
    if path.is_empty() || path.len() > 1024 {
        return Err(format!("relative_path {path:?} has an unusable length"));
    }
    if path.split('/').all(is_safe_component) {
        Ok(())
    } else {
        Err(format!("relative_path {path:?} has an unsafe component"))
    }
}

/// Derive a forward-slash, traversal-free relative path of `file` under `root`.
/// Returns `None` when `file` is not under `root` or any component is unsafe.
pub fn relative_path_under(root: &Path, file: &Path) -> Option<String> {
    let relative = file.strip_prefix(root).ok()?;
    let mut parts = Vec::new();
    for component in relative.components() {
        match component {
            std::path::Component::Normal(part) => {
                let part = part.to_str()?;
                if !is_safe_component(part) {
                    return None;
                }
                parts.push(part.to_string());
            }
            // CurDir / ParentDir / RootDir / Prefix are all traversal risks.
            _ => return None,
        }
    }
    if parts.is_empty() {
        return None;
    }
    Some(parts.join("/"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use bro_capabilities::inline_transcript_increments;

    fn key() -> StreamKey {
        StreamKey {
            source: "claude".into(),
            account: "claude".into(),
            session_id: "sess-1".into(),
            relative_path: "projects/repo-a/sess-1.jsonl".into(),
        }
    }

    #[test]
    fn round_trips_through_server_validation() {
        let data = b"{\"a\":1}\n{\"b\":2}\n{\"c\":3}\n";
        let mut cursor = 0u64;
        let records = build_increment_records("collector:host-b1", &key(), 0, data, || {
            cursor += 1;
            cursor
        });
        // Small input -> a single contiguous record covering the whole buffer.
        assert_eq!(records.len(), 1);
        let increments = inline_transcript_increments(&records).unwrap();
        assert_eq!(increments.len(), 1);
        assert_eq!(increments[0].byte_start, 0);
        assert_eq!(increments[0].byte_end, data.len() as u64);
        assert_eq!(increments[0].bytes, data);
        assert_eq!(
            increments[0].stream,
            "collector:host-b1/claude/claude/projects/repo-a/sess-1.jsonl"
        );
    }

    #[test]
    fn chunks_stay_contiguous_and_respect_target_size() {
        // Each line ~1 KiB; enough lines to force several TARGET_RAW_CHUNK
        // chunks. Every chunk must end on a newline and tile contiguously.
        let line = format!("{{\"pad\":\"{}\"}}\n", "x".repeat(1000));
        let mut data = Vec::new();
        for _ in 0..2000 {
            data.extend_from_slice(line.as_bytes());
        }
        let mut cursor = 0u64;
        let records = build_increment_records("collector:h", &key(), 0, &data, || {
            cursor += 1;
            cursor
        });
        assert!(records.len() > 1, "large buffer must split into chunks");

        let increments = inline_transcript_increments(&records).unwrap();
        // Server enforces ascending + contiguous within a batch; the fact this
        // returns Ok proves tiling. Cross-check start/end coverage explicitly.
        assert_eq!(increments.first().unwrap().byte_start, 0);
        assert_eq!(increments.last().unwrap().byte_end, data.len() as u64);
        for pair in increments.windows(2) {
            assert_eq!(pair[0].byte_end, pair[1].byte_start);
            assert_eq!(*pair[0].bytes.last().unwrap(), b'\n');
            assert!(pair[0].bytes.len() <= TARGET_RAW_CHUNK + line.len());
        }
    }

    #[test]
    fn oversized_single_line_splits_into_continuation_chunks() {
        // One line whose base64 + envelope exceeds MAX_RECORD_BYTES: it must
        // split into byte-contiguous sub-chunks where only the last ends the
        // line, every record fits the cap, and the server contract accepts
        // the sequence (line_complete=false relaxes the newline rule).
        let mut data = vec![b'x'; MAX_RECORD_BYTES];
        data.push(b'\n');
        let mut cursor = 0u64;
        let records = build_increment_records("collector:h", &key(), 0, &data, || {
            cursor += 1;
            cursor
        });
        assert!(records.len() > 1, "oversized line must split");
        for record in &records {
            assert!(serde_json::to_vec(record).unwrap().len() <= MAX_RECORD_BYTES);
        }
        for (index, record) in records.iter().enumerate() {
            let complete = record
                .payload
                .get("line_complete")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(true);
            assert_eq!(complete, index == records.len() - 1);
        }

        let increments = inline_transcript_increments(&records).unwrap();
        assert_eq!(increments.first().unwrap().byte_start, 0);
        assert_eq!(increments.last().unwrap().byte_end, data.len() as u64);
        for pair in increments.windows(2) {
            assert_eq!(pair[0].byte_end, pair[1].byte_start);
        }
        assert_eq!(*increments.last().unwrap().bytes.last().unwrap(), b'\n');

        // A truncated sequence (missing the final line-completing chunk) is
        // still valid at the wire layer; byte contiguity carries recovery
        // across ticks, and the archive tail reads as torn until completed.
        let partial = inline_transcript_increments(&records[..records.len() - 1]);
        assert!(partial.is_ok());
    }

    #[test]
    fn relative_path_under_rejects_traversal() {
        let root = Path::new("/home/me/.claude");
        assert_eq!(
            relative_path_under(root, Path::new("/home/me/.claude/projects/x/s.jsonl")).as_deref(),
            Some("projects/x/s.jsonl")
        );
        assert_eq!(
            relative_path_under(root, Path::new("/home/me/.claude/history.jsonl")).as_deref(),
            Some("history.jsonl")
        );
        assert!(relative_path_under(root, Path::new("/etc/passwd")).is_none());
    }

    #[test]
    fn stream_key_validate_catches_unsafe_pieces() {
        let mut k = key();
        assert!(k.validate().is_ok());
        k.relative_path = "projects/../etc/passwd".into();
        assert!(k.validate().is_err());
    }
}
