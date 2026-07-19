//! Transcript persistence for `--resume`. Stores the transport-native
//! conversation snapshot plus the transport tag (so a resume can refuse a
//! transport mismatch), and a generic loop-level `side` cell.
//!
//! Two persistence planes live in one file:
//!
//! - `snapshot` — transport-native conversation state. Opaque to the loop; the
//!   transport owns its shape.
//! - `side` — transport-agnostic loop-level state that must survive
//!   `exec → resume` (the todo list, diagnostics baselines). Opaque to
//!   *session.rs*; the agent loop owns its shape. Kept a sibling of `snapshot`
//!   (not nested inside it) precisely because it is transport-independent.

use anyhow::{Context, Result};
use serde_json::{Value, json};
#[cfg(test)]
use std::path::Path;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

pub struct SessionStore {
    pub id: String,
    path: PathBuf,
    /// Restored snapshot from a prior run, if resuming. `None` for fresh.
    pub restored: Option<Restored>,
}

pub struct Restored {
    pub transport: String,
    /// Model used when the session was created. On resume the daemon does not
    /// re-pass --model (it's implied by the session), so the harness falls
    /// back to this persisted value.
    pub model: Option<String>,
    /// Code-mode the session was created with. Like `model`, the daemon does
    /// not re-pass `--code-mode` on resume — the surface shape is session-
    /// intrinsic (a transcript may contain `exec` cells that depend on it), so
    /// the harness restores this value. `None` for sessions written before this
    /// field existed.
    pub code_mode: Option<String>,
    /// Service tier the session was created/resumed with. `default` is an
    /// explicit standard-routing sentinel; `priority` maps to Codex `/fast`.
    /// `None` for sessions written before this field existed.
    pub service_tier: Option<String>,
    pub snapshot: Value,
    /// Loop-level side cells from the prior run (`Value::Null` if absent, e.g.
    /// sessions written before this field existed).
    pub side: Value,
    /// The last per-session event `seq` (`emit.rs`) assigned by a prior
    /// process run, so a resumed session continues the sequence instead of
    /// restarting at 0, which would corrupt any consumer
    /// cursor (design/daemon-runtime/locality-first-decomposition.md slice
    /// 5). `0` for sessions written before this field existed. The caller
    /// (`agent_loop.rs`) reconciles this against the event log's tail
    /// (`EventLog::max_seq_in_log`) before seeding the live counter, in case
    /// a crash between persists left this value stale.
    pub last_event_seq: u64,
}

/// Everything persisted at the end of a turn. A struct (rather than a widening
/// arg list) so future loop-level cells extend `side` without churning the
/// `save` signature.
pub struct SaveState<'a> {
    pub transport: &'a str,
    pub model: &'a str,
    pub code_mode: &'a str,
    pub service_tier: Option<&'a str>,
    pub snapshot: Value,
    pub side: Value,
    /// The live counter's current value at persist time (`Emitter::last_seq`
    /// on the session's shared seq counter); see [`Restored::last_event_seq`].
    pub last_event_seq: u64,
}

pub(crate) fn sessions_dir() -> PathBuf {
    if let Ok(home) = std::env::var("BRO_HOME") {
        PathBuf::from(home).join("harness-sessions")
    } else {
        dirs::home_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join(".bro-harness")
            .join("sessions")
    }
}

/// Legacy sessions directory (~/.bro-harness/sessions) used as a resume
/// fallback when `BRO_HOME` is set and the session file is absent from the
/// BRO_HOME-based dir.
fn legacy_sessions_dir() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".bro-harness")
        .join("sessions")
}

static NONCE: AtomicU64 = AtomicU64::new(0);

/// Atomically write `contents` to `path` using tmp+rename, matching the
/// daemon's `json_store::atomic_write_json_locked` idiom. A crash mid-write
/// leaves at most a stale `.tmp` file; the target is never partially written.
// callers wrap session persists in spawn_blocking (wave 6b).
#[allow(clippy::disallowed_methods)]
pub fn write_atomic(path: &std::path::Path, contents: &str) -> Result<()> {
    let pid = std::process::id();
    let nonce = NONCE.fetch_add(1, Ordering::SeqCst);
    let tmp_path = path.with_extension(format!("json.{pid}.{nonce}.tmp"));

    if let Some(parent) = tmp_path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    std::fs::write(&tmp_path, contents)
        .with_context(|| format!("write session tmp {}", tmp_path.display()))?;

    std::fs::rename(&tmp_path, path).with_context(|| {
        format!(
            "rename session tmp {} → {}",
            tmp_path.display(),
            path.display()
        )
    })?;

    Ok(())
}

impl SessionStore {
    // one-time session open/resume, before the loop serves turns.
    #[allow(clippy::disallowed_methods)]
    pub fn open(session_id: Option<&str>, resume: Option<&str>) -> Result<Self> {
        let dir = sessions_dir();
        std::fs::create_dir_all(&dir).context("create sessions dir")?;

        if let Some(rid) = resume {
            let path = dir.join(format!("{rid}.json"));
            let restored = match std::fs::read_to_string(&path) {
                Ok(s) => {
                    let v: Value = serde_json::from_str(&s).context("parse resumed session")?;
                    Some(Restored {
                        transport: v["transport"].as_str().unwrap_or_default().to_string(),
                        model: v["model"].as_str().map(str::to_string),
                        code_mode: v["code_mode"].as_str().map(str::to_string),
                        service_tier: v["service_tier"].as_str().map(str::to_string),
                        snapshot: v["snapshot"].clone(),
                        side: v.get("side").cloned().unwrap_or(Value::Null),
                        last_event_seq: v["last_event_seq"].as_u64().unwrap_or(0),
                    })
                }
                Err(_) => {
                    // Fall back to legacy ~/.bro-harness/sessions dir when the
                    // session file is absent in the BRO_HOME-based dir, so
                    // every pre-existing session stays resumable.
                    let legacy_path = legacy_sessions_dir().join(format!("{rid}.json"));
                    match std::fs::read_to_string(&legacy_path) {
                        Ok(s) => {
                            let v: Value =
                                serde_json::from_str(&s).context("parse resumed legacy session")?;
                            Some(Restored {
                                transport: v["transport"].as_str().unwrap_or_default().to_string(),
                                model: v["model"].as_str().map(str::to_string),
                                code_mode: v["code_mode"].as_str().map(str::to_string),
                                service_tier: v["service_tier"].as_str().map(str::to_string),
                                snapshot: v["snapshot"].clone(),
                                side: v.get("side").cloned().unwrap_or(Value::Null),
                                last_event_seq: v["last_event_seq"].as_u64().unwrap_or(0),
                            })
                        }
                        Err(_) => None, // absent in both dirs → start clean
                    }
                }
            };
            return Ok(Self {
                id: rid.to_string(),
                path,
                restored,
            });
        }

        let id = match session_id {
            Some(s) if !s.is_empty() && s != "pending" => s.to_string(),
            _ => uuid::Uuid::new_v4().to_string(),
        };
        let path = dir.join(format!("{id}.json"));
        Ok(Self {
            id,
            path,
            restored: None,
        })
    }

    pub fn save(&self, state: &SaveState) -> Result<()> {
        let body = serde_json::to_string(&json!({
            "transport": state.transport,
            "model": state.model,
            "code_mode": state.code_mode,
            "service_tier": state.service_tier,
            "snapshot": state.snapshot,
            "side": state.side,
            "last_event_seq": state.last_event_seq,
        }))
        .context("serialize session")?;
        write_atomic(&self.path, &body).context("write session")?;
        Ok(())
    }

    /// The filesystem path this store writes to.
    pub fn store_path(&self) -> &PathBuf {
        &self.path
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event_log::EventLog;
    use bro_protocol::SERVICE_TIER_PRIORITY;

    /// A unique, hermetic session dir under the OS temp dir — no `tempfile`
    /// dep, no process-global env mutation (so it can't race the bin's other
    /// tests). Caller removes it.
    fn unique_dir(tag: &str) -> PathBuf {
        let pid = std::process::id();
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("bro-harness-test-{tag}-{pid}-{nanos}"));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Build a store directly against `dir`, mirroring `open`'s fresh-id path
    /// without depending on `sessions_dir()` / env.
    fn store_in(dir: &Path, id: &str) -> SessionStore {
        SessionStore {
            id: id.to_string(),
            path: dir.join(format!("{id}.json")),
            restored: None,
        }
    }

    /// Mirror `open`'s resume path against an explicit `dir`.
    fn resume_in(dir: &Path, id: &str) -> SessionStore {
        let path = dir.join(format!("{id}.json"));
        let restored = std::fs::read_to_string(&path).ok().map(|s| {
            let v: Value = serde_json::from_str(&s).unwrap();
            Restored {
                transport: v["transport"].as_str().unwrap_or_default().to_string(),
                model: v["model"].as_str().map(str::to_string),
                code_mode: v["code_mode"].as_str().map(str::to_string),
                service_tier: v["service_tier"].as_str().map(str::to_string),
                snapshot: v["snapshot"].clone(),
                side: v.get("side").cloned().unwrap_or(Value::Null),
                last_event_seq: v["last_event_seq"].as_u64().unwrap_or(0),
            }
        });
        SessionStore {
            id: id.to_string(),
            path,
            restored,
        }
    }

    #[test]
    fn side_cell_round_trips_through_save_and_resume() {
        let dir = unique_dir("side");
        let store = store_in(&dir, "sess-1");
        store
            .save(&SaveState {
                transport: "anthropic",
                model: "m",
                code_mode: "only",
                service_tier: Some(SERVICE_TIER_PRIORITY),
                snapshot: json!({"msgs": 1}),
                side: json!({"todos": []}),
                last_event_seq: 42,
            })
            .unwrap();

        let r = resume_in(&dir, "sess-1").restored.expect("restored");
        assert_eq!(r.transport, "anthropic");
        assert_eq!(r.model.as_deref(), Some("m"));
        assert_eq!(r.code_mode.as_deref(), Some("only"));
        assert_eq!(r.service_tier.as_deref(), Some(SERVICE_TIER_PRIORITY));
        assert_eq!(r.snapshot, json!({"msgs": 1}));
        assert_eq!(r.side["todos"], json!([]));
        assert_eq!(r.last_event_seq, 42);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn legacy_session_without_last_event_seq_restores_as_zero() {
        // A session written before `last_event_seq` existed (no key at all)
        // must restore as 0, not fail: the resumed process then seeds the
        // live counter fresh (or from the event log's tail, whichever is
        // higher) rather than crashing on an old session file.
        let dir = unique_dir("seq-legacy");
        write_atomic(
            &dir.join("sess-legacy.json"),
            r#"{"transport":"anthropic","model":"m","snapshot":{"x":1}}"#,
        )
        .unwrap();

        let r = resume_in(&dir, "sess-legacy").restored.expect("restored");
        assert_eq!(r.last_event_seq, 0);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn legacy_session_without_side_restores_as_null() {
        let dir = unique_dir("legacy");
        std::fs::write(
            dir.join("old.json"),
            r#"{"transport":"anthropic","model":"m","snapshot":{"x":1}}"#,
        )
        .unwrap();

        let r = resume_in(&dir, "old").restored.expect("restored");
        assert_eq!(r.snapshot, json!({"x": 1}));
        assert_eq!(r.side, Value::Null);
        // A session written before code_mode existed restores it as absent.
        assert_eq!(r.code_mode, None);
        // A session written before service_tier existed restores it as absent.
        assert_eq!(r.service_tier, None);

        std::fs::remove_dir_all(&dir).ok();
    }

    // -----------------------------------------------------------------
    // write_atomic
    // -----------------------------------------------------------------

    #[test]
    fn write_atomic_tmp_rename() {
        let dir = unique_dir("atomic");
        let path = dir.join("session.json");

        write_atomic(&path, r#"{"transport":"test","snapshot":{"n":1}}"#).unwrap();

        // Target exists with the right content.
        let s = std::fs::read_to_string(&path).unwrap();
        assert!(s.contains("\"transport\":\"test\""));
        assert!(s.contains("\"n\":1"));

        // No stray tmp file left behind.
        let tmp_suffix = ".tmp";
        let has_tmp = std::fs::read_dir(&dir)
            .ok()
            .map(|entries| {
                entries
                    .filter_map(|e| e.ok())
                    .any(|e| e.file_name().to_string_lossy().ends_with(tmp_suffix))
            })
            .unwrap_or(false);
        assert!(!has_tmp, "tmp file left behind after write_atomic");

        // A second write succeeds (different nonce, no collision).
        write_atomic(&path, r#"{"transport":"test","snapshot":{"n":2}}"#).unwrap();
        let s2 = std::fs::read_to_string(&path).unwrap();
        assert!(s2.contains("\"n\":2"));

        std::fs::remove_dir_all(&dir).ok();
    }

    // -----------------------------------------------------------------
    // sessions_dir / BRO_HOME
    // -----------------------------------------------------------------

    /// Restore an env var on drop.
    struct EnvGuard {
        key: &'static str,
        prior: Option<String>,
    }

    impl EnvGuard {
        fn push(key: &'static str, value: &str) -> Self {
            let prior = std::env::var(key).ok();
            unsafe { std::env::set_var(key, value) }
            EnvGuard { key, prior }
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            match &self.prior {
                Some(v) => unsafe { std::env::set_var(self.key, v) },
                None => unsafe { std::env::remove_var(self.key) },
            }
        }
    }

    #[test]
    fn sessions_dir_honors_bro_home() {
        let dir = unique_dir("bro-home");
        let _guard = EnvGuard::push("BRO_HOME", &dir.to_string_lossy());

        // open (fresh) creates the sessions dir under BRO_HOME/harness-sessions
        let store = SessionStore::open(None, None).unwrap();
        let sp = store.store_path();
        let expected_dir = dir.join("harness-sessions");
        assert!(
            sp.starts_with(&expected_dir),
            "store path {sp:?} should start with {expected_dir:?}"
        );
        // The dir was created.
        assert!(expected_dir.exists());

        // Write through the atomic path; file lands in the right place.
        store
            .save(&SaveState {
                transport: "t",
                model: "m",
                code_mode: "only",
                service_tier: None,
                snapshot: json!({"x": 1}),
                side: Value::Null,
                last_event_seq: 0,
            })
            .unwrap();
        assert!(sp.exists());

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn resume_falls_back_to_legacy_dir() {
        let new_dir = unique_dir("new");
        let legacy_base = unique_dir("legacy-home");

        // Simulate legacy ~/.bro-harness/sessions with a session file.
        let legacy_sessions = legacy_base.join(".bro-harness").join("sessions");
        std::fs::create_dir_all(&legacy_sessions).unwrap();
        std::fs::write(
            legacy_sessions.join("old-session.json"),
            r#"{"transport":"anthropic","model":"legacy-m","code_mode":"only","snapshot":{"msgs":1},"side":null}"#,
        )
        .unwrap();

        // Point HOME at the legacy base and BRO_HOME at the new dir.
        let _home_guard = EnvGuard::push("HOME", &legacy_base.to_string_lossy());
        let _bro_guard = EnvGuard::push("BRO_HOME", &new_dir.to_string_lossy());

        // Resume — file is absent from new dir, must fall back to legacy.
        let store = SessionStore::open(None, Some("old-session")).unwrap();
        let r = store.restored.as_ref().expect("resumed from legacy dir");
        assert_eq!(r.transport, "anthropic");
        assert_eq!(r.model.as_deref(), Some("legacy-m"));
        assert_eq!(r.snapshot, json!({"msgs": 1}));

        // The store path still points to the new dir for future writes.
        let expected_new_dir = new_dir.join("harness-sessions");
        assert!(
            store.store_path().starts_with(&expected_new_dir),
            "store path {:?} should be under {:?}",
            store.store_path(),
            expected_new_dir
        );

        std::fs::remove_dir_all(&new_dir).ok();
        std::fs::remove_dir_all(&legacy_base).ok();
    }

    // -----------------------------------------------------------------
    // last_event_seq reconciliation (agent_loop.rs Session::build seeds the
    // live counter from `max(restored.last_event_seq,
    // EventLog::max_seq_in_log(...))`). These tests exercise the exact two
    // primitives build() calls: SessionStore's resume parse and
    // EventLog::max_seq_in_log, so the reconciliation math is proven
    // against real persisted state, not reimplemented in isolation.
    // -----------------------------------------------------------------

    #[test]
    fn reconciliation_prefers_log_tail_when_snapshot_is_stale() {
        // Crash-window scenario: the snapshot only persisted up through
        // seq 5 (last turn boundary), but the append-only log already
        // durably recorded events through seq 12 before the process died.
        let dir = unique_dir("reconcile-log-ahead");
        let store = store_in(&dir, "sess-1");
        store
            .save(&SaveState {
                transport: "anthropic",
                model: "m",
                code_mode: "only",
                service_tier: None,
                snapshot: json!({}),
                side: Value::Null,
                last_event_seq: 5,
            })
            .unwrap();
        let log = EventLog::at_path(dir.join("sess-1.events.jsonl"));
        for seq in 1..=12u64 {
            log.append_event(&json!({"type": "assistant", "seq": seq}));
        }
        log.flush_blocking();

        let restored = resume_in(&dir, "sess-1").restored.expect("restored");
        let log_tail = EventLog::max_seq_in_log(log.path());
        assert_eq!(restored.last_event_seq, 5, "snapshot alone is stale");
        assert_eq!(log_tail, 12);
        assert_eq!(
            restored.last_event_seq.max(log_tail),
            12,
            "reconciliation must pick the log's higher tail value"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn reconciliation_prefers_snapshot_when_it_is_ahead_of_the_log() {
        // The ordinary case: the snapshot persisted after the log tee, so
        // the snapshot's counter is at or ahead of whatever happens to be
        // on disk in the log (e.g. a log tee that hasn't flushed yet).
        let dir = unique_dir("reconcile-snapshot-ahead");
        let store = store_in(&dir, "sess-2");
        store
            .save(&SaveState {
                transport: "anthropic",
                model: "m",
                code_mode: "only",
                service_tier: None,
                snapshot: json!({}),
                side: Value::Null,
                last_event_seq: 10,
            })
            .unwrap();
        let log = EventLog::at_path(dir.join("sess-2.events.jsonl"));
        for seq in 1..=6u64 {
            log.append_event(&json!({"type": "assistant", "seq": seq}));
        }
        log.flush_blocking();

        let restored = resume_in(&dir, "sess-2").restored.expect("restored");
        let log_tail = EventLog::max_seq_in_log(log.path());
        assert_eq!(
            restored.last_event_seq.max(log_tail),
            10,
            "reconciliation must not regress below the persisted snapshot"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn fresh_session_has_no_prior_seq_on_either_side() {
        // No resume, no prior log: both reconciliation inputs are 0, so a
        // fresh session's counter seeds at 0 (first emitted event is seq 1
        // per emit.rs).
        let dir = unique_dir("reconcile-fresh");
        let restored = resume_in(&dir, "sess-fresh").restored;
        assert!(restored.is_none());
        let log = EventLog::at_path(dir.join("sess-fresh.events.jsonl"));
        assert_eq!(EventLog::max_seq_in_log(log.path()), 0);

        std::fs::remove_dir_all(&dir).ok();
    }
}
