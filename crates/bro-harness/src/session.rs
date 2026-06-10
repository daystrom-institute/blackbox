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
}

fn sessions_dir() -> PathBuf {
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
                    })
                }
                Err(_) => {
                    // Fall back to legacy ~/.bro-harness/sessions dir when the
                    // session file is absent in the BRO_HOME-based dir, so
                    // every pre-existing session stays resumable.
                    let legacy_path = legacy_sessions_dir().join(format!("{rid}.json"));
                    match std::fs::read_to_string(&legacy_path) {
                        Ok(s) => {
                            let v: Value = serde_json::from_str(&s)
                                .context("parse resumed legacy session")?;
                            Some(Restored {
                                transport: v["transport"]
                                    .as_str()
                                    .unwrap_or_default()
                                    .to_string(),
                                model: v["model"].as_str().map(str::to_string),
                                code_mode: v["code_mode"].as_str().map(str::to_string),
                                service_tier: v["service_tier"]
                                    .as_str()
                                    .map(str::to_string),
                                snapshot: v["snapshot"].clone(),
                                side: v.get("side").cloned().unwrap_or(Value::Null),
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
            })
            .unwrap();

        let r = resume_in(&dir, "sess-1").restored.expect("restored");
        assert_eq!(r.transport, "anthropic");
        assert_eq!(r.model.as_deref(), Some("m"));
        assert_eq!(r.code_mode.as_deref(), Some("only"));
        assert_eq!(r.service_tier.as_deref(), Some(SERVICE_TIER_PRIORITY));
        assert_eq!(r.snapshot, json!({"msgs": 1}));
        assert_eq!(r.side["todos"], json!([]));

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
}
