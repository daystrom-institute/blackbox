use std::collections::HashMap;
use std::fs;
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use anyhow::{Context, Result};

use super::types::{JournalEnvelope, SystemEvent};

/// Retain at most this many events per compaction pass.
pub const EVENT_RETENTION_MAX: usize = 10_000;
/// Drop events older than this many days.
pub const EVENT_RETENTION_DAYS: i64 = 7;

pub struct EventStore {
    root: PathBuf,
    lock: Mutex<()>,
    index: Mutex<HashMap<String, SystemEvent>>,
}

#[derive(Debug, Default, serde::Serialize)]
pub struct EventCompactionReport {
    pub before: usize,
    pub after: usize,
    pub dropped_by_age: usize,
    pub dropped_by_count: usize,
}

impl EventStore {
    pub fn new(bro_home: &Path) -> Self {
        let root = bro_home.join("events").join("journal");
        Self::build(root)
    }

    // Not `#[cfg(test)]` gated: consumer crates (the root crate's runtime
    // and tool tests) call this from their own test modules, where this
    // crate compiles as a normal dependency and `cfg(test)` is false.
    pub fn new_at(root: PathBuf) -> Self {
        Self::build(root)
    }

    fn build(root: PathBuf) -> Self {
        let index = Mutex::new(HashMap::new());
        let store = Self {
            root,
            lock: Mutex::new(()),
            index,
        };
        // Sweep orphan `current.tmp` left by a crashed compaction. The
        // complete `current.jsonl` is authoritative on reopen.
        store.remove_stale_tmp();
        if let Ok(envelopes) = store.load_all_from_disk() {
            let mut idx = store.index.lock().unwrap();
            for env in envelopes {
                idx.insert(env.event.id.clone(), env.event);
            }
        }
        store
    }

    fn current_path(&self) -> PathBuf {
        self.root.join("current.jsonl")
    }

    pub fn append(&self, envelope: &JournalEnvelope) -> Result<()> {
        let _guard = self.lock.lock().unwrap();
        fs::create_dir_all(&self.root)
            .with_context(|| format!("creating event journal dir {}", self.root.display()))?;
        let path = self.current_path();
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .with_context(|| format!("opening journal file {}", path.display()))?;
        serde_json::to_writer(&mut file, envelope)
            .with_context(|| format!("writing event to {}", path.display()))?;
        writeln!(file)?;
        file.sync_all()?;
        self.index
            .lock()
            .unwrap()
            .insert(envelope.event.id.clone(), envelope.event.clone());
        Ok(())
    }

    pub fn load_all(&self) -> Result<Vec<JournalEnvelope>> {
        let _guard = self.lock.lock().unwrap();
        self.load_all_from_disk()
    }

    fn load_all_from_disk(&self) -> Result<Vec<JournalEnvelope>> {
        let path = self.current_path();
        if !path.exists() {
            return Ok(Vec::new());
        }
        let file = File::open(&path)
            .with_context(|| format!("opening journal file {}", path.display()))?;
        let reader = BufReader::new(file);
        let mut envelopes = Vec::new();
        for (i, line) in reader.lines().enumerate() {
            let line =
                line.with_context(|| format!("reading line {} from {}", i + 1, path.display()))?;
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            let envelope: JournalEnvelope = serde_json::from_str(trimmed)
                .with_context(|| format!("parsing line {} from {}", i + 1, path.display()))?;
            envelopes.push(envelope);
        }
        Ok(envelopes)
    }

    fn tmp_path(&self) -> PathBuf {
        self.root.join("current.tmp")
    }

    fn remove_stale_tmp(&self) {
        let tmp = self.tmp_path();
        if tmp.exists() {
            let _ = fs::remove_file(&tmp);
        }
    }

    /// Drop events older than `EVENT_RETENTION_DAYS` (relative to `now_rfc3339`)
    /// and cap the retained set at `EVENT_RETENTION_MAX` newest entries.
    /// Writes via copy-forward (temp file + fsync + atomic rename). An interrupted
    /// compaction leaves a partial `current.tmp` that is ignored on the next load
    /// and removed on the next compaction; `current.jsonl` is never partially overwritten.
    pub fn compact_with_now(&self, now_rfc3339: &str) -> Result<EventCompactionReport> {
        let _guard = self.lock.lock().unwrap();
        let cutoff = parse_rfc3339(now_rfc3339)
            .with_context(|| format!("parsing now_rfc3339 '{now_rfc3339}' for compaction"))?
            - chrono::Duration::days(EVENT_RETENTION_DAYS);

        self.remove_stale_tmp();

        let envelopes = self.load_all_from_disk()?;
        let before = envelopes.len();
        let mut report = EventCompactionReport {
            before,
            ..Default::default()
        };

        let mut kept: Vec<JournalEnvelope> = Vec::with_capacity(envelopes.len());
        for env in envelopes {
            let occurred = match parse_rfc3339(&env.event.occurred_at) {
                Ok(ts) => ts,
                Err(_) => {
                    // Unparseable timestamps are retained — never drop on bad metadata.
                    kept.push(env);
                    continue;
                }
            };
            if occurred < cutoff {
                report.dropped_by_age += 1;
            } else {
                kept.push(env);
            }
        }

        if kept.len() > EVENT_RETENTION_MAX {
            let drop_count = kept.len() - EVENT_RETENTION_MAX;
            report.dropped_by_count = drop_count;
            kept.drain(0..drop_count);
        }
        report.after = kept.len();

        if report.dropped_by_age == 0 && report.dropped_by_count == 0 {
            return Ok(report);
        }

        self.rewrite_locked(&kept)?;

        let mut idx = self.index.lock().unwrap();
        idx.clear();
        for env in &kept {
            idx.insert(env.event.id.clone(), env.event.clone());
        }
        Ok(report)
    }

    fn rewrite_locked(&self, envelopes: &[JournalEnvelope]) -> Result<()> {
        fs::create_dir_all(&self.root)
            .with_context(|| format!("creating event journal dir {}", self.root.display()))?;
        let tmp = self.tmp_path();
        let final_path = self.current_path();
        {
            let mut file = OpenOptions::new()
                .create(true)
                .write(true)
                .truncate(true)
                .open(&tmp)
                .with_context(|| format!("opening compaction tmp {}", tmp.display()))?;
            for env in envelopes {
                serde_json::to_writer(&mut file, env)?;
                writeln!(file)?;
            }
            file.sync_all()?;
        }
        fs::rename(&tmp, &final_path)
            .with_context(|| format!("renaming compaction tmp into {}", final_path.display()))?;
        Ok(())
    }

    pub fn causation_chain(&self, event_id: &str) -> Result<Vec<SystemEvent>> {
        let index = self.index.lock().unwrap();
        let mut chain = Vec::new();
        let mut current_id = event_id;
        for _ in 0..4 {
            let Some(event) = index.get(current_id) else {
                break;
            };
            chain.push(event.clone());
            match &event.causation_id {
                Some(cid) => current_id = cid.as_str(),
                None => break,
            }
        }
        Ok(chain)
    }
}

fn parse_rfc3339(s: &str) -> Result<chrono::DateTime<chrono::Utc>> {
    let dt = chrono::DateTime::parse_from_rfc3339(s)?;
    Ok(dt.with_timezone(&chrono::Utc))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::system_events::types::*;
    use serde_json::json;
    use std::sync::Arc;
    use std::thread;
    use tempfile::tempdir;

    fn test_event(producer: &str) -> SystemEvent {
        make_event(
            SystemEventKind::TaskStarted,
            producer,
            None,
            None,
            None,
            serde_json::Map::new(),
            None,
            json!({}),
        )
    }

    fn event_at(producer: &str, occurred_at: &str) -> SystemEvent {
        let mut e = test_event(producer);
        e.occurred_at = occurred_at.to_string();
        e
    }

    fn test_event_with_causation(producer: &str, causation_id: Option<String>) -> SystemEvent {
        make_event(
            SystemEventKind::TaskCompleted,
            producer,
            None,
            None,
            None,
            serde_json::Map::new(),
            causation_id,
            json!({}),
        )
    }

    #[test]
    fn event_store_append_and_reload_preserves_order() {
        let dir = tempdir().unwrap();
        let store = EventStore::new_at(dir.path().to_path_buf());
        for p in &["alpha", "beta", "gamma"] {
            store.append(&JournalEnvelope::wrap(test_event(p))).unwrap();
        }
        let loaded = store.load_all().unwrap();
        assert_eq!(loaded.len(), 3);
        assert_eq!(loaded[0].event.producer, "alpha");
        assert_eq!(loaded[1].event.producer, "beta");
        assert_eq!(loaded[2].event.producer, "gamma");
    }

    #[test]
    fn event_store_loads_empty_when_no_file() {
        let dir = tempdir().unwrap();
        let store = EventStore::new_at(dir.path().to_path_buf());
        assert!(store.load_all().unwrap().is_empty());
    }

    #[test]
    fn concurrent_event_store_appends_preserve_all_events() {
        let dir = tempdir().unwrap();
        let store = Arc::new(EventStore::new_at(dir.path().to_path_buf()));
        let n = 100;
        let mut handles = Vec::new();
        for i in 0..n {
            let s = Arc::clone(&store);
            handles.push(thread::spawn(move || {
                s.append(&JournalEnvelope::wrap(test_event(&format!(
                    "concurrent-{i}"
                ))))
                .unwrap();
            }));
        }
        for h in handles {
            h.join().unwrap();
        }
        let loaded = store.load_all().unwrap();
        assert_eq!(loaded.len(), n, "expected {n} events, got {}", loaded.len());
        let mut ids = std::collections::HashSet::new();
        for env in &loaded {
            assert!(ids.insert(env.event.id.clone()), "duplicate id");
        }
        assert_eq!(ids.len(), n);
    }

    #[test]
    fn index_rebuilt_from_disk_on_construction() {
        let dir = tempdir().unwrap();
        let root = dir.path().to_path_buf();

        let first = test_event("on-disk");
        let first_id = first.id.clone();
        let second = test_event_with_causation("derived", Some(first_id.clone()));
        let second_id = second.id.clone();

        {
            let store = EventStore::new_at(root.clone());
            store.append(&JournalEnvelope::wrap(first)).unwrap();
            store.append(&JournalEnvelope::wrap(second)).unwrap();
        }

        let reopened = EventStore::new_at(root);
        let chain = reopened.causation_chain(&second_id).unwrap();
        assert_eq!(chain.len(), 2);
        assert_eq!(chain[0].id, second_id);
        assert_eq!(chain[1].id, first_id);
        assert!(chain[1].causation_id.is_none());
    }

    #[test]
    fn append_updates_in_memory_index() {
        let dir = tempdir().unwrap();
        let store = EventStore::new_at(dir.path().to_path_buf());

        let event = test_event("indexed");
        let event_id = event.id.clone();
        store.append(&JournalEnvelope::wrap(event)).unwrap();

        let chain = store.causation_chain(&event_id).unwrap();
        assert_eq!(chain.len(), 1);
        assert_eq!(chain[0].producer, "indexed");
    }

    #[test]
    fn compact_drops_events_older_than_seven_days() {
        let dir = tempdir().unwrap();
        let store = EventStore::new_at(dir.path().to_path_buf());

        store
            .append(&JournalEnvelope::wrap(event_at(
                "ancient",
                "2026-01-01T00:00:00Z",
            )))
            .unwrap();
        store
            .append(&JournalEnvelope::wrap(event_at(
                "recent",
                "2026-05-12T00:00:00Z",
            )))
            .unwrap();

        let report = store.compact_with_now("2026-05-13T00:00:00Z").unwrap();
        assert_eq!(report.before, 2);
        assert_eq!(report.dropped_by_age, 1);
        assert_eq!(report.after, 1);

        let loaded = store.load_all().unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].event.producer, "recent");
    }

    #[test]
    fn compact_caps_at_ten_thousand_keeping_newest() {
        let dir = tempdir().unwrap();
        let store = EventStore::new_at(dir.path().to_path_buf());
        // All within window — only the count cap applies.
        for i in 0..(EVENT_RETENTION_MAX + 5) {
            let occurred = format!("2026-05-12T00:00:{:02}Z", i % 60);
            let mut ev = event_at(&format!("p-{i}"), &occurred);
            ev.id = format!("evt-fixed-{i:05}");
            store.append(&JournalEnvelope::wrap(ev)).unwrap();
        }

        let report = store.compact_with_now("2026-05-12T01:00:00Z").unwrap();
        assert_eq!(report.before, EVENT_RETENTION_MAX + 5);
        assert_eq!(report.dropped_by_count, 5);
        assert_eq!(report.after, EVENT_RETENTION_MAX);

        let loaded = store.load_all().unwrap();
        assert_eq!(loaded.len(), EVENT_RETENTION_MAX);
        // The first 5 (oldest in append order) should be dropped.
        assert_eq!(loaded.first().unwrap().event.producer, "p-5");
        assert_eq!(
            loaded.last().unwrap().event.producer,
            format!("p-{}", EVENT_RETENTION_MAX + 4)
        );
    }

    #[test]
    fn compact_uses_copy_forward_and_keeps_temp_orphans_invisible() {
        let dir = tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let store = EventStore::new_at(root.clone());

        // Plant a partial temp file that pretends a previous compaction crashed.
        fs::create_dir_all(&root).unwrap();
        let tmp = root.join("current.tmp");
        fs::write(&tmp, b"{ this is not valid json and will never be loaded\n").unwrap();
        assert!(tmp.exists());

        store
            .append(&JournalEnvelope::wrap(event_at(
                "kept",
                "2026-05-12T00:00:00Z",
            )))
            .unwrap();

        // Re-open: the partial tmp must NOT be merged into the in-memory index.
        let reopened = EventStore::new_at(root.clone());
        let loaded = reopened.load_all().unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].event.producer, "kept");

        // First successful compaction sweeps the orphan tmp.
        reopened.compact_with_now("2026-05-12T01:00:00Z").unwrap();
        // No retention triggers fired, so the tmp may or may not have been removed
        // by the no-op fast path. Force a second run that does mutate state.
        reopened
            .append(&JournalEnvelope::wrap(event_at(
                "ancient",
                "2024-01-01T00:00:00Z",
            )))
            .unwrap();
        // Re-plant tmp to simulate crash-during-compaction.
        fs::write(&tmp, b"partial garbage\n").unwrap();
        let report = reopened.compact_with_now("2026-05-12T01:00:00Z").unwrap();
        assert!(
            report.dropped_by_age >= 1,
            "ancient event should be dropped"
        );
        assert!(
            !tmp.exists(),
            "tmp must not survive a successful compaction"
        );
        // current.jsonl must contain only the surviving record.
        let final_load = reopened.load_all().unwrap();
        assert_eq!(final_load.len(), 1);
        assert_eq!(final_load[0].event.producer, "kept");
    }

    #[test]
    fn interrupted_compaction_does_not_lose_complete_segment() {
        let dir = tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let store = EventStore::new_at(root.clone());

        store
            .append(&JournalEnvelope::wrap(event_at(
                "a",
                "2026-05-12T00:00:00Z",
            )))
            .unwrap();
        store
            .append(&JournalEnvelope::wrap(event_at(
                "b",
                "2026-05-12T00:00:01Z",
            )))
            .unwrap();
        store
            .append(&JournalEnvelope::wrap(event_at(
                "c",
                "2026-05-12T00:00:02Z",
            )))
            .unwrap();

        // Simulate crash mid-compaction: a partial tmp exists alongside current.jsonl.
        // The complete segment (current.jsonl) must win on reload.
        fs::write(
            root.join("current.tmp"),
            b"{\"schema\":\"system-event/v1\",\"event\":{\"id\":\"evt-corrupt\",\"kind\":\"task.started\",\"occurred_at\":\"2026-05-12T00:00:00Z\",\"producer\":\"corrupt\"}}\n",
        )
        .unwrap();

        let reopened = EventStore::new_at(root);
        let loaded = reopened.load_all().unwrap();
        assert_eq!(loaded.len(), 3);
        // All three originals must be present and ordered.
        let producers: Vec<&str> = loaded.iter().map(|e| e.event.producer.as_str()).collect();
        assert_eq!(producers, vec!["a", "b", "c"]);
        // The corrupt tmp record must never appear.
        assert!(!loaded.iter().any(|e| e.event.id == "evt-corrupt"));
    }

    #[test]
    fn reopen_sweeps_orphan_tmp_and_preserves_current_jsonl() {
        let dir = tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let store = EventStore::new_at(root.clone());
        store
            .append(&JournalEnvelope::wrap(event_at(
                "kept",
                "2026-05-12T00:00:00Z",
            )))
            .unwrap();
        drop(store);

        // Plant an orphan tmp as if a compaction crashed mid-write.
        let tmp = root.join("current.tmp");
        fs::write(&tmp, b"partial garbage that must never be merged\n").unwrap();
        assert!(tmp.exists());

        let reopened = EventStore::new_at(root.clone());
        assert!(!tmp.exists(), "reopen must sweep orphan current.tmp");
        let loaded = reopened.load_all().unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].event.producer, "kept");
    }

    #[test]
    fn compact_no_op_when_nothing_to_drop() {
        let dir = tempdir().unwrap();
        let store = EventStore::new_at(dir.path().to_path_buf());
        store
            .append(&JournalEnvelope::wrap(event_at(
                "fresh",
                "2026-05-12T00:00:00Z",
            )))
            .unwrap();
        let report = store.compact_with_now("2026-05-12T01:00:00Z").unwrap();
        assert_eq!(report.dropped_by_age, 0);
        assert_eq!(report.dropped_by_count, 0);
        assert_eq!(report.after, 1);
    }
}
