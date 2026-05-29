//! Transcript persistence for `--resume`. Stores the transport-native
//! conversation snapshot plus the transport tag (so a resume can refuse a
//! transport mismatch). Opaque to the loop — the transport owns the shape.

use anyhow::{Context, Result};
use serde_json::{Value, json};
use std::path::PathBuf;

pub struct SessionStore {
    pub id: String,
    path: PathBuf,
    /// Restored snapshot from a prior run, if resuming. `None` for fresh.
    pub restored: Option<Restored>,
}

pub struct Restored {
    pub transport: String,
    pub snapshot: Value,
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
                        snapshot: v["snapshot"].clone(),
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

    pub fn save(&self, transport: &str, snapshot: Value) -> Result<()> {
        let body = serde_json::to_string(&json!({
            "transport": transport,
            "snapshot": snapshot,
        }))
        .context("serialize session")?;
        std::fs::write(&self.path, body).context("write session")?;
        Ok(())
    }
}
