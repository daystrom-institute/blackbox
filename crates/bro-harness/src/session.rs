//! Transcript persistence for `--resume`. Stores the transport-native
//! conversation snapshot plus the transport tag (so a resume can refuse a
//! transport mismatch), and a generic loop-level `side` cell.
//!
//! Two persistence planes live in one file:
//!
//! - `snapshot` — transport-native conversation state. Opaque to the loop; the
//!   transport owns its shape.
//! - `side` — transport-agnostic loop-level state that must survive
//!   `exec → resume` (the clipboard registers, the todo list). Opaque to
//!   *session.rs*; the agent loop owns its shape. Kept a sibling of `snapshot`
//!   (not nested inside it) precisely because it is transport-independent.

use anyhow::{Context, Result};
use serde_json::{Value, json};
#[cfg(test)]
use std::path::Path;
use std::path::PathBuf;

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
                        snapshot: v["snapshot"].clone(),
                        side: v.get("side").cloned().unwrap_or(Value::Null),
                    })
                }
                Err(_) => None, // resume id with no prior file → start clean
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
            "snapshot": state.snapshot,
            "side": state.side,
        }))
        .context("serialize session")?;
        std::fs::write(&self.path, body).context("write session")?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
                snapshot: json!({"msgs": 1}),
                side: json!({"clipboard": {"@": "hello"}, "todos": []}),
            })
            .unwrap();

        let r = resume_in(&dir, "sess-1").restored.expect("restored");
        assert_eq!(r.transport, "anthropic");
        assert_eq!(r.model.as_deref(), Some("m"));
        assert_eq!(r.snapshot, json!({"msgs": 1}));
        assert_eq!(r.side["clipboard"]["@"], json!("hello"));

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

        std::fs::remove_dir_all(&dir).ok();
    }
}
