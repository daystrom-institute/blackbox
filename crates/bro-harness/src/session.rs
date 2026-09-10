//! Versioned transport snapshots and exclusive session-writer ownership.
//!
//! Explicit resume requires a valid snapshot. The event log is evidence, not a
//! recovery source: conversation or actions beyond its saved checkpoint require
//! explicit recovery rather than silently starting from older model history.

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
    pub snapshot: Value,
    pub side: Value,
    pub last_event_seq: u64,
    pub event_log_offset: Option<u64>,
}

pub struct SaveState<'a> {
    pub transport: &'a str,
    pub model: &'a str,
    pub code_mode: &'a str,
    pub service_tier: Option<&'a str>,
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
        let restored = parse_restored(&body)
            .with_context(|| format!("invalid resumed session {}", source_path.display()))?;
        validate_event_checkpoint(
            &event_log_path(&source_path),
            restored.event_log_offset,
            restored.last_event_seq,
        )?;
        // A destination log with no destination snapshot cannot be merged with
        // a legacy source: it may describe a different, unsaved conversation.
        if source_path != path {
            validate_event_checkpoint(&event_log_path(&path), Some(0), restored.last_event_seq)?;
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
    if let Some(outstanding) = side.get("runtime_work_outstanding") {
        let outstanding = outstanding
            .as_bool()
            .context("runtime_work_outstanding must be a boolean")?;
        anyhow::ensure!(
            !outstanding,
            "session checkpoint contains outstanding runtime work or unconsumed cell/shell output; explicit recovery required before resume. Inspect durable effects and recover the recorded outcomes; process-local handles cannot be restored."
        );
    }
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
        snapshot: snapshot.clone(),
        side,
        last_event_seq,
        event_log_offset,
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
            Some("init" | "context_pressure" | "mcp_readiness") => true,
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

#[allow(clippy::disallowed_methods)]
fn validate_event_checkpoint(
    path: &Path,
    event_log_offset: Option<u64>,
    last_event_seq: u64,
) -> Result<()> {
    let mut file = match File::open(path) {
        Ok(file) => file,
        Err(error)
            if error.kind() == std::io::ErrorKind::NotFound
                && event_log_offset.unwrap_or(0) == 0 =>
        {
            return Ok(());
        }
        Err(error) => {
            return Err(error)
                .with_context(|| format!("read resumed event log {}", path.display()));
        }
    };
    if let Some(offset) = event_log_offset {
        anyhow::ensure!(
            file.metadata()?.len() >= offset,
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
    }
    let mut unsequenced_meaningful = false;
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
        anyhow::ensure!(
            line.ends_with('\n'),
            "event log contains an incomplete final record: {}",
            path.display()
        );
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
            anyhow::ensure!(
                !meaningful,
                "session event log contains conversation or actions after the saved snapshot; explicit recovery required: {}",
                path.display()
            );
        } else if let Some(seq) = seq {
            if seq <= last_event_seq {
                unsequenced_meaningful = false;
            } else {
                anyhow::ensure!(
                    !meaningful && !unsequenced_meaningful,
                    "session event log is ahead of its saved snapshot; explicit recovery required: {}",
                    path.display()
                );
            }
        } else {
            unsequenced_meaningful |= meaningful;
        }
    }
    anyhow::ensure!(
        event_log_offset.is_some() || !unsequenced_meaningful,
        "session event log has uncheckpointed conversation or actions; explicit recovery required: {}",
        path.display()
    );
    Ok(())
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
        assert_eq!(restored.last_event_seq, 7);
    }

    #[test]
    fn outstanding_runtime_checkpoint_refuses_resume_and_id_reuse() {
        let (_dir, root) = root();
        let store = SessionStore::open_in(&root, None, Some("outstanding"), None).unwrap();
        let mut snapshot = state();
        snapshot.side["runtime_work_outstanding"] = json!(true);
        store.save(&snapshot).unwrap();
        let path = store.store_path().clone();
        let before = std::fs::read(&path).unwrap();
        drop(store);
        let error = format!("{:#}", resume(&root, "outstanding").unwrap_err());
        assert!(error.contains("outstanding runtime work"), "{error}");
        assert!(error.contains("explicit recovery required"), "{error}");
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
    }

    #[test]
    fn meaningful_event_log_tail_refuses_stale_resume_even_without_sequence() {
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
            let error = resume(&root, &id).unwrap_err();
            assert!(format!("{error:#}").contains("explicit recovery"));
        }
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
    fn malformed_tail_and_invalid_byte_checkpoints_fail_closed() {
        let (_dir, root) = root();
        let path = create_saved(&root, "torn");
        std::fs::write(event_log_path(&path), "{torn").unwrap();
        assert!(resume(&root, "torn").is_err());
        std::fs::write(
            event_log_path(&path),
            json!({"event":{"type":"result"}}).to_string(),
        )
        .unwrap();
        assert!(resume(&root, "torn").is_err());
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
    fn legacy_sequence_checkpoint_rejects_new_actions_and_unsequenced_turns() {
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
        drop(resume(&root, "legacy-seq").unwrap());
        append_events(
            &log,
            &[json!({"type":"user","message":{"content":"unsaved"}})],
        );
        assert!(resume(&root, "legacy-seq").is_err());
        std::fs::write(&log, "").unwrap();
        append_events(&log, &[json!({"type":"assistant","seq":8})]);
        assert!(resume(&root, "legacy-seq").is_err());
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
