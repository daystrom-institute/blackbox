//! The publication cycle end to end: a real Slack client against a fixture
//! workspace, and a model of the corpus landing store.
//!
//! One test per gate clause from design section 9 (S1 plus the S3 thread
//! machinery this deployment cannot defer), because thread completeness and
//! edit/delete reconciliation are the two places an ingestion lane most easily
//! looks correct while being wrong.

mod support;

use std::path::PathBuf;

use anyhow::Result;
use async_trait::async_trait;
use bbox_conversation_source::{
    ChannelRosterReceiptV1, ChannelRosterRequestV1, ConversationBatchReceiptV1,
    ConversationBatchV1, ConversationCatalogOnboardRequestV1, ConversationCatalogOnboardResponseV1,
    ConversationCursorsResponseV1, ConversationRevisionsReceiptV1, ConversationRevisionsRequestV1,
    ConversationStatusResponseV1,
};
use bbox_corpus_core::project_catalog::ConnectorScope;
use bbox_slack_collector::{
    BackfillHorizon, ConversationSink, Journal, SatelliteConfig, Shutdown, SlackClient,
    run_publication_cycle, run_publication_cycle_with_shutdown,
};
use support::{
    FakeChannel, FakeMessage, FakeSlack, FakeSlackState, ModelSink, RefusingSink, fast_rate_policy,
    one_app_scopes, pinned_now, test_config, ts_before,
};

const CHANNEL: &str = "C0FIXTURE01";
const OTHER_CHANNEL: &str = "C0FIXTURE02";

struct Harness {
    slack: FakeSlack,
    client: SlackClient,
    sink: ModelSink,
    config: SatelliteConfig,
    journal_path: PathBuf,
    _directory: tempfile::TempDir,
}

impl Harness {
    async fn start(state: FakeSlackState) -> Self {
        let directory = tempfile::tempdir().unwrap();
        // Canonicalized before anything derives a path from it: on macOS the
        // raw tempdir is /var/... while everything downstream sees /private/var.
        let root = directory.path().canonicalize().unwrap();
        let journal_path = root.join("journal.json");
        let slack = FakeSlack::start(state).await;
        let client = SlackClient::new(
            slack.base_url.as_str(),
            "xoxb-fixture-token",
            fast_rate_policy(),
        )
        .unwrap();
        let config = test_config(&slack.base_url, journal_path.clone(), &["engineering"]);
        Self {
            slack,
            client,
            sink: ModelSink::new(),
            config,
            journal_path,
            _directory: directory,
        }
    }

    async fn cycle(&self, offset_secs: i64) -> bbox_slack_collector::CycleOutcome {
        run_publication_cycle(
            &self.client,
            &self.sink,
            &self.config,
            &self.journal_path,
            pinned_now(offset_secs),
        )
        .await
        .unwrap()
    }
}

fn workspace(messages: Vec<FakeMessage>) -> FakeSlackState {
    FakeSlackState {
        granted_scopes: one_app_scopes(),
        channels: vec![FakeChannel::public(CHANNEL, "engineering")],
        history: [(CHANNEL.to_string(), messages)].into_iter().collect(),
        page_size: 50,
        ..FakeSlackState::default()
    }
}

fn three_messages() -> Vec<FakeMessage> {
    vec![
        FakeMessage::new(&ts_before(600, 1), "U0HUMAN", "first"),
        FakeMessage::new(&ts_before(300, 2), "U0OTHER", "second"),
        FakeMessage::new(&ts_before(120, 3), "U0HUMAN", "third"),
    ]
}

#[tokio::test]
async fn a_steady_state_sweep_lands_every_new_message_exactly_once() {
    let harness = Harness::start(workspace(three_messages())).await;

    let first = harness.cycle(0).await;
    assert_eq!(first.channels_enrolled, 1);
    assert_eq!(first.messages_landed, 3);
    assert_eq!(first.duplicates, 0);
    assert_eq!(first.windows_deferred, 0);

    // A second cycle over the same workspace re-reads nothing it already
    // landed. Exactly once means exactly once ACROSS cycles, which is the half
    // a single-run assertion misses.
    let second = harness.cycle(60).await;
    assert_eq!(second.messages_landed, 0);
    assert_eq!(second.duplicates, 0);

    let mut landed = harness.sink.timestamps(CHANNEL);
    landed.sort();
    assert_eq!(
        landed,
        vec![ts_before(600, 1), ts_before(300, 2), ts_before(120, 3)]
    );
    assert_eq!(
        harness
            .sink
            .record(CHANNEL, &ts_before(300, 2))
            .unwrap()
            .text,
        "second"
    );
}

#[tokio::test]
async fn an_unchanged_channel_produces_no_batch_at_all() {
    let harness = Harness::start(workspace(three_messages())).await;
    harness.cycle(0).await;
    let batches_after_first = harness.sink.batches();
    assert_eq!(batches_after_first, 1);

    harness.cycle(60).await;

    // Not "an empty batch", not "a batch the server deduplicates": NO batch.
    // An idle workspace must not turn "no news" into write traffic.
    assert_eq!(harness.sink.batches(), batches_after_first);
}

#[tokio::test]
async fn a_thread_whose_replies_were_never_broadcast_is_fully_indexed() {
    // The gate that decides whether this connector is honest. Unbroadcast
    // replies never appear in conversations.history, so a history-only
    // collector silently loses most thread bodies while looking healthy.
    let parent_ts = ts_before(600, 1);
    let reply_one = ts_before(400, 5);
    let reply_two = ts_before(100, 9);
    let mut state = workspace(vec![
        FakeMessage::new(&parent_ts, "U0HUMAN", "thread starter").parent(2, &reply_two),
        FakeMessage::new(&ts_before(300, 2), "U0OTHER", "unrelated"),
    ]);
    state.replies.insert(
        (CHANNEL.to_string(), parent_ts.clone()),
        vec![
            FakeMessage::new(&reply_one, "U0OTHER", "reply one").reply_to(&parent_ts),
            FakeMessage::new(&reply_two, "U0HUMAN", "reply two").reply_to(&parent_ts),
        ],
    );
    let harness = Harness::start(state).await;

    let outcome = harness.cycle(0).await;

    assert_eq!(outcome.threads_swept, 1);
    assert_eq!(outcome.thread_replies_landed, 2);
    // Four records: two channel-visible messages plus two replies that no
    // history sweep would ever have returned.
    assert_eq!(harness.sink.timestamps(CHANNEL).len(), 4);
    let landed_reply = harness.sink.record(CHANNEL, &reply_two).unwrap();
    assert_eq!(landed_reply.text, "reply two");
    assert_eq!(
        landed_reply.thread_parent_ts.as_deref(),
        Some(parent_ts.as_str())
    );
    // The parent is landed once, by the history sweep. The replies sweep
    // returns it on every page and must filter it.
    assert_eq!(
        harness.sink.record(CHANNEL, &parent_ts).unwrap().text,
        "thread starter"
    );

    // An idle thread costs nothing on the next cycle: the advertised
    // latest_reply has not moved, so the cheap test skips it.
    let before = harness.slack.request_count("conversations.replies");
    harness.cycle(60).await;
    assert_eq!(
        harness.slack.request_count("conversations.replies"),
        before,
        "an idle thread must not be resswept while nothing has changed"
    );
}

#[tokio::test]
async fn an_edit_produces_a_revision_and_a_delete_produces_a_tombstone() {
    let mut harness = Harness::start(workspace(three_messages())).await;
    // Reconcile every cycle so the gate does not depend on cadence arithmetic.
    harness.config.reconciliation.every_cycles = 1;

    harness.cycle(0).await;
    assert_eq!(harness.sink.timestamps(CHANNEL).len(), 3);

    // The workspace changes underneath the corpus: one message is edited in
    // place (same ts, moved edit stamp) and one is deleted (simply absent).
    // Both are invisible to a forward-only cursor.
    harness.slack.with(|state| {
        let messages = state.history.get_mut(CHANNEL).unwrap();
        messages.retain(|message| message.ts != ts_before(120, 3));
        for message in messages.iter_mut() {
            if message.ts == ts_before(300, 2) {
                *message = message
                    .clone()
                    .edited(&ts_before(30, 0), "second, corrected");
            }
        }
    });

    let outcome = harness.cycle(60).await;

    assert!(outcome.reconciled);
    assert_eq!(outcome.revisions_emitted, 1);
    assert_eq!(outcome.tombstones_emitted, 1);

    // The edit superseded IN PLACE: same identity, higher revision, new text.
    let edited = harness.sink.record(CHANNEL, &ts_before(300, 2)).unwrap();
    assert_eq!(edited.text, "second, corrected");
    assert_eq!(edited.revision, 1);
    assert!(!edited.tombstoned);

    // The deletion MARKED the record rather than erasing it, so the corpus can
    // still tell a redaction from a message it never saw.
    let deleted = harness.sink.record(CHANNEL, &ts_before(120, 3)).unwrap();
    assert!(deleted.tombstoned);
    assert_eq!(deleted.revision, 1);

    // And a third cycle with nothing further changed emits neither again.
    let quiet = harness.cycle(120).await;
    assert_eq!(quiet.revisions_emitted, 0);
    assert_eq!(quiet.tombstones_emitted, 0);
}

#[tokio::test]
async fn a_satellite_restart_without_its_journal_resumes_from_the_server_cursor() {
    let harness = Harness::start(workspace(three_messages())).await;
    harness.cycle(0).await;
    assert_eq!(harness.sink.timestamps(CHANNEL).len(), 3);

    // Producer working state is losable BY DESIGN. Delete it entirely and add a
    // message: the corpus cursor alone must produce a clean resume, with no
    // gap and no re-landing of what it already holds.
    std::fs::remove_file(&harness.journal_path).unwrap();
    assert_eq!(Journal::load(&harness.journal_path), Journal::default());
    harness.slack.with(|state| {
        state
            .history
            .get_mut(CHANNEL)
            .unwrap()
            .push(FakeMessage::new(
                &ts_before(-30, 4),
                "U0HUMAN",
                "after the restart",
            ));
    });

    let outcome = harness.cycle(60).await;

    assert_eq!(outcome.messages_landed, 1);
    assert_eq!(
        outcome.duplicates, 0,
        "the server watermark is the resume point, so nothing already landed is re-sent"
    );
    assert_eq!(harness.sink.timestamps(CHANNEL).len(), 4);
    assert_eq!(
        harness
            .sink
            .record(CHANNEL, &ts_before(-30, 4))
            .unwrap()
            .text,
        "after the restart"
    );
}

#[tokio::test]
async fn a_missing_read_scope_refuses_before_any_durable_write() {
    let mut state = workspace(three_messages());
    // The interactive bot's grant, minus the one read this collector needs.
    // Under the one-app posture this is the only scope failure it can refuse,
    // and refusing matters: without the scope every sweep comes back empty and
    // the deployment presents as a healthy satellite over an empty corpus.
    state.granted_scopes = vec!["channels:read".to_string(), "chat:write".to_string()];
    let slack = FakeSlack::start(state).await;
    let client = SlackClient::new(
        slack.base_url.as_str(),
        "xoxb-fixture-token",
        fast_rate_policy(),
    )
    .unwrap();
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap();
    let journal_path = root.join("journal.json");
    let config = test_config(&slack.base_url, journal_path.clone(), &["engineering"]);

    // The sink refuses every verb, so a cycle that reached the corpus at all
    // would fail with a different message than the one asserted here.
    let error = run_publication_cycle(
        &client,
        &RefusingSink,
        &config,
        &journal_path,
        pinned_now(0),
    )
    .await
    .unwrap_err()
    .to_string();

    assert!(error.contains("channels:history"), "{error}");
    assert!(error.contains("missing required read scope"), "{error}");
    assert!(!journal_path.exists(), "nothing durable may be written");
}

#[tokio::test]
async fn a_credential_that_reached_another_workspace_is_refused() {
    let mut harness = Harness::start(workspace(three_messages())).await;
    harness.config.expected_workspace_id = Some("T0SOMEWHEREELSE".to_string());

    let error = run_publication_cycle(
        &harness.client,
        &RefusingSink,
        &harness.config,
        &harness.journal_path,
        pinned_now(0),
    )
    .await
    .unwrap_err()
    .to_string();

    assert!(error.contains("T0SOMEWHEREELSE"), "{error}");
}

#[tokio::test]
async fn a_channel_outside_the_allowlist_is_never_swept() {
    let mut state = workspace(three_messages());
    state
        .channels
        .push(FakeChannel::public(OTHER_CHANNEL, "random"));
    state.history.insert(
        OTHER_CHANNEL.to_string(),
        vec![FakeMessage::new(
            &ts_before(200, 7),
            "U0HUMAN",
            "off limits",
        )],
    );
    let harness = Harness::start(state).await;

    let outcome = harness.cycle(0).await;

    assert_eq!(outcome.channels_enrolled, 1);
    assert_eq!(
        outcome.channels_skipped.get("not_included").copied(),
        Some(1)
    );
    assert!(harness.sink.timestamps(OTHER_CHANNEL).is_empty());
    // Not enrolled means not READ. Policy reaches the request, not just the
    // landing decision, so the stronger claim is available: the fixture was
    // never even asked about that channel.
    assert_eq!(harness.slack.channel_read_count(OTHER_CHANNEL), 0);
    assert!(harness.slack.channel_read_count(CHANNEL) > 0);
}

#[tokio::test]
async fn a_window_that_did_not_finish_enumerating_is_deferred_rather_than_landed() {
    // The cursor-integrity gate. History pages newest-first, so a
    // budget-truncated window holds the newest messages rather than a
    // contiguous run from the watermark. Landing it would advance the cursor
    // past the older two forever.
    let mut state = workspace(three_messages());
    state.page_size = 1;
    let mut harness = Harness::start(state).await;
    harness.config.sweep.max_pages_per_window = 1;

    let deferred = harness.cycle(0).await;

    assert_eq!(deferred.windows_deferred, 1);
    assert_eq!(deferred.messages_landed, 0);
    assert_eq!(
        harness.sink.batches(),
        0,
        "a partial window must not be published at all"
    );

    // Nothing was lost: with room to finish, the same range lands whole.
    harness.config.sweep.max_pages_per_window = 10;
    let complete = harness.cycle(60).await;
    assert_eq!(complete.windows_deferred, 0);
    assert_eq!(complete.messages_landed, 3);
    assert_eq!(harness.sink.timestamps(CHANNEL).len(), 3);
}

#[tokio::test]
async fn structural_channel_noise_is_counted_rather_than_indexed() {
    let mut messages = three_messages();
    messages.push(
        FakeMessage::new(&ts_before(90, 8), "U0HUMAN", "has joined the channel")
            .subtype("channel_join"),
    );
    let harness = Harness::start(workspace(messages)).await;

    let outcome = harness.cycle(0).await;

    assert_eq!(outcome.messages_landed, 3);
    assert_eq!(
        outcome
            .normalization_skips
            .get("subtype_channel_join")
            .copied(),
        Some(1)
    );
    assert!(harness.sink.record(CHANNEL, &ts_before(90, 8)).is_none());
}

#[tokio::test]
async fn membership_mode_posts_a_complete_roster_and_explicit_mode_does_not() {
    // gap-e84231d3: only membership mode's roster read (`users.conversations`)
    // is provably the producer's entire current member set, so only it may
    // claim `complete` on the wire. Explicit mode's enrolled set is narrowed
    // by an operator glob and must not tell the corpus host it saw everything.
    let mut membership_harness = Harness::start(workspace(three_messages())).await;
    membership_harness.config.channels.enrollment =
        bbox_slack_collector::EnrollmentMode::Membership;
    membership_harness.config.channels.include = Vec::new();
    membership_harness.cycle(0).await;
    assert_eq!(
        membership_harness.sink.last_roster_complete(),
        Some(true),
        "membership mode's roster read is the bot's own full membership"
    );

    let explicit_harness = Harness::start(workspace(three_messages())).await;
    explicit_harness.cycle(0).await;
    assert_eq!(
        explicit_harness.sink.last_roster_complete(),
        Some(false),
        "explicit mode's policy-narrowed enrolled set is not a proven full sweep"
    );
}

// ---------------------------------------------------------------------------
// Shutdown
// ---------------------------------------------------------------------------

/// A sink that requests shutdown when the FIRST batch lands, standing in for
/// the watch loop's signal arriving mid-cycle.
struct StopOnFirstBatch {
    inner: ModelSink,
    shutdown: Shutdown,
    batches: std::sync::atomic::AtomicU32,
}

#[async_trait]
impl ConversationSink for StopOnFirstBatch {
    async fn onboard(
        &self,
        request: &ConversationCatalogOnboardRequestV1,
    ) -> Result<ConversationCatalogOnboardResponseV1> {
        self.inner.onboard(request).await
    }
    async fn post_channels(
        &self,
        request: &ChannelRosterRequestV1,
    ) -> Result<ChannelRosterReceiptV1> {
        self.inner.post_channels(request).await
    }
    async fn cursors(&self, scope: &ConnectorScope) -> Result<ConversationCursorsResponseV1> {
        self.inner.cursors(scope).await
    }
    async fn post_batch(&self, batch: &ConversationBatchV1) -> Result<ConversationBatchReceiptV1> {
        let seen = self
            .batches
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        if seen == 0 {
            self.shutdown.request();
        }
        self.inner.post_batch(batch).await
    }
    async fn post_revisions(
        &self,
        request: &ConversationRevisionsRequestV1,
    ) -> Result<ConversationRevisionsReceiptV1> {
        self.inner.post_revisions(request).await
    }
    async fn status(&self, scope: &ConnectorScope) -> Result<ConversationStatusResponseV1> {
        self.inner.status(scope).await
    }
}

#[tokio::test]
async fn a_shutdown_raised_mid_cycle_finishes_the_current_channel_and_skips_the_rest() {
    let state = FakeSlackState {
        granted_scopes: one_app_scopes(),
        channels: vec![
            FakeChannel::public(CHANNEL, "engineering"),
            FakeChannel::public(OTHER_CHANNEL, "other"),
        ],
        history: [
            (CHANNEL.to_string(), three_messages()),
            (OTHER_CHANNEL.to_string(), three_messages()),
        ]
        .into_iter()
        .collect(),
        page_size: 50,
        ..FakeSlackState::default()
    };
    let mut harness = Harness::start(state).await;
    harness.config.channels.include = vec!["engineering".into(), "other".into()];
    let shutdown = Shutdown::default();
    let sink = StopOnFirstBatch {
        inner: ModelSink::new(),
        shutdown: shutdown.clone(),
        batches: std::sync::atomic::AtomicU32::new(0),
    };

    let outcome = run_publication_cycle_with_shutdown(
        &harness.client,
        &sink,
        &harness.config,
        &harness.journal_path,
        pinned_now(0),
        &shutdown,
    )
    .await
    .unwrap();

    // Both channels enrolled (the roster ran), but only the in-flight channel
    // was swept: the flag went up during the FIRST channel's batch post, that
    // channel finished through its journal save, and the loop stopped before
    // ever asking Slack about the second one.
    assert_eq!(outcome.channels_enrolled, 2);
    assert_eq!(outcome.messages_landed, 3);
    assert!(harness.slack.channel_read_count(CHANNEL) > 0);
    assert_eq!(
        harness.slack.channel_read_count(OTHER_CHANNEL),
        0,
        "a channel the cycle never started must not be asked about at all"
    );
    let journal = Journal::load(&harness.journal_path);
    assert!(journal.channel(CHANNEL).is_some());
    assert!(
        journal.channel(OTHER_CHANNEL).is_none(),
        "the skipped channel costs no journal state either"
    );
}

#[tokio::test]
async fn a_shutdown_raised_before_the_channel_loop_publishes_the_roster_and_lands_nothing() {
    let harness = Harness::start(workspace(three_messages())).await;
    let shutdown = Shutdown::default();
    shutdown.request();

    let outcome = run_publication_cycle_with_shutdown(
        &harness.client,
        &harness.sink,
        &harness.config,
        &harness.journal_path,
        pinned_now(0),
        &shutdown,
    )
    .await
    .unwrap();

    // The roster and cursor reads are cycle bookkeeping and still happen; the
    // per-channel lanes are the part a shutdown may skip.
    assert_eq!(outcome.channels_enrolled, 1);
    assert_eq!(outcome.messages_landed, 0);
    assert_eq!(harness.slack.channel_read_count(CHANNEL), 0);
    assert_eq!(harness.sink.batches(), 0);
}

/// One day, the only window size these backfill tests use, so every walk is a
/// countable number of day windows.
const DAY: i64 = 86_400;

fn expected_floor_ts(days_before_pinned_now: i64) -> String {
    format!(
        "{}.000000",
        support::PINNED_NOW_SECS - days_before_pinned_now * DAY
    )
}

#[tokio::test]
async fn a_backfill_lane_stops_at_channel_creation_and_is_reported_complete() {
    let old_message = ts_before(2 * DAY + DAY / 2, 2);
    let mut state = workspace(vec![
        FakeMessage::new(&ts_before(90, 1), "U0HUMAN", "recent"),
        FakeMessage::new(&old_message, "U0HUMAN", "old"),
    ]);
    // Created two days ago, so with a one-day skew allowance the creation floor
    // (three days back) is DEEPER than the six-day horizon: creation wins.
    state.channels[0] =
        FakeChannel::public(CHANNEL, "engineering").created_at(support::PINNED_NOW_SECS - 2 * DAY);
    let mut harness = Harness::start(state).await;
    harness.config.sweep.window_secs = DAY as u64;
    harness.config.backfill.horizon = BackfillHorizon::Days { days: 6 };
    harness.config.backfill.windows_per_cycle = 8;

    let outcome = harness.cycle(0).await;

    // Three windows: [now-1d-90s..now-90s], [now-2d-90s..now-1d-90s], and the
    // last clamped to [creation-1d..now-2d-90s], which holds the old message.
    assert_eq!(outcome.backfill_windows, 3);
    assert_eq!(outcome.channels_backfilling, 0);
    assert_eq!(
        outcome.oldest_backfilled_to.as_deref(),
        Some(expected_floor_ts(3).as_str())
    );
    assert!(harness.sink.record(CHANNEL, &old_message).is_some());
    let journal = Journal::load(&harness.journal_path);
    let channel = journal
        .channel(CHANNEL)
        .expect("enrolled channels are journaled");
    assert!(channel.backfill_complete);
    assert_eq!(
        channel.created_epoch_secs,
        Some(support::PINNED_NOW_SECS - 2 * DAY)
    );

    // The finished lane is skipped, not re-walked: same state, no windows.
    let outcome = harness.cycle(0).await;
    assert_eq!(outcome.backfill_windows, 0);
    assert_eq!(outcome.channels_backfilling, 0);
    assert_eq!(
        outcome.oldest_backfilled_to.as_deref(),
        Some(expected_floor_ts(3).as_str())
    );
}

/// A one-window backfill budget must report the lane as IN PROGRESS while it
/// is still walking: `channels_backfilling == 1` on every cycle before the
/// floor, and 0 only on the cycle that completes the walk. That distinction is
/// the whole point of the counter: "deep horizon, healthy progress" must read
/// differently from "no backfill at all".
#[tokio::test]
async fn a_one_window_backfill_budget_reports_the_lane_in_progress_until_it_finishes() {
    let mut harness = Harness::start(workspace(vec![FakeMessage::new(
        &ts_before(90, 1),
        "U0HUMAN",
        "recent",
    )]))
    .await;
    harness.config.sweep.window_secs = DAY as u64;
    harness.config.backfill.horizon = BackfillHorizon::Days { days: 4 };
    harness.config.backfill.windows_per_cycle = 1;

    for cycle in 1..=3 {
        let outcome = harness.cycle(0).await;
        assert_eq!(
            outcome.backfill_windows, 1,
            "cycle {cycle}: the budget is one window per cycle"
        );
        assert_eq!(
            outcome.channels_backfilling, 1,
            "cycle {cycle}: the lane is mid-walk, not missing"
        );
    }

    let done = harness.cycle(0).await;
    assert_eq!(done.backfill_windows, 1);
    assert_eq!(
        done.channels_backfilling, 0,
        "the cycle that completes the walk reports the lane finished"
    );
    assert_eq!(
        done.oldest_backfilled_to.as_deref(),
        Some(expected_floor_ts(4).as_str())
    );
}

#[tokio::test]
async fn a_deeper_horizon_rearms_a_completed_backfill_lane() {
    // No `created` on the fixture channel, which must impose NO bound: the
    // first cycle walks the whole four-day horizon rather than declaring
    // itself finished for want of a roster field.
    let mut harness = Harness::start(workspace(vec![FakeMessage::new(
        &ts_before(90, 1),
        "U0HUMAN",
        "recent",
    )]))
    .await;
    harness.config.sweep.window_secs = DAY as u64;
    harness.config.backfill.horizon = BackfillHorizon::Days { days: 4 };
    harness.config.backfill.windows_per_cycle = 8;

    let first = harness.cycle(0).await;
    assert_eq!(first.backfill_windows, 4);
    assert_eq!(first.channels_backfilling, 0);
    assert_eq!(
        first.oldest_backfilled_to.as_deref(),
        Some(expected_floor_ts(4).as_str())
    );

    // The horizon now reaches further back than the floor the completion was
    // earned against, so the lane re-arms and walks only the difference.
    harness.config.backfill.horizon = BackfillHorizon::Days { days: 6 };
    let second = harness.cycle(0).await;
    assert_eq!(second.backfill_windows, 2);
    assert_eq!(second.channels_backfilling, 0);
    assert_eq!(
        second.oldest_backfilled_to.as_deref(),
        Some(expected_floor_ts(6).as_str())
    );
}

#[tokio::test]
async fn an_earlier_created_rearms_a_lane_and_an_absent_created_does_not() {
    let mut state = workspace(vec![FakeMessage::new(
        &ts_before(90, 1),
        "U0HUMAN",
        "recent",
    )]);
    state.channels[0] =
        FakeChannel::public(CHANNEL, "engineering").created_at(support::PINNED_NOW_SECS - 2 * DAY);
    let mut harness = Harness::start(state).await;
    harness.config.sweep.window_secs = DAY as u64;
    harness.config.backfill.horizon = BackfillHorizon::Days { days: 6 };
    harness.config.backfill.windows_per_cycle = 8;

    let first = harness.cycle(0).await;
    assert_eq!(first.backfill_windows, 3);
    assert_eq!(first.channels_backfilling, 0);

    // The roster now reports an EARLIER creation, moving the floor under the
    // recorded completion: the lane re-arms and walks to the new floor.
    harness.slack.with(|state| {
        state.channels[0].created = Some(support::PINNED_NOW_SECS - 5 * DAY);
    });
    let second = harness.cycle(0).await;
    assert_eq!(second.backfill_windows, 3);
    assert_eq!(
        second.oldest_backfilled_to.as_deref(),
        Some(expected_floor_ts(6).as_str())
    );

    // A roster read that omits `created` entirely is sticky-none: the journal
    // keeps the last known creation and the finished lane stays skipped.
    harness.slack.with(|state| {
        state.channels[0].created = None;
    });
    let third = harness.cycle(0).await;
    assert_eq!(third.backfill_windows, 0);
    assert_eq!(third.channels_backfilling, 0);
}
