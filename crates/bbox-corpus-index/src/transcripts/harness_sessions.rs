//! Transcript adapter for standalone bro-harness process sessions.
//!
//! The harness writes two artifacts per session under
//! `$BRO_HOME/harness-sessions` (else `~/.bro-harness/sessions`):
//!
//! - `<session-id>.json` — the resume-state snapshot (transport-native item
//!   array, no timestamps, rewritten by compaction). Useless for time-based
//!   postmortems; this adapter never reads it.
//! - `<session-id>.events.jsonl` — the sidecar append-only event log
//!   (`crates/bro-harness/src/event_log.rs`): one
//!   `{"ts": rfc3339, "event": <claude stream-json envelope>}` line per
//!   protocol event plus harness milestones. This is the source this adapter
//!   indexes.
//!
//! Because the envelope is Claude stream-json shaped, each logged event is
//! re-projected into the Claude transcript record shape (`sessionId` /
//! `timestamp` / `cwd` top-level keys) and parsed with the existing
//! `parse_transcript_line_rich`, so user text, assistant text/thinking,
//! tool_use, and tool_result events all flow through the same normalization
//! used for every other provider.
//!
//! Provider attribution: the registry is keyed by [`Provider`], but all
//! harness providers share one sessions dir, so one adapter instance exists
//! per harness provider and each claims only the sessions whose recorded
//! `session_start` milestone resolves to its provider (the daemon stamps
//! `BRO_HARNESS_PROVIDER` into the per-session env at dispatch). Logs without
//! a provider stamp fall back to transport+model inference; logs with no
//! readable metadata at all are owned by the [`FALLBACK_PROVIDER`] instance so
//! exactly one adapter indexes each file.

use std::path::{Path, PathBuf};
use std::str::FromStr;

use serde_json::Value;

use bro_core::Provider;
use bro_transcript::parse_transcript_line_rich;

use super::adapters::{TranscriptReadAdapter, TranscriptScanTarget};
use super::types::{
    NormalizedTranscriptEvent, RawTranscriptRef, TranscriptBatch, TranscriptCursor,
    TranscriptLocation, TranscriptReadError, TranscriptSource, TranscriptStorage,
};

/// File suffix of the harness sidecar event log.
pub const EVENT_LOG_SUFFIX: &str = ".events.jsonl";

/// Owner of logs whose provider cannot be determined (no `session_start`
/// milestone, no inferable transport/model).
const FALLBACK_PROVIDER: Provider = Provider::Glm;

/// All harness providers that persist sessions into the shared sessions dir.
const HARNESS_PROVIDERS: &[Provider] = &[
    Provider::Glm,
    Provider::Deepseek,
    Provider::Minimax,
    Provider::Kimi,
    Provider::Brodex,
    Provider::VibeBh,
];

/// Resolve the harness sessions dir the way the harness itself does
/// (`crates/bro-harness/src/session.rs::sessions_dir`): `$BRO_HOME/
/// harness-sessions`, else `~/.bro-harness/sessions`. The daemon exports
/// `BRO_HOME` during single-threaded startup (`server/open.rs`), so runtime
/// callers agree with standalone harness children on the dir.
pub fn env_sessions_dir() -> PathBuf {
    if let Ok(home) = std::env::var("BRO_HOME") {
        PathBuf::from(home).join("harness-sessions")
    } else {
        dirs::home_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join(".bro-harness")
            .join("sessions")
    }
}

pub struct HarnessSessionsAdapter {
    provider: Provider,
    dir: PathBuf,
}

impl HarnessSessionsAdapter {
    pub fn new(provider: Provider, dir: PathBuf) -> Self {
        Self { provider, dir }
    }

    /// One adapter instance per harness provider, all over the same dir.
    pub fn all_for_dir(dir: &Path) -> Vec<Box<dyn TranscriptReadAdapter>> {
        HARNESS_PROVIDERS
            .iter()
            .map(|provider| {
                Box::new(Self::new(*provider, dir.to_path_buf())) as Box<dyn TranscriptReadAdapter>
            })
            .collect()
    }

    fn log_path(&self, session_id: &str) -> PathBuf {
        self.dir.join(format!("{session_id}{EVENT_LOG_SUFFIX}"))
    }

    fn location_for(
        &self,
        session_id: &str,
        path: PathBuf,
        meta: &SessionMeta,
    ) -> TranscriptLocation {
        TranscriptLocation {
            source: TranscriptSource::Harness(self.provider),
            storage: TranscriptStorage::JsonlFile,
            path,
            account: None,
            session_id: Some(session_id.to_string()),
            project: meta.cwd.clone(),
            cwd: meta.cwd.clone(),
            is_subagent: false,
        }
    }
}

/// Session metadata mined from the log's `session_start` / `session_resume`
/// milestone (scanned from the head of the file).
#[derive(Debug, Default, Clone)]
struct SessionMeta {
    provider: Option<String>,
    transport: Option<String>,
    model: Option<String>,
    cwd: Option<String>,
}

impl SessionMeta {
    /// Resolve the owning provider: explicit stamp first, then transport +
    /// model inference, then the shared fallback.
    fn resolve_provider(&self) -> Provider {
        if let Some(p) = self.provider.as_deref()
            && let Ok(provider) = Provider::from_str(p)
        {
            return provider;
        }
        match self.transport.as_deref() {
            Some("openai-responses") => Provider::Brodex,
            Some("openai-chat") => Provider::VibeBh,
            _ => {
                let model = self.model.as_deref().unwrap_or("").to_ascii_lowercase();
                if model.starts_with("deepseek") {
                    Provider::Deepseek
                } else if model.starts_with("minimax") || model.starts_with("abab") {
                    Provider::Minimax
                } else if model.starts_with("kimi") || model.starts_with("k3") {
                    Provider::Kimi
                } else if model.starts_with("glm") {
                    Provider::Glm
                } else {
                    FALLBACK_PROVIDER
                }
            }
        }
    }
}

/// Scan the head of an event log for the session milestone. Only the first
/// few lines are examined — the harness writes the milestone before any
/// protocol event, so this stays cheap for large logs.
fn read_session_meta(path: &Path) -> SessionMeta {
    const HEAD_LINES: usize = 8;
    let Ok(contents) = read_head(path, 64 * 1024) else {
        return SessionMeta::default();
    };
    for line in contents.lines().take(HEAD_LINES) {
        let Ok(v) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        let event = &v["event"];
        if event["type"].as_str() != Some("harness_milestone") {
            continue;
        }
        if !matches!(
            event["milestone"].as_str(),
            Some("session_start" | "session_resume")
        ) {
            continue;
        }
        return SessionMeta {
            provider: event["provider"].as_str().map(str::to_string),
            transport: event["transport"].as_str().map(str::to_string),
            model: event["model"].as_str().map(str::to_string),
            cwd: event["cwd"].as_str().map(str::to_string),
        };
    }
    SessionMeta::default()
}

/// Read at most `cap` bytes from the head of `path` as lossy UTF-8.
fn read_head(path: &Path, cap: usize) -> std::io::Result<String> {
    use std::io::Read;
    let mut file = std::fs::File::open(path)?;
    let mut buf = vec![0u8; cap];
    let mut filled = 0;
    loop {
        let n = file.read(&mut buf[filled..])?;
        if n == 0 {
            break;
        }
        filled += n;
        if filled == buf.len() {
            break;
        }
    }
    buf.truncate(filled);
    Ok(String::from_utf8_lossy(&buf).into_owned())
}

/// Session id from an event-log file name (`<id>.events.jsonl`).
fn session_id_from_path(path: &Path) -> Option<String> {
    let name = path.file_name()?.to_str()?;
    name.strip_suffix(EVENT_LOG_SUFFIX).map(str::to_string)
}

impl TranscriptReadAdapter for HarnessSessionsAdapter {
    fn source(&self) -> TranscriptSource {
        TranscriptSource::Harness(self.provider)
    }

    fn locate(&self, session_id: &str) -> Result<Option<TranscriptLocation>, TranscriptReadError> {
        let path = self.log_path(session_id);
        if !path.is_file() {
            return Ok(None);
        }
        let meta = read_session_meta(&path);
        Ok(Some(self.location_for(session_id, path, &meta)))
    }

    fn scan_locations(
        &self,
        target: TranscriptScanTarget,
    ) -> Result<Vec<TranscriptLocation>, TranscriptReadError> {
        if target == TranscriptScanTarget::History {
            return Ok(Vec::new());
        }
        let entries = match std::fs::read_dir(&self.dir) {
            Ok(entries) => entries,
            // Absent dir ⇒ no harness sessions yet; not an error.
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(err) => return Err(TranscriptReadError::io("read_dir", &self.dir, err)),
        };
        let mut out = Vec::new();
        for entry in entries.flatten() {
            let path = entry.path();
            let Some(session_id) = session_id_from_path(&path) else {
                continue;
            };
            let meta = read_session_meta(&path);
            // Each file is owned by exactly one provider instance so the
            // registry's per-adapter scans never double-index it.
            if meta.resolve_provider() != self.provider {
                continue;
            }
            out.push(self.location_for(&session_id, path, &meta));
        }
        out.sort_by(|a, b| a.path.cmp(&b.path));
        Ok(out)
    }

    fn read_since(
        &self,
        location: &TranscriptLocation,
        cursor: Option<&TranscriptCursor>,
    ) -> Result<TranscriptBatch, TranscriptReadError> {
        let start_offset = match cursor {
            None => 0u64,
            Some(TranscriptCursor::ByteOffset { offset }) => *offset,
            Some(other) => {
                return Err(TranscriptReadError::UnsupportedCursor {
                    source: TranscriptSource::Harness(self.provider),
                    cursor: other.clone(),
                });
            }
        };
        let contents = std::fs::read_to_string(&location.path)
            .map_err(|err| TranscriptReadError::io("read", &location.path, err))?;
        let fallback_session_id = location
            .session_id
            .clone()
            .or_else(|| session_id_from_path(&location.path))
            .unwrap_or_default();
        let meta = read_session_meta(&location.path);

        let mut events = Vec::new();
        let mut offset = 0u64;
        for line in contents.split_inclusive('\n') {
            let line_offset = offset;
            offset += line.len() as u64;
            if line_offset < start_offset {
                continue;
            }
            let trimmed = line.trim_end_matches(['\n', '\r']);
            if trimmed.is_empty() {
                continue;
            }
            let Ok(record) = serde_json::from_str::<Value>(trimmed) else {
                // A torn tail line (crash mid-append) is expected for a log
                // whose point is crashed-session postmortems; skip it.
                continue;
            };
            let Some(envelope) = record.get("event") else {
                continue;
            };
            for (event_idx, normalized) in project_envelope(
                envelope,
                record["ts"].as_str(),
                &fallback_session_id,
                meta.cwd.as_deref(),
                TranscriptSource::Harness(self.provider),
            )
            .into_iter()
            .enumerate()
            {
                let raw = RawTranscriptRef::jsonl(
                    TranscriptSource::Harness(self.provider),
                    TranscriptStorage::JsonlFile,
                    &location.path,
                    line_offset,
                    event_idx as u32,
                    trimmed.len(),
                );
                events.push(NormalizedTranscriptEvent { raw, ..normalized });
            }
        }
        Ok(TranscriptBatch {
            location: location.clone(),
            events,
            cursor: Some(TranscriptCursor::byte_offset(offset)),
            reached_end: true,
        })
    }
}

/// Project one logged envelope event into normalized transcript events by
/// re-shaping it into the Claude transcript record `parse_transcript_line_rich`
/// understands (`sessionId`/`timestamp`/`cwd` top-level). Non-conversational
/// envelopes (milestones, results, control responses) project to nothing.
fn project_envelope(
    envelope: &Value,
    ts: Option<&str>,
    fallback_session_id: &str,
    cwd: Option<&str>,
    provider: TranscriptSource,
) -> Vec<NormalizedTranscriptEvent> {
    let Some(event_type) = envelope["type"].as_str() else {
        return Vec::new();
    };
    if !matches!(event_type, "user" | "assistant") {
        return Vec::new();
    }
    let mut record = envelope.clone();
    let session_id = envelope["session_id"]
        .as_str()
        .filter(|s| !s.is_empty())
        .unwrap_or(fallback_session_id);
    record["sessionId"] = Value::from(session_id);
    if let Some(ts) = ts {
        record["timestamp"] = Value::from(ts);
    }
    if record.get("cwd").is_none()
        && let Some(cwd) = cwd
    {
        record["cwd"] = Value::from(cwd);
    }
    parse_transcript_line_rich(&record.to_string())
        .iter()
        .filter_map(|rich| {
            NormalizedTranscriptEvent::from_transcript_event(
                provider,
                rich,
                // Placeholder; the caller stamps the real per-line ref.
                RawTranscriptRef::jsonl(
                    provider,
                    TranscriptStorage::JsonlFile,
                    PathBuf::new(),
                    0,
                    0,
                    0,
                ),
            )
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transcripts::types::TranscriptEventKind;
    use serde_json::json;

    fn write_log(dir: &Path, session_id: &str, lines: &[Value]) -> PathBuf {
        let path = dir.join(format!("{session_id}{EVENT_LOG_SUFFIX}"));
        let body: String = lines.iter().map(|l| format!("{l}\n")).collect();
        std::fs::write(&path, body).unwrap();
        path
    }

    fn milestone(session_id: &str, provider: &str, transport: &str, cwd: &str) -> Value {
        json!({
            "ts": "2026-06-10T01:00:00.000Z",
            "event": {
                "type": "harness_milestone",
                "milestone": "session_start",
                "session_id": session_id,
                "transport": transport,
                "model": "m",
                "cwd": cwd,
                "provider": provider,
            }
        })
    }

    fn synthetic_session(dir: &Path, session_id: &str, provider: &str) -> PathBuf {
        write_log(
            dir,
            session_id,
            &[
                milestone(session_id, provider, "anthropic", "/repo/x"),
                json!({
                    "ts": "2026-06-10T01:00:01.000Z",
                    "event": {
                        "type": "user",
                        "session_id": session_id,
                        "message": {"role": "user", "content": [{"type": "text", "text": "fix the bug"}]},
                    }
                }),
                json!({
                    "ts": "2026-06-10T01:00:05.000Z",
                    "event": {
                        "type": "assistant",
                        "session_id": session_id,
                        "message": {"role": "assistant", "content": [
                            {"type": "thinking", "thinking": "hmm"},
                            {"type": "text", "text": "done, fixed"},
                            {"type": "tool_use", "id": "t1", "name": "file_edit", "input": {"file_path": "/repo/x/src/lib.rs"}},
                        ]},
                    }
                }),
                json!({
                    "ts": "2026-06-10T01:00:06.000Z",
                    "event": {
                        "type": "user",
                        "session_id": session_id,
                        "message": {"role": "user", "content": [
                            {"type": "tool_result", "tool_use_id": "t1", "content": "ok", "is_error": false},
                        ]},
                    }
                }),
                json!({
                    "ts": "2026-06-10T01:00:07.000Z",
                    "event": {"type": "result", "subtype": "success", "session_id": session_id, "result": "done, fixed"}
                }),
            ],
        )
    }

    #[test]
    fn scan_attributes_sessions_to_the_recorded_provider() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        synthetic_session(&root, "sess-glm", "glm");
        synthetic_session(&root, "sess-brodex", "brodex");

        let glm = HarnessSessionsAdapter::new(Provider::Glm, root.clone());
        let brodex = HarnessSessionsAdapter::new(Provider::Brodex, root.clone());

        let glm_locs = glm.scan_locations(TranscriptScanTarget::Sessions).unwrap();
        assert_eq!(glm_locs.len(), 1);
        assert_eq!(glm_locs[0].session_id.as_deref(), Some("sess-glm"));
        assert_eq!(glm_locs[0].source, TranscriptSource::Harness(Provider::Glm));
        assert_eq!(glm_locs[0].cwd.as_deref(), Some("/repo/x"));

        let brodex_locs = brodex
            .scan_locations(TranscriptScanTarget::Sessions)
            .unwrap();
        assert_eq!(brodex_locs.len(), 1);
        assert_eq!(brodex_locs[0].session_id.as_deref(), Some("sess-brodex"));

        // History target is empty — harness sessions have no history file.
        assert!(
            glm.scan_locations(TranscriptScanTarget::History)
                .unwrap()
                .is_empty()
        );
        // Absent dir is not an error.
        let empty = HarnessSessionsAdapter::new(Provider::Glm, root.join("nope"));
        assert!(
            empty
                .scan_locations(TranscriptScanTarget::Sessions)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn provider_inference_without_stamp() {
        // No provider stamp: transport decides for responses/chat; model
        // prefix decides on the anthropic transport.
        let cases = [
            (json!({"transport": "openai-responses"}), Provider::Brodex),
            (json!({"transport": "openai-chat"}), Provider::VibeBh),
            (
                json!({"transport": "anthropic", "model": "deepseek-v4-pro"}),
                Provider::Deepseek,
            ),
            (
                json!({"transport": "anthropic", "model": "MiniMax-M3"}),
                Provider::Minimax,
            ),
            (
                json!({"transport": "anthropic", "model": "k3"}),
                Provider::Kimi,
            ),
            (
                json!({"transport": "anthropic", "model": "kimi-k2.7-code"}),
                Provider::Kimi,
            ),
            (
                json!({"transport": "anthropic", "model": "glm-5.1"}),
                Provider::Glm,
            ),
            (json!({}), FALLBACK_PROVIDER),
        ];
        for (fields, expected) in cases {
            let meta = SessionMeta {
                provider: None,
                transport: fields["transport"].as_str().map(str::to_string),
                model: fields["model"].as_str().map(str::to_string),
                cwd: None,
            };
            assert_eq!(meta.resolve_provider(), expected, "{fields}");
        }
    }

    #[test]
    fn read_since_normalizes_events_with_timestamps_and_cursor() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        synthetic_session(&root, "sess-1", "glm");

        let adapter = HarnessSessionsAdapter::new(Provider::Glm, root.clone());
        let location = adapter.locate("sess-1").unwrap().expect("located");
        assert!(
            location
                .path
                .to_string_lossy()
                .ends_with("sess-1.events.jsonl")
        );

        let batch = adapter.read_since(&location, None).unwrap();
        assert!(batch.reached_end);
        let kinds: Vec<_> = batch.events.iter().map(|e| e.kind).collect();
        assert_eq!(
            kinds,
            vec![
                TranscriptEventKind::Message,  // user prompt
                TranscriptEventKind::Thinking, // assistant thinking
                TranscriptEventKind::Message,  // assistant text
                TranscriptEventKind::ToolUse,  // file_edit
                TranscriptEventKind::ToolResult,
            ]
        );
        // The sidecar ts is the event timestamp — the whole point.
        assert_eq!(
            batch.events[0].timestamp.as_deref(),
            Some("2026-06-10T01:00:01.000Z")
        );
        assert_eq!(batch.events[0].content, "fix the bug");
        assert_eq!(batch.events[0].session_id, "sess-1");
        assert_eq!(batch.events[0].cwd.as_deref(), Some("/repo/x"));
        assert_eq!(
            batch.events[3].tool_call.as_ref().map(|t| t.name.as_str()),
            Some("file_edit")
        );

        // Cursor resume: re-reading from the returned cursor yields nothing
        // new; events appended after it are picked up.
        let cursor = batch.cursor.clone().expect("cursor");
        let empty = adapter.read_since(&location, Some(&cursor)).unwrap();
        assert!(empty.events.is_empty());

        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&location.path)
            .unwrap();
        use std::io::Write as _;
        writeln!(
            file,
            "{}",
            json!({
                "ts": "2026-06-10T01:00:09.000Z",
                "event": {
                    "type": "assistant",
                    "session_id": "sess-1",
                    "message": {"role": "assistant", "content": [{"type": "text", "text": "follow-up"}]},
                }
            })
        )
        .unwrap();
        let next = adapter.read_since(&location, Some(&cursor)).unwrap();
        assert_eq!(next.events.len(), 1);
        assert_eq!(next.events[0].content, "follow-up");
        assert_eq!(
            next.events[0].timestamp.as_deref(),
            Some("2026-06-10T01:00:09.000Z")
        );
    }

    #[test]
    fn torn_tail_line_is_skipped() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let path = synthetic_session(&root, "sess-torn", "glm");
        // Simulate a crash mid-append: a truncated, non-JSON tail line.
        let mut contents = std::fs::read_to_string(&path).unwrap();
        contents.push_str("{\"ts\":\"2026-06-10T01:00:10.000Z\",\"event\":{\"type\":\"assist");
        std::fs::write(&path, contents).unwrap();

        let adapter = HarnessSessionsAdapter::new(Provider::Glm, root);
        let location = adapter.locate("sess-torn").unwrap().expect("located");
        let batch = adapter.read_since(&location, None).unwrap();
        assert_eq!(batch.events.len(), 5, "torn line skipped, rest intact");
    }
}
