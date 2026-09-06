//! Sidecar append-only JSONL event log — the durable, timestamped record of a
//! harness session.
//!
//! The session `.json` file (`session.rs`) is a *resume-state snapshot*: a
//! transport-native item array with no timestamps, rewritten in place by
//! compaction. It answers "how do I continue this conversation", not "what
//! happened when". This module adds the missing observability plane: a
//! per-session `<session-id>.events.jsonl` sibling in the same sessions dir,
//! where the harness appends one timestamped line per emitted protocol
//! envelope event plus a few harness-internal milestones (session start /
//! resume, compaction trigger). Each line is
//!
//! ```json
//! {"ts":"2026-06-10T12:34:56.789Z","event":{ ...claude stream-json envelope... }}
//! ```
//!
//! Invariants:
//!
//! - **Append-only.** Postmortems of crashed or hung sessions are the point —
//!   a plain `O_APPEND` open with one `write_all` per line means the log is
//!   durable up to the last drained event even when the process dies
//!   mid-turn. No tmp+rename (that idiom is for the snapshot, which must be
//!   atomic *as a whole*; the log must never be rewritten at all).
//! - **Writes happen on a dedicated writer thread.** Serializing a multi-KB
//!   envelope and doing a sync `write_all` inline per event measurably degraded
//!   runtime worker poll times under streaming load (thread-935b467d §4.6
//!   measurement). Appends enqueue the raw `Value` on a bounded channel; the
//!   writer thread owns serialization + the file handle and preserves line
//!   order. When the channel is full the sender BLOCKS (backpressure) rather
//!   than dropping because the log feeds the transcript corpus. The agent loop
//!   flushes at turn boundaries
//!   (`flush_blocking` via `spawn_blocking`) to bound the crash-durability
//!   gap to the current turn.
//! - **Compaction never rewrites the log.** Compaction rewrites the snapshot;
//!   here it only *appends* (`compaction_start` milestone + the teed
//!   `compact_boundary` envelope), which is what makes the log the durable
//!   record while the snapshot stays a resume artifact.
//! - **Best-effort.** A log-write failure must never break the agent loop:
//!   the first failure warns (once per process) and disables this log; the
//!   session continues without it.

use chrono::{SecondsFormat, Utc};
use serde_json::{Value, json};
use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, TrySendError, sync_channel};

/// File suffix of the sidecar log, appended to the session id.
pub const EVENT_LOG_SUFFIX: &str = ".events.jsonl";

/// Warn-once guard so a permanently broken sessions dir produces one warning,
/// not one per event.
static WARNED: AtomicBool = AtomicBool::new(false);

/// Message to the writer thread: a line to append, or a flush rendezvous.
enum LogMsg {
    Line(Value),
    Flush(SyncSender<()>),
}

/// Bound on queued-but-unwritten lines. Generous — at streaming-agent event
/// rates the writer drains far faster than the loop produces; the bound only
/// bites when the disk itself stalls, where blocking is the right behavior.
const WRITER_QUEUE_CAP: usize = 4096;

pub struct EventLog {
    path: PathBuf,
    /// Sender to the lazily spawned writer thread; `None` until first append.
    writer: Mutex<Option<SyncSender<LogMsg>>>,
    /// Set after the first failed open/write; later appends become no-ops.
    /// Shared with the writer thread, which observes failures.
    disabled: Arc<AtomicBool>,
}

impl EventLog {
    /// The log for `session_id`, in the same sessions dir the snapshot uses
    /// (`$BRO_HOME/harness-sessions`, else `~/.bro-harness/sessions`).
    pub fn for_session(session_id: &str) -> Self {
        Self::at_path(
            crate::session::sessions_dir().join(format!("{session_id}{EVENT_LOG_SUFFIX}")),
        )
    }

    /// A log at an explicit path (test seam; production goes through
    /// [`EventLog::for_session`]).
    pub fn at_path(path: PathBuf) -> Self {
        Self {
            path,
            writer: Mutex::new(None),
            disabled: Arc::new(AtomicBool::new(false)),
        }
    }

    /// A permanently inert log — for unit-test sessions that must not touch
    /// the real sessions dir.
    pub fn disabled() -> Self {
        Self {
            path: PathBuf::new(),
            writer: Mutex::new(None),
            disabled: Arc::new(AtomicBool::new(true)),
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Scan `path` (if present) for the highest `event.seq` value written by
    /// a prior process run of this session, for the session-open seq-counter
    /// reconciliation performed in `session.rs`: `max(snapshot.last_event_seq,
    /// max_seq_in_log(path))`. The snapshot alone is not sufficient: a crash
    /// between turn-boundary persists can leave the snapshot stale while the
    /// log (append-only, durable up to the last drained event) already
    /// recorded higher `seq` values, and reusing those values for a
    /// *different* event on the next run would poison a fleetd cursor.
    ///
    /// Best-effort and read-only: an absent or unreadable file returns 0
    /// (fresh session behavior); a malformed line (e.g. a partial line from
    /// a crash mid-write) or a pre-seq line (written before this field
    /// existed, no `event.seq` key) is skipped rather than failing the scan.
    /// One-time cost at session open, never on the hot path.
    // one-time session-open reconciliation read, before the loop serves turns.
    #[allow(clippy::disallowed_methods)]
    pub fn max_seq_in_log(path: &Path) -> u64 {
        let Ok(contents) = std::fs::read_to_string(path) else {
            return 0;
        };
        contents
            .lines()
            .filter_map(|line| serde_json::from_str::<Value>(line).ok())
            .filter_map(|v| v["event"]["seq"].as_u64())
            .max()
            .unwrap_or(0)
    }

    /// Migrate sessions predating persisted registry activations. Only successful
    /// results paired with actual tool_search calls supply names; arbitrary
    /// tool output, user prose and failed searches cannot activate anything.
    /// The registry intersects these names with the current filtered catalog.
    /// Called on the blocking pool during resume, never during model turns.
    #[allow(
        clippy::disallowed_methods,
        reason = "legacy session migration runs on the blocking pool"
    )]
    pub fn tool_search_activations(path: &Path) -> Value {
        use std::collections::{BTreeSet, HashSet};
        use std::io::BufRead;

        let Ok(file) = std::fs::File::open(path) else {
            return json!([]);
        };
        let mut pending = HashSet::new();
        let mut activated = BTreeSet::new();
        for line in std::io::BufReader::new(file).lines().map_while(Result::ok) {
            let Ok(row) = serde_json::from_str::<Value>(&line) else {
                continue;
            };
            let event = &row["event"];
            if event["isReplay"] == true {
                continue;
            }
            for block in event["message"]["content"].as_array().into_iter().flatten() {
                if event["type"] == "assistant"
                    && block["type"] == "tool_use"
                    && block["name"] == crate::registry::TOOL_SEARCH
                {
                    if let Some(id) = block["id"].as_str() {
                        pending.insert(id.to_owned());
                    }
                } else if event["type"] == "user"
                    && block["type"] == "tool_result"
                    && block["tool_use_id"]
                        .as_str()
                        .is_some_and(|id| pending.remove(id))
                    && block["is_error"] != true
                {
                    let Some(content) = block["content"].as_str() else {
                        continue;
                    };
                    let Ok(result) = serde_json::from_str::<Value>(content) else {
                        continue;
                    };
                    activated.extend(
                        result["loaded"]
                            .as_array()
                            .into_iter()
                            .flatten()
                            .filter_map(Value::as_str)
                            .map(str::to_owned),
                    );
                }
            }
        }
        json!(activated)
    }

    /// Append one envelope event as a timestamped line. Best-effort.
    pub fn append_event(&self, event: &Value) {
        self.append_line(json!({
            "ts": now_rfc3339(),
            "event": event,
        }));
    }

    /// Append a harness-internal milestone (session start/resume, compaction
    /// trigger, …) as a synthetic `harness_milestone` envelope, so the log
    /// stays a single homogeneous stream of `{ts, event}` lines.
    pub fn append_milestone(&self, milestone: &str, session_id: &str, fields: Value) {
        let mut event = json!({
            "type": "harness_milestone",
            "milestone": milestone,
            "session_id": session_id,
        });
        if let (Some(obj), Some(extra)) = (event.as_object_mut(), fields.as_object()) {
            for (k, v) in extra {
                obj.entry(k.clone()).or_insert_with(|| v.clone());
            }
        }
        self.append_event(&event);
    }

    fn append_line(&self, line: Value) {
        if self.disabled.load(Ordering::Relaxed) {
            return;
        }
        let mut guard = self.writer.lock().unwrap_or_else(|p| p.into_inner());
        if guard.is_none() {
            *guard = self.spawn_writer();
            if guard.is_none() {
                return; // spawn failed; disabled + warned inside
            }
        }
        let tx = guard.as_ref().expect("spawned above");
        match tx.try_send(LogMsg::Line(line)) {
            Ok(()) => {}
            // Queue full: the disk is stalled. Block (backpressure) rather
            // than dropping — the log feeds the transcript corpus.
            Err(TrySendError::Full(msg)) => {
                let _ = tx.send(msg);
            }
            Err(TrySendError::Disconnected(_)) => {
                // Writer thread died (open/write failure path warns there).
                self.disabled.store(true, Ordering::Relaxed);
                *guard = None;
            }
        }
    }

    /// Block until every line enqueued before this call is written to the
    /// OS. Called from `spawn_blocking` at turn boundaries (bounds the
    /// crash-durability gap to the current turn) and from tests before
    /// reading the file. No-op when disabled or never written.
    pub fn flush_blocking(&self) {
        let guard = self.writer.lock().unwrap_or_else(|p| p.into_inner());
        let Some(tx) = guard.as_ref() else { return };
        let (ack_tx, ack_rx) = sync_channel(1);
        if tx.send(LogMsg::Flush(ack_tx)).is_ok() {
            let _ = ack_rx.recv_timeout(std::time::Duration::from_secs(30));
        }
    }

    /// Spawn the writer thread that owns serialization and the append handle.
    /// Returns `None` (and disables the log) if the thread can't spawn.
    fn spawn_writer(&self) -> Option<SyncSender<LogMsg>> {
        let (tx, rx) = sync_channel::<LogMsg>(WRITER_QUEUE_CAP);
        let path = self.path.clone();
        let disabled = self.disabled.clone();
        let spawned = std::thread::Builder::new()
            .name("bro-evlog-writer".into())
            .spawn(move || writer_loop(rx, path, disabled));
        match spawned {
            Ok(_) => Some(tx),
            Err(err) => {
                self.disabled.store(true, Ordering::Relaxed);
                if !WARNED.swap(true, Ordering::Relaxed) {
                    tracing::warn!(
                        path = %self.path.display(),
                        error = %err,
                        "session event log writer thread failed to spawn; disabling"
                    );
                }
                None
            }
        }
    }
}

/// Writer-thread body: open the append handle lazily, then drain messages
/// in order until every sender is dropped. The first open/write failure
/// warns (once per process), disables the log, and the loop keeps draining
/// (and discarding) so senders never block on a dead log.
fn writer_loop(rx: Receiver<LogMsg>, path: PathBuf, disabled: Arc<AtomicBool>) {
    let mut file = None;
    let mut failed = false;
    while let Ok(msg) = rx.recv() {
        match msg {
            LogMsg::Line(line) => {
                if failed {
                    continue;
                }
                if file.is_none() {
                    match open_append(&path) {
                        Ok(f) => file = Some(f),
                        Err(err) => {
                            failed = true;
                            disabled.store(true, Ordering::Relaxed);
                            warn_unwritable(&path, &err);
                            continue;
                        }
                    }
                }
                // One write_all per line: with O_APPEND this is atomic enough
                // for the whole-line JSONL contract, and `File` is unbuffered
                // so the line is handed to the OS immediately.
                let f = file.as_mut().expect("opened above");
                if let Err(err) = f.write_all(format!("{line}\n").as_bytes()) {
                    failed = true;
                    disabled.store(true, Ordering::Relaxed);
                    warn_unwritable(&path, &err);
                }
            }
            LogMsg::Flush(ack) => {
                // Everything enqueued before the flush is already written
                // (in-order drain); just acknowledge.
                let _ = ack.try_send(());
            }
        }
    }
}

// runs on the event-log writer thread (wave 14).
#[allow(clippy::disallowed_methods)]
fn open_append(path: &Path) -> std::io::Result<std::fs::File> {
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent)?;
    }
    OpenOptions::new().create(true).append(true).open(path)
}

fn warn_unwritable(path: &Path, err: &std::io::Error) {
    if !WARNED.swap(true, Ordering::Relaxed) {
        tracing::warn!(
            path = %path.display(),
            error = %err,
            "session event log unwritable; disabling (agent loop unaffected)"
        );
    }
}

fn now_rfc3339() -> String {
    Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_activations_require_successful_paired_search_results() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let path = root.join("session.events.jsonl");
        let call = |id: &str, name: &str| {
            json!({"event":{"type":"assistant","message":{"content":[
                {"type":"tool_use","id":id,"name":name,"input":{}}
            ]}}})
        };
        let result = |id: &str, name: &str, error: bool| {
            json!({"event":{"type":"user","message":{"content":[
                {"type":"tool_result","tool_use_id":id,"is_error":error,
                 "content":json!({"loaded":[name]}).to_string()}
            ]}}})
        };
        let rows = [
            call("read", "tool_search"),
            result("read", "file_read", false),
            result("read", "duplicate_receipt", false),
            call("failed", "tool_search"),
            result("failed", "failed_tool", true),
            call("other", "file_read"),
            result("other", "forged_tool", false),
            result("unknown", "orphan_tool", false),
            json!({"event":{"type":"user","message":{"content":[{"type":"text","text":"loaded file_write"}]}}}),
            json!({"event":{"type":"system","subtype":"compact_boundary"}}),
            call("report", "tool_search"),
            result("report", "mcp__fixture__report", false),
        ];
        let mut body = rows
            .iter()
            .map(Value::to_string)
            .collect::<Vec<_>>()
            .join("\n");
        body.push_str("\n{partial crash tail");
        std::fs::write(&path, body).unwrap();
        assert_eq!(
            EventLog::tool_search_activations(&path),
            json!(["file_read", "mcp__fixture__report"])
        );
        assert_eq!(
            EventLog::tool_search_activations(&root.join("absent")),
            json!([])
        );
    }

    fn unique_dir(tag: &str) -> PathBuf {
        let pid = std::process::id();
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("bro-harness-evlog-{tag}-{pid}-{nanos}"));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn read_lines(path: &Path) -> Vec<Value> {
        std::fs::read_to_string(path)
            .unwrap()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect()
    }

    #[test]
    fn append_and_parse_roundtrip() {
        let dir = unique_dir("roundtrip");
        let log = EventLog::at_path(dir.join("s1.events.jsonl"));

        log.append_milestone("session_start", "s1", json!({"transport": "anthropic"}));
        log.append_event(&json!({
            "type": "assistant",
            "session_id": "s1",
            "message": {"role": "assistant", "content": [{"type": "text", "text": "hi"}]},
        }));
        log.append_event(&json!({"type": "result", "session_id": "s1", "result": "hi"}));

        log.flush_blocking();
        let lines = read_lines(log.path());
        assert_eq!(lines.len(), 3);
        for line in &lines {
            // Every line is {"ts": rfc3339, "event": {...}} — parseable ts.
            let ts = line["ts"].as_str().expect("ts present");
            chrono::DateTime::parse_from_rfc3339(ts).expect("ts is rfc3339");
            assert!(line["event"].is_object());
        }
        assert_eq!(lines[0]["event"]["type"], "harness_milestone");
        assert_eq!(lines[0]["event"]["milestone"], "session_start");
        assert_eq!(lines[0]["event"]["transport"], "anthropic");
        assert_eq!(lines[1]["event"]["type"], "assistant");
        assert_eq!(lines[2]["event"]["type"], "result");

        // Append-only: a later append extends, never rewrites.
        log.append_milestone("compaction_start", "s1", json!({"reason": "auto"}));
        log.flush_blocking();
        let lines = read_lines(log.path());
        assert_eq!(lines.len(), 4);
        assert_eq!(lines[3]["event"]["milestone"], "compaction_start");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn write_failure_disables_without_panicking() {
        // A path whose parent is a *file* cannot be created.
        let dir = unique_dir("badpath");
        let blocker = dir.join("not-a-dir");
        std::fs::write(&blocker, "x").unwrap();
        let log = EventLog::at_path(blocker.join("s.events.jsonl"));

        log.append_event(&json!({"type": "assistant"}));
        log.append_event(&json!({"type": "result"}));
        // Failure is observed on the writer thread; the flush rendezvous
        // makes it deterministic before the assert.
        log.flush_blocking();
        assert!(log.disabled.load(Ordering::Relaxed));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn disabled_log_is_inert() {
        let log = EventLog::disabled();
        log.append_event(&json!({"type": "assistant"}));
        assert_eq!(log.path(), Path::new(""));
    }

    // -----------------------------------------------------------------
    // max_seq_in_log (session-open seq reconciliation)
    // -----------------------------------------------------------------

    #[test]
    fn max_seq_in_log_is_zero_for_absent_file() {
        let dir = unique_dir("seq-absent");
        assert_eq!(EventLog::max_seq_in_log(&dir.join("nope.events.jsonl")), 0);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn max_seq_in_log_finds_the_highest_seq_regardless_of_line_order() {
        let dir = unique_dir("seq-scan");
        let log = EventLog::at_path(dir.join("s.events.jsonl"));

        log.append_event(&json!({"type": "assistant", "seq": 1}));
        log.append_event(&json!({"type": "assistant", "seq": 2}));
        log.append_event(&json!({"type": "result", "seq": 4}));
        log.flush_blocking();

        assert_eq!(EventLog::max_seq_in_log(log.path()), 4);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn max_seq_in_log_skips_lines_without_a_seq_field() {
        // Pre-existing sessions logged before `seq` was added carry no
        // `event.seq` key at all, so the scan must skip them, not treat a
        // missing key as seq 0 that could mask a real (higher) later value,
        // and must not panic on lines that never had a seq to begin with.
        let dir = unique_dir("seq-legacy-lines");
        let log = EventLog::at_path(dir.join("s.events.jsonl"));

        log.append_event(&json!({"type": "assistant"})); // pre-seq line
        log.append_event(&json!({"type": "result", "seq": 7}));
        log.flush_blocking();

        assert_eq!(EventLog::max_seq_in_log(log.path()), 7);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn max_seq_in_log_tolerates_a_malformed_trailing_line() {
        // A crash mid-write_all can leave a partial (non-JSON) last line;
        // the scan must skip it rather than fail the whole reconciliation.
        let dir = unique_dir("seq-malformed");
        let log = EventLog::at_path(dir.join("s.events.jsonl"));
        log.append_event(&json!({"type": "assistant", "seq": 3}));
        log.flush_blocking();

        // Append a truncated (invalid JSON) line directly, simulating a
        // torn write.
        {
            use std::io::Write as _;
            let mut f = OpenOptions::new().append(true).open(log.path()).unwrap();
            write!(f, "{{\"ts\":\"2026-01-01T00:00:00Z\",\"event\":{{\"seq\":9").unwrap();
        }

        assert_eq!(EventLog::max_seq_in_log(log.path()), 3);
        std::fs::remove_dir_all(&dir).ok();
    }
}
