//! Versioned transport snapshots and exclusive session-writer ownership.
//!
//! Explicit resume requires a valid snapshot. The snapshot is authoritative for
//! model history; the event log is durable evidence. When the log records
//! conversation or actions beyond the saved checkpoint (the previous process
//! was cancelled, killed, or crashed before its next checkpoint), resume
//! proceeds from the snapshot and carries a [`CheckpointGap`] digest of the
//! uncheckpointed tail, so the resumed model and the caller are told exactly
//! what happened after the checkpoint instead of the session being refused.
//! Structural corruption of the checkpoint itself (a log shorter than its
//! offset, an offset that is not a record boundary, an unparsable complete
//! record) still fails closed.

use anyhow::{Context, Result, bail};
use fs2::FileExt;
use serde_json::{Value, json};
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

pub const SNAPSHOT_VERSION: u64 = 1;

#[derive(Debug)]
pub struct SessionStore {
    pub id: String,
    path: PathBuf,
    pub restored: Option<Restored>,
    // Advisory locks outlive all async persists and are released by File::drop.
    // A legacy migration also holds its source lock until this store closes.
    _writer_locks: Vec<Arc<File>>,
    #[cfg(test)]
    _test_dir: Option<tempfile::TempDir>,
}

#[derive(Debug)]
pub struct Restored {
    pub transport: String,
    pub model: Option<String>,
    pub code_mode: Option<String>,
    pub service_tier: Option<String>,
    pub effort: Option<String>,
    pub snapshot: Value,
    pub side: Value,
    pub last_event_seq: u64,
    pub event_log_offset: Option<u64>,
    /// The previous process checkpointed while JavaScript cells or shell
    /// sessions were still outstanding. Process-local handles cannot be
    /// restored; resume discloses this to the model instead of refusing.
    pub runtime_work_outstanding: bool,
    /// Conversation or actions the event log recorded after the checkpoint.
    /// `None` when the log and snapshot agree.
    pub checkpoint_gap: Option<CheckpointGap>,
}

/// Digest of the event-log tail a snapshot does not cover. Built at resume,
/// appended to the log as a `checkpoint_gap_recovered` milestone, and rendered
/// into a model-facing notice so uncheckpointed effects can be re-verified.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct CheckpointGap {
    pub records: u64,
    pub meaningful_records: u64,
    pub assistant_steps: u64,
    pub user_messages: u64,
    pub tool_results: u64,
    /// `name arguments` in log order, bounded by [`Self::MAX_TOOL_CALLS`].
    pub tool_calls: Vec<String>,
    pub omitted_tool_calls: u64,
    /// User prompts and steers received after the checkpoint, bounded.
    pub user_texts: Vec<String>,
    pub omitted_user_texts: u64,
    pub last_assistant_text: Option<String>,
    /// `result` subtypes seen after the checkpoint.
    pub results: Vec<String>,
    pub first_ts: Option<String>,
    pub last_ts: Option<String>,
    /// Bytes of a torn final record (no trailing newline) removed from the log
    /// so later appends stay parseable.
    pub discarded_partial_bytes: u64,
}

impl CheckpointGap {
    pub const MAX_TOOL_CALLS: usize = 60;
    pub const MAX_USER_TEXTS: usize = 4;
    const ARG_CHARS: usize = 160;
    const TEXT_CHARS: usize = 400;

    fn observe(&mut self, record: &Value, meaningful: bool) {
        self.records += 1;
        if let Some(ts) = record.get("ts").and_then(Value::as_str) {
            if self.first_ts.is_none() {
                self.first_ts = Some(ts.to_owned());
            }
            self.last_ts = Some(ts.to_owned());
        }
        if !meaningful {
            return;
        }
        self.meaningful_records += 1;
        let event = &record["event"];
        match event["type"].as_str() {
            Some("assistant") => {
                self.assistant_steps += 1;
                for block in event["message"]["content"].as_array().into_iter().flatten() {
                    match block["type"].as_str() {
                        Some("tool_use") => {
                            if self.tool_calls.len() < Self::MAX_TOOL_CALLS {
                                let name = block["name"].as_str().unwrap_or("?");
                                let args =
                                    block.get("input").map(Value::to_string).unwrap_or_default();
                                self.tool_calls.push(format!(
                                    "{name} {}",
                                    truncate_chars(&args, Self::ARG_CHARS)
                                ));
                            } else {
                                self.omitted_tool_calls += 1;
                            }
                        }
                        Some("text") => {
                            if let Some(text) = block["text"].as_str()
                                && !text.trim().is_empty()
                            {
                                self.last_assistant_text =
                                    Some(truncate_chars(text.trim(), Self::TEXT_CHARS));
                            }
                        }
                        _ => {}
                    }
                }
            }
            Some("user") if event.get("subtype").is_none() => {
                let content = &event["message"]["content"];
                let tool_results = content
                    .as_array()
                    .map(|blocks| {
                        blocks
                            .iter()
                            .filter(|block| block["type"] == "tool_result")
                            .count() as u64
                    })
                    .unwrap_or(0);
                if tool_results > 0 {
                    self.tool_results += tool_results;
                } else {
                    self.user_messages += 1;
                    let text = match content {
                        Value::String(text) => text.clone(),
                        Value::Array(blocks) => blocks
                            .iter()
                            .filter_map(|block| block["text"].as_str())
                            .collect::<Vec<_>>()
                            .join("\n"),
                        _ => String::new(),
                    };
                    if !text.trim().is_empty() {
                        if self.user_texts.len() < Self::MAX_USER_TEXTS {
                            self.user_texts
                                .push(truncate_chars(text.trim(), Self::TEXT_CHARS));
                        } else {
                            self.omitted_user_texts += 1;
                        }
                    }
                }
            }
            Some("result") => {
                let subtype = event["subtype"].as_str().unwrap_or("unknown");
                if let Some(text) = event["result"].as_str()
                    && !text.trim().is_empty()
                {
                    self.last_assistant_text = Some(truncate_chars(text.trim(), Self::TEXT_CHARS));
                }
                self.results.push(subtype.to_owned());
            }
            _ => {}
        }
    }

    pub fn is_empty(&self) -> bool {
        self.meaningful_records == 0 && self.discarded_partial_bytes == 0
    }

    pub fn to_json(&self) -> Value {
        json!({
            "records": self.records,
            "meaningful_records": self.meaningful_records,
            "assistant_steps": self.assistant_steps,
            "user_messages": self.user_messages,
            "tool_results": self.tool_results,
            "tool_calls": self.tool_calls,
            "omitted_tool_calls": self.omitted_tool_calls,
            "user_texts": self.user_texts,
            "omitted_user_texts": self.omitted_user_texts,
            "last_assistant_text": self.last_assistant_text,
            "results": self.results,
            "first_ts": self.first_ts,
            "last_ts": self.last_ts,
            "discarded_partial_bytes": self.discarded_partial_bytes,
        })
    }

    /// Model-facing notice. `runtime_work_outstanding` adds the process-local
    /// handle disclosure when the checkpoint itself carried that marker.
    pub fn model_notice(&self, runtime_work_outstanding: bool) -> String {
        let mut out = String::from(
            "[Checkpoint gap recovered] The previous process for this session ended before \
             saving its next checkpoint (for example it was cancelled or killed mid-turn). \
             Everything up to the checkpoint is in your history. The following happened AFTER \
             the checkpoint and is NOT in your history, although the durable session log \
             recorded it:",
        );
        out.push_str(&format!(
            "\n- {} model steps, {} tool results, {} user messages",
            self.assistant_steps, self.tool_results, self.user_messages
        ));
        if let (Some(first), Some(last)) = (&self.first_ts, &self.last_ts) {
            out.push_str(&format!(" between {first} and {last}"));
        }
        out.push('.');
        if !self.user_texts.is_empty() {
            out.push_str("\n- User messages received after the checkpoint:");
            for text in &self.user_texts {
                out.push_str(&format!("\n    - {text:?}"));
            }
            if self.omitted_user_texts > 0 {
                out.push_str(&format!("\n    - (+{} more)", self.omitted_user_texts));
            }
        }
        if !self.tool_calls.is_empty() {
            out.push_str("\n- Tool calls made after the checkpoint, in order:");
            for call in &self.tool_calls {
                out.push_str(&format!("\n    - {call}"));
            }
            if self.omitted_tool_calls > 0 {
                out.push_str(&format!("\n    - (+{} more)", self.omitted_tool_calls));
            }
        }
        if let Some(text) = &self.last_assistant_text {
            out.push_str(&format!(
                "\n- Last assistant text after the checkpoint: {text:?}"
            ));
        }
        if !self.results.is_empty() {
            out.push_str(&format!(
                "\n- Turn results recorded after the checkpoint: {}.",
                self.results.join(", ")
            ));
        }
        if self.discarded_partial_bytes > 0 {
            out.push_str(&format!(
                "\n- {} bytes of a torn final log record were discarded.",
                self.discarded_partial_bytes
            ));
        }
        if runtime_work_outstanding {
            out.push_str(
                "\n- The previous process also had unfinished JavaScript cells or shell sessions \
                 whose output was never delivered.",
            );
        }
        out.push_str(
            "\nFiles, commits, and other durable effects of those actions are real. Re-read files \
             and re-check state before repeating any of them; do not assume a listed action still \
             needs doing, and do not assume it succeeded.",
        );
        out
    }
}

fn truncate_chars(text: &str, max_chars: usize) -> String {
    let mut chars = text.char_indices();
    match chars.nth(max_chars) {
        Some((index, _)) => format!("{}…(+{} chars)", &text[..index], text.len() - index),
        None => text.to_owned(),
    }
}

pub struct SaveState<'a> {
    pub transport: &'a str,
    pub model: &'a str,
    pub code_mode: &'a str,
    pub service_tier: Option<&'a str>,
    pub effort: Option<&'a str>,
    pub snapshot: Value,
    pub side: Value,
    pub last_event_seq: u64,
}

pub(crate) fn sessions_dir() -> PathBuf {
    if let Ok(home) = std::env::var("BRO_HOME") {
        PathBuf::from(home).join("harness-sessions")
    } else {
        legacy_sessions_dir()
    }
}

fn legacy_sessions_dir() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".bro-harness")
        .join("sessions")
}

static NONCE: AtomicU64 = AtomicU64::new(0);

/// Atomic replacement while the caller retains the session's writer lock.
#[allow(clippy::disallowed_methods)]
pub fn write_atomic(path: &Path, contents: &str) -> Result<()> {
    let pid = std::process::id();
    let nonce = NONCE.fetch_add(1, Ordering::SeqCst);
    let tmp_path = path.with_extension(format!("json.{pid}.{nonce}.tmp"));
    if let Some(parent) = tmp_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&tmp_path)
        .with_context(|| format!("create session tmp {}", tmp_path.display()))?;
    file.write_all(contents.as_bytes())
        .with_context(|| format!("write session tmp {}", tmp_path.display()))?;
    file.sync_all()
        .context("sync session snapshot before replacement")?;
    std::fs::rename(&tmp_path, path).with_context(|| {
        format!(
            "rename session tmp {} to {}",
            tmp_path.display(),
            path.display()
        )
    })?;
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    File::open(parent)?
        .sync_all()
        .context("sync session snapshot directory")?;
    Ok(())
}

impl SessionStore {
    #[cfg(test)]
    pub(crate) fn for_test(path: PathBuf) -> Self {
        let lock = acquire_writer_lock(&path).expect("lock isolated test session");
        Self {
            id: "isolated-test-session".into(),
            path,
            restored: None,
            _writer_locks: vec![lock],
            _test_dir: None,
        }
    }

    #[cfg(test)]
    pub(crate) fn temporary_for_test() -> Self {
        let dir = tempfile::tempdir().expect("isolated session directory");
        let root = dir
            .path()
            .canonicalize()
            .expect("canonical test session directory");
        let mut store = Self::for_test(root.join("session.json"));
        store._test_dir = Some(dir);
        store
    }

    /// Open with one exclusive writer for the whole harness session lifetime.
    pub fn open(session_id: Option<&str>, resume: Option<&str>) -> Result<Self> {
        Self::open_in(
            &sessions_dir(),
            Some(&legacy_sessions_dir()),
            session_id,
            resume,
        )
    }

    // Production path with explicit roots, also used by hermetic lifecycle tests.
    #[allow(clippy::disallowed_methods)]
    pub(crate) fn open_in(
        dir: &Path,
        legacy_dir: Option<&Path>,
        session_id: Option<&str>,
        resume: Option<&str>,
    ) -> Result<Self> {
        if let Some(id) = session_id.filter(|id| *id != "pending") {
            validate_session_id(id)?;
        }
        if let Some(id) = resume {
            validate_session_id(id)?;
            if let Some(requested) = session_id.filter(|id| *id != "pending") {
                anyhow::ensure!(
                    requested == id,
                    "session ID conflicts with explicit resume ID"
                );
            }
        }
        let id = match resume.or(session_id.filter(|id| *id != "pending")) {
            Some(id) => id.to_owned(),
            None => uuid::Uuid::new_v4().to_string(),
        };
        std::fs::create_dir_all(dir).context("create sessions dir")?;
        let path = dir.join(format!("{id}.json"));
        let mut writer_locks = vec![acquire_writer_lock(&path)?];
        if resume.is_none() {
            for existing in [&path, &event_log_path(&path)] {
                match std::fs::symlink_metadata(existing) {
                    Ok(_) => bail!(
                        "session '{id}' already exists; use explicit resume: {}",
                        existing.display()
                    ),
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                    Err(error) => {
                        return Err(error)
                            .with_context(|| format!("inspect session {}", existing.display()));
                    }
                }
            }
            return Ok(Self {
                id,
                path,
                restored: None,
                _writer_locks: writer_locks,
                #[cfg(test)]
                _test_dir: None,
            });
        }
        let (source_path, body) = match std::fs::read_to_string(&path) {
            Ok(body) => (path.clone(), body),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                let legacy_path = legacy_dir
                    .map(|dir| dir.join(format!("{id}.json")))
                    .filter(|legacy| legacy != &path)
                    .with_context(|| {
                        format!(
                            "explicit resume session '{id}' is missing: {}",
                            path.display()
                        )
                    })?;
                // Do not create a legacy directory merely to report absence.
                match std::fs::symlink_metadata(&legacy_path) {
                    Ok(_) => {}
                    Err(error) => {
                        return Err(error).with_context(|| {
                            format!(
                                "explicit resume session '{id}' not readable at {} or {}",
                                path.display(),
                                legacy_path.display()
                            )
                        });
                    }
                }
                writer_locks.push(acquire_writer_lock(&legacy_path)?);
                let body = std::fs::read_to_string(&legacy_path).with_context(|| {
                    format!("read resumed legacy session {}", legacy_path.display())
                })?;
                (legacy_path, body)
            }
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("read resumed session {}", path.display()));
            }
        };
        let mut restored = parse_restored(&body)
            .with_context(|| format!("invalid resumed session {}", source_path.display()))?;
        restored.checkpoint_gap = inspect_event_checkpoint(
            &event_log_path(&source_path),
            restored.event_log_offset,
            restored.last_event_seq,
        )?;
        // A destination log with no destination snapshot cannot be merged with
        // a legacy source: it may describe a different, unsaved conversation.
        if source_path != path
            && let Some(gap) =
                inspect_event_checkpoint(&event_log_path(&path), Some(0), restored.last_event_seq)?
        {
            anyhow::bail!(
                "destination event log {} already records {} events without a snapshot; it may describe a different conversation",
                event_log_path(&path).display(),
                gap.records
            );
        }
        Ok(Self {
            id,
            path,
            restored: Some(restored),
            _writer_locks: writer_locks,
            #[cfg(test)]
            _test_dir: None,
        })
    }

    /// Pure serialization. Async callers flush their event log and supply its
    /// exact byte length while persisting a quiescent snapshot on a blocking worker.
    pub fn serialize(state: &SaveState, event_log_offset: Option<u64>) -> Result<String> {
        serde_json::to_string(&json!({
            "version": SNAPSHOT_VERSION,
            "transport":state.transport,
            "model":state.model,
            "code_mode":state.code_mode,
            "service_tier":state.service_tier,
            "effort":state.effort,
            "snapshot":state.snapshot,
            "side":state.side,
            "last_event_seq":state.last_event_seq,
            "event_log_offset":event_log_offset,
        }))
        .context("serialize session")
    }

    /// Synchronous save for callers that have already drained their log writer.
    #[allow(clippy::disallowed_methods)]
    pub fn save(&self, state: &SaveState) -> Result<()> {
        let offset = match std::fs::metadata(event_log_path(&self.path)) {
            Ok(metadata) => metadata.len(),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => 0,
            Err(error) => return Err(error).context("read session event-log checkpoint"),
        };
        write_atomic(&self.path, &Self::serialize(state, Some(offset))?).context("write session")
    }

    pub fn store_path(&self) -> &PathBuf {
        &self.path
    }

    /// Persistence workers retain writer ownership even if their awaiter exits.
    pub(crate) fn writer_lease(&self) -> Vec<Arc<File>> {
        self._writer_locks.clone()
    }
}

fn validate_session_id(id: &str) -> Result<()> {
    anyhow::ensure!(
        !id.is_empty()
            && id.len() <= 200
            && id != "."
            && id != ".."
            && id
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'-' | b'_' | b'.')),
        "invalid session ID: expected one nonempty filename component of letters, digits, dots, hyphens, or underscores"
    );
    Ok(())
}

#[allow(clippy::disallowed_methods)]
fn acquire_writer_lock(snapshot_path: &Path) -> Result<Arc<File>> {
    let path = snapshot_path.with_extension("lock");
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&path)
        .with_context(|| format!("open session writer lock {}", path.display()))?;
    file.try_lock_exclusive().with_context(|| {
        format!(
            "session already has an active writer: {}",
            snapshot_path.display()
        )
    })?;
    Ok(Arc::new(file))
}

fn event_log_path(snapshot_path: &Path) -> PathBuf {
    snapshot_path.with_extension("events.jsonl")
}

fn parse_restored(body: &str) -> Result<Restored> {
    let value: Value = serde_json::from_str(body).context("parse session JSON")?;
    let object = value.as_object().context("session must be an object")?;
    let version = match object.get("version") {
        None => 0,
        Some(value) if value.as_u64() == Some(SNAPSHOT_VERSION) => SNAPSHOT_VERSION,
        Some(_) => bail!("unsupported or invalid session snapshot version"),
    };
    let transport = object
        .get("transport")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .context("session requires a nonempty transport tag")?;
    let snapshot = object
        .get("snapshot")
        .context("session requires a transport snapshot")?;
    crate::transport::validate_snapshot(transport, snapshot)?;
    let side = object.get("side").cloned().unwrap_or(Value::Null);
    anyhow::ensure!(
        side.is_null() || side.is_object(),
        "session side state must be an object or null"
    );
    if let Some(outcomes) = side.get("remote_outcomes_unknown") {
        let outcomes = outcomes
            .as_array()
            .context("remote_outcomes_unknown must be an array")?;
        anyhow::ensure!(
            outcomes.iter().all(Value::is_string),
            "remote_outcomes_unknown entries must be strings"
        );
        anyhow::ensure!(
            outcomes.is_empty(),
            "session has unresolved remote tool outcomes; reconcile external effects before starting a fresh session, because replaying or retrying may duplicate them"
        );
    }
    // Outstanding cells or shell sessions are process-local and cannot be
    // restored. Resume proceeds and discloses the loss to the model (see
    // `CheckpointGap::model_notice`) instead of refusing the session.
    let runtime_work_outstanding = match side.get("runtime_work_outstanding") {
        None => false,
        Some(outstanding) => outstanding
            .as_bool()
            .context("runtime_work_outstanding must be a boolean")?,
    };
    let last_event_seq = match object.get("last_event_seq") {
        None if version == 0 => 0,
        Some(value) => value
            .as_u64()
            .context("last_event_seq must be an unsigned integer")?,
        None => bail!("versioned session requires last_event_seq"),
    };
    let event_log_offset = match object.get("event_log_offset") {
        None if version == 0 => None,
        Some(Value::Null) => None,
        Some(value) => Some(
            value
                .as_u64()
                .context("event_log_offset must be an unsigned integer")?,
        ),
        None => bail!("versioned session requires event_log_offset"),
    };
    let code_mode = optional_string(object, "code_mode")?;
    anyhow::ensure!(
        code_mode
            .as_deref()
            .is_none_or(|mode| matches!(mode, "off" | "optional" | "only")),
        "invalid persisted code_mode"
    );
    Ok(Restored {
        transport: transport.to_owned(),
        model: optional_string(object, "model")?,
        code_mode,
        service_tier: optional_string(object, "service_tier")?,
        effort: optional_string(object, "effort")?,
        snapshot: snapshot.clone(),
        side,
        last_event_seq,
        event_log_offset,
        runtime_work_outstanding,
        checkpoint_gap: None,
    })
}

fn optional_string(object: &serde_json::Map<String, Value>, key: &str) -> Result<Option<String>> {
    match object.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(value)) if !value.is_empty() => Ok(Some(value.clone())),
        Some(_) => bail!("session {key} must be a nonempty string or null"),
    }
}

// A lifecycle label alone does not make an event harmless. Terminal diagnostics
// can contain the only durable receipt of outstanding work, and result text can
// be a substantive answer or failure. Resume cannot assume either is redundant.
fn harmless_trailing_event(event: &Value) -> bool {
    match event["type"].as_str() {
        Some("control_response") => matches!(
            event["response"]["subtype"].as_str(),
            Some("success" | "error")
        ),
        Some("result") => {
            matches!(
                event["subtype"].as_str(),
                Some("success" | "incomplete" | "interrupted" | "error")
            ) && event
                .get("result")
                .is_some_and(|value| value.as_str() == Some(""))
                && event
                    .get("suspicious_turn_end")
                    .is_none_or(harmless_turn_diagnostics)
        }
        Some("harness_milestone") => matches!(
            event["milestone"].as_str(),
            Some("session_start" | "session_resume" | "compaction_start")
        ),
        Some("system") => match event["subtype"].as_str() {
            Some(
                "init"
                | "context_pressure"
                | "mcp_readiness"
                | "checkpoint_gap_recovered"
                | "termination_signal",
            ) => true,
            Some("turn_end_diagnostics") => {
                event.get("turn_end").is_some_and(harmless_turn_diagnostics)
            }
            _ => false,
        },
        _ => false,
    }
}

fn harmless_turn_diagnostics(value: &Value) -> bool {
    value.is_object()
        && value
            .get("last_tool_results")
            .is_none_or(|results| results.as_array().is_some_and(Vec::is_empty))
        && value
            .get("outstanding_shell_sessions")
            .is_none_or(|sessions| {
                sessions.is_object()
                    && sessions["count"].as_u64() == Some(0)
                    && sessions["ids"].as_array().is_some_and(Vec::is_empty)
            })
}

/// Inspect the event log against the snapshot's checkpoint. Returns the digest
/// of any conversation or actions recorded beyond the checkpoint (the caller
/// resumes from the snapshot and discloses the digest), or `None` when the log
/// and snapshot agree. A torn final record (a crash mid-write leaves no trailing
/// newline) is removed so later appends stay parseable; its byte count is
/// reported in the digest. Checkpoint corruption still fails closed: a log
/// shorter than its offset, an offset that is not a record boundary, or an
/// unparsable complete record.
#[allow(clippy::disallowed_methods)]
pub(crate) fn inspect_event_checkpoint(
    path: &Path,
    event_log_offset: Option<u64>,
    last_event_seq: u64,
) -> Result<Option<CheckpointGap>> {
    let mut file = match File::open(path) {
        Ok(file) => file,
        Err(error)
            if error.kind() == std::io::ErrorKind::NotFound
                && event_log_offset.unwrap_or(0) == 0 =>
        {
            return Ok(None);
        }
        Err(error) => {
            return Err(error)
                .with_context(|| format!("read resumed event log {}", path.display()));
        }
    };
    let log_len = file.metadata()?.len();
    let mut consumed = 0u64;
    if let Some(offset) = event_log_offset {
        anyhow::ensure!(
            log_len >= offset,
            "session event log is shorter than its saved checkpoint: {}",
            path.display()
        );
        if offset > 0 {
            file.seek(SeekFrom::Start(offset - 1))?;
            let mut boundary = [0];
            file.read_exact(&mut boundary)?;
            anyhow::ensure!(
                boundary[0] == b'\n',
                "session event-log checkpoint is not a complete record boundary"
            );
        }
        file.seek(SeekFrom::Start(offset))?;
        consumed = offset;
    }
    let mut gap = CheckpointGap::default();
    // Legacy sequence checkpoints: unsequenced records are covered only when a
    // later sequenced record is itself covered by the snapshot.
    let mut pending_unsequenced: Vec<(Value, bool)> = Vec::new();
    let mut partial_trailing_bytes = 0u64;
    let mut reader = BufReader::new(file);
    let mut line = String::new();
    let mut line_number = 0;
    loop {
        line.clear();
        if reader
            .read_line(&mut line)
            .with_context(|| format!("read event log {}", path.display()))?
            == 0
        {
            break;
        }
        line_number += 1;
        if !line.ends_with('\n') {
            partial_trailing_bytes = line.len() as u64;
            break;
        }
        consumed += line.len() as u64;
        if line.trim().is_empty() {
            continue;
        }
        let record: Value = serde_json::from_str(&line).with_context(|| {
            format!(
                "invalid event log {} record {}",
                path.display(),
                line_number
            )
        })?;
        let event = record
            .get("event")
            .filter(|value| value.is_object())
            .context("event log record requires an event object")?;
        let meaningful = !harmless_trailing_event(event);
        let seq = event
            .get("seq")
            .map(|value| {
                value
                    .as_u64()
                    .context("event sequence must be an unsigned integer")
            })
            .transpose()?;
        if event_log_offset.is_some() {
            gap.observe(&record, meaningful);
        } else if let Some(seq) = seq {
            if seq <= last_event_seq {
                pending_unsequenced.clear();
            } else {
                for (pending, pending_meaningful) in pending_unsequenced.drain(..) {
                    gap.observe(&pending, pending_meaningful);
                }
                gap.observe(&record, meaningful);
            }
        } else {
            pending_unsequenced.push((record, meaningful));
        }
    }
    for (pending, pending_meaningful) in pending_unsequenced {
        gap.observe(&pending, pending_meaningful);
    }
    drop(reader);
    if partial_trailing_bytes > 0 {
        // The torn bytes are not a record; dropping them keeps the next append
        // from fusing with them into one unparsable line.
        OpenOptions::new()
            .write(true)
            .open(path)
            .and_then(|file| file.set_len(consumed))
            .with_context(|| format!("drop torn final record from {}", path.display()))?;
        gap.discarded_partial_bytes = partial_trailing_bytes;
    }
    Ok((!gap.is_empty()).then_some(gap))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn root() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        (dir, root)
    }

    fn state() -> SaveState<'static> {
        SaveState {
            transport: "anthropic",
            model: "synthetic-model",
            code_mode: "optional",
            service_tier: Some("priority"),
            effort: Some("high"),
            snapshot: json!([{"role":"user","content":[{"type":"text","text":"hello"}]}]),
            side: json!({"todos":[],"marker":"retained"}),
            last_event_seq: 7,
        }
    }

    fn append_events(path: &Path, events: &[Value]) {
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .unwrap();
        for event in events {
            writeln!(
                file,
                "{}",
                json!({"ts":"2026-01-01T00:00:00Z","event":event})
            )
            .unwrap();
        }
    }

    fn create_saved(dir: &Path, id: &str) -> PathBuf {
        let store = SessionStore::open_in(dir, None, Some(id), None).unwrap();
        store.save(&state()).unwrap();
        store.store_path().clone()
    }

    fn resume(dir: &Path, id: &str) -> Result<SessionStore> {
        SessionStore::open_in(dir, None, None, Some(id))
    }

    #[test]
    fn production_save_reopen_preserves_native_snapshot_and_side_state() {
        let (_dir, root) = root();
        let store = SessionStore::open_in(&root, None, Some("roundtrip"), None).unwrap();
        assert!(store.restored.is_none());
        let log = event_log_path(store.store_path());
        append_events(
            &log,
            &[
                json!({"type":"user","message":{"content":"hello"}}),
                json!({"type":"assistant","seq":7}),
            ],
        );
        store.save(&state()).unwrap();
        let persisted: Value =
            serde_json::from_str(&std::fs::read_to_string(store.store_path()).unwrap()).unwrap();
        assert_eq!(persisted["version"], 1);
        assert_eq!(
            persisted["event_log_offset"],
            std::fs::metadata(&log).unwrap().len()
        );
        drop(store);
        let store = resume(&root, "roundtrip").unwrap();
        let restored = store.restored.as_ref().unwrap();
        assert_eq!(restored.transport, "anthropic");
        assert_eq!(restored.snapshot, state().snapshot);
        assert_eq!(restored.side, state().side);
        assert_eq!(restored.model.as_deref(), Some("synthetic-model"));
        assert_eq!(restored.code_mode.as_deref(), Some("optional"));
        assert_eq!(restored.service_tier.as_deref(), Some("priority"));
        assert_eq!(restored.effort.as_deref(), Some("high"));
        assert_eq!(restored.last_event_seq, 7);
    }

    #[test]
    fn outstanding_runtime_checkpoint_resumes_with_disclosure_and_refuses_id_reuse() {
        let (_dir, root) = root();
        let store = SessionStore::open_in(&root, None, Some("outstanding"), None).unwrap();
        let mut snapshot = state();
        snapshot.side["runtime_work_outstanding"] = json!(true);
        store.save(&snapshot).unwrap();
        let path = store.store_path().clone();
        let before = std::fs::read(&path).unwrap();
        drop(store);
        // Process-local handles cannot come back; resume proceeds and the
        // constructor discloses the loss to the model instead of refusing.
        let resumed = resume(&root, "outstanding").unwrap();
        let restored = resumed.restored.as_ref().unwrap();
        assert!(restored.runtime_work_outstanding);
        assert!(restored.checkpoint_gap.is_none());
        assert_eq!(restored.snapshot, state().snapshot);
        drop(resumed);
        assert!(SessionStore::open_in(&root, None, Some("outstanding"), None).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), before);
    }

    #[test]
    fn later_quiescent_checkpoint_clears_runtime_marker_and_invalid_marker_refuses() {
        let (_dir, root) = root();
        let store = SessionStore::open_in(&root, None, Some("drained"), None).unwrap();
        let mut snapshot = state();
        snapshot.side["runtime_work_outstanding"] = json!(true);
        store.save(&snapshot).unwrap();
        snapshot.side["runtime_work_outstanding"] = json!(false);
        store.save(&snapshot).unwrap();
        drop(store);
        let restored = resume(&root, "drained").unwrap();
        assert_eq!(
            restored.restored.as_ref().unwrap().side["runtime_work_outstanding"],
            false
        );
        for malformed in [Value::Null, json!("false"), json!(0), json!([])] {
            snapshot.side["runtime_work_outstanding"] = malformed;
            let body = SessionStore::serialize(&snapshot, Some(0)).unwrap();
            assert!(
                parse_restored(&body)
                    .unwrap_err()
                    .to_string()
                    .contains("must be a boolean")
            );
        }
    }

    #[test]
    fn explicit_missing_resume_fails_instead_of_starting_fresh() {
        let (_dir, root) = root();
        let primary = root.join("primary");
        let legacy = root.join("legacy");
        assert!(SessionStore::open_in(&primary, Some(&legacy), None, Some("absent")).is_err());
        assert!(!primary.join("absent.json").exists());
        assert!(!legacy.exists());
    }

    #[test]
    fn fresh_session_cannot_replace_snapshot_or_orphaned_event_log() {
        let (_dir, root) = root();
        let path = create_saved(&root, "exists");
        let before = std::fs::read(&path).unwrap();
        assert!(SessionStore::open_in(&root, None, Some("exists"), None).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), before);
        append_events(&root.join("orphan.events.jsonl"), &[json!({"type":"user"})]);
        assert!(SessionStore::open_in(&root, None, Some("orphan"), None).is_err());
    }

    #[test]
    fn exclusive_writer_lock_lasts_until_store_drop() {
        let (_dir, root) = root();
        let store = SessionStore::open_in(&root, None, Some("writer"), None).unwrap();
        store.save(&state()).unwrap();
        assert!(
            resume(&root, "writer")
                .unwrap_err()
                .to_string()
                .contains("active writer")
        );
        assert!(
            SessionStore::open_in(&root, None, Some("writer"), None)
                .unwrap_err()
                .to_string()
                .contains("active writer")
        );
        // A different session remains available while the first writer is held.
        let other = SessionStore::open_in(&root, None, Some("other"), None).unwrap();
        drop(store);
        let reopened = resume(&root, "writer").unwrap();
        assert!(reopened.restored.is_some());
        drop(other);
    }

    #[test]
    fn abandoned_fresh_lock_does_not_claim_a_session_forever() {
        let (_dir, root) = root();
        drop(SessionStore::open_in(&root, None, Some("unused"), None).unwrap());
        assert!(SessionStore::open_in(&root, None, Some("unused"), None).is_ok());
    }

    #[test]
    fn invalid_ids_are_rejected_before_filesystem_creation() {
        let (_dir, root) = root();
        for id in [
            "",
            ".",
            "..",
            "../escape",
            "nested/id",
            "nested\\id",
            "/absolute",
            "bad\nname",
        ] {
            let dir = root.join("never-created");
            assert!(
                SessionStore::open_in(&dir, None, Some(id), None).is_err(),
                "fresh {id:?}"
            );
            assert!(
                SessionStore::open_in(&dir, None, None, Some(id)).is_err(),
                "resume {id:?}"
            );
            assert!(!dir.exists());
        }
        assert!(SessionStore::open_in(&root, None, Some("one"), Some("two")).is_err());
    }

    #[test]
    fn pending_and_absent_ids_generate_distinct_fresh_sessions() {
        let (_dir, root) = root();
        let one = SessionStore::open_in(&root, None, Some("pending"), None).unwrap();
        let two = SessionStore::open_in(&root, None, None, None).unwrap();
        assert_ne!(one.id, "pending");
        assert_ne!(one.id, two.id);
        uuid::Uuid::parse_str(&one.id).unwrap();
    }

    #[test]
    fn missing_primary_can_migrate_valid_legacy_and_holds_both_locks() {
        let (_dir, root) = root();
        let primary = root.join("primary");
        let legacy = root.join("legacy");
        create_saved(&legacy, "migrate");
        let store = SessionStore::open_in(&primary, Some(&legacy), None, Some("migrate")).unwrap();
        assert!(store.restored.is_some());
        assert!(
            resume(&legacy, "migrate")
                .unwrap_err()
                .to_string()
                .contains("active writer")
        );
        assert!(
            resume(&primary, "migrate")
                .unwrap_err()
                .to_string()
                .contains("active writer")
        );
        store.save(&state()).unwrap();
        drop(store);
        assert!(resume(&primary, "migrate").is_ok());
        assert!(resume(&legacy, "migrate").is_ok());
    }

    #[test]
    fn corrupt_or_unreadable_primary_never_falls_back_to_legacy() {
        let (_dir, root) = root();
        let primary = root.join("primary");
        let legacy = root.join("legacy");
        create_saved(&legacy, "broken");
        std::fs::create_dir_all(&primary).unwrap();
        let path = primary.join("broken.json");
        std::fs::write(&path, "{broken").unwrap();
        assert!(SessionStore::open_in(&primary, Some(&legacy), None, Some("broken")).is_err());
        std::fs::remove_file(&path).unwrap();
        // Opening a directory as snapshot fails on all supported platforms,
        // without relying on process privileges to enforce permission bits.
        std::fs::create_dir(&path).unwrap();
        assert!(SessionStore::open_in(&primary, Some(&legacy), None, Some("broken")).is_err());
    }

    #[test]
    fn legacy_migration_refuses_unsaved_destination_conversation() {
        let (_dir, root) = root();
        let primary = root.join("primary");
        let legacy = root.join("legacy");
        create_saved(&legacy, "migrate");
        std::fs::create_dir_all(&primary).unwrap();
        append_events(
            &primary.join("migrate.events.jsonl"),
            &[json!({"type":"user"})],
        );
        assert!(SessionStore::open_in(&primary, Some(&legacy), None, Some("migrate")).is_err());
    }

    #[test]
    fn snapshot_header_rejects_wrong_types_missing_requirements_and_versions() {
        let (_dir, root) = root();
        let path = create_saved(&root, "invalid");
        let valid: Value =
            serde_json::from_str(&SessionStore::serialize(&state(), Some(0)).unwrap()).unwrap();
        let mutations = [
            ("version", json!(2)),
            ("version", json!("1")),
            ("transport", json!("unknown")),
            ("transport", Value::Null),
            ("snapshot", json!({"silently":"ignored before"})),
            ("side", json!([])),
            ("model", json!(4)),
            ("code_mode", json!("invalid")),
            ("service_tier", json!(false)),
            ("effort", json!(false)),
            ("effort", json!("")),
            ("last_event_seq", json!(-1)),
            ("event_log_offset", json!("12")),
        ];
        for (key, value) in mutations {
            let mut invalid = valid.clone();
            invalid[key] = value;
            write_atomic(&path, &invalid.to_string()).unwrap();
            assert!(resume(&root, "invalid").is_err(), "accepted invalid {key}");
        }
        for key in [
            "transport",
            "snapshot",
            "last_event_seq",
            "event_log_offset",
        ] {
            let mut invalid = valid.clone();
            invalid.as_object_mut().unwrap().remove(key);
            write_atomic(&path, &invalid.to_string()).unwrap();
            assert!(resume(&root, "invalid").is_err(), "accepted absent {key}");
        }
        write_atomic(&path, "[]").unwrap();
        assert!(resume(&root, "invalid").is_err());
    }

    #[test]
    fn legacy_minimal_snapshot_preserves_supported_missing_metadata() {
        let (_dir, root) = root();
        write_atomic(
            &root.join("legacy.json"),
            &json!({"transport":"anthropic","snapshot":[]}).to_string(),
        )
        .unwrap();
        let store = resume(&root, "legacy").unwrap();
        let restored = store.restored.unwrap();
        assert_eq!(restored.last_event_seq, 0);
        assert_eq!(restored.event_log_offset, None);
        assert!(restored.side.is_null());
        assert_eq!(restored.model, None);
        assert_eq!(restored.code_mode, None);
        assert_eq!(restored.service_tier, None);
        assert_eq!(restored.effort, None);
    }

    #[test]
    fn versioned_snapshot_accepts_absent_or_null_effort() {
        let mut snapshot: Value =
            serde_json::from_str(&SessionStore::serialize(&state(), Some(0)).unwrap()).unwrap();
        snapshot.as_object_mut().unwrap().remove("effort");
        assert_eq!(parse_restored(&snapshot.to_string()).unwrap().effort, None);
        snapshot["effort"] = Value::Null;
        assert_eq!(parse_restored(&snapshot.to_string()).unwrap().effort, None);
        let mut saved = state();
        saved.effort = None;
        assert_eq!(
            parse_restored(&SessionStore::serialize(&saved, Some(0)).unwrap())
                .unwrap()
                .effort,
            None
        );
    }

    #[test]
    fn meaningful_event_log_tail_is_recovered_as_a_checkpoint_gap_even_without_sequence() {
        let (_dir, root) = root();
        let events = [
            json!({"type":"user","message":{"content":"new unsaved turn"}}),
            json!({"type":"assistant","seq":8}),
            json!({"type":"user","subtype":"tool_cancellation_outcomes","seq":8}),
            json!({"type":"system","subtype":"compact_boundary","seq":8}),
            json!({"type":"system","subtype":"failed_turn_observation","seq":8}),
            json!({"type":"report","seq":8}),
            json!({"type":"result","subtype":"success","result":"unsaved answer","seq":8}),
            json!({"type":"result","subtype":"error","result":"unsaved failure","seq":8}),
            json!({"type":"result","subtype":"success","result":"","suspicious_turn_end":{"last_tool_results":[{"name":"file_write"}]},"seq":8}),
            json!({"type":"system","subtype":"turn_end_diagnostics","turn_end":{"outstanding_shell_sessions":{"count":1,"ids":["synthetic-shell"]}},"seq":8}),
            json!({"type":"control_response","response":{"subtype":"unknown"},"seq":8}),
            json!({"type":"unknown_future_event","seq":8}),
        ];
        for (index, event) in events.into_iter().enumerate() {
            let id = format!("stale-{index}");
            let path = create_saved(&root, &id);
            append_events(&event_log_path(&path), &[event]);
            let store = resume(&root, &id).unwrap_or_else(|error| panic!("{id}: {error:#}"));
            let restored = store.restored.as_ref().unwrap();
            let gap = restored
                .checkpoint_gap
                .clone()
                .unwrap_or_else(|| panic!("{id}: expected a checkpoint gap"));
            assert_eq!(gap.records, 1, "{id}");
            assert_eq!(gap.meaningful_records, 1, "{id}");
            // The snapshot is still the authority for model history.
            assert_eq!(restored.snapshot, state().snapshot, "{id}");
            assert_eq!(restored.last_event_seq, 7, "{id}");
        }
    }

    #[test]
    fn uncheckpointed_tail_is_digested_for_recovery() {
        let (_dir, root) = root();
        let path = create_saved(&root, "tail");
        append_events(
            &event_log_path(&path),
            &[
                json!({"type":"user","message":{"content":[{"type":"text","text":"continue the migration"}]}}),
                json!({"type":"assistant","seq":8,"message":{"content":[
                    {"type":"text","text":"Reading the plan."},
                    {"type":"tool_use","id":"c1","name":"file_read","input":{"file_path":"PLAN.md"}}
                ]}}),
                json!({"type":"user","seq":9,"message":{"content":[{"type":"tool_result","tool_use_id":"c1","content":"plan body"}]}}),
                json!({"type":"assistant","seq":10,"message":{"content":[
                    {"type":"tool_use","id":"c2","name":"shell_run","input":{"command":"cargo check"}}
                ]}}),
                json!({"type":"system","subtype":"context_pressure","seq":11}),
            ],
        );
        let store = resume(&root, "tail").unwrap();
        let restored = store.restored.as_ref().unwrap();
        let gap = restored.checkpoint_gap.clone().unwrap();
        assert_eq!(gap.records, 5);
        assert_eq!(gap.meaningful_records, 4);
        assert_eq!(gap.assistant_steps, 2);
        assert_eq!(gap.user_messages, 1);
        assert_eq!(gap.tool_results, 1);
        assert_eq!(gap.user_texts, vec!["continue the migration"]);
        assert_eq!(
            gap.tool_calls,
            vec![
                r#"file_read {"file_path":"PLAN.md"}"#,
                r#"shell_run {"command":"cargo check"}"#
            ]
        );
        assert_eq!(
            gap.last_assistant_text.as_deref(),
            Some("Reading the plan.")
        );
        assert_eq!(gap.first_ts.as_deref(), Some("2026-01-01T00:00:00Z"));
        assert_eq!(gap.discarded_partial_bytes, 0);
        let notice = gap.model_notice(false);
        for expected in [
            "[Checkpoint gap recovered]",
            "continue the migration",
            "file_read",
            "cargo check",
            "NOT in your history",
            "Reading the plan.",
        ] {
            assert!(notice.contains(expected), "{expected}: {notice}");
        }
        assert!(!notice.contains("shell sessions"));
        assert!(gap.model_notice(true).contains("shell sessions"));
        assert_eq!(gap.to_json()["assistant_steps"], 2);
        assert_eq!(gap.to_json()["tool_calls"].as_array().unwrap().len(), 2);
        assert_eq!(restored.snapshot, state().snapshot);
        assert_eq!(restored.last_event_seq, 7);
    }

    #[test]
    fn legacy_sequence_checkpoint_ahead_of_snapshot_is_recovered_not_refused() {
        let (_dir, root) = root();
        let path = root.join("legacy.json");
        write_atomic(
            &path,
            &json!({
                "transport":"anthropic",
                "model":"m",
                "snapshot":[{"role":"user","content":[{"type":"text","text":"legacy task"}]}],
                "last_event_seq": 7,
            })
            .to_string(),
        )
        .unwrap();
        append_events(
            &event_log_path(&path),
            &[
                json!({"type":"assistant","seq":7}),
                json!({"type":"user","message":{"content":"steer after checkpoint"}}),
                json!({"type":"assistant","seq":8,"message":{"content":[
                    {"type":"tool_use","id":"x","name":"file_write","input":{"file_path":"a.txt"}}
                ]}}),
            ],
        );
        let store = resume(&root, "legacy").unwrap();
        let gap = store
            .restored
            .as_ref()
            .unwrap()
            .checkpoint_gap
            .clone()
            .unwrap();
        assert_eq!(gap.meaningful_records, 2);
        assert_eq!(gap.user_texts, vec!["steer after checkpoint"]);
        assert_eq!(gap.tool_calls, vec![r#"file_write {"file_path":"a.txt"}"#]);
    }

    #[test]
    fn harmless_trailing_lifecycle_and_control_metadata_allows_resume() {
        let (_dir, root) = root();
        let path = create_saved(&root, "metadata");
        let log = std::sync::Arc::new(crate::event_log::EventLog::at_path(event_log_path(&path)));
        let emitter =
            crate::emit::Emitter::with_callback("metadata".into(), std::sync::Arc::new(|_| {}))
                .with_event_log(log.clone())
                .with_seq_counter(std::sync::Arc::new(AtomicU64::new(7)));
        emitter.system_init_session();
        emitter.control_response_success(Some("synthetic-request"));
        emitter.context_pressure(10, Some(100), Some(75));
        emitter.turn_end_diagnostics(
            json!({"last_tool_results":[],"outstanding_shell_sessions":{"count":0,"ids":[]}}),
        );
        emitter.result(
            "",
            &crate::transport::Usage::default(),
            1,
            None,
            None,
            None,
            None,
        );
        log.append_milestone("session_resume", "metadata", json!({}));
        log.append_milestone("compaction_start", "metadata", json!({"reason":"manual"}));
        log.flush_blocking();
        let store = resume(&root, "metadata").unwrap();
        assert_eq!(store.restored.unwrap().last_event_seq, 7);
    }

    #[test]
    fn torn_tails_recover_and_invalid_byte_checkpoints_fail_closed() {
        let (_dir, root) = root();
        // A torn final record (crash mid-write, no trailing newline) is dropped
        // and reported, never refused: the bytes are not a record.
        let path = create_saved(&root, "torn");
        std::fs::write(event_log_path(&path), "{torn").unwrap();
        let store = resume(&root, "torn").unwrap();
        let gap = store
            .restored
            .as_ref()
            .unwrap()
            .checkpoint_gap
            .clone()
            .unwrap();
        assert_eq!(gap.discarded_partial_bytes, 5);
        assert_eq!(gap.meaningful_records, 0);
        assert!(gap.model_notice(false).contains("torn final log record"));
        assert_eq!(std::fs::read(event_log_path(&path)).unwrap(), b"");
        drop(store);
        // A complete record without its newline is torn too, and dropping it
        // keeps the next append parseable.
        std::fs::write(
            event_log_path(&path),
            json!({"event":{"type":"result"}}).to_string(),
        )
        .unwrap();
        let store = resume(&root, "torn").unwrap();
        assert!(
            store
                .restored
                .as_ref()
                .unwrap()
                .checkpoint_gap
                .as_ref()
                .unwrap()
                .discarded_partial_bytes
                > 0
        );
        assert_eq!(std::fs::metadata(event_log_path(&path)).unwrap().len(), 0);
        drop(store);
        // A complete but unparsable record is corruption, not a torn write.
        std::fs::write(event_log_path(&path), "{torn\n").unwrap();
        assert!(resume(&root, "torn").is_err());
        std::fs::remove_file(event_log_path(&path)).unwrap();
        // Checkpoint corruption still fails closed.
        let path = create_saved(&root, "boundary");
        let log = event_log_path(&path);
        append_events(&log, &[json!({"type":"result"})]);
        write_atomic(&path, &SessionStore::serialize(&state(), Some(1)).unwrap()).unwrap();
        assert!(resume(&root, "boundary").is_err());
        write_atomic(
            &path,
            &SessionStore::serialize(&state(), Some(10000)).unwrap(),
        )
        .unwrap();
        assert!(resume(&root, "boundary").is_err());
        std::fs::remove_file(log).unwrap();
        assert!(resume(&root, "boundary").is_err());
    }

    #[test]
    fn legacy_sequence_checkpoint_recovers_new_actions_and_unsequenced_turns() {
        let (_dir, root) = root();
        let path = create_saved(&root, "legacy-seq");
        write_atomic(
            &path,
            &json!({"transport":"anthropic","snapshot":[],"last_event_seq":7}).to_string(),
        )
        .unwrap();
        let log = event_log_path(&path);
        append_events(
            &log,
            &[json!({"type":"user"}), json!({"type":"assistant","seq":7})],
        );
        // Everything up to the covered sequence is the snapshot's: no gap.
        let covered = resume(&root, "legacy-seq").unwrap();
        assert!(covered.restored.as_ref().unwrap().checkpoint_gap.is_none());
        drop(covered);
        // An unsequenced user turn after the covered sequence is a gap.
        append_events(
            &log,
            &[json!({"type":"user","message":{"content":"unsaved"}})],
        );
        let store = resume(&root, "legacy-seq").unwrap();
        let gap = store
            .restored
            .as_ref()
            .unwrap()
            .checkpoint_gap
            .clone()
            .unwrap();
        assert_eq!(gap.meaningful_records, 1);
        assert_eq!(gap.user_texts, vec!["unsaved"]);
        drop(store);
        // So is a sequenced action beyond the checkpoint.
        std::fs::write(&log, "").unwrap();
        append_events(&log, &[json!({"type":"assistant","seq":8})]);
        let store = resume(&root, "legacy-seq").unwrap();
        let gap = store
            .restored
            .as_ref()
            .unwrap()
            .checkpoint_gap
            .clone()
            .unwrap();
        assert_eq!(gap.meaningful_records, 1);
        assert_eq!(gap.assistant_steps, 1);
    }

    #[test]
    fn serializer_requires_no_filesystem_and_keeps_explicit_checkpoint() {
        let serialized = SessionStore::serialize(&state(), Some(123)).unwrap();
        let value: Value = serde_json::from_str(&serialized).unwrap();
        assert_eq!(value["event_log_offset"], 123);
        assert_eq!(value["snapshot"], state().snapshot);
        assert_eq!(value["side"], state().side);
    }
    #[test]
    fn unresolved_remote_marker_refuses_resume_without_replacing_evidence() {
        let (_directory, root) = root();
        let store = SessionStore::open_in(&root, None, Some("remote-unknown"), None).unwrap();
        let mut saved = state();
        saved.side["remote_outcomes_unknown"] =
            json!(["MCP server fixture: deadline_exceeded; remote completion unknown"]);
        store.save(&saved).unwrap();
        let path = store.store_path().clone();
        let before = std::fs::read(&path).unwrap();
        drop(store);
        let error = resume(&root, "remote-unknown").unwrap_err();
        assert!(format!("{error:#}").contains("unresolved remote tool outcomes"));
        assert_eq!(
            std::fs::read(path).unwrap(),
            before,
            "refused resume preserves the original evidence"
        );
    }

    #[test]
    fn remote_marker_shape_is_strict_and_empty_legacy_state_remains_resumable() {
        let (_directory, root) = root();
        let path = create_saved(&root, "marker-shape");
        for marker in [
            Value::Null,
            json!("unknown"),
            json!({}),
            json!([false]),
            json!([""]),
        ] {
            let mut saved = state();
            saved.side["remote_outcomes_unknown"] = marker.clone();
            write_atomic(&path, &SessionStore::serialize(&saved, Some(0)).unwrap()).unwrap();
            assert!(
                resume(&root, "marker-shape").is_err(),
                "invalid or unresolved marker {marker}"
            );
        }
        for side in [json!({}), json!({"remote_outcomes_unknown":[]})] {
            let mut saved = state();
            saved.side = side;
            write_atomic(&path, &SessionStore::serialize(&saved, Some(0)).unwrap()).unwrap();
            drop(resume(&root, "marker-shape").unwrap());
        }
    }
}
