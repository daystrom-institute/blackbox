//! Durable envelope spool for the Slack sidecar.
//!
//! Socket Mode has no server-side replay: once the sidecar acks an
//! envelope_id, Slack forgets it. The v1 sidecar acked after a bounded
//! POST retry budget and dropped the envelope when that budget ran out,
//! so a daemon restart longer than ~4.5s silently lost every event that
//! landed inside the window.
//!
//! This module moves the durability boundary off Slack and onto local
//! disk. The ordering contract is:
//!
//!   1. normalize + enrich the envelope
//!   2. write it to the spool and fsync (this module)
//!   3. ONLY THEN ack Slack
//!   4. attempt delivery to the daemon
//!   5. delete the spool entry on 2xx; leave it in place on any failure
//!
//! Step 2 failing means step 3 does not happen, so Slack redelivers and
//! nothing is lost. Steps 4/5 failing means the entry survives for the
//! boot replay and the periodic retry sweep to pick up.
//!
//! Entries are one JSON file per envelope in a flat directory. That is
//! deliberately boring: it is crash-safe with a tmp-write plus rename, it
//! needs no index to recover, and an operator can read a stuck envelope
//! with `cat`.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::io::AsyncWriteExt;

/// On-disk schema version for a spool entry. Bump when the shape changes
/// incompatibly; `list` treats an unknown version as unreadable and
/// quarantines rather than guessing.
pub const SPOOL_ENTRY_VERSION: u32 = 1;

/// Default gap between retry sweeps.
pub const DEFAULT_SWEEP_INTERVAL_SECS: u64 = 300;

/// Default age at which a spooled envelope is discarded.
///
/// 24h is chosen to cover an overnight daemon outage while staying inside
/// the window where a Slack event is still actionable: threads move on,
/// suspended arcs time out per their Wait policies, and replaying a
/// two-day-old `/bbox triage` into a live workspace is worse than not
/// replaying it. Discards are loud (error log plus a health counter),
/// never silent.
pub const DEFAULT_MAX_AGE_SECS: u64 = 86_400;

/// Default cap on retained spool entries.
///
/// At a typical normalized envelope of a few KB this bounds the spool at
/// roughly 20MB. Overflow evicts the OLDEST entries rather than refusing
/// the newest: dropping live traffic to preserve a day-old backlog is the
/// wrong trade, and refusing to spool would mean refusing to ack, which
/// turns into a Slack redelivery storm.
pub const DEFAULT_MAX_ENTRIES: usize = 5_000;

/// How recently an entry may have been touched and still be skipped by a
/// sweep. This is what keeps the sweep from racing the inline delivery
/// attempt that the socket loop is still running for a just-spooled
/// envelope (worst case ~4.5s), with a wide margin.
pub const SWEEP_QUIET_PERIOD: Duration = Duration::from_secs(60);

// ── Entry ───────────────────────────────────────────────────────────

/// One spooled envelope, already normalized and ACL-enriched.
///
/// The stored body is the normalized event rather than the raw Socket
/// Mode frame, because ACL enrichment reads the identity map loaded at
/// startup: re-normalizing at replay time could attribute a message to a
/// different bbox user than the one in effect when it arrived.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SpoolEntry {
    pub version: u32,
    pub envelope_id: String,
    /// When the envelope was first durably written (the ack point).
    pub spooled_at: DateTime<Utc>,
    /// Completed delivery rounds, where a round is one full POST retry
    /// budget. Starts at 0 for an entry that has not been attempted.
    #[serde(default)]
    pub attempts: u32,
    #[serde(default)]
    pub last_attempt_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub last_error: Option<String>,
    /// The normalized webhook body to POST.
    pub event: Value,
}

impl SpoolEntry {
    /// The timestamp a sweep measures quiet time from: the last delivery
    /// attempt, or the spool write when nothing has been attempted yet.
    pub fn last_touched_at(&self) -> DateTime<Utc> {
        self.last_attempt_at.unwrap_or(self.spooled_at)
    }
}

// ── Naming ──────────────────────────────────────────────────────────

/// Map an envelope_id to its spool file name.
///
/// Slack envelope_ids are UUIDs, but the id arrives over the network and
/// is never trusted as a path component: everything outside
/// `[A-Za-z0-9_-]` is replaced, and the name is length-bounded. Because
/// replacement and truncation are both lossy, a short digest of the
/// ORIGINAL id is appended so two distinct ids can never collide onto one
/// file.
pub fn spool_file_name(envelope_id: &str) -> String {
    use sha2::{Digest, Sha256};

    let digest = Sha256::digest(envelope_id.as_bytes());
    let suffix = hex::encode(&digest[..4]);

    let mut safe = String::with_capacity(envelope_id.len());
    for ch in envelope_id.chars() {
        if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' {
            safe.push(ch);
        } else {
            safe.push('_');
        }
    }
    // Every retained char is ASCII, so this cannot split a code point.
    if safe.len() > 96 {
        safe.truncate(96);
    }
    if safe.is_empty() {
        safe.push_str("envelope");
    }
    format!("{safe}-{suffix}.json")
}

// ── Sweep policy ────────────────────────────────────────────────────

/// Growth and retry bounds for the spool.
#[derive(Debug, Clone)]
pub struct SpoolPolicy {
    pub max_age: Duration,
    pub max_entries: usize,
}

impl Default for SpoolPolicy {
    fn default() -> Self {
        Self {
            max_age: Duration::from_secs(DEFAULT_MAX_AGE_SECS),
            max_entries: DEFAULT_MAX_ENTRIES,
        }
    }
}

/// What a sweep should do with one entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SweepDecision {
    /// Re-attempt delivery now.
    Retry,
    /// Leave alone: touched too recently, likely still being delivered by
    /// the inline path.
    Wait,
    /// Past the age bound: drop it, loudly.
    DiscardAged,
}

/// Decide an entry's fate from its ages. Pure so the ordering of the
/// three rules is testable without a filesystem or a clock.
///
/// Age wins over quiet time: an entry that has aged out is discarded even
/// if something touched it a moment ago, otherwise a permanently failing
/// envelope retried every sweep would never reach its age bound.
pub fn classify_spool_entry(
    age: Duration,
    since_last_touch: Duration,
    quiet_period: Duration,
    policy: &SpoolPolicy,
) -> SweepDecision {
    if age >= policy.max_age {
        return SweepDecision::DiscardAged;
    }
    if since_last_touch < quiet_period {
        return SweepDecision::Wait;
    }
    SweepDecision::Retry
}

/// How many of the oldest entries must go to admit `incoming` new ones
/// under `max_entries`. A cap of 0 means unbounded (no eviction).
pub fn overflow_evictions(current_depth: usize, incoming: usize, max_entries: usize) -> usize {
    if max_entries == 0 {
        return 0;
    }
    current_depth
        .saturating_add(incoming)
        .saturating_sub(max_entries)
        .min(current_depth)
}

/// The partition a sweep acts on.
#[derive(Debug, Default)]
pub struct SweepPlan {
    pub retry: Vec<SpoolEntry>,
    pub discard: Vec<SpoolEntry>,
    /// Entries left alone this pass (inside the quiet period).
    pub waiting: usize,
}

// ── Spool ───────────────────────────────────────────────────────────

/// A directory of durably written envelopes awaiting daemon delivery.
pub struct EnvelopeSpool {
    dir: PathBuf,
    policy: SpoolPolicy,
    depth: AtomicU64,
    evicted_overflow: AtomicU64,
}

impl EnvelopeSpool {
    /// Create (or adopt) the spool directory and count what is already
    /// there. Failing here is fatal at startup: a sidecar that cannot
    /// write its spool cannot honestly ack Slack.
    pub async fn open(dir: impl AsRef<Path>, policy: SpoolPolicy) -> Result<Self> {
        let dir = dir.as_ref().to_path_buf();
        tokio::fs::create_dir_all(&dir)
            .await
            .with_context(|| format!("creating the slack spool directory {}", dir.display()))?;
        // Canonicalize so logged paths and the entry paths agree on macOS,
        // where the tempdir root resolves through /private.
        let dir = tokio::fs::canonicalize(&dir).await.unwrap_or(dir);

        let spool = Self {
            dir,
            policy,
            depth: AtomicU64::new(0),
            evicted_overflow: AtomicU64::new(0),
        };
        // `list` is the authoritative recount and sets `depth`.
        let existing = spool.list().await?;
        if !existing.is_empty() {
            tracing::info!(
                spool_dir = %spool.dir.display(),
                entries = existing.len(),
                "adopted spooled Slack envelopes awaiting delivery"
            );
        }
        Ok(spool)
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn policy(&self) -> &SpoolPolicy {
        &self.policy
    }

    /// Entries currently on disk, as tracked by writes and removals. The
    /// counter is refreshed to the true value on every `list`.
    pub fn depth(&self) -> u64 {
        self.depth.load(Ordering::Relaxed)
    }

    /// Monotonic count of entries dropped to stay under the entry cap.
    /// Mirrored into the health endpoint by the caller.
    pub fn evicted_overflow(&self) -> u64 {
        self.evicted_overflow.load(Ordering::Relaxed)
    }

    pub fn entry_path(&self, envelope_id: &str) -> PathBuf {
        self.dir.join(spool_file_name(envelope_id))
    }

    /// Durably write an envelope. Returns only after the bytes and the
    /// containing directory entry are on stable storage, so the caller may
    /// ack Slack the instant this returns Ok.
    pub async fn persist(&self, envelope_id: &str, event: &Value) -> Result<()> {
        self.enforce_capacity().await;

        let path = self.entry_path(envelope_id);
        let already_present = tokio::fs::try_exists(&path).await.unwrap_or(false);

        let entry = SpoolEntry {
            version: SPOOL_ENTRY_VERSION,
            envelope_id: envelope_id.to_string(),
            spooled_at: Utc::now(),
            attempts: 0,
            last_attempt_at: None,
            last_error: None,
            event: event.clone(),
        };
        self.write_entry(&entry).await?;

        if !already_present {
            self.depth.fetch_add(1, Ordering::Relaxed);
        }
        Ok(())
    }

    /// Drop an entry after confirmed delivery. Missing is success: the
    /// sweep and the inline path can both reach this for one envelope.
    pub async fn remove(&self, envelope_id: &str) -> Result<()> {
        let path = self.entry_path(envelope_id);
        match tokio::fs::remove_file(&path).await {
            Ok(()) => {
                self.decrement_depth();
                self.sync_dir().await;
                Ok(())
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e).with_context(|| format!("removing spool entry {}", path.display())),
        }
    }

    /// Stamp a failed delivery round onto the entry so operators can see
    /// how long an envelope has been stuck and why.
    pub async fn record_failure(&self, envelope_id: &str, error: &str) -> Result<()> {
        let path = self.entry_path(envelope_id);
        let bytes = tokio::fs::read(&path)
            .await
            .with_context(|| format!("reading spool entry {}", path.display()))?;
        let mut entry: SpoolEntry = serde_json::from_slice(&bytes)
            .with_context(|| format!("parsing spool entry {}", path.display()))?;

        entry.attempts = entry.attempts.saturating_add(1);
        entry.last_attempt_at = Some(Utc::now());
        let mut reason = error.to_string();
        if reason.len() > 500 {
            reason.truncate(500);
        }
        entry.last_error = Some(reason);
        self.write_entry(&entry).await
    }

    /// Every readable entry, oldest first. Unreadable files are
    /// quarantined to `.corrupt` rather than deleted or re-read forever.
    /// Refreshes the depth counter to the true on-disk count.
    pub async fn list(&self) -> Result<Vec<SpoolEntry>> {
        let mut reader = tokio::fs::read_dir(&self.dir)
            .await
            .with_context(|| format!("reading the slack spool directory {}", self.dir.display()))?;

        let mut entries = Vec::new();
        while let Some(dir_entry) = reader.next_entry().await.with_context(|| {
            format!("iterating the slack spool directory {}", self.dir.display())
        })? {
            let path = dir_entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            match tokio::fs::read(&path).await {
                Ok(bytes) => match serde_json::from_slice::<SpoolEntry>(&bytes) {
                    Ok(entry) if entry.version == SPOOL_ENTRY_VERSION => entries.push(entry),
                    Ok(entry) => {
                        tracing::error!(
                            path = %path.display(),
                            version = entry.version,
                            expected = SPOOL_ENTRY_VERSION,
                            "spool entry has an unknown schema version; quarantining"
                        );
                        self.quarantine(&path).await;
                    }
                    Err(e) => {
                        tracing::error!(
                            path = %path.display(),
                            error = %e,
                            "unparseable spool entry; quarantining instead of dropping"
                        );
                        self.quarantine(&path).await;
                    }
                },
                Err(e) => {
                    tracing::warn!(
                        path = %path.display(),
                        error = %e,
                        "could not read spool entry this pass"
                    );
                }
            }
        }

        entries.sort_by_key(|e| e.spooled_at);
        self.depth.store(entries.len() as u64, Ordering::Relaxed);
        Ok(entries)
    }

    /// Partition the spool into what a sweep should retry, discard, and
    /// leave alone. `quiet_period` is zero for the boot replay, where
    /// nothing is being delivered inline yet.
    pub async fn plan_sweep(
        &self,
        now: DateTime<Utc>,
        quiet_period: Duration,
    ) -> Result<SweepPlan> {
        let entries = self.list().await?;
        let mut plan = SweepPlan::default();
        for entry in entries {
            let age = elapsed_since(now, entry.spooled_at);
            let quiet = elapsed_since(now, entry.last_touched_at());
            match classify_spool_entry(age, quiet, quiet_period, &self.policy) {
                SweepDecision::Retry => plan.retry.push(entry),
                SweepDecision::DiscardAged => plan.discard.push(entry),
                SweepDecision::Wait => plan.waiting += 1,
            }
        }
        Ok(plan)
    }

    /// Evict oldest-first so one incoming envelope fits under the cap.
    /// Loud on every eviction: a spool that is shedding is a daemon that
    /// has been unreachable for a long time.
    async fn enforce_capacity(&self) {
        if self.policy.max_entries == 0 {
            return;
        }
        if (self.depth() as usize) < self.policy.max_entries {
            return;
        }

        let entries = match self.list().await {
            Ok(e) => e,
            Err(e) => {
                tracing::warn!(error = %e, "could not inventory the spool to enforce its cap");
                return;
            }
        };
        let to_evict = overflow_evictions(entries.len(), 1, self.policy.max_entries);
        for entry in entries.into_iter().take(to_evict) {
            tracing::error!(
                envelope_id = %entry.envelope_id,
                spooled_at = %entry.spooled_at,
                attempts = entry.attempts,
                last_error = entry.last_error.as_deref().unwrap_or(""),
                max_entries = self.policy.max_entries,
                "slack spool is at capacity; evicting the oldest undelivered envelope"
            );
            match self.remove(&entry.envelope_id).await {
                Ok(()) => {
                    self.evicted_overflow.fetch_add(1, Ordering::Relaxed);
                }
                Err(e) => {
                    tracing::warn!(envelope_id = %entry.envelope_id, error = %e, "spool eviction failed")
                }
            }
        }
    }

    async fn write_entry(&self, entry: &SpoolEntry) -> Result<()> {
        let path = self.entry_path(&entry.envelope_id);
        let tmp = path.with_extension("json.tmp");
        let bytes = serde_json::to_vec(entry).context("serializing a spool entry")?;

        let mut file = tokio::fs::File::create(&tmp)
            .await
            .with_context(|| format!("creating spool temp file {}", tmp.display()))?;
        file.write_all(&bytes)
            .await
            .with_context(|| format!("writing spool temp file {}", tmp.display()))?;
        // fsync before the rename: a rename that lands ahead of the data
        // is exactly the crash window this spool exists to close.
        file.sync_all()
            .await
            .with_context(|| format!("fsync of spool temp file {}", tmp.display()))?;
        drop(file);

        tokio::fs::rename(&tmp, &path)
            .await
            .with_context(|| format!("publishing spool entry {}", path.display()))?;
        self.sync_dir().await;
        Ok(())
    }

    /// fsync the directory so the rename itself is durable. Best effort:
    /// some filesystems refuse a directory fsync, and failing the write
    /// over that would be worse than the residual risk.
    async fn sync_dir(&self) {
        match tokio::fs::File::open(&self.dir).await {
            Ok(dir) => {
                if let Err(e) = dir.sync_all().await {
                    tracing::debug!(dir = %self.dir.display(), error = %e, "spool directory fsync unsupported");
                }
            }
            Err(e) => {
                tracing::debug!(dir = %self.dir.display(), error = %e, "could not open the spool directory for fsync");
            }
        }
    }

    async fn quarantine(&self, path: &Path) {
        let target = path.with_extension("json.corrupt");
        if let Err(e) = tokio::fs::rename(path, &target).await {
            tracing::warn!(
                path = %path.display(),
                error = %e,
                "could not quarantine an unreadable spool entry; it will be re-read next pass"
            );
        }
    }

    fn decrement_depth(&self) {
        let _ = self
            .depth
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |d| {
                Some(d.saturating_sub(1))
            });
    }
}

/// Wall-clock elapsed time, clamped at zero. A spooled_at in the future
/// (clock step, restored backup) reads as "just touched" rather than as a
/// negative age that would wrap.
fn elapsed_since(now: DateTime<Utc>, then: DateTime<Utc>) -> Duration {
    now.signed_duration_since(then)
        .to_std()
        .unwrap_or(Duration::ZERO)
}

// ── Tests ───────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn policy(max_age_secs: u64, max_entries: usize) -> SpoolPolicy {
        SpoolPolicy {
            max_age: Duration::from_secs(max_age_secs),
            max_entries,
        }
    }

    fn body(id: &str) -> Value {
        json!({"_meta": {"envelope_id": id}, "text": "hello"})
    }

    /// A per-test spool rooted in a canonicalized tempdir, so path
    /// assertions match on macOS where /var resolves through /private.
    async fn spool_fixture(p: SpoolPolicy) -> (tempfile::TempDir, EnvelopeSpool) {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let spool = EnvelopeSpool::open(root.join("slack-spool"), p)
            .await
            .unwrap();
        (dir, spool)
    }

    // ── Naming ──────────────────────────────────────────────────

    #[test]
    fn spool_file_name_keeps_a_uuid_readable() {
        let name = spool_file_name("d1e2f3a4-0000-4000-8000-abcdefabcdef");
        assert!(
            name.starts_with("d1e2f3a4-0000-4000-8000-abcdefabcdef-"),
            "the operator can still find the envelope by id: {name}"
        );
        assert!(name.ends_with(".json"));
    }

    #[test]
    fn spool_file_name_neutralizes_path_traversal() {
        let name = spool_file_name("../../etc/passwd");
        assert!(
            !name.contains('/') && !name.contains(".."),
            "a hostile envelope_id must not escape the spool directory: {name}"
        );
    }

    #[test]
    fn spool_file_name_disambiguates_ids_that_sanitize_alike() {
        // Both collapse to the same sanitized stem; the digest separates them.
        assert_ne!(spool_file_name("a/b"), spool_file_name("a:b"));
    }

    #[test]
    fn spool_file_name_bounds_a_pathological_id() {
        let name = spool_file_name(&"z".repeat(4096));
        assert!(
            name.len() < 160,
            "filesystem name limits respected: {}",
            name.len()
        );
    }

    // ── Sweep classification ────────────────────────────────────

    #[test]
    fn a_freshly_spooled_entry_waits_for_the_inline_attempt() {
        let decision = classify_spool_entry(
            Duration::from_secs(2),
            Duration::from_secs(2),
            SWEEP_QUIET_PERIOD,
            &policy(86_400, 5_000),
        );
        assert_eq!(decision, SweepDecision::Wait);
    }

    #[test]
    fn a_quiet_entry_is_retried() {
        let decision = classify_spool_entry(
            Duration::from_secs(600),
            Duration::from_secs(600),
            SWEEP_QUIET_PERIOD,
            &policy(86_400, 5_000),
        );
        assert_eq!(decision, SweepDecision::Retry);
    }

    #[test]
    fn an_aged_entry_is_discarded_even_when_just_retried() {
        // Age beats quiet time: otherwise an envelope retried every sweep
        // would never reach its age bound and the spool would not shrink.
        let decision = classify_spool_entry(
            Duration::from_secs(90_000),
            Duration::from_secs(1),
            SWEEP_QUIET_PERIOD,
            &policy(86_400, 5_000),
        );
        assert_eq!(decision, SweepDecision::DiscardAged);
    }

    #[test]
    fn the_boot_replay_ignores_the_quiet_period() {
        let decision = classify_spool_entry(
            Duration::from_secs(1),
            Duration::from_secs(1),
            Duration::ZERO,
            &policy(86_400, 5_000),
        );
        assert_eq!(decision, SweepDecision::Retry);
    }

    // ── Capacity arithmetic ─────────────────────────────────────

    #[test]
    fn overflow_evicts_nothing_below_the_cap() {
        assert_eq!(overflow_evictions(10, 1, 100), 0);
        assert_eq!(overflow_evictions(99, 1, 100), 0);
    }

    #[test]
    fn overflow_evicts_exactly_the_excess() {
        assert_eq!(overflow_evictions(100, 1, 100), 1);
        assert_eq!(overflow_evictions(105, 1, 100), 6);
    }

    #[test]
    fn overflow_never_evicts_more_than_is_present() {
        assert_eq!(overflow_evictions(2, 50, 1), 2);
    }

    #[test]
    fn a_zero_cap_means_unbounded() {
        assert_eq!(overflow_evictions(9_000, 1, 0), 0);
    }

    // ── Durability ──────────────────────────────────────────────

    #[tokio::test]
    async fn a_persisted_envelope_survives_a_reopen() {
        let (dir, spool) = spool_fixture(SpoolPolicy::default()).await;
        spool.persist("env-1", &body("env-1")).await.unwrap();
        assert_eq!(spool.depth(), 1);
        let spool_dir = spool.dir().to_path_buf();
        drop(spool);

        // A restarted sidecar adopts what the previous process wrote.
        let reopened = EnvelopeSpool::open(&spool_dir, SpoolPolicy::default())
            .await
            .unwrap();
        let entries = reopened.list().await.unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].envelope_id, "env-1");
        assert_eq!(entries[0].event, body("env-1"));
        assert_eq!(entries[0].attempts, 0);
        assert!(spool_dir.starts_with(dir.path().canonicalize().unwrap()));
    }

    #[tokio::test]
    async fn removal_is_idempotent() {
        let (_dir, spool) = spool_fixture(SpoolPolicy::default()).await;
        spool.persist("env-1", &body("env-1")).await.unwrap();
        spool.remove("env-1").await.unwrap();
        assert_eq!(spool.depth(), 0);
        // The sweep and the inline path can both reach this for one id.
        spool.remove("env-1").await.unwrap();
        assert_eq!(spool.depth(), 0);
        assert!(spool.list().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_failed_round_is_stamped_without_losing_the_body() {
        let (_dir, spool) = spool_fixture(SpoolPolicy::default()).await;
        spool.persist("env-1", &body("env-1")).await.unwrap();
        spool
            .record_failure("env-1", "daemon POST budget exhausted")
            .await
            .unwrap();
        spool
            .record_failure("env-1", "connection refused")
            .await
            .unwrap();

        let entries = spool.list().await.unwrap();
        assert_eq!(entries.len(), 1, "a stamped entry stays in the spool");
        assert_eq!(entries[0].attempts, 2);
        assert_eq!(entries[0].last_error.as_deref(), Some("connection refused"));
        assert!(entries[0].last_attempt_at.is_some());
        assert_eq!(entries[0].event, body("env-1"), "the payload is untouched");
        assert_eq!(spool.depth(), 1);
    }

    #[tokio::test]
    async fn re_spooling_one_envelope_does_not_double_count() {
        let (_dir, spool) = spool_fixture(SpoolPolicy::default()).await;
        spool.persist("env-1", &body("env-1")).await.unwrap();
        spool.persist("env-1", &body("env-1")).await.unwrap();
        assert_eq!(spool.depth(), 1);
        assert_eq!(spool.list().await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn the_cap_evicts_the_oldest_and_admits_the_newest() {
        let (_dir, spool) = spool_fixture(policy(86_400, 2)).await;
        spool.persist("old", &body("old")).await.unwrap();
        // Distinct spooled_at values, so oldest-first ordering is real.
        tokio::time::sleep(Duration::from_millis(5)).await;
        spool.persist("middle", &body("middle")).await.unwrap();
        tokio::time::sleep(Duration::from_millis(5)).await;
        spool.persist("newest", &body("newest")).await.unwrap();

        let ids: Vec<_> = spool
            .list()
            .await
            .unwrap()
            .into_iter()
            .map(|e| e.envelope_id)
            .collect();
        assert_eq!(ids, vec!["middle".to_string(), "newest".to_string()]);
        assert_eq!(spool.depth(), 2);
    }

    #[tokio::test]
    async fn plan_sweep_partitions_by_age_and_quiet_time() {
        let (_dir, spool) = spool_fixture(policy(3_600, 5_000)).await;
        spool.persist("fresh", &body("fresh")).await.unwrap();
        spool.persist("stale", &body("stale")).await.unwrap();
        spool.persist("ancient", &body("ancient")).await.unwrap();

        // Rewrite two entries with fabricated timestamps rather than
        // waiting an hour of wall clock.
        let now = Utc::now();
        backdate(&spool, "stale", now - chrono::Duration::seconds(600)).await;
        backdate(&spool, "ancient", now - chrono::Duration::seconds(7_200)).await;

        let plan = spool.plan_sweep(now, SWEEP_QUIET_PERIOD).await.unwrap();
        assert_eq!(
            plan.waiting, 1,
            "the fresh entry is left to the inline path"
        );
        assert_eq!(
            plan.retry
                .iter()
                .map(|e| e.envelope_id.as_str())
                .collect::<Vec<_>>(),
            vec!["stale"]
        );
        assert_eq!(
            plan.discard
                .iter()
                .map(|e| e.envelope_id.as_str())
                .collect::<Vec<_>>(),
            vec!["ancient"]
        );
    }

    #[tokio::test]
    async fn an_unreadable_entry_is_quarantined_not_replayed_forever() {
        let (_dir, spool) = spool_fixture(SpoolPolicy::default()).await;
        spool.persist("good", &body("good")).await.unwrap();
        tokio::fs::write(spool.entry_path("torn"), b"{not json")
            .await
            .unwrap();

        let entries = spool.list().await.unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].envelope_id, "good");
        assert_eq!(spool.depth(), 1, "the counter reflects deliverable entries");
        assert!(
            tokio::fs::try_exists(spool.entry_path("torn").with_extension("json.corrupt"))
                .await
                .unwrap(),
            "the bytes are preserved for an operator, not deleted"
        );
    }

    #[tokio::test]
    async fn temp_files_are_never_mistaken_for_entries() {
        let (_dir, spool) = spool_fixture(SpoolPolicy::default()).await;
        tokio::fs::write(spool.dir().join("half-written.json.tmp"), b"{}")
            .await
            .unwrap();
        assert!(spool.list().await.unwrap().is_empty());
    }

    async fn backdate(spool: &EnvelopeSpool, envelope_id: &str, when: DateTime<Utc>) {
        let path = spool.entry_path(envelope_id);
        let bytes = tokio::fs::read(&path).await.unwrap();
        let mut entry: SpoolEntry = serde_json::from_slice(&bytes).unwrap();
        entry.spooled_at = when;
        entry.last_attempt_at = Some(when);
        tokio::fs::write(&path, serde_json::to_vec(&entry).unwrap())
            .await
            .unwrap();
    }
}
