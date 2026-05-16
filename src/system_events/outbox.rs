use std::fs;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::PathBuf;

use anyhow::{Context, Result};

use super::types::{OutboxRecord, OutboxStatus};

/// Drop succeeded outbox records older than this many days. All non-success
/// statuses are retained regardless of age.
pub const OUTBOX_SUCCESS_RETENTION_DAYS: i64 = 7;

pub struct OutboxStore {
    root: PathBuf,
    lock: std::sync::Mutex<()>,
}

#[derive(Debug, Default, serde::Serialize)]
pub struct OutboxCompactionReport {
    pub before: usize,
    pub after: usize,
    pub dropped_succeeded: usize,
}

impl OutboxStore {
    pub fn new(root: PathBuf) -> Result<Self> {
        fs::create_dir_all(&root)?;
        let store = Self {
            root,
            lock: std::sync::Mutex::new(()),
        };
        // Sweep orphan `current.tmp` left by a crashed compaction. The
        // complete `current.jsonl` is authoritative on reopen.
        let tmp = store.current_path().with_extension("tmp");
        if tmp.exists() {
            let _ = fs::remove_file(&tmp);
        }
        Ok(store)
    }

    fn current_path(&self) -> PathBuf {
        self.root.join("current.jsonl")
    }

    pub fn append(&self, record: &OutboxRecord) -> Result<()> {
        let _guard = self.lock.lock().unwrap();
        self.append_locked(record)
    }

    fn append_locked(&self, record: &OutboxRecord) -> Result<()> {
        let path = self.current_path();
        let mut file = OpenOptions::new().create(true).append(true).open(&path)?;
        serde_json::to_writer(&mut file, record)?;
        writeln!(file)?;
        file.sync_all()?;
        Ok(())
    }

    pub fn load_all(&self) -> Result<Vec<OutboxRecord>> {
        let _guard = self.lock.lock().unwrap();
        self.load_locked()
    }

    fn load_locked(&self) -> Result<Vec<OutboxRecord>> {
        let path = self.current_path();
        if !path.exists() {
            return Ok(Vec::new());
        }
        let content = fs::read_to_string(&path)?;
        content
            .lines()
            .filter(|l| !l.trim().is_empty())
            .map(|line| serde_json::from_str(line).map_err(Into::into))
            .collect()
    }

    fn rewrite_locked(&self, records: &[OutboxRecord]) -> Result<()> {
        let path = self.current_path();
        let tmp_path = path.with_extension("tmp");
        {
            let mut file = OpenOptions::new()
                .create(true)
                .write(true)
                .truncate(true)
                .open(&tmp_path)?;
            for record in records {
                serde_json::to_writer(&mut file, record)?;
                writeln!(file)?;
            }
            file.sync_all()?;
        }
        fs::rename(&tmp_path, &path)?;
        Ok(())
    }

    /// Drop `Succeeded` outbox records older than `OUTBOX_SUCCESS_RETENTION_DAYS`.
    /// All other statuses are retained regardless of age. Copy-forward via
    /// temp + fsync + rename — an interrupted compaction leaves the tmp
    /// orphaned and `current.jsonl` untouched.
    pub fn compact_with_now(&self, now_rfc3339: &str) -> Result<OutboxCompactionReport> {
        let _guard = self.lock.lock().unwrap();
        let cutoff = parse_rfc3339(now_rfc3339).with_context(|| {
            format!("parsing now_rfc3339 '{now_rfc3339}' for outbox compaction")
        })? - chrono::Duration::days(OUTBOX_SUCCESS_RETENTION_DAYS);

        // Clear any orphaned tmp from a prior crashed compaction before we write a new one.
        let tmp = self.current_path().with_extension("tmp");
        if tmp.exists() {
            let _ = fs::remove_file(&tmp);
        }

        let records = self.load_locked()?;
        let before = records.len();
        let mut kept = Vec::with_capacity(records.len());
        let mut dropped_succeeded = 0usize;
        for record in records {
            if record.status == OutboxStatus::Succeeded {
                match parse_rfc3339(&record.updated_at) {
                    Ok(ts) if ts < cutoff => {
                        dropped_succeeded += 1;
                        continue;
                    }
                    _ => {}
                }
            }
            kept.push(record);
        }
        let after = kept.len();
        if dropped_succeeded > 0 {
            self.rewrite_locked(&kept)?;
        }
        Ok(OutboxCompactionReport {
            before,
            after,
            dropped_succeeded,
        })
    }

    pub fn create_record(
        &self,
        event_id: &str,
        reaction_name: &str,
        idempotency_key: Option<String>,
    ) -> Result<OutboxRecord> {
        let record = OutboxRecord::new(event_id, reaction_name, idempotency_key);
        self.append(&record)?;
        Ok(record)
    }

    pub fn claim_next(&self, now: &str, process_id: &str) -> Result<Option<OutboxRecord>> {
        let _guard = self.lock.lock().unwrap();
        let mut records = self.load_locked()?;
        let mut claimed: Option<OutboxRecord> = None;
        for record in &mut records {
            if record.status != OutboxStatus::Pending && record.status != OutboxStatus::RetryAt {
                continue;
            }
            if record.status == OutboxStatus::RetryAt {
                if let Some(ref next) = record.next_attempt_at {
                    if next.as_str() > now {
                        continue;
                    }
                }
            }
            record.status = OutboxStatus::Claimed;
            record.claimed_at = Some(now.to_string());
            record.claimed_by = Some(process_id.to_string());
            record.attempt_count += 1;
            record.updated_at = crate::util::now_iso();
            claimed = Some(record.clone());
            break;
        }
        if claimed.is_some() {
            self.rewrite_locked(&records)?;
        }
        Ok(claimed)
    }

    pub fn claim_record(
        &self,
        id: &str,
        now: &str,
        process_id: &str,
    ) -> Result<Option<OutboxRecord>> {
        let _guard = self.lock.lock().unwrap();
        let mut records = self.load_locked()?;
        let mut claimed: Option<OutboxRecord> = None;
        for record in &mut records {
            if record.id != id {
                continue;
            }
            if record.status != OutboxStatus::Pending && record.status != OutboxStatus::RetryAt {
                break;
            }
            record.status = OutboxStatus::Claimed;
            record.claimed_at = Some(now.to_string());
            record.claimed_by = Some(process_id.to_string());
            record.attempt_count += 1;
            record.updated_at = crate::util::now_iso();
            claimed = Some(record.clone());
            break;
        }
        if claimed.is_some() {
            self.rewrite_locked(&records)?;
        }
        Ok(claimed)
    }

    pub fn mark_succeeded(&self, id: &str, summary: Option<serde_json::Value>) -> Result<()> {
        let _guard = self.lock.lock().unwrap();
        let mut records = self.load_locked()?;
        for record in &mut records {
            if record.id == id {
                record.status = OutboxStatus::Succeeded;
                record.claimed_at = None;
                record.claimed_by = None;
                record.response_summary = summary;
                record.updated_at = crate::util::now_iso();
                break;
            }
        }
        self.rewrite_locked(&records)
    }

    pub fn mark_retry_at(&self, id: &str, next_attempt_at: &str, error: &str) -> Result<()> {
        let _guard = self.lock.lock().unwrap();
        let mut records = self.load_locked()?;
        for record in &mut records {
            if record.id == id {
                record.status = OutboxStatus::RetryAt;
                record.claimed_at = None;
                record.claimed_by = None;
                record.next_attempt_at = Some(next_attempt_at.to_string());
                record.last_error = Some(error.to_string());
                record.updated_at = crate::util::now_iso();
                break;
            }
        }
        self.rewrite_locked(&records)
    }

    pub fn mark_dead_lettered(&self, id: &str, reason: &str, error: Option<&str>) -> Result<()> {
        let _guard = self.lock.lock().unwrap();
        let mut records = self.load_locked()?;
        for record in &mut records {
            if record.id == id {
                record.status = OutboxStatus::DeadLettered;
                record.dead_letter_reason = Some(reason.to_string());
                record.last_error = error.map(|e| e.to_string());
                record.claimed_at = None;
                record.claimed_by = None;
                record.updated_at = crate::util::now_iso();
                break;
            }
        }
        self.rewrite_locked(&records)
    }

    pub fn retry_dead_lettered(&self, id: &str) -> Result<bool> {
        let _guard = self.lock.lock().unwrap();
        let mut records = self.load_locked()?;
        let mut found = false;
        for record in &mut records {
            if record.id == id && record.status == OutboxStatus::DeadLettered {
                record.status = OutboxStatus::Pending;
                record.dead_letter_reason = None;
                record.last_error = None;
                record.claimed_at = None;
                record.claimed_by = None;
                record.next_attempt_at = None;
                record.updated_at = crate::util::now_iso();
                found = true;
                break;
            }
        }
        if found {
            self.rewrite_locked(&records)?;
        }
        Ok(found)
    }

    pub fn recover_stale_claims(&self) -> RecoveryReport {
        let _guard = self.lock.lock().unwrap();
        let records = self.load_locked();
        let Ok(mut records) = records else {
            return RecoveryReport::default();
        };
        let mut report = RecoveryReport::default();
        for record in &mut records {
            if record.status != OutboxStatus::Claimed {
                continue;
            }
            match record.idempotency_key {
                Some(_) => {
                    record.status = OutboxStatus::Pending;
                    record.claimed_at = None;
                    record.claimed_by = None;
                    record.updated_at = crate::util::now_iso();
                    report.requeued += 1;
                }
                None => {
                    let ctx = format!(
                        "crash_recovery_non_idempotent: claimed_at={:?}, claimed_by={:?}, attempt_count={}",
                        record.claimed_at, record.claimed_by, record.attempt_count
                    );
                    record.status = OutboxStatus::DeadLettered;
                    record.dead_letter_reason = Some("crash_recovery_non_idempotent".to_string());
                    record.last_error = Some(ctx);
                    record.updated_at = crate::util::now_iso();
                    report.dead_lettered += 1;
                }
            }
        }
        if report.requeued > 0 || report.dead_lettered > 0 {
            if let Err(e) = self.rewrite_locked(&records) {
                tracing::error!("recovery rewrite failed: {e:#}");
            }
        }
        report
    }

    pub fn get_record(&self, id: &str) -> Result<Option<OutboxRecord>> {
        let records = self.load_all()?;
        Ok(records.into_iter().find(|r| r.id == id))
    }

    pub fn list_by_event(&self, event_id: &str) -> Result<Vec<OutboxRecord>> {
        let records = self.load_all()?;
        Ok(records
            .into_iter()
            .filter(|r| r.event_id == event_id)
            .collect())
    }

    // kept: public OutboxStore filter helper alongside `load_all`/`list_for_event`; admin path wired in follow-up
    #[allow(dead_code)]
    pub fn list_by_status(&self, status: OutboxStatus) -> Result<Vec<OutboxRecord>> {
        let records = self.load_all()?;
        Ok(records.into_iter().filter(|r| r.status == status).collect())
    }
}

#[derive(Debug, Default, serde::Serialize)]
pub struct RecoveryReport {
    pub requeued: usize,
    pub dead_lettered: usize,
}

fn parse_rfc3339(s: &str) -> Result<chrono::DateTime<chrono::Utc>> {
    let dt = chrono::DateTime::parse_from_rfc3339(s)?;
    Ok(dt.with_timezone(&chrono::Utc))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn outbox_append_and_load_preserve_order() {
        let dir = tempfile::tempdir().unwrap();
        let store = OutboxStore::new(dir.path().to_path_buf()).unwrap();
        for i in 0..5 {
            let rec = OutboxRecord::new(
                &format!("evt-{i}"),
                "test-reaction",
                Some(format!("key-{i}")),
            );
            store.append(&rec).unwrap();
        }
        let all = store.load_all().unwrap();
        assert_eq!(all.len(), 5);
        for (i, rec) in all.iter().enumerate() {
            assert_eq!(rec.event_id, format!("evt-{i}"));
            assert_eq!(rec.status, OutboxStatus::Pending);
        }
    }

    #[test]
    fn outbox_create_record_returns_record() {
        let dir = tempfile::tempdir().unwrap();
        let store = OutboxStore::new(dir.path().to_path_buf()).unwrap();
        let rec = store
            .create_record("evt-1", "my-reaction", Some("key".to_string()))
            .unwrap();
        assert_eq!(rec.event_id, "evt-1");
        let loaded = store.load_all().unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].id, rec.id);
    }

    #[test]
    fn outbox_load_empty() {
        let dir = tempfile::tempdir().unwrap();
        let store = OutboxStore::new(dir.path().to_path_buf()).unwrap();
        assert!(store.load_all().unwrap().is_empty());
    }

    #[test]
    fn claim_next_transitions_to_claimed() {
        let dir = tempfile::tempdir().unwrap();
        let store = OutboxStore::new(dir.path().to_path_buf()).unwrap();
        store
            .create_record("evt-1", "react", Some("key".to_string()))
            .unwrap();

        let claimed = store.claim_next("2026-01-01T00:00:00Z", "pid-1").unwrap();
        assert!(claimed.is_some());
        let c = claimed.unwrap();
        assert_eq!(c.status, OutboxStatus::Claimed);
        assert_eq!(c.attempt_count, 1);
        assert_eq!(c.claimed_by.as_deref(), Some("pid-1"));

        let second = store.claim_next("2026-01-01T00:00:01Z", "pid-1").unwrap();
        assert!(
            second.is_none(),
            "claimed record should not be re-claimable"
        );
    }

    #[test]
    fn claim_record_claims_exact_id_only() {
        let dir = tempfile::tempdir().unwrap();
        let store = OutboxStore::new(dir.path().to_path_buf()).unwrap();
        let first = store
            .create_record("evt-1", "react", Some("key-1".to_string()))
            .unwrap();
        let second = store
            .create_record("evt-2", "react", Some("key-2".to_string()))
            .unwrap();

        let claimed = store
            .claim_record(&second.id, "2026-01-01T00:00:00Z", "pid-2")
            .unwrap()
            .unwrap();
        assert_eq!(claimed.id, second.id);
        assert_eq!(claimed.status, OutboxStatus::Claimed);
        assert_eq!(claimed.attempt_count, 1);

        let first_after = store.get_record(&first.id).unwrap().unwrap();
        assert_eq!(first_after.status, OutboxStatus::Pending);
        assert_eq!(first_after.attempt_count, 0);
    }

    #[test]
    fn mark_succeeded_clears_claim() {
        let dir = tempfile::tempdir().unwrap();
        let store = OutboxStore::new(dir.path().to_path_buf()).unwrap();
        let rec = store
            .create_record("evt-1", "react", Some("key".to_string()))
            .unwrap();
        store.claim_next("2026-01-01T00:00:00Z", "pid-1").unwrap();

        store
            .mark_succeeded(&rec.id, Some(serde_json::json!({"status": "ok"})))
            .unwrap();

        let loaded = store.get_record(&rec.id).unwrap().unwrap();
        assert_eq!(loaded.status, OutboxStatus::Succeeded);
        assert!(loaded.claimed_at.is_none());
        assert!(loaded.claimed_by.is_none());
        assert!(loaded.response_summary.is_some());
    }

    #[test]
    fn retry_dead_lettered_moves_to_pending() {
        let dir = tempfile::tempdir().unwrap();
        let store = OutboxStore::new(dir.path().to_path_buf()).unwrap();
        let rec = store
            .create_record("evt-1", "react", Some("key".to_string()))
            .unwrap();
        store
            .mark_dead_lettered(&rec.id, "test reason", Some("detail"))
            .unwrap();

        let found = store.retry_dead_lettered(&rec.id).unwrap();
        assert!(found);

        let loaded = store.get_record(&rec.id).unwrap().unwrap();
        assert_eq!(loaded.status, OutboxStatus::Pending);
        assert!(loaded.dead_letter_reason.is_none());
        assert!(loaded.last_error.is_none());
    }

    #[test]
    fn recover_requeues_idempotent_claims() {
        let dir = tempfile::tempdir().unwrap();
        let store = OutboxStore::new(dir.path().to_path_buf()).unwrap();
        let mut rec = OutboxRecord::new("evt-1", "react", Some("key-1".to_string()));
        rec.status = OutboxStatus::Claimed;
        rec.claimed_at = Some("2026-01-01T00:00:00Z".to_string());
        rec.claimed_by = Some("pid-old".to_string());
        store.append(&rec).unwrap();

        let report = store.recover_stale_claims();
        assert_eq!(report.requeued, 1);
        assert_eq!(report.dead_lettered, 0);

        let loaded = store.load_all().unwrap();
        assert_eq!(loaded[0].status, OutboxStatus::Pending);
        assert!(loaded[0].claimed_at.is_none());
    }

    fn record_with(id_suffix: &str, status: OutboxStatus, updated_at: &str) -> OutboxRecord {
        let mut rec = OutboxRecord::new(
            &format!("evt-{id_suffix}"),
            "react",
            Some(format!("key-{id_suffix}")),
        );
        rec.id = format!("outbox-fixed-{id_suffix}");
        rec.status = status;
        rec.updated_at = updated_at.to_string();
        rec
    }

    #[test]
    fn compact_drops_succeeded_older_than_seven_days() {
        let dir = tempfile::tempdir().unwrap();
        let store = OutboxStore::new(dir.path().to_path_buf()).unwrap();

        store
            .append(&record_with(
                "old",
                OutboxStatus::Succeeded,
                "2026-01-01T00:00:00Z",
            ))
            .unwrap();
        store
            .append(&record_with(
                "fresh",
                OutboxStatus::Succeeded,
                "2026-05-12T00:00:00Z",
            ))
            .unwrap();

        let report = store.compact_with_now("2026-05-13T00:00:00Z").unwrap();
        assert_eq!(report.before, 2);
        assert_eq!(report.dropped_succeeded, 1);
        assert_eq!(report.after, 1);

        let loaded = store.load_all().unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].id, "outbox-fixed-fresh");
    }

    #[test]
    fn compact_retains_non_success_regardless_of_age() {
        let dir = tempfile::tempdir().unwrap();
        let store = OutboxStore::new(dir.path().to_path_buf()).unwrap();

        for (suf, status) in [
            ("pending", OutboxStatus::Pending),
            ("claimed", OutboxStatus::Claimed),
            ("retry", OutboxStatus::RetryAt),
            ("dead", OutboxStatus::DeadLettered),
        ] {
            store
                .append(&record_with(suf, status, "2026-01-01T00:00:00Z"))
                .unwrap();
        }

        let report = store.compact_with_now("2026-05-13T00:00:00Z").unwrap();
        assert_eq!(report.dropped_succeeded, 0);
        assert_eq!(report.after, 4);

        let statuses: Vec<OutboxStatus> = store
            .load_all()
            .unwrap()
            .into_iter()
            .map(|r| r.status)
            .collect();
        assert!(statuses.contains(&OutboxStatus::Pending));
        assert!(statuses.contains(&OutboxStatus::Claimed));
        assert!(statuses.contains(&OutboxStatus::RetryAt));
        assert!(statuses.contains(&OutboxStatus::DeadLettered));
    }

    #[test]
    fn interrupted_outbox_compaction_preserves_complete_segment() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let store = OutboxStore::new(root.clone()).unwrap();

        store
            .append(&record_with(
                "a",
                OutboxStatus::DeadLettered,
                "2026-05-12T00:00:00Z",
            ))
            .unwrap();
        store
            .append(&record_with(
                "b",
                OutboxStatus::Pending,
                "2026-05-12T00:00:01Z",
            ))
            .unwrap();

        // Simulate a crash mid-compaction: a partial tmp file alongside the complete current.jsonl.
        fs::write(
            root.join("current.tmp"),
            b"{ partial bytes that are not valid json\n",
        )
        .unwrap();

        // Reopen: the partial tmp must NOT contaminate load_all.
        let reopened = OutboxStore::new(root.clone()).unwrap();
        let loaded = reopened.load_all().unwrap();
        assert_eq!(loaded.len(), 2);
        let ids: Vec<&str> = loaded.iter().map(|r| r.id.as_str()).collect();
        assert!(ids.contains(&"outbox-fixed-a"));
        assert!(ids.contains(&"outbox-fixed-b"));

        // Add a successful record old enough to be compacted, replant tmp, run compaction.
        reopened
            .append(&record_with(
                "old-success",
                OutboxStatus::Succeeded,
                "2026-01-01T00:00:00Z",
            ))
            .unwrap();
        fs::write(root.join("current.tmp"), b"orphan tmp\n").unwrap();

        let report = reopened.compact_with_now("2026-05-13T00:00:00Z").unwrap();
        assert_eq!(report.dropped_succeeded, 1);
        assert!(
            !root.join("current.tmp").exists(),
            "tmp must not survive a successful compaction"
        );

        // dead-lettered + pending remain; old success is gone.
        let final_load = reopened.load_all().unwrap();
        assert_eq!(final_load.len(), 2);
    }

    #[test]
    fn reopen_sweeps_orphan_tmp_and_preserves_current_jsonl() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let store = OutboxStore::new(root.clone()).unwrap();
        store
            .append(&record_with(
                "kept",
                OutboxStatus::Pending,
                "2026-05-12T00:00:00Z",
            ))
            .unwrap();
        drop(store);

        // Plant an orphan tmp as if a compaction crashed mid-write.
        let tmp = root.join("current.tmp");
        fs::write(&tmp, b"partial garbage that must never be merged\n").unwrap();
        assert!(tmp.exists());

        let reopened = OutboxStore::new(root.clone()).unwrap();
        assert!(!tmp.exists(), "reopen must sweep orphan current.tmp");
        let loaded = reopened.load_all().unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].id, "outbox-fixed-kept");
    }

    #[test]
    fn compact_no_op_when_nothing_old_enough() {
        let dir = tempfile::tempdir().unwrap();
        let store = OutboxStore::new(dir.path().to_path_buf()).unwrap();
        store
            .append(&record_with(
                "fresh",
                OutboxStatus::Succeeded,
                "2026-05-12T00:00:00Z",
            ))
            .unwrap();
        let report = store.compact_with_now("2026-05-13T00:00:00Z").unwrap();
        assert_eq!(report.dropped_succeeded, 0);
        assert_eq!(report.after, 1);
    }

    #[test]
    fn recover_dead_letters_non_idempotent_claims() {
        let dir = tempfile::tempdir().unwrap();
        let store = OutboxStore::new(dir.path().to_path_buf()).unwrap();
        let mut rec = OutboxRecord::new("evt-2", "react", None);
        rec.status = OutboxStatus::Claimed;
        rec.claimed_at = Some("2026-01-01T00:00:00Z".to_string());
        rec.claimed_by = Some("pid-old".to_string());
        rec.attempt_count = 2;
        store.append(&rec).unwrap();

        let report = store.recover_stale_claims();
        assert_eq!(report.requeued, 0);
        assert_eq!(report.dead_lettered, 1);

        let loaded = store.load_all().unwrap();
        assert_eq!(loaded[0].status, OutboxStatus::DeadLettered);
        let reason = loaded[0].dead_letter_reason.as_deref().unwrap();
        assert_eq!(reason, "crash_recovery_non_idempotent");
        let err = loaded[0].last_error.as_deref().unwrap();
        assert!(
            err.contains("attempt_count=2"),
            "should include attempt count: {err}"
        );
    }
}
