//! The session event log an executor writes for a worker that writes none
//! of its own.
//!
//! The harness tees its stdout envelope into
//! `$BRO_HOME/harness-sessions/<session>.events.jsonl` as `{ts, event}`
//! records, skipping `stream_event` partials. A vendor CLI worker emits the
//! same envelope but keeps no such log, so the executor that owns its stdout
//! (the daemon's local executor or fleetd) writes the identical shape at the
//! spec's pinned path. Both executors share this writer so the cockpit, the
//! indexer and the replay window read one format whoever produced it.

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::Value;
use tokio::io::AsyncWriteExt as _;

/// Appends `{ts, event}` records for a worker's stdout lines.
pub struct SessionLogWriter {
    path: PathBuf,
    file: tokio::fs::File,
    next_seq: u64,
}

impl SessionLogWriter {
    /// Open (creating parents) the log at `path` for appending. A log that
    /// cannot be opened is reported and skipped: the stdout relay must not
    /// fail because the durable copy cannot be written.
    pub async fn open(path: &Path) -> Option<Self> {
        if let Some(parent) = path.parent()
            && let Err(error) = tokio::fs::create_dir_all(parent).await
        {
            eprintln!(
                "cannot create session log directory {}: {error}",
                parent.display()
            );
            return None;
        }
        match tokio::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .await
        {
            Ok(file) => Some(Self {
                path: path.to_path_buf(),
                file,
                next_seq: tokio::fs::read_to_string(path)
                    .await
                    .ok()
                    .into_iter()
                    .flat_map(|body| {
                        body.lines()
                            .filter_map(|line| serde_json::from_str::<Value>(line).ok())
                            .collect::<Vec<_>>()
                    })
                    .filter_map(|record| record["event"]["seq"].as_u64())
                    .max()
                    .unwrap_or(0)
                    .saturating_add(1),
            }),
            Err(error) => {
                eprintln!("cannot open session log {}: {error}", path.display());
                None
            }
        }
    }

    /// Give the first task log its provider session name without invalidating
    /// fleetd's pinned replay path. Both names refer to the same append stream.
    pub async fn bind_session(&self, session: &str) -> std::io::Result<()> {
        if session.is_empty()
            || session.len() > 128
            || !session
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
        {
            return Err(std::io::Error::other("invalid provider session id"));
        }
        let target = self.path.with_file_name(format!("{session}.events.jsonl"));
        if target != self.path {
            tokio::fs::hard_link(&self.path, target).await?;
        }
        Ok(())
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Record one raw stdout line. Lines that are not JSON objects and
    /// `stream_event` partials are not recorded.
    pub async fn record_line(&mut self, line: &str) {
        let Some(record) = session_log_record(line, SystemTime::now()) else {
            return;
        };
        if let Err(error) = self.file.write_all(&record).await {
            eprintln!("cannot append session log {}: {error}", self.path.display());
        }
    }

    /// CLI init events do not identify the routed provider account themselves.
    pub async fn record_provider_event(
        &mut self,
        provider: bro_core::Provider,
        line: &str,
    ) -> std::io::Result<String> {
        let mut event: Value = serde_json::from_str(line).map_err(std::io::Error::other)?;
        if event["type"] == "system" && event["subtype"] == "init" {
            event["provider"] = provider.as_str().into();
        }
        self.sequence_and_record(&event.to_string()).await
    }

    /// Stamp and append the same envelope that will be relayed.
    /// Partials stay unsequenced because they are not replayed.
    pub async fn sequence_and_record(&mut self, line: &str) -> std::io::Result<String> {
        let mut event: Value = serde_json::from_str(line).map_err(std::io::Error::other)?;
        if event["type"] == "stream_event" {
            return Ok(line.to_string());
        }
        let object = event
            .as_object_mut()
            .ok_or_else(|| std::io::Error::other("event must be an object"))?;
        object.insert("seq".into(), self.next_seq.into());
        self.next_seq = self
            .next_seq
            .checked_add(1)
            .ok_or_else(|| std::io::Error::other("event sequence exhausted"))?;
        let line = event.to_string();
        let record = session_log_record(&line, SystemTime::now()).expect("object event");
        self.file.write_all(&record).await?;
        self.file.flush().await?;
        Ok(line)
    }

    pub async fn finish(&mut self) {
        let _ = self.file.flush().await;
    }
}

/// The `{ts, event}` record for one stdout line, newline terminated, or
/// `None` when the line is not an event to keep.
pub fn session_log_record(line: &str, at: SystemTime) -> Option<Vec<u8>> {
    let event: Value = serde_json::from_str(line).ok()?;
    if !event.is_object() || event.get("type").and_then(Value::as_str) == Some("stream_event") {
        return None;
    }
    let mut record = serde_json::to_vec(&serde_json::json!({
        "ts": rfc3339_millis(at),
        "event": event,
    }))
    .ok()?;
    record.push(b'\n');
    Some(record)
}

/// `2026-06-10T12:34:56.789Z` for `at`, without a date crate.
pub fn rfc3339_millis(at: SystemTime) -> String {
    let since_epoch = at.duration_since(UNIX_EPOCH).unwrap_or_default();
    let secs = since_epoch.as_secs() as i64;
    let millis = since_epoch.subsec_millis();
    let days = secs.div_euclid(86_400);
    let time_of_day = secs.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}.{millis:03}Z",
        time_of_day / 3600,
        (time_of_day % 3600) / 60,
        time_of_day % 60,
    )
}

/// Proleptic Gregorian civil date from days since 1970-01-01
/// (Howard Hinnant's `civil_from_days`).
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if month <= 2 { year + 1 } else { year }, month, day)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn timestamps_render_as_rfc3339_with_millis() {
        assert_eq!(rfc3339_millis(UNIX_EPOCH), "1970-01-01T00:00:00.000Z");
        let at = UNIX_EPOCH + Duration::from_millis(1_781_181_296_789);
        assert_eq!(rfc3339_millis(at), "2026-06-11T12:34:56.789Z");
        // A leap day and a year boundary.
        let leap = UNIX_EPOCH + Duration::from_secs(1_709_164_800);
        assert_eq!(rfc3339_millis(leap), "2024-02-29T00:00:00.000Z");
        let eve = UNIX_EPOCH + Duration::from_secs(1_735_689_599);
        assert_eq!(rfc3339_millis(eve), "2024-12-31T23:59:59.000Z");
    }

    #[test]
    fn records_wrap_events_and_skip_partials_and_noise() {
        let at = UNIX_EPOCH + Duration::from_millis(1_000);
        let record = session_log_record(r#"{"type":"assistant","seq":3}"#, at).unwrap();
        let parsed: Value = serde_json::from_slice(&record).unwrap();
        assert_eq!(parsed["ts"], "1970-01-01T00:00:01.000Z");
        assert_eq!(parsed["event"]["type"], "assistant");
        assert_eq!(parsed["event"]["seq"], 3);
        assert_eq!(record.last(), Some(&b'\n'));

        assert!(session_log_record(r#"{"type":"stream_event","event":{}}"#, at).is_none());
        assert!(session_log_record("not json", at).is_none());
        assert!(session_log_record("[1,2]", at).is_none());
    }

    #[tokio::test]
    async fn resume_preserves_sequence_and_the_pinned_replay_path() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("task.events.jsonl");
        let canonical = dir.path().join("thread-1.events.jsonl");
        let mut writer = SessionLogWriter::open(&path).await.unwrap();
        writer.bind_session("thread-1").await.unwrap();
        assert!(writer.bind_session("../escape").await.is_err());
        let first = writer
            .sequence_and_record(r#"{"type":"system","subtype":"init","session_id":"thread-1"}"#)
            .await
            .unwrap();
        writer
            .sequence_and_record(r#"{"type":"stream_event"}"#)
            .await
            .unwrap();
        drop(writer);
        let mut resumed = SessionLogWriter::open(&canonical).await.unwrap();
        let second = resumed
            .sequence_and_record(r#"{"type":"result","session_id":"thread-1"}"#)
            .await
            .unwrap();
        let records: Vec<Value> = tokio::fs::read_to_string(&path)
            .await
            .unwrap()
            .lines()
            .map(|s| serde_json::from_str(s).unwrap())
            .collect();
        assert_eq!(records.len(), 2);
        assert_eq!(
            records[0]["event"],
            serde_json::from_str::<Value>(&first).unwrap()
        );
        assert_eq!(
            records[1]["event"],
            serde_json::from_str::<Value>(&second).unwrap()
        );
        assert_eq!(records[0]["event"]["seq"], 1);
        assert_eq!(records[1]["event"]["seq"], 2);
    }

    #[tokio::test]
    async fn writer_appends_records_at_the_pinned_path() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sessions").join("s.events.jsonl");
        let mut writer = SessionLogWriter::open(&path).await.unwrap();
        writer
            .record_line(r#"{"type":"system","subtype":"init"}"#)
            .await;
        writer
            .record_line(r#"{"type":"stream_event","event":{"type":"message_start"}}"#)
            .await;
        writer
            .record_line(r#"{"type":"result","is_error":false}"#)
            .await;
        writer.finish().await;

        let body = tokio::fs::read_to_string(&path).await.unwrap();
        let lines: Vec<Value> = body
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0]["event"]["subtype"], "init");
        assert_eq!(lines[1]["event"]["type"], "result");
    }
}
