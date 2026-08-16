//! Producer working state: reconciliation baselines and sweep marks.
//!
//! **The server owns the cursor.** That invariant is not weakened by anything
//! in this file. The satellite asks the corpus host where to resume, and a
//! satellite that lost this journal entirely still resumes correctly, because
//! the resume point is `max(server watermark, local mark)` and the local mark
//! only ever moves FORWARD of a watermark it has already seen the server
//! acknowledge.
//!
//! What the journal buys is the three things a cursor structurally cannot say:
//!
//! - **Edits and deletions.** Slack exposes no "changed since" query. An edited
//!   message returns from a resweep with its original `ts` and a moved edit
//!   stamp; a deleted message is simply absent. Both are invisible to a
//!   forward-only cursor and both are visible against a BASELINE of what this
//!   producer last observed (design 5.4).
//! - **Empty-window skipping.** A window that contained nothing does not move
//!   the server's watermark, because the watermark is derived from landed
//!   records. Without a local mark, an idle channel would re-walk the same
//!   empty windows every cycle forever.
//! - **Thread resweep avoidance.** A parent's latest-reply timestamp is the
//!   cheap test for whether a thread needs resweeping (design 5.3), and it is
//!   only cheap if the last swept value is remembered.
//!
//! Losing this file costs edit and delete detection for one window plus some
//! redundant sweeps. It never costs corpus data, and it can never cause a gap:
//! every advance recorded here happens AFTER the corpus acknowledged the batch
//! that justified it.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use bbox_conversation_source::message_ts_is_after;
use serde::{Deserialize, Serialize};

const JOURNAL_VERSION: u32 = 1;

/// Slack's `created` is server clock against a client-derived horizon, so the
/// created term of the backfill floor keeps one day of slack: a channel whose
/// recorded creation lands slightly after the horizon floor must still stop,
/// not oscillate around the boundary every cycle.
pub(crate) const BACKFILL_CREATED_SLACK_SECS: i64 = 86_400;

/// What this producer last observed about one landed message.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct MessageBaseline {
    /// The provider's edit stamp at the moment of the last observation. A
    /// MOVED stamp is the only edit signal Slack gives.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub edited_ts: Option<String>,
    /// The highest revision this producer has emitted for the message.
    pub revision: u64,
    /// True when this message is only ever visible through
    /// `conversations.replies`.
    ///
    /// Load-bearing, and the reason this field exists at all: a channel-history
    /// resweep does not return unbroadcast thread replies, so every reply
    /// baseline would look ABSENT to the reconciliation diff and be tombstoned.
    /// Deletion detection therefore covers channel-visible messages only, which
    /// is a narrower honest claim than design 5.4's window bound and is stated
    /// as such on the status surface rather than quietly assumed.
    #[serde(default)]
    pub thread_reply: bool,
}

/// What this producer last swept of one thread.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct ThreadMark {
    /// The newest reply `ts` this producer has landed for the thread.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub swept_through_ts: Option<String>,
    /// The `latest_reply` the channel sweep last advertised for the parent.
    /// Equal values mean the thread is idle and costs zero calls.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub latest_reply_seen: Option<String>,
    /// The cycle this thread was last swept, driving the ROTATION.
    ///
    /// The cheap latest-reply test only works while the parent is still inside
    /// a channel-history window the sweep re-reads. An older thread that takes
    /// a new unbroadcast reply advertises nothing to anyone, so a
    /// latest-reply-only policy would never resweep it and the reply would
    /// never land. The rotation is the answer: every known thread comes back
    /// around within a bounded number of cycles whether or not anything
    /// advertised a change.
    #[serde(default)]
    pub last_swept_cycle: u64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct ChannelJournal {
    /// The forward mark. Advanced ONLY after the corpus acknowledged the batch
    /// covering everything below it, which is what makes
    /// `max(server_watermark, this)` safe.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub swept_through_ts: Option<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub threads: BTreeMap<String, ThreadMark>,
    /// Keyed by `message_ts`. Pruned to the reconciliation window every cycle,
    /// so this grows with the window rather than with the channel.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub baseline: BTreeMap<String, MessageBaseline>,
    /// The oldest point the backfill lane has fully enumerated.
    ///
    /// Separate from the forward mark because backfill walks the other way and
    /// because the server's `oldest_observed` only moves when a window actually
    /// CONTAINED something: without this, an empty stretch of old history would
    /// be re-walked every cycle forever.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub backfilled_to_ts: Option<String>,
    /// Epoch seconds of the channel's creation, as the roster last observed it.
    ///
    /// The backfill floor's second term: no history can predate the channel, so
    /// a walk that reaches this point minus the clock-skew slack is done. Never
    /// cleared once set, so a roster read that omits the field for a cycle does
    /// not reopen a finished lane.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_epoch_secs: Option<i64>,
    /// Whether the backfill lane reached this channel's floor and may be
    /// skipped. Re-armed automatically whenever the floor moves further back
    /// (a deeper horizon, or an earlier observed `created`).
    #[serde(default)]
    pub backfill_complete: bool,
    /// The floor that was current when `backfill_complete` was set, so a later
    /// floor below it re-arms the lane instead of trusting a stale finish.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub backfill_floor_epoch_secs: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Journal {
    pub version: u32,
    /// Cycles this satellite has completed, driving the reconciliation cadence.
    #[serde(default)]
    pub cycles: u64,
    #[serde(default)]
    pub channels: BTreeMap<String, ChannelJournal>,
}

impl Default for Journal {
    fn default() -> Self {
        Self {
            version: JOURNAL_VERSION,
            cycles: 0,
            channels: BTreeMap::new(),
        }
    }
}

impl Journal {
    /// Load, treating an absent or unreadable journal as empty.
    ///
    /// A corrupt journal degrades to a full-baseline reseed rather than
    /// refusing to start, because the corpus is authority for everything that
    /// actually matters and a satellite that will not start observes nothing at
    /// all. The degradation is logged rather than swallowed.
    pub fn load(path: &Path) -> Self {
        let Ok(text) = std::fs::read_to_string(path) else {
            return Self::default();
        };
        match serde_json::from_str::<Self>(&text) {
            Ok(journal) if journal.version == JOURNAL_VERSION => journal,
            Ok(journal) => {
                tracing::warn!(
                    found = journal.version,
                    expected = JOURNAL_VERSION,
                    "discarding a journal from another version; reconciliation reseeds"
                );
                Self::default()
            }
            Err(error) => {
                tracing::warn!(
                    error = %error,
                    path = %path.display(),
                    "unreadable producer journal; reconciliation reseeds from the next sweep"
                );
                Self::default()
            }
        }
    }

    /// Publish atomically: temporary sibling, fsync, rename, fsync the parent.
    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }
        let temporary = temporary_sibling(path);
        let bytes = serde_json::to_vec_pretty(self).context("encoding the producer journal")?;
        {
            use std::io::Write as _;
            let mut file = std::fs::File::create(&temporary)
                .with_context(|| format!("creating {}", temporary.display()))?;
            file.write_all(&bytes)?;
            file.sync_all()?;
        }
        std::fs::rename(&temporary, path)
            .with_context(|| format!("publishing {}", path.display()))?;
        if let Some(parent) = path.parent()
            && let Ok(directory) = std::fs::File::open(parent)
        {
            let _ = directory.sync_all();
        }
        Ok(())
    }

    pub fn channel(&self, channel_id: &str) -> Option<&ChannelJournal> {
        self.channels.get(channel_id)
    }

    pub fn channel_mut(&mut self, channel_id: &str) -> &mut ChannelJournal {
        self.channels.entry(channel_id.to_string()).or_default()
    }

    /// The point a forward sweep resumes from.
    ///
    /// The SERVER's watermark is the floor and the local mark can only raise
    /// it. Reversed, a stale local mark would skip messages the corpus never
    /// received; as written, a lost journal costs a redundant resweep and the
    /// store reports the redundancy as duplicates.
    pub fn resume_from(&self, channel_id: &str, server_watermark: Option<&str>) -> Option<String> {
        let local = self
            .channel(channel_id)
            .and_then(|channel| channel.swept_through_ts.clone());
        match (server_watermark, local) {
            (Some(server), Some(local)) => Some(if message_ts_is_after(&local, server) {
                local
            } else {
                server.to_string()
            }),
            (Some(server), None) => Some(server.to_string()),
            (None, local) => local,
        }
    }
}

impl ChannelJournal {
    /// Advance the forward mark, never backwards.
    pub fn advance_swept_through(&mut self, ts: &str) {
        let advance = match &self.swept_through_ts {
            Some(current) => message_ts_is_after(ts, current),
            None => true,
        };
        if advance {
            self.swept_through_ts = Some(ts.to_string());
        }
    }

    /// Record a landed message as the reconciliation baseline.
    ///
    /// A message that ALREADY carries an edit stamp seeds revision 1 rather
    /// than 0. It is known to have been edited at least once, and seeding 0
    /// would make the next observed edit emit revision 1, which a corpus that
    /// already applied one would refuse as a duplicate. This matters most right
    /// after a journal loss, which is exactly when baselines are reseeded from
    /// messages with edit history.
    pub fn observe_landed(
        &mut self,
        message_ts: &str,
        edited_ts: Option<&str>,
        thread_reply: bool,
    ) {
        let entry = self
            .baseline
            .entry(message_ts.to_string())
            .or_insert_with(|| MessageBaseline {
                edited_ts: None,
                revision: u64::from(edited_ts.is_some()),
                thread_reply,
            });
        entry.edited_ts = edited_ts.map(str::to_string);
        // A message first seen through a replies sweep and later observed in
        // channel history was BROADCAST, so it becomes channel-visible and
        // reconcilable. The flag only ever relaxes.
        entry.thread_reply = entry.thread_reply && thread_reply;
    }

    /// Lower the backfill mark, never raise it.
    pub fn lower_backfill_mark(&mut self, ts: &str) {
        let lower = match &self.backfilled_to_ts {
            Some(current) => message_ts_is_after(current, ts),
            None => true,
        };
        if lower {
            self.backfilled_to_ts = Some(ts.to_string());
        }
    }

    /// The channel's effective backfill floor: the deeper of the operator's
    /// horizon floor and creation-less-slack. No history can predate the
    /// channel, so a walk that reaches this point is done even when the
    /// horizon reaches further back. An unknown `created` adds NO bound: the
    /// horizon stays the floor, rather than a missing roster field silently
    /// declaring the lane finished.
    pub fn backfill_floor_secs(&self, horizon_floor_secs: i64) -> i64 {
        horizon_floor_secs.max(
            self.created_epoch_secs
                .unwrap_or(i64::MIN)
                .saturating_sub(BACKFILL_CREATED_SLACK_SECS),
        )
    }

    /// Record the channel's creation time as the roster last observed it.
    ///
    /// Sticky by design: `Some` overwrites, `None` never clears, so a roster
    /// read that omits the field does not reopen a finished backfill lane.
    pub fn observe_created(&mut self, created_epoch_secs: Option<i64>) {
        if let Some(created) = created_epoch_secs {
            self.created_epoch_secs = Some(created);
        }
    }

    /// Whether the backfill lane for this channel may be skipped.
    ///
    /// Complete is only trustworthy for the floor it was earned against: a
    /// horizon that now reaches further back (or a `created` that moved
    /// earlier) lowers the floor below the recorded one and re-arms the lane.
    pub fn backfill_lane_complete(&self, floor_epoch_secs: i64) -> bool {
        self.backfill_complete
            && self
                .backfill_floor_epoch_secs
                .is_none_or(|completed_at| completed_at <= floor_epoch_secs)
    }

    /// Mark the backfill lane finished against this floor.
    pub fn mark_backfill_complete(&mut self, floor_epoch_secs: i64) {
        self.backfill_complete = true;
        self.backfill_floor_epoch_secs = Some(floor_epoch_secs);
    }

    /// Drop baseline rows below the reconciliation window.
    ///
    /// This is what bounds the journal: it grows with the window, not with the
    /// channel's history. Design 5.4's honest limit lives here too, since a
    /// message pruned out of the baseline is a message whose deletion this
    /// producer can no longer detect.
    pub fn prune_baseline(&mut self, window_start_ts: &str) -> usize {
        let before = self.baseline.len();
        self.baseline
            .retain(|message_ts, _| !message_ts_is_after(window_start_ts, message_ts));
        before.saturating_sub(self.baseline.len())
    }

    pub fn thread(&self, parent_ts: &str) -> Option<&ThreadMark> {
        self.threads.get(parent_ts)
    }

    pub fn thread_mut(&mut self, parent_ts: &str) -> &mut ThreadMark {
        self.threads.entry(parent_ts.to_string()).or_default()
    }

    /// Whether a thread needs a `conversations.replies` call this cycle.
    ///
    /// The cheap test from design 5.3. An unknown parent always needs one: a
    /// producer that has never swept a thread cannot know whether it holds its
    /// replies, and guessing "no" is how a thread body silently never lands.
    pub fn thread_needs_sweep(&self, parent_ts: &str, latest_reply: Option<&str>) -> bool {
        let Some(mark) = self.threads.get(parent_ts) else {
            return true;
        };
        match (latest_reply, mark.latest_reply_seen.as_deref()) {
            (Some(latest), Some(seen)) => message_ts_is_after(latest, seen),
            // The channel sweep did not advertise a latest reply, so the cheap
            // test is unavailable and the honest answer is to sweep.
            (Some(_), None) => true,
            (None, _) => false,
        }
    }

    /// Bound the tracked-thread set by COUNT, never by age.
    ///
    /// Deliberately not pruned by the reconciliation window. An old thread that
    /// is still taking replies stays in the corpus's own inflight-parent set,
    /// so dropping it here by age would hand it back every cycle as an unknown
    /// parent and resweep it forever. Count is the honest bound, and it matches
    /// the corpus's own [`bbox_conversation_source::MAX_INFLIGHT_THREAD_PARENTS`]
    /// discipline of keeping the newest.
    pub fn prune_threads(&mut self, keep: usize) {
        while self.threads.len() > keep {
            // BTreeMap iterates in key order and message timestamps sort
            // oldest-first for the ranges a real workspace produces, so the
            // oldest thread is the one dropped. An old thread is the one least
            // likely to still be taking replies, which is the same tradeoff the
            // corpus makes on its inflight-parent bound.
            let Some(oldest) = self.threads.keys().next().cloned() else {
                break;
            };
            self.threads.remove(&oldest);
        }
    }
}

fn temporary_sibling(path: &Path) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(format!(".{}.tmp", std::process::id()));
    path.with_file_name(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    const CHANNEL: &str = "C0FIXTURE01";

    #[test]
    fn the_server_watermark_is_the_floor_of_a_resume() {
        let mut journal = Journal::default();
        journal
            .channel_mut(CHANNEL)
            .advance_swept_through("1755000000.000100");

        // A local mark ahead of the server raises the resume point.
        assert_eq!(
            journal.resume_from(CHANNEL, Some("1754000000.000000")),
            Some("1755000000.000100".to_string())
        );
        // A server watermark ahead of the local mark WINS. The reverse would
        // skip messages the corpus never received.
        assert_eq!(
            journal.resume_from(CHANNEL, Some("1756000000.000000")),
            Some("1756000000.000000".to_string())
        );
    }

    #[test]
    fn a_lost_journal_resumes_from_the_server_alone() {
        let journal = Journal::default();
        assert_eq!(
            journal.resume_from(CHANNEL, Some("1755000000.000100")),
            Some("1755000000.000100".to_string())
        );
        assert_eq!(journal.resume_from(CHANNEL, None), None);
    }

    #[test]
    fn the_forward_mark_never_moves_backwards() {
        let mut journal = Journal::default();
        let channel = journal.channel_mut(CHANNEL);
        channel.advance_swept_through("1755000010.000000");
        channel.advance_swept_through("1755000000.000000");
        assert_eq!(
            channel.swept_through_ts.as_deref(),
            Some("1755000010.000000")
        );
    }

    #[test]
    fn a_message_that_already_carries_an_edit_seeds_revision_one() {
        let mut journal = Journal::default();
        let channel = journal.channel_mut(CHANNEL);
        channel.observe_landed("1755000000.000100", None, false);
        channel.observe_landed("1755000001.000100", Some("1755000500.000000"), false);
        assert_eq!(channel.baseline["1755000000.000100"].revision, 0);
        assert_eq!(channel.baseline["1755000001.000100"].revision, 1);
    }

    #[test]
    fn a_reply_that_is_later_broadcast_becomes_channel_visible() {
        let mut journal = Journal::default();
        let channel = journal.channel_mut(CHANNEL);
        channel.observe_landed("1755000009.000200", None, true);
        assert!(channel.baseline["1755000009.000200"].thread_reply);
        channel.observe_landed("1755000009.000200", None, false);
        assert!(!channel.baseline["1755000009.000200"].thread_reply);
    }

    #[test]
    fn the_backfill_mark_never_moves_forward() {
        let mut journal = Journal::default();
        let channel = journal.channel_mut(CHANNEL);
        channel.lower_backfill_mark("1755000000.000000");
        channel.lower_backfill_mark("1755009999.000000");
        assert_eq!(
            channel.backfilled_to_ts.as_deref(),
            Some("1755000000.000000")
        );
        channel.lower_backfill_mark("1750000000.000000");
        assert_eq!(
            channel.backfilled_to_ts.as_deref(),
            Some("1750000000.000000")
        );
    }

    #[test]
    fn pruning_bounds_the_baseline_to_the_window() {
        let mut journal = Journal::default();
        let channel = journal.channel_mut(CHANNEL);
        channel.observe_landed("1755000000.000100", None, false);
        channel.observe_landed("1755009999.000100", None, false);
        assert_eq!(channel.prune_baseline("1755005000.000000"), 1);
        assert!(channel.baseline.contains_key("1755009999.000100"));
    }

    #[test]
    fn an_idle_thread_costs_no_calls_and_an_unknown_one_always_sweeps() {
        let mut journal = Journal::default();
        let channel = journal.channel_mut(CHANNEL);
        assert!(channel.thread_needs_sweep("1755000000.000100", Some("1755000009.000000")));

        let mark = channel.thread_mut("1755000000.000100");
        mark.latest_reply_seen = Some("1755000009.000000".into());
        mark.swept_through_ts = Some("1755000009.000000".into());
        assert!(!channel.thread_needs_sweep("1755000000.000100", Some("1755000009.000000")));
        assert!(channel.thread_needs_sweep("1755000000.000100", Some("1755000010.000000")));
    }

    #[test]
    fn a_journal_round_trips_through_its_file() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let path = root.join("nested").join("journal.json");

        let mut journal = Journal::default();
        journal.cycles = 3;
        let channel = journal.channel_mut(CHANNEL);
        channel.advance_swept_through("1755000000.000100");
        channel.observe_landed("1755000000.000100", Some("1755000500.000000"), false);
        journal.save(&path).unwrap();

        let reloaded = Journal::load(&path);
        assert_eq!(reloaded, journal);
        assert!(path.starts_with(&root));
    }

    #[test]
    fn an_unreadable_journal_degrades_to_empty_rather_than_refusing() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let path = root.join("journal.json");
        std::fs::write(&path, "{ not json").unwrap();
        assert_eq!(Journal::load(&path), Journal::default());
    }

    /// The v1 document on disk BEFORE this branch's backfill fields existed:
    /// a hand-written literal, not a serialization of the current type, so the
    /// defaults are proven rather than assumed. It must LOAD (version matches,
    /// so there is no reseed) with the new fields defaulted, because a
    /// satellite upgrading in place must not discard its forward marks.
    #[test]
    fn a_v1_journal_without_the_backfill_fields_loads_with_defaults() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let path = root.join("journal.json");
        let document = format!(
            r#"{{"version":1,"cycles":3,"channels":{{"{CHANNEL}":{{"swept_through_ts":"1755000000.000100"}}}}}}"#
        );
        std::fs::write(&path, document).unwrap();

        let journal = Journal::load(&path);
        assert_eq!(journal.version, 1, "same major version: no reseed");
        assert_eq!(journal.cycles, 3);
        let channel = journal
            .channel(CHANNEL)
            .expect("the v1 channel and its forward mark survive the upgrade");
        assert_eq!(
            channel.swept_through_ts.as_deref(),
            Some("1755000000.000100")
        );
        assert_eq!(channel.created_epoch_secs, None);
        assert!(!channel.backfill_complete);
        assert_eq!(channel.backfill_floor_epoch_secs, None);
        assert_eq!(channel.backfilled_to_ts, None);
    }

    /// The floor is the FIRST bound a backwards walk reaches: the
    /// later-in-time (larger epoch) of the horizon and creation-less-slack.
    /// An unknown `created` imposes NO bound at all, so a missing roster field
    /// cannot silently declare a lane finished.
    #[test]
    fn the_backfill_floor_is_the_first_bound_reached_and_unknown_created_binds_nothing() {
        let mut journal = Journal::default();
        let unknown_created = journal.channel_mut(CHANNEL);
        assert_eq!(
            unknown_created.backfill_floor_secs(1_000),
            1_000,
            "no created: the horizon is the floor"
        );

        // Created at 200_000 with one day of slack: the creation bound
        // (113_600) is later than the horizon (1_000), so the walk reaches it
        // first and the channel's beginning is the floor.
        let channel = journal.channel_mut(CHANNEL);
        channel.observe_created(Some(200_000));
        assert_eq!(
            channel.backfill_floor_secs(1_000),
            200_000 - BACKFILL_CREATED_SLACK_SECS
        );

        // A horizon later than the creation bound is reached first instead:
        // the walk stops where the operator told it to, not at the channel's
        // beginning.
        channel.observe_created(Some(10_000));
        assert_eq!(
            channel.backfill_floor_secs(1_000),
            1_000,
            "the creation bound lies deeper than the horizon; the horizon is the floor"
        );
    }

    /// `observe_created` is sticky for Some and immune to None: a roster read
    /// that omits the field must not reopen a finished lane, and a roster that
    /// revises the creation must be able to move it (the re-arm path).
    #[test]
    fn created_observations_are_sticky_for_some_and_immune_to_none() {
        let mut journal = Journal::default();
        let channel = journal.channel_mut(CHANNEL);
        channel.observe_created(None);
        assert_eq!(channel.created_epoch_secs, None);
        channel.observe_created(Some(10_000));
        assert_eq!(channel.created_epoch_secs, Some(10_000));
        channel.observe_created(None);
        assert_eq!(
            channel.created_epoch_secs,
            Some(10_000),
            "an omitted roster field never clears a known creation"
        );
        channel.observe_created(Some(5_000));
        assert_eq!(
            channel.created_epoch_secs,
            Some(5_000),
            "a revised creation overwrites, which is the re-arm trigger"
        );
    }

    /// Completion is only trustworthy for the floor it was earned against: a
    /// floor that moved DEEPER (smaller epoch) re-arms the lane, and
    /// re-completing records the new floor so the state is not trusted
    /// forever.
    #[test]
    fn backfill_completion_rearms_when_the_floor_moves_deeper() {
        let mut journal = Journal::default();
        let channel = journal.channel_mut(CHANNEL);
        assert!(
            !channel.backfill_lane_complete(100),
            "a lane that never walked is not complete"
        );

        channel.mark_backfill_complete(100);
        assert!(channel.backfill_complete);
        assert_eq!(channel.backfill_floor_epoch_secs, Some(100));
        assert!(
            channel.backfill_lane_complete(100),
            "the floor it was earned against"
        );
        assert!(
            channel.backfill_lane_complete(150),
            "a shallower floor does not reopen finished history"
        );
        assert!(
            !channel.backfill_lane_complete(50),
            "a deeper floor re-arms the lane"
        );

        channel.mark_backfill_complete(50);
        assert!(
            channel.backfill_lane_complete(50),
            "re-earned against the deeper floor"
        );
        assert_eq!(channel.backfill_floor_epoch_secs, Some(50));
    }
}
