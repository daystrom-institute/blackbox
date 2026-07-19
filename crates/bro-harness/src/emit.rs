//! Emits the exact Claude `stream-json` envelope the daemon's
//! `parse_claude_event` consumes (`src/orchestration/providers/events.rs`).
//! Every line carries a top-level `session_id` (the parser reads
//! `evt["session_id"]`) and a top-level `seq`: a per-session, strictly
//! monotonically increasing `u64` assigned at emission time (first event is
//! `1`; `0` is reserved as the pre-session cursor sentinel meaning "nothing
//! consumed yet"). `seq` is the replay-cursor foundation for the fleetd
//! extraction (design/daemon-runtime/locality-first-decomposition.md slice
//! 5): the daemon is the authority on its own cursor (last-ingested seq per
//! session) and streams the event-log tail from there on reconnect. Unknown
//! fields are additive; existing consumers ignore `seq` today. Only protocol
//! JSON goes to stdout; all logging goes to stderr.

use crate::event_log::EventLog;
use crate::transport::{ToolResult, Usage};
use serde_json::{Value, json};
use std::io::Write;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

pub type EventCallback = Arc<dyn Fn(Value) + Send + Sync + 'static>;

#[derive(Clone)]
pub struct Emitter {
    session_id: String,
    callback: Option<EventCallback>,
    /// Sidecar append-only event log (`event_log.rs`). When present, every
    /// emitted envelope event is teed there with a timestamp — except
    /// `stream_event` deltas (per-chunk noise whose content is already
    /// captured whole by the `assistant` turn event) and `isReplay` user
    /// echoes (the loop logs the authoritative user turn itself).
    event_log: Option<Arc<EventLog>>,
    /// Per-session monotonic event sequence counter, the replay-cursor
    /// foundation for the fleetd extraction
    /// (design/daemon-runtime/locality-first-decomposition.md slice 5: "the
    /// daemon is the authority on its own cursor (last-ingested seq per
    /// session)"). Holds the last `seq` assigned so far (0 before the first
    /// event); every `write_line` call does one `fetch_add` to claim the next
    /// value. Every `Emitter` instance that can write to the SAME session's
    /// stdout (the loop's own emitter, the stdin-reader's replay emitter, the
    /// control-response emitter, the `report` tool's emitter) MUST share one
    /// `Arc<AtomicU64>` via [`Emitter::with_seq_counter`]: independent
    /// counters would hand out colliding `seq` values for the same session.
    /// Defaults to a fresh, unshared counter so standalone/test emitters
    /// still get valid (if session-local-only) sequencing.
    seq_counter: Arc<AtomicU64>,
}

impl Emitter {
    pub fn new(session_id: String) -> Self {
        Self {
            session_id,
            callback: None,
            event_log: None,
            seq_counter: Arc::new(AtomicU64::new(0)),
        }
    }

    pub fn with_callback(session_id: String, callback: EventCallback) -> Self {
        Self {
            session_id,
            callback: Some(callback),
            event_log: None,
            seq_counter: Arc::new(AtomicU64::new(0)),
        }
    }

    /// Attach the session's sidecar event log; see [`Emitter::event_log`].
    pub fn with_event_log(mut self, log: Arc<EventLog>) -> Self {
        self.event_log = Some(log);
        self
    }

    /// Attach a shared sequence counter, so multiple `Emitter` instances
    /// writing to the same session's stdout hand out one strictly
    /// monotonically increasing `seq` stream. `initial` is the last `seq`
    /// already used by a prior process run of this session (0 for a fresh
    /// session), see `session.rs`'s snapshot+log-tail reconciliation.
    pub fn with_seq_counter(mut self, counter: Arc<AtomicU64>) -> Self {
        self.seq_counter = counter;
        self
    }

    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    /// The last `seq` assigned so far (0 if nothing has been emitted through
    /// this counter yet). Read at turn boundaries to persist
    /// `last_event_seq` in the session snapshot.
    pub fn last_seq(&self) -> u64 {
        self.seq_counter.load(Ordering::SeqCst)
    }

    fn write_line(&self, v: serde_json::Value) {
        let mut v = v;
        // Claim the next seq before the event-log tee decision, so the
        // stdout line and the (possibly teed) log line carry the identical
        // value. Every write_line call gets one, including stream_event
        // partials and isReplay echoes, which the log excludes below; the
        // resulting gaps in the log's seq sequence are expected (the log is
        // not the seq authority, the stdout stream is).
        let seq = self.seq_counter.fetch_add(1, Ordering::SeqCst) + 1;
        v["seq"] = json!(seq);
        if let Some(log) = &self.event_log
            && v["type"].as_str() != Some("stream_event")
            && v["isReplay"].as_bool() != Some(true)
        {
            log.append_event(&v);
        }
        if let Some(callback) = &self.callback {
            callback(v);
            return;
        }
        let mut out = std::io::stdout().lock();
        // A failed write to stdout (closed pipe) is unrecoverable for the
        // protocol; ignore and let the process wind down.
        let _ = writeln!(out, "{v}");
        let _ = out.flush();
    }

    /// `{"type":"system","subtype":"init","session_id":...}` — establishes the
    /// session id for the daemon up front.
    pub fn system_init(&self) {
        self.write_line(json!({
            "type": "system",
            "subtype": "init",
            "session_id": self.session_id,
        }));
    }

    /// `system/init` for bidirectional mode, advertising the in-stream slash
    /// commands the harness accepts (currently `/compact`) so a driver knows the
    /// control surface (NDJSON_FORMAT.md §system/init `slash_commands`).
    pub fn system_init_session(&self) {
        self.write_line(json!({
            "type": "system",
            "subtype": "init",
            "session_id": self.session_id,
            "slash_commands": ["compact"],
        }));
    }

    /// Re-emit a stdin user message as a `user` event (for
    /// `--replay-user-messages`), tagged `isReplay` so the driver can tell it
    /// from a fresh turn.
    pub fn replay_user(&self, message: &Value) {
        self.write_line(json!({
            "type": "user",
            "session_id": self.session_id,
            "isReplay": true,
            "message": message,
        }));
    }

    /// A successful `control_response` for a `control_request` (e.g. interrupt,
    /// set_model). The Claude Agent SDK shape: `response.{subtype,request_id}`.
    pub fn control_response_success(&self, request_id: Option<&str>) {
        self.write_line(json!({
            "type": "control_response",
            "response": {
                "subtype": "success",
                "request_id": request_id,
            },
        }));
    }

    /// One incremental streaming event — the inner Anthropic `event` wrapped as
    /// a Claude `stream_event` line. The daemon's parser folds
    /// `content_block_delta` text into the live assistant message and reads
    /// `message_delta` usage; the fleet TUI's live transcript reads the richer
    /// `content_block_start` (tool_use) and `input_json_delta` (args) too.
    pub fn stream_event(&self, event: Value) {
        self.write_line(json!({
            "type": "stream_event",
            "session_id": self.session_id,
            "event": event,
        }));
    }

    /// The full assistant turn as an Anthropic content array (text + thinking +
    /// tool_use blocks). When the turn streamed, the daemon dedupes the text
    /// against what it already accumulated from `stream_event` deltas
    /// (`streaming_captured` guard); the tool_use blocks are what surface tool
    /// calls to supervision and the fleet transcript. When the turn did NOT
    /// stream (non-streaming transports), this is the authoritative text.
    ///
    /// `stop` and `usage` are the model step's stop reason and token usage —
    /// the same per-step `TurnOutput` fields the suspicious-turn-end
    /// diagnostics read. They are persisted on `message` in the
    /// Anthropic-native shape (`stop_reason: "max_tokens"`, `usage:
    /// {input_tokens, ...}`) so events.jsonl forensics can tell an
    /// output-token-cap cut from a natural `end_turn` per step, not only at
    /// session termination. Both are additive/optional: `None` omits the
    /// field entirely, keeping old logs and existing consumers parseable.
    pub fn assistant_message(
        &self,
        content: Vec<Value>,
        stop: Option<&crate::transport::StopReason>,
        usage: Option<&Usage>,
    ) {
        let mut message = json!({
            "role": "assistant",
            "content": content,
            "session_id": self.session_id,
        });
        if let Some(stop) = stop {
            message["stop_reason"] = json!(stop.anthropic_wire_label());
        }
        if let Some(usage) = usage {
            // Anthropic-native usage shape, matching the terminal `result`
            // event: fresh `input_tokens` plus cache read/creation breakdown.
            message["usage"] = json!({
                "input_tokens": usage.input_tokens,
                "output_tokens": usage.output_tokens,
                "cache_read_input_tokens": usage.cached_input_tokens,
                "cache_creation_input_tokens": usage.cache_creation_input_tokens,
            });
        }
        self.write_line(json!({
            "type": "assistant",
            "session_id": self.session_id,
            "message": message,
        }));
    }

    /// Tool results for the calls just dispatched, as a `user` turn of
    /// `tool_result` blocks (the Anthropic shape). The daemon's claude parser
    /// ignores `user` lines (no handler), so this is purely additive — it gives
    /// the fleet transcript the tool responses to render inline. Skips emission
    /// when there are no results.
    pub fn tool_results(&self, results: &[ToolResult]) {
        if results.is_empty() {
            return;
        }
        let blocks: Vec<Value> = results
            .iter()
            .map(|r| {
                json!({
                    "type": "tool_result",
                    "tool_use_id": r.id,
                    "content": r.content,
                    "is_error": r.is_error,
                })
            })
            .collect();
        self.write_line(json!({
            "type": "user",
            "session_id": self.session_id,
            "message": {
                "role": "user",
                "content": blocks,
                "session_id": self.session_id,
            },
        }));
    }

    /// Marks an auto-compaction boundary in the stream — the harness summarized
    /// and replaced the older conversation prefix. The daemon's claude parser
    /// ignores unknown `system` subtypes, so this is purely a marker for the
    /// fleet TUI transcript (rendered as a divider). `pre_tokens` is the prompt
    /// size that tripped the threshold.
    pub fn compact_boundary(&self, trigger: &str, pre_tokens: u64, summary_chars: usize) {
        self.write_line(json!({
            "type": "system",
            "subtype": "compact_boundary",
            "session_id": self.session_id,
            "compact_metadata": {
                "trigger": trigger,
                "pre_tokens": pre_tokens,
                "summary_chars": summary_chars,
            },
        }));
    }

    /// The builtin `report` tool's status/needs signal — drives the cockpit's
    /// Waiting bucket and row summary (fleet-tui.md §2.2). Distinct from the
    /// daemon's `bro_report`; the daemon's claude parser ignores `report` lines.
    pub fn report(&self, message: &str, needs_input: bool) {
        self.write_line(json!({
            "type": "report",
            "session_id": self.session_id,
            "report": {
                "message": message,
                "needs_input": needs_input,
            },
        }));
    }

    /// Diagnostic marker emitted immediately before a terminal `result` when the
    /// turn is ending with outstanding async work or other suspicious state. The
    /// daemon ignores unknown `system` subtypes; fleet transcripts can retain it
    /// as evidence for premature turn-end investigations.
    pub fn turn_end_diagnostics(&self, metadata: Value) {
        self.write_line(json!({
            "type": "system",
            "subtype": "turn_end_diagnostics",
            "session_id": self.session_id,
            "turn_end": metadata,
        }));
    }

    /// Terminal `result` event with usage/turns/cost. `suspicious_turn_end`
    /// carries the turn-end diagnostics when the loop flagged the stop as
    /// suspicious (empty-output stop, outstanding async work) — the session
    /// still ends `subtype: success`, but orchestrators can see the
    /// deliverable may be missing instead of trusting `result` blindly.
    pub fn result(
        &self,
        text: &str,
        usage: &Usage,
        num_turns: u64,
        cost_usd: Option<f64>,
        suspicious_turn_end: Option<&Value>,
        compaction_threshold: Option<u64>,
    ) {
        // Emit the Anthropic-native usage shape (fresh `input_tokens` plus
        // `cache_read_input_tokens` / `cache_creation_input_tokens`) so the
        // daemon's claude parser captures the cache breakdown identically to a
        // real Claude CLI run.
        let mut v = json!({
            "type": "result",
            "subtype": "success",
            "session_id": self.session_id,
            "result": text,
            "num_turns": num_turns,
            "usage": {
                "input_tokens": usage.input_tokens,
                "output_tokens": usage.output_tokens,
                "cache_read_input_tokens": usage.cached_input_tokens,
                "cache_creation_input_tokens": usage.cache_creation_input_tokens,
            },
        });
        if let Some(c) = cost_usd {
            v["total_cost_usd"] = json!(c);
        }
        if let Some(diag) = suspicious_turn_end {
            v["suspicious_turn_end"] = diag.clone();
        }
        if let Some(t) = compaction_threshold {
            v["compaction_threshold"] = json!(t);
        }
        self.write_line(v);
    }

    /// Terminal `result` event for a turn interrupted by operator control. This
    /// is deliberately not `subtype: success`: callers that key on successful
    /// completion keep their existing behavior for natural finishes, while
    /// interruption-aware consumers can distinguish a partial deliverable.
    pub fn result_interrupted(
        &self,
        text: &str,
        usage: &Usage,
        num_turns: u64,
        compaction_threshold: Option<u64>,
    ) {
        let mut v = json!({
            "type": "result",
            "subtype": "interrupted",
            "interrupted": true,
            "session_id": self.session_id,
            "result": text,
            "num_turns": num_turns,
            "usage": {
                "input_tokens": usage.input_tokens,
                "output_tokens": usage.output_tokens,
                "cache_read_input_tokens": usage.cached_input_tokens,
                "cache_creation_input_tokens": usage.cache_creation_input_tokens,
            },
        });
        if let Some(t) = compaction_threshold {
            v["compaction_threshold"] = json!(t);
        }
        self.write_line(v);
    }

    /// Terminal `result` event for a turn that FAILED. Mirrors `result()` but
    /// carries `is_error: true` and the error text, so the failure is a captured
    /// protocol event (transcript + daemon ingest) rather than a stray stderr
    /// line that the controlled-session loop would otherwise swallow. The daemon
    /// recognizes `is_error: true` (`parse_claude_event` / `detect_disruption`)
    /// and maps it to a failed task with the message preserved.
    pub fn result_error(&self, error: &str, num_turns: u64) {
        self.write_line(json!({
            "type": "result",
            "subtype": "error",
            "is_error": true,
            "session_id": self.session_id,
            "result": error,
            "num_turns": num_turns,
        }));
    }
}

impl crate::transport::TurnSink for Emitter {
    fn stream_event(&self, event: Value) {
        Emitter::stream_event(self, event);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    #[test]
    fn callback_emitter_captures_protocol_events_in_process() {
        let captured = Arc::new(Mutex::new(Vec::new()));
        let sink = {
            let captured = captured.clone();
            Arc::new(move |event: Value| {
                captured.lock().unwrap().push(event);
            })
        };
        let emitter = Emitter::with_callback("session-1".into(), sink);

        emitter.system_init();
        emitter.result("done", &Usage::default(), 1, None, None, None);

        let events = captured.lock().unwrap();
        assert_eq!(events.len(), 2);
        assert_eq!(events[0]["type"], "system");
        assert_eq!(events[0]["session_id"], "session-1");
        assert_eq!(events[1]["type"], "result");
        assert_eq!(events[1]["result"], "done");
    }

    #[test]
    fn assistant_message_carries_step_stop_reason_and_usage() {
        let captured = Arc::new(Mutex::new(Vec::new()));
        let sink = {
            let captured = captured.clone();
            Arc::new(move |event: Value| {
                captured.lock().unwrap().push(event);
            })
        };
        let emitter = Emitter::with_callback("session-stop".into(), sink);

        let usage = Usage {
            input_tokens: 12,
            output_tokens: 3000,
            cached_input_tokens: 4500,
            cache_creation_input_tokens: 78,
        };
        emitter.assistant_message(
            vec![json!({"type": "thinking", "thinking": "cut mid-thought"})],
            Some(&crate::transport::StopReason::Length),
            Some(&usage),
        );

        let events = captured.lock().unwrap();
        assert_eq!(events.len(), 1);
        let message = &events[0]["message"];
        // max_tokens cut is distinguishable from a natural end_turn.
        assert_eq!(message["stop_reason"], "max_tokens");
        assert_eq!(message["usage"]["input_tokens"], 12);
        assert_eq!(message["usage"]["output_tokens"], 3000);
        assert_eq!(message["usage"]["cache_read_input_tokens"], 4500);
        assert_eq!(message["usage"]["cache_creation_input_tokens"], 78);
    }

    #[test]
    fn assistant_message_stop_reason_maps_anthropic_vocabulary() {
        use crate::transport::StopReason;
        for (stop, expect) in [
            (StopReason::ToolCalls, "tool_use"),
            (StopReason::Done, "end_turn"),
            (StopReason::Length, "max_tokens"),
            (StopReason::Other("content_filter".into()), "content_filter"),
        ] {
            assert_eq!(stop.anthropic_wire_label(), expect);
        }
    }

    #[test]
    fn assistant_message_without_step_metadata_keeps_legacy_shape() {
        let captured = Arc::new(Mutex::new(Vec::new()));
        let sink = {
            let captured = captured.clone();
            Arc::new(move |event: Value| {
                captured.lock().unwrap().push(event);
            })
        };
        let emitter = Emitter::with_callback("session-legacy".into(), sink);

        emitter.assistant_message(vec![json!({"type": "text", "text": "hi"})], None, None);

        let events = captured.lock().unwrap();
        assert_eq!(events.len(), 1);
        let message = events[0]["message"].as_object().unwrap();
        // None omits the fields entirely — the pre-gap-dab30623 wire shape.
        assert!(!message.contains_key("stop_reason"));
        assert!(!message.contains_key("usage"));
        assert_eq!(message["role"], "assistant");
    }

    #[test]
    fn persisted_assistant_event_round_trips_with_and_without_step_metadata() {
        use crate::event_log::EventLog;

        // Per-test tempdir; EventLog::at_path never touches the real
        // sessions dir / $HOME.
        let dir = tempfile::tempdir().unwrap();
        let log = Arc::new(EventLog::at_path(dir.path().join("s.events.jsonl")));

        // Old-shape line, as an existing events.jsonl would carry it.
        log.append_event(&json!({
            "type": "assistant",
            "session_id": "session-rt",
            "message": {"role": "assistant", "content": [{"type": "text", "text": "old"}]},
        }));

        // New-shape line, persisted through the emitter tee. No-op callback so
        // the protocol line goes nowhere but the log (not test stdout).
        let emitter = Emitter::with_callback("session-rt".into(), Arc::new(|_| {}))
            .with_event_log(log.clone());
        emitter.assistant_message(
            vec![json!({"type": "text", "text": "new"})],
            Some(&crate::transport::StopReason::Done),
            Some(&Usage {
                input_tokens: 1,
                output_tokens: 2,
                cached_input_tokens: 3,
                cache_creation_input_tokens: 4,
            }),
        );

        log.flush_blocking();
        let lines: Vec<Value> = std::fs::read_to_string(log.path())
            .unwrap()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        assert_eq!(lines.len(), 2);

        // Old log line still parses and simply lacks the new fields.
        let old_msg = lines[0]["event"]["message"].as_object().unwrap();
        assert!(!old_msg.contains_key("stop_reason"));
        assert!(!old_msg.contains_key("usage"));

        // New line round-trips the step metadata.
        let new_msg = &lines[1]["event"]["message"];
        assert_eq!(new_msg["stop_reason"], "end_turn");
        assert_eq!(new_msg["usage"]["input_tokens"], 1);
        assert_eq!(new_msg["usage"]["output_tokens"], 2);
        assert_eq!(new_msg["usage"]["cache_read_input_tokens"], 3);
        assert_eq!(new_msg["usage"]["cache_creation_input_tokens"], 4);
    }

    #[test]
    fn result_error_emits_is_error_result_event() {
        let captured = Arc::new(Mutex::new(Vec::new()));
        let sink = {
            let captured = captured.clone();
            Arc::new(move |event: Value| {
                captured.lock().unwrap().push(event);
            })
        };
        let emitter = Emitter::with_callback("session-err".into(), sink);

        emitter.result_error("anthropic messages 400 Bad Request: boom", 3);

        let events = captured.lock().unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0]["type"], "result");
        assert_eq!(events[0]["subtype"], "error");
        assert_eq!(events[0]["is_error"], true);
        assert_eq!(events[0]["session_id"], "session-err");
        assert_eq!(
            events[0]["result"],
            "anthropic messages 400 Bad Request: boom"
        );
        assert_eq!(events[0]["num_turns"], 3);
    }

    #[test]
    fn result_interrupted_emits_non_success_terminal_event() {
        let captured = Arc::new(Mutex::new(Vec::new()));
        let sink = {
            let captured = captured.clone();
            Arc::new(move |event: Value| {
                captured.lock().unwrap().push(event);
            })
        };
        let emitter = Emitter::with_callback("session-int".into(), sink);

        emitter.result_interrupted("partial answer", &Usage::default(), 0, None);

        let events = captured.lock().unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0]["type"], "result");
        assert_eq!(events[0]["subtype"], "interrupted");
        assert_eq!(events[0]["interrupted"], true);
        assert_eq!(events[0]["session_id"], "session-int");
        assert_eq!(events[0]["result"], "partial answer");
        assert_eq!(events[0]["num_turns"], 0);
    }

    // -----------------------------------------------------------------
    // Per-session event seq (replay-cursor foundation, slice 5)
    // -----------------------------------------------------------------

    #[test]
    fn fresh_session_seq_starts_at_one_and_increases_strictly() {
        let captured = Arc::new(Mutex::new(Vec::new()));
        let sink = {
            let captured = captured.clone();
            Arc::new(move |event: Value| {
                captured.lock().unwrap().push(event);
            })
        };
        let emitter = Emitter::with_callback("session-seq".into(), sink);

        emitter.system_init();
        emitter.assistant_message(vec![json!({"type": "text", "text": "hi"})], None, None);
        emitter.result("done", &Usage::default(), 1, None, None, None);

        let events = captured.lock().unwrap();
        let seqs: Vec<u64> = events.iter().map(|e| e["seq"].as_u64().unwrap()).collect();
        // A fresh session's first emitted line is seq 1 (0 is reserved as the
        // pre-session cursor sentinel), and every subsequent line strictly
        // increases by exactly 1.
        assert_eq!(seqs, vec![1, 2, 3]);
        assert_eq!(emitter.last_seq(), 3);
    }

    #[test]
    fn stream_event_partials_and_replay_echoes_consume_seq_too() {
        // Every write_line call claims a seq, including the two shapes the
        // event log deliberately excludes from its own tee (stream_event
        // deltas, isReplay echoes): the stdout stream is the seq authority,
        // not the log, so gaps in the log's seq sequence are expected.
        let captured = Arc::new(Mutex::new(Vec::new()));
        let sink = {
            let captured = captured.clone();
            Arc::new(move |event: Value| {
                captured.lock().unwrap().push(event);
            })
        };
        let emitter = Emitter::with_callback("session-gaps".into(), sink);

        emitter.stream_event(json!({"type": "content_block_delta"}));
        emitter.replay_user(&json!({"role": "user", "content": "echoed"}));
        emitter.system_init();

        let events = captured.lock().unwrap();
        let seqs: Vec<u64> = events.iter().map(|e| e["seq"].as_u64().unwrap()).collect();
        assert_eq!(seqs, vec![1, 2, 3]);
    }

    #[test]
    fn seeded_seq_counter_continues_monotonically_past_its_initial_value() {
        // Simulates a resumed session: the counter is seeded from a prior
        // run's `last_event_seq` (session.rs reconciliation) instead of
        // starting fresh at 0, so the sequence never restarts.
        let captured = Arc::new(Mutex::new(Vec::new()));
        let sink = {
            let captured = captured.clone();
            Arc::new(move |event: Value| {
                captured.lock().unwrap().push(event);
            })
        };
        let seeded = Arc::new(AtomicU64::new(41));
        let emitter =
            Emitter::with_callback("session-resumed".into(), sink).with_seq_counter(seeded);

        emitter.system_init();
        emitter.result("done", &Usage::default(), 1, None, None, None);

        let events = captured.lock().unwrap();
        let seqs: Vec<u64> = events.iter().map(|e| e["seq"].as_u64().unwrap()).collect();
        assert_eq!(seqs, vec![42, 43]);
    }

    #[test]
    fn shared_seq_counter_gives_multiple_emitters_one_collision_free_stream() {
        // Mirrors agent_loop.rs's make_emitter: the loop's own emitter, a
        // control-response emitter, and the report tool's emitter all write
        // to the SAME session's stdout and must share one counter.
        let captured = Arc::new(Mutex::new(Vec::new()));
        let sink = {
            let captured = captured.clone();
            Arc::new(move |event: Value| {
                captured.lock().unwrap().push(event);
            })
        };
        let shared = Arc::new(AtomicU64::new(0));
        let loop_emitter = Emitter::with_callback("session-shared".into(), sink.clone())
            .with_seq_counter(shared.clone());
        let ctrl_emitter =
            Emitter::with_callback("session-shared".into(), sink).with_seq_counter(shared);

        loop_emitter.system_init();
        ctrl_emitter.control_response_success(Some("req-1"));
        loop_emitter.result("done", &Usage::default(), 1, None, None, None);

        let events = captured.lock().unwrap();
        let seqs: Vec<u64> = events.iter().map(|e| e["seq"].as_u64().unwrap()).collect();
        assert_eq!(seqs, vec![1, 2, 3]);
    }

    #[test]
    fn logged_event_carries_the_same_seq_as_the_stdout_line() {
        let dir = tempfile::tempdir().unwrap();
        let log = Arc::new(EventLog::at_path(dir.path().join("seq.events.jsonl")));

        let emitter = Emitter::with_callback("session-log-seq".into(), Arc::new(|_| {}))
            .with_event_log(log.clone());

        emitter.system_init();
        emitter.assistant_message(vec![json!({"type": "text", "text": "hi"})], None, None);
        // stream_event is excluded from the log tee: confirms the log's
        // seq values are a (gapped) subsequence of the stdout stream's, not
        // a separately-numbered sequence.
        emitter.stream_event(json!({"type": "content_block_delta"}));
        emitter.result("done", &Usage::default(), 1, None, None, None);

        log.flush_blocking();
        let lines: Vec<Value> = std::fs::read_to_string(log.path())
            .unwrap()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        assert_eq!(lines.len(), 3);
        let logged_seqs: Vec<u64> = lines
            .iter()
            .map(|l| l["event"]["seq"].as_u64().unwrap())
            .collect();
        // system_init=1, assistant_message=2, (stream_event=3, excluded),
        // result=4.
        assert_eq!(logged_seqs, vec![1, 2, 4]);
    }
}
