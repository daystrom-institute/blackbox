//! File-attach transcript source for the zoom view.
//!
//! The harness session event log (`<sessions>/<session_id>.events.jsonl`) is
//! the single transcript coordinate space: the harness tees every emitted
//! envelope event there (minus `stream_event` partials and replay echoes), in
//! order, append-only, across every run of the session. The cockpit runs on
//! the same host as the daemon by design, so the zoom view attaches to that
//! file directly — the daemon hands the path on the roster row
//! (`transcript_path`) — instead of streaming events over HTTP.
//!
//! Because a resume reuses the session (same file), the transcript view
//! carries across task swaps with zero cursor reconciliation: the tail just
//! keeps reading the same file. `bro fleet` and `bro agent` share this
//! componentry; anything the transcript is missing is an emit-side lever in
//! the harness, not a transport feature.

use bro_fleet_client::{TranscriptItem, parse_transcript};
use serde_json::Value;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

/// Incremental reader over one session event log. Holds the byte offset of
/// the first unconsumed byte (always at a line boundary; an incomplete
/// trailing line is buffered in `partial` until its newline arrives) plus the
/// parsed transcript items, recomputed only when new events land.
pub(super) struct TranscriptFileTail {
    path: PathBuf,
    offset: u64,
    partial: Vec<u8>,
    events: Vec<Value>,
    items: Vec<TranscriptItem>,
}

impl TranscriptFileTail {
    /// Attach to a session event log and consume everything already in it.
    /// The file may not exist yet (fresh dispatch) — that's an empty
    /// transcript until the first poll that finds it.
    pub fn attach(path: impl Into<PathBuf>) -> Self {
        let mut tail = Self {
            path: path.into(),
            offset: 0,
            partial: Vec::new(),
            events: Vec::new(),
            items: Vec::new(),
        };
        tail.poll();
        tail
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Parsed transcript items for everything consumed so far.
    pub fn items(&self) -> &[TranscriptItem] {
        &self.items
    }

    /// Consume newly appended bytes. Returns `true` when the consumed state
    /// changed (new events, or a truncation reset that requires a re-flow).
    pub fn poll(&mut self) -> bool {
        let outcome = self.poll_detail();
        outcome.appended || outcome.reset
    }

    /// Like [`poll`](Self::poll), but reports only whether the file was
    /// truncated/replaced underneath us — the caller must re-flow committed
    /// scrollback in that case. Ordinary appends need no special handling.
    pub fn poll_reset(&mut self) -> bool {
        self.poll_detail().reset
    }

    fn poll_detail(&mut self) -> TailPoll {
        let Ok(mut file) = std::fs::File::open(&self.path) else {
            return TailPoll::default();
        };
        let len = file.metadata().map(|m| m.len()).unwrap_or(0);
        let mut outcome = TailPoll::default();
        if len < self.offset {
            // Truncated or replaced underneath us — restart from the top.
            self.offset = 0;
            self.partial.clear();
            self.events.clear();
            outcome.reset = true;
        }
        if len == self.offset {
            if outcome.reset {
                self.items = parse_transcript(&self.events);
            }
            return outcome;
        }
        if file.seek(SeekFrom::Start(self.offset)).is_err() {
            return outcome;
        }
        let mut buf = Vec::new();
        if file
            .take(len.saturating_sub(self.offset))
            .read_to_end(&mut buf)
            .is_err()
        {
            return outcome;
        }
        self.offset = self.offset.saturating_add(buf.len() as u64);
        self.partial.extend_from_slice(&buf);

        // Consume complete lines only; a write may land mid-line (or mid
        // UTF-8 sequence), so everything after the last newline stays
        // buffered until its terminator arrives.
        let Some(last_newline) = self.partial.iter().rposition(|b| *b == b'\n') else {
            if outcome.reset {
                self.items = parse_transcript(&self.events);
            }
            return outcome;
        };
        let complete: Vec<u8> = self.partial.drain(..=last_newline).collect();
        for line in complete.split(|b| *b == b'\n') {
            if line.is_empty() {
                continue;
            }
            let Ok(value) = serde_json::from_slice::<Value>(line) else {
                continue;
            };
            self.events.push(unwrap_log_line(value));
            outcome.appended = true;
        }
        if outcome.appended || outcome.reset {
            self.items = parse_transcript(&self.events);
        }
        outcome
    }
}

#[derive(Default, Clone, Copy)]
struct TailPoll {
    appended: bool,
    reset: bool,
}

/// Event-log lines wrap the envelope event as `{"ts": ..., "event": {...}}`
/// (see `bro-harness::event_log`); the transcript parser consumes bare
/// envelope events. Unwrap the log shape, pass anything else through.
fn unwrap_log_line(value: Value) -> Value {
    match value {
        Value::Object(mut map) if map.contains_key("ts") && map.contains_key("event") => map
            .remove("event")
            .unwrap_or(Value::Object(serde_json::Map::new())),
        other => other,
    }
}

#[cfg(test)]
// Filesystem fixtures intentionally exercise incremental transcript tailing.
#[allow(clippy::disallowed_methods)]
mod tests {
    use super::*;
    use std::io::Write;

    fn log_line(event: serde_json::Value) -> String {
        serde_json::json!({ "ts": "2026-06-11T00:00:00.000Z", "event": event }).to_string()
    }

    fn assistant_event(text: &str) -> serde_json::Value {
        serde_json::json!({
            "type": "assistant",
            "message": { "role": "assistant", "content": [{ "type": "text", "text": text }] },
        })
    }

    #[test]
    fn attach_consumes_existing_lines_and_unwraps_log_shape() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.events.jsonl");
        std::fs::write(
            &path,
            format!(
                "{}\n{}\n",
                log_line(serde_json::json!({
                    "type": "harness_milestone", "milestone": "session_start"
                })),
                log_line(assistant_event("hello")),
            ),
        )
        .unwrap();

        let tail = TranscriptFileTail::attach(&path);
        assert_eq!(tail.events.len(), 2, "both lines consumed");
        assert_eq!(
            tail.items().len(),
            1,
            "milestone is ignored by the parser; assistant text renders"
        );
        assert!(matches!(
            &tail.items()[0],
            TranscriptItem::AssistantText(t) if t == "hello"
        ));
    }

    #[test]
    fn poll_consumes_appends_and_buffers_partial_lines() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.events.jsonl");
        std::fs::write(&path, format!("{}\n", log_line(assistant_event("one")))).unwrap();
        let mut tail = TranscriptFileTail::attach(&path);
        assert_eq!(tail.items().len(), 1);
        assert!(!tail.poll(), "no growth → no change");

        // Append a SPLIT line: first half without the newline…
        let full = log_line(assistant_event("two"));
        let (head, rest) = full.split_at(10);
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        f.write_all(head.as_bytes()).unwrap();
        f.flush().unwrap();
        assert!(!tail.poll(), "partial line must not parse yet");
        assert_eq!(tail.items().len(), 1);

        // …then the remainder including the newline.
        f.write_all(rest.as_bytes()).unwrap();
        f.write_all(b"\n").unwrap();
        f.flush().unwrap();
        assert!(tail.poll(), "completed line parses");
        assert_eq!(tail.items().len(), 2);
        assert!(matches!(
            &tail.items()[1],
            TranscriptItem::AssistantText(t) if t == "two"
        ));
    }

    #[test]
    fn attach_tolerates_missing_file_then_picks_it_up() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("not-yet.events.jsonl");
        let mut tail = TranscriptFileTail::attach(&path);
        assert!(tail.items().is_empty());

        std::fs::write(&path, format!("{}\n", log_line(assistant_event("late")))).unwrap();
        assert!(tail.poll());
        assert_eq!(tail.items().len(), 1);
    }

    #[test]
    fn truncation_resets_and_reflows() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.events.jsonl");
        std::fs::write(
            &path,
            format!(
                "{}\n{}\n",
                log_line(assistant_event("a")),
                log_line(assistant_event("b"))
            ),
        )
        .unwrap();
        let mut tail = TranscriptFileTail::attach(&path);
        assert_eq!(tail.items().len(), 2);

        std::fs::write(&path, format!("{}\n", log_line(assistant_event("fresh")))).unwrap();
        assert!(tail.poll(), "truncation must signal a change");
        assert_eq!(tail.items().len(), 1);
        assert!(matches!(
            &tail.items()[0],
            TranscriptItem::AssistantText(t) if t == "fresh"
        ));
    }
}
