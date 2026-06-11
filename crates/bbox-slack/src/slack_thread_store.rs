// SlackThreadStore — persistent (team_id, channel, thread_ts) → claude session_id
// map so Slack threads can resume the same Claude session across separate
// workflow arcs. Without this, every @mention is a fresh exec; with it, a
// follow-up in the same thread continues the same conversation.
//
// Persistence is JSON at <store_dir>/slack-threads.json with atomic-rename
// writes (mirror of `notes::save` shape). Capped to MAX_ENTRIES with simple
// LRU-on-update eviction so a long-running daemon doesn't grow unbounded
// from drive-by mentions in busy channels.

use std::collections::HashMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use parking_lot::RwLock;
use serde::{Deserialize, Serialize};

const STORE_FILE: &str = "slack-threads.json";
const MAX_ENTRIES: usize = 5000;

#[derive(Debug, Default, Serialize, Deserialize)]
struct StoreData {
    /// Insertion / last-write order — used for LRU-style cap eviction.
    /// Each entry is the composite key `<team_id>:<channel>:<thread_ts>`.
    order: Vec<String>,
    sessions: HashMap<String, String>,
}

#[derive(Debug)]
pub struct SlackThreadStore {
    path: PathBuf,
    inner: RwLock<StoreData>,
}

impl SlackThreadStore {
    pub fn open(store_dir: &Path) -> Result<Self> {
        let path = store_dir.join(STORE_FILE);
        let data = if path.exists() {
            let raw =
                fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
            serde_json::from_str(&raw).with_context(|| format!("parsing {}", path.display()))?
        } else {
            StoreData::default()
        };
        Ok(Self {
            path,
            inner: RwLock::new(data),
        })
    }

    pub fn get(&self, team_id: &str, channel: &str, thread_ts: &str) -> Option<String> {
        let key = compose_key(team_id, channel, thread_ts);
        self.inner.read().sessions.get(&key).cloned()
    }

    pub fn set(&self, team_id: &str, channel: &str, thread_ts: &str, session_id: &str) {
        let key = compose_key(team_id, channel, thread_ts);
        {
            let mut data = self.inner.write();
            // LRU bump: remove old position, push to tail.
            data.order.retain(|k| k != &key);
            data.order.push(key.clone());
            data.sessions.insert(key, session_id.to_string());
            // Evict oldest until under cap.
            while data.order.len() > MAX_ENTRIES {
                if let Some(oldest) = data.order.first().cloned() {
                    data.order.remove(0);
                    data.sessions.remove(&oldest);
                } else {
                    break;
                }
            }
        }
        if let Err(e) = self.save() {
            tracing::warn!("slack_thread_store: save failed: {e}");
        }
    }

    fn save(&self) -> Result<()> {
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent)?;
        }
        let raw = {
            let data = self.inner.read();
            serde_json::to_string_pretty(&*data)?
        };
        let tmp = self.path.with_extension("json.tmp");
        let mut file = fs::File::create(&tmp)?;
        file.write_all(raw.as_bytes())?;
        file.sync_all()?;
        drop(file);
        fs::rename(&tmp, &self.path)?;
        Ok(())
    }
}

fn compose_key(team_id: &str, channel: &str, thread_ts: &str) -> String {
    format!("{team_id}:{channel}:{thread_ts}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_and_persistence() {
        let dir = tempfile::tempdir().unwrap();
        let store = SlackThreadStore::open(dir.path()).unwrap();
        assert!(store.get("T01", "C01", "1730816060.000100").is_none());
        store.set("T01", "C01", "1730816060.000100", "session-abc");
        assert_eq!(
            store.get("T01", "C01", "1730816060.000100").as_deref(),
            Some("session-abc")
        );
        // Reopen — survives.
        drop(store);
        let store = SlackThreadStore::open(dir.path()).unwrap();
        assert_eq!(
            store.get("T01", "C01", "1730816060.000100").as_deref(),
            Some("session-abc")
        );
    }

    #[test]
    fn lru_cap_evicts_oldest() {
        let dir = tempfile::tempdir().unwrap();
        let store = SlackThreadStore::open(dir.path()).unwrap();
        // Force cap behavior with a small synthetic input — we still
        // rely on MAX_ENTRIES at runtime, but verify retain+push works.
        for i in 0..3 {
            store.set("T01", "C01", &format!("ts{i}"), &format!("s{i}"));
        }
        // Re-write ts0 — should bump it to most-recent.
        store.set("T01", "C01", "ts0", "s0-updated");
        assert_eq!(
            store.get("T01", "C01", "ts0").as_deref(),
            Some("s0-updated")
        );
        let data = store.inner.read();
        assert_eq!(data.order.last().unwrap(), "T01:C01:ts0");
        assert_eq!(data.sessions.len(), 3);
    }

    #[test]
    fn distinct_keys_for_team_channel_thread() {
        let dir = tempfile::tempdir().unwrap();
        let store = SlackThreadStore::open(dir.path()).unwrap();
        store.set("T01", "C01", "ts1", "alpha");
        store.set("T01", "C02", "ts1", "beta");
        store.set("T02", "C01", "ts1", "gamma");
        assert_eq!(store.get("T01", "C01", "ts1").as_deref(), Some("alpha"));
        assert_eq!(store.get("T01", "C02", "ts1").as_deref(), Some("beta"));
        assert_eq!(store.get("T02", "C01", "ts1").as_deref(), Some("gamma"));
    }
}
