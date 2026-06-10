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
//! - **Append-only, flushed per line.** Postmortems of crashed or hung
//!   sessions are the point — a plain `O_APPEND` open with one `write_all`
//!   per line means the log is durable up to the last completed event even
//!   when the process dies mid-turn. No tmp+rename (that idiom is for the
//!   snapshot, which must be atomic *as a whole*; the log must never be
//!   rewritten at all).
//! - **Compaction never rewrites the log.** Compaction rewrites the snapshot;
//!   here it only *appends* (`compaction_start` milestone + the teed
//!   `compact_boundary` envelope), which is what makes the log the durable
//!   record while the snapshot stays a resume artifact.
//! - **Best-effort.** A log-write failure must never break the agent loop:
//!   the first failure warns (once per process) and disables this log; the
//!   session continues without it.

use chrono::{SecondsFormat, Utc};
use serde_json::{Value, json};
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

/// File suffix of the sidecar log, appended to the session id.
pub const EVENT_LOG_SUFFIX: &str = ".events.jsonl";

/// Warn-once guard so a permanently broken sessions dir produces one warning,
/// not one per event.
static WARNED: AtomicBool = AtomicBool::new(false);

pub struct EventLog {
    path: PathBuf,
    /// Lazily opened append handle; `None` until the first append.
    file: Mutex<Option<File>>,
    /// Set after the first failed open/write; later appends become no-ops.
    disabled: AtomicBool,
}

impl EventLog {
    /// The log for `session_id`, in the same sessions dir the snapshot uses
    /// (`$BRO_HOME/harness-sessions`, else `~/.bro-harness/sessions`).
    pub fn for_session(session_id: &str) -> Self {
        Self::at_path(crate::session::sessions_dir().join(format!("{session_id}{EVENT_LOG_SUFFIX}")))
    }

    /// A log at an explicit path (test seam; production goes through
    /// [`EventLog::for_session`]).
    pub fn at_path(path: PathBuf) -> Self {
        Self {
            path,
            file: Mutex::new(None),
            disabled: AtomicBool::new(false),
        }
    }

    /// A permanently inert log — for unit-test sessions that must not touch
    /// the real sessions dir.
    pub fn disabled() -> Self {
        Self {
            path: PathBuf::new(),
            file: Mutex::new(None),
            disabled: AtomicBool::new(true),
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
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
        if let Err(err) = self.try_append(&line) {
            self.disabled.store(true, Ordering::Relaxed);
            if !WARNED.swap(true, Ordering::Relaxed) {
                tracing::warn!(
                    path = %self.path.display(),
                    error = %err,
                    "session event log unwritable; disabling (agent loop unaffected)"
                );
            }
        }
    }

    fn try_append(&self, line: &Value) -> std::io::Result<()> {
        let mut guard = self.file.lock().unwrap_or_else(|p| p.into_inner());
        if guard.is_none() {
            if let Some(parent) = self.path.parent()
                && !parent.as_os_str().is_empty()
            {
                std::fs::create_dir_all(parent)?;
            }
            *guard = Some(
                OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(&self.path)?,
            );
        }
        let file = guard.as_mut().expect("opened above");
        // One write_all per line: with O_APPEND this is atomic enough for the
        // whole-line JSONL contract, and `File` is unbuffered so the line is
        // handed to the OS immediately (no explicit flush needed).
        file.write_all(format!("{line}\n").as_bytes())
    }
}

fn now_rfc3339() -> String {
    Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true)
}

#[cfg(test)]
mod tests {
    use super::*;

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
        assert!(log.disabled.load(Ordering::Relaxed));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn disabled_log_is_inert() {
        let log = EventLog::disabled();
        log.append_event(&json!({"type": "assistant"}));
        assert_eq!(log.path(), Path::new(""));
    }
}
