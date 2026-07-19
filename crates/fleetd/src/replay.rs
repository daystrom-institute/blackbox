//! Replaying a session's durable event log to a reconnecting daemon.
//!
//! The slice 5 contract: there is NO in-memory replay buffer. The session's
//! event-log JSONL is the backlog and the daemon owns its own cursor. This is
//! the collector shape: durable file as backlog, consumer-owned cursor.
//!
//! Log line shape, written by the harness child's `EventLog`:
//!
//! ```text
//! {"ts":"2026-07-19T12:34:56.789Z","event":{ ...envelope..., "seq": 42 }}
//! ```
//!
//! Two things follow from that shape:
//!
//! 1. What fleetd relays is the INNER `event` object, re-serialized, because
//!    that is byte-for-byte the shape a live stdout line has. A daemon must
//!    not have to care whether an event arrived live or by replay.
//! 2. Replay is seq-positioned, so a line carrying no `event.seq` is skipped.
//!    That matches `EventLog::max_seq_in_log`, which computes the harness's
//!    own high-water mark the same way. A log written entirely by a pre-seq
//!    harness build therefore replays as empty rather than as an unordered
//!    dump the daemon cannot position against its cursor.
//!
//! Replay is chunked and yields between chunks. A session with a very long
//! log must not starve live control traffic on the same connection.

use std::path::Path;

use tokio::io::{AsyncBufReadExt, BufReader};

/// Maximum events fleetd sends before yielding back to the runtime.
pub const MAX_EVENTS_PER_CHUNK: usize = 256;
/// Maximum payload bytes fleetd accumulates before yielding back.
pub const MAX_BYTES_PER_CHUNK: usize = 512 * 1024;

/// One replayable event: the inner envelope, re-serialized, plus its seq.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReplayLine {
    pub seq: u64,
    pub line: String,
}

/// What fleetd should do about a `ReplayFrom` request, decided by a single
/// bounded scan of the log before any event is sent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReplayDecision {
    /// The log does not reach back far enough to satisfy `from_seq`. The
    /// daemon learns the exact window that DOES exist so it can choose
    /// between resuming at `earliest_available` (accepting a documented gap)
    /// and abandoning the session's history.
    Unavailable {
        earliest_available: u64,
        latest_available: u64,
    },
    /// Replay can proceed; `latest_available` is the seq the stream will end
    /// at (0 when there is nothing to send).
    Stream { latest_available: u64 },
}

/// The seq window a log covers: `(earliest, latest)` over seq-carrying lines,
/// or `None` when the file is absent, empty, or entirely pre-seq.
///
/// Best-effort and read-only, exactly like `EventLog::max_seq_in_log`: a
/// malformed line (a partial write from a crash) is skipped, not fatal.
pub async fn log_window(path: &Path) -> std::io::Result<Option<(u64, u64)>> {
    let file = match tokio::fs::File::open(path).await {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    let mut lines = BufReader::new(file).lines();
    let mut window: Option<(u64, u64)> = None;
    while let Some(line) = lines.next_line().await? {
        let Some(seq) = logged_seq(&line) else {
            continue;
        };
        window = Some(match window {
            None => (seq, seq),
            Some((earliest, latest)) => (earliest.min(seq), latest.max(seq)),
        });
    }
    Ok(window)
}

/// Decide whether a replay from `from_seq` (exclusive) can be served.
pub async fn plan_replay(path: &Path, from_seq: u64) -> std::io::Result<ReplayDecision> {
    let Some((earliest, latest)) = log_window(path).await? else {
        // No log content at all. Asking from the beginning is trivially
        // satisfiable (there is simply nothing yet); asking to resume after a
        // seq we have no record of is a genuine gap.
        return Ok(if from_seq == 0 {
            ReplayDecision::Stream {
                latest_available: 0,
            }
        } else {
            ReplayDecision::Unavailable {
                earliest_available: 0,
                latest_available: 0,
            }
        });
    };
    // The daemon wants everything with seq > from_seq. The log covers that
    // iff its oldest retained event is no newer than from_seq + 1.
    if earliest > from_seq.saturating_add(1) {
        return Ok(ReplayDecision::Unavailable {
            earliest_available: earliest,
            latest_available: latest,
        });
    }
    Ok(ReplayDecision::Stream {
        latest_available: latest,
    })
}

/// A bounded, chunked reader over a session's event log.
///
/// The whole file is never held in memory: each `next_chunk` accumulates at
/// most [`MAX_EVENTS_PER_CHUNK`] events or [`MAX_BYTES_PER_CHUNK`] bytes, and
/// the caller yields between chunks.
pub struct ReplayStream {
    lines: tokio::io::Lines<BufReader<tokio::fs::File>>,
    from_seq: u64,
    max_events: usize,
    max_bytes: usize,
}

impl ReplayStream {
    /// Open a replay stream over `path`, skipping events at or below
    /// `from_seq`. Returns `Ok(None)` when the log does not exist.
    pub async fn open(path: &Path, from_seq: u64) -> std::io::Result<Option<Self>> {
        Self::open_with_limits(path, from_seq, MAX_EVENTS_PER_CHUNK, MAX_BYTES_PER_CHUNK).await
    }

    /// Chunk-limit-injecting constructor, so tests can exercise chunking
    /// without writing megabyte fixtures.
    pub async fn open_with_limits(
        path: &Path,
        from_seq: u64,
        max_events: usize,
        max_bytes: usize,
    ) -> std::io::Result<Option<Self>> {
        let file = match tokio::fs::File::open(path).await {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error),
        };
        Ok(Some(Self {
            lines: BufReader::new(file).lines(),
            from_seq,
            max_events: max_events.max(1),
            max_bytes: max_bytes.max(1),
        }))
    }

    /// The next bounded batch of replayable events, or `None` at end of log.
    pub async fn next_chunk(&mut self) -> std::io::Result<Option<Vec<ReplayLine>>> {
        let mut chunk: Vec<ReplayLine> = Vec::new();
        let mut bytes = 0_usize;
        while let Some(raw) = self.lines.next_line().await? {
            let Some((seq, line)) = replayable(&raw, self.from_seq) else {
                continue;
            };
            bytes += line.len();
            chunk.push(ReplayLine { seq, line });
            if chunk.len() >= self.max_events || bytes >= self.max_bytes {
                return Ok(Some(chunk));
            }
        }
        Ok((!chunk.is_empty()).then_some(chunk))
    }
}

/// The `event.seq` of a log line, if it has one.
fn logged_seq(raw: &str) -> Option<u64> {
    serde_json::from_str::<serde_json::Value>(raw)
        .ok()?
        .get("event")?
        .get("seq")?
        .as_u64()
}

/// Extract the inner `event` object of a log line as the string a live stdout
/// relay would have carried, when its seq is strictly greater than `from_seq`.
fn replayable(raw: &str, from_seq: u64) -> Option<(u64, String)> {
    let value: serde_json::Value = serde_json::from_str(raw).ok()?;
    let event = value.get("event")?;
    let seq = event.get("seq")?.as_u64()?;
    if seq <= from_seq {
        return None;
    }
    Some((seq, serde_json::to_string(event).ok()?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    async fn write_log(dir: &Path, name: &str, seqs: &[u64]) -> PathBuf {
        let path = dir.join(name);
        let mut body = String::new();
        for seq in seqs {
            body.push_str(&format!(
                r#"{{"ts":"2026-07-19T00:00:00Z","event":{{"type":"assistant","seq":{seq}}}}}"#
            ));
            body.push('\n');
        }
        tokio::fs::write(&path, body).await.unwrap();
        path
    }

    async fn drain(stream: &mut ReplayStream) -> Vec<Vec<ReplayLine>> {
        let mut chunks = Vec::new();
        while let Some(chunk) = stream.next_chunk().await.unwrap() {
            chunks.push(chunk);
        }
        chunks
    }

    #[tokio::test]
    async fn window_spans_seq_carrying_lines_only() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let path = write_log(&root, "s.events.jsonl", &[10, 11, 12]).await;
        assert_eq!(log_window(&path).await.unwrap(), Some((10, 12)));

        // Absent log has no window at all.
        assert_eq!(
            log_window(&root.join("missing.events.jsonl"))
                .await
                .unwrap(),
            None
        );
    }

    /// A crash can leave a half-written final line; the scan must skip it
    /// rather than failing the whole replay.
    #[tokio::test]
    async fn malformed_and_pre_seq_lines_are_skipped() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let path = root.join("s.events.jsonl");
        let body = concat!(
            r#"{"ts":"t","event":{"type":"a","seq":1}}"#,
            "\n",
            r#"{"ts":"t","event":{"type":"legacy"}}"#,
            "\n",
            r#"{"ts":"t","event":{"type":"a","seq":2}}"#,
            "\n",
            r#"{"ts":"t","event":{"type":"trunc"#,
        );
        tokio::fs::write(&path, body).await.unwrap();
        assert_eq!(log_window(&path).await.unwrap(), Some((1, 2)));

        let mut stream = ReplayStream::open(&path, 0).await.unwrap().unwrap();
        let events: Vec<ReplayLine> = drain(&mut stream).await.into_iter().flatten().collect();
        assert_eq!(events.iter().map(|e| e.seq).collect::<Vec<_>>(), vec![1, 2]);
    }

    /// What comes back is the INNER event object, identical to a live stdout
    /// line. The `ts` wrapper is a log-file concern and never crosses the wire.
    #[tokio::test]
    async fn replay_emits_the_inner_event_not_the_log_wrapper() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let path = write_log(&root, "s.events.jsonl", &[7]).await;
        let mut stream = ReplayStream::open(&path, 0).await.unwrap().unwrap();
        let chunk = stream.next_chunk().await.unwrap().unwrap();
        let value: serde_json::Value = serde_json::from_str(&chunk[0].line).unwrap();
        assert_eq!(value["seq"], 7);
        assert_eq!(value["type"], "assistant");
        assert!(value.get("ts").is_none(), "log wrapper must not be relayed");
        assert!(value.get("event").is_none());
    }

    #[tokio::test]
    async fn from_seq_is_exclusive() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let path = write_log(&root, "s.events.jsonl", &[1, 2, 3, 4]).await;
        let mut stream = ReplayStream::open(&path, 2).await.unwrap().unwrap();
        let events: Vec<u64> = drain(&mut stream)
            .await
            .into_iter()
            .flatten()
            .map(|e| e.seq)
            .collect();
        assert_eq!(events, vec![3, 4]);
    }

    #[tokio::test]
    async fn chunking_respects_the_event_cap() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let seqs: Vec<u64> = (1..=10).collect();
        let path = write_log(&root, "s.events.jsonl", &seqs).await;
        let mut stream = ReplayStream::open_with_limits(&path, 0, 3, usize::MAX)
            .await
            .unwrap()
            .unwrap();
        let chunks = drain(&mut stream).await;
        assert_eq!(
            chunks.iter().map(Vec::len).collect::<Vec<_>>(),
            vec![3, 3, 3, 1]
        );
        let flat: Vec<u64> = chunks.into_iter().flatten().map(|e| e.seq).collect();
        assert_eq!(flat, seqs);
    }

    #[tokio::test]
    async fn chunking_respects_the_byte_cap() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let path = write_log(&root, "s.events.jsonl", &[1, 2, 3, 4]).await;
        // A byte cap smaller than one event forces one event per chunk.
        let mut stream = ReplayStream::open_with_limits(&path, 0, usize::MAX, 1)
            .await
            .unwrap()
            .unwrap();
        let chunks = drain(&mut stream).await;
        assert_eq!(chunks.len(), 4, "byte cap must split every event");
        assert!(chunks.iter().all(|chunk| chunk.len() == 1));
    }

    #[tokio::test]
    async fn plan_detects_a_gap_below_the_retained_window() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let path = write_log(&root, "s.events.jsonl", &[90, 91, 92]).await;

        // Cursor at 3: the log starts at 90, so 4..=89 are gone.
        assert_eq!(
            plan_replay(&path, 3).await.unwrap(),
            ReplayDecision::Unavailable {
                earliest_available: 90,
                latest_available: 92,
            }
        );
        // Cursor at 89: the very next event (90) IS retained, so this is a
        // clean resume, not a gap. The boundary is from_seq + 1.
        assert_eq!(
            plan_replay(&path, 89).await.unwrap(),
            ReplayDecision::Stream {
                latest_available: 92
            }
        );
        // Cursor already past the log: nothing to send, still not a gap.
        assert_eq!(
            plan_replay(&path, 92).await.unwrap(),
            ReplayDecision::Stream {
                latest_available: 92
            }
        );
    }

    #[tokio::test]
    async fn plan_for_a_missing_log_depends_on_the_cursor() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let missing = root.join("missing.events.jsonl");

        // Fresh session, nothing written yet: nothing to replay, no gap.
        assert_eq!(
            plan_replay(&missing, 0).await.unwrap(),
            ReplayDecision::Stream {
                latest_available: 0
            }
        );
        // Daemon holds a cursor for events we have no record of: real gap.
        assert_eq!(
            plan_replay(&missing, 5).await.unwrap(),
            ReplayDecision::Unavailable {
                earliest_available: 0,
                latest_available: 0,
            }
        );
    }

    #[tokio::test]
    async fn opening_a_missing_log_is_not_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let stream = ReplayStream::open(&root.join("missing.jsonl"), 0)
            .await
            .unwrap();
        assert!(stream.is_none());
    }
}
