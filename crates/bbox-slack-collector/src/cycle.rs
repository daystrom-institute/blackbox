//! One publication cycle: identify, enroll, sweep, thread, reconcile, report.
//!
//! Written against [`ConversationSink`] and [`crate::slack::SlackRead`] rather
//! than against HTTP directly, for the reason the file lane states and this
//! lane inherits: the interesting properties of a cycle are properties of the
//! DECISIONS, not of the transport.
//!
//! # The four properties this loop exists to hold
//!
//! **A message lands exactly once.** The server owns the cursor, the sweep
//! resumes from `max(server watermark, local mark)`, both window bounds are
//! inclusive, and the caller filters the resume mark itself. Inclusive bounds
//! with a caller-side filter is the only arrangement without a boundary hole:
//! Slack's `inclusive` flag covers `oldest` and `latest` together, so exclusive
//! bounds drop a message whose `ts` falls exactly on a window edge.
//!
//! **A cursor never advances over a hole.** `conversations.history` pages
//! NEWEST first, so a sweep that exhausted its page budget holds the newest part
//! of its window rather than a contiguous run from the watermark. An incomplete
//! window is therefore DISCARDED, not landed, and the mark stays where it was.
//! This is the single most important line in the file: landing a partial window
//! would advance the watermark past messages nothing would ever come back for.
//!
//! **Thread replies land even when nothing announces them.** Unbroadcast
//! replies never appear in a channel sweep (design 5.3). The cheap
//! latest-reply test only helps while the parent is still inside a window being
//! re-read, so known threads also come around on a bounded ROTATION. A thread
//! sweep that finds nothing is cheap; a thread body that never lands is the
//! failure that makes an ingestion lane look correct and not be.
//!
//! **Marks advance only after the corpus acknowledged the batch.** Every
//! journal write in this file happens after a receipt. A crash between sweep and
//! post costs a resweep, which the store reports as duplicates; a crash after
//! the post costs a resweep too. Neither costs a message.
//!
//! # Lane priority
//!
//! Design 5.5 fixes it as steady state, then reconciliation, then backfill, and
//! this loop expresses it two ways: the ORDER of the passes within a channel,
//! and separate budgets per lane, with backfill's deliberately the smallest.
//! Every request in every lane crosses the same pacer, so a saturated backfill
//! cannot outrun steady state, it can only wait behind it.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use anyhow::{Context, Result, anyhow, bail};
use async_trait::async_trait;
use bbox_conversation_source::{
    CATALOG_ONBOARD_SCHEMA_VERSION, CONVERSATION_POLICY_VERSION, ChannelCursorV1,
    ChannelObservationV1, ChannelRosterReceiptV1, ChannelRosterRequestV1,
    ConversationBatchReceiptV1, ConversationBatchV1, ConversationCatalogOnboardRequestV1,
    ConversationCatalogOnboardResponseV1, ConversationCursorsResponseV1,
    ConversationMessageRecordV1, ConversationRevisionRecordV1, ConversationRevisionV1,
    ConversationRevisionsReceiptV1, ConversationRevisionsRequestV1, ConversationStatusResponseV1,
    ConversationTombstoneRecordV1, MAX_BATCH_RECORDS, MAX_INFLIGHT_THREAD_PARENTS,
    MAX_REVISIONS_PER_REQUEST, SCHEMA_VERSION, batch_digest, message_ts_is_after,
    message_ts_order_key,
};
use bbox_corpus_core::project_catalog::ConnectorScope;
use chrono::{DateTime, SecondsFormat, Utc};

use crate::config::{BackfillHorizon, SatelliteConfig};
use crate::journal::Journal;
use crate::normalize::{edit_stamp, normalize};
use crate::policy::{ChannelDecision, CompiledChannelPolicy, SkipCounters};
use crate::slack::{
    ChannelListRequest, HistoryRequest, RawMessage, RepliesRequest, SlackIdentity, SlackRead,
};

/// The corpus side of one publication, as the cycle needs it.
///
/// Mirrors the `/internal/conversation-source/v1/*` verbs one for one. Nothing
/// here knows about bearers, retries, or URLs; the HTTP implementation owns
/// those.
#[async_trait]
pub trait ConversationSink: Send + Sync {
    async fn onboard(
        &self,
        request: &ConversationCatalogOnboardRequestV1,
    ) -> Result<ConversationCatalogOnboardResponseV1>;
    async fn post_channels(
        &self,
        request: &ChannelRosterRequestV1,
    ) -> Result<ChannelRosterReceiptV1>;
    async fn cursors(&self, scope: &ConnectorScope) -> Result<ConversationCursorsResponseV1>;
    async fn post_batch(&self, batch: &ConversationBatchV1) -> Result<ConversationBatchReceiptV1>;
    async fn post_revisions(
        &self,
        request: &ConversationRevisionsRequestV1,
    ) -> Result<ConversationRevisionsReceiptV1>;
    async fn status(&self, scope: &ConnectorScope) -> Result<ConversationStatusResponseV1>;
}

/// What one cycle did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CycleOutcome {
    pub workspace_id: String,
    /// Every scope the shared credential carries. Under the one-app posture the
    /// collector cannot refuse a write scope, so it RECORDS the whole grant:
    /// this is the honest substitute for an assertion this posture cannot make.
    pub granted_scopes: Vec<String>,
    /// The write subset of the above, surfaced separately because it is the
    /// number an operator watches to know what one-app actually costs.
    pub write_scopes: Vec<String>,
    pub channels_enrolled: u64,
    pub channels_skipped: BTreeMap<String, u64>,
    /// Records the corpus accepted this cycle.
    pub messages_landed: u64,
    /// Records the corpus already held. Ordinary on a resume; a standing
    /// nonzero value in steady state means a mark is not advancing.
    pub duplicates: u64,
    pub thread_replies_landed: u64,
    pub threads_swept: u64,
    pub revisions_emitted: u64,
    pub tombstones_emitted: u64,
    /// Windows discarded because their enumeration did not complete. Nonzero
    /// means a channel is busier than one window's page budget, and the fix is
    /// a smaller `sweep.window_secs`, not a bigger page budget.
    pub windows_deferred: u64,
    pub backfill_windows: u64,
    pub normalization_skips: BTreeMap<String, u64>,
    /// Whether the reconciliation pass ran this cycle (it runs on a slower
    /// cadence than steady state).
    pub reconciled: bool,
    /// The worst per-channel lag the corpus reported, which is where a
    /// throttled producer shows up (design 5.5).
    pub max_lag_seconds: Option<i64>,
}

/// The read scopes the collector cannot run without.
///
/// Under the one-app posture a MISSING read scope is the only scope failure the
/// collector can refuse, and refusing it matters: without it every sweep
/// returns `not_in_channel` or an empty page and the deployment presents as a
/// healthy satellite over an empty corpus.
pub fn required_read_scopes(private_channels: bool) -> Vec<&'static str> {
    let mut scopes = vec!["channels:read", "channels:history"];
    if private_channels {
        scopes.extend(["groups:read", "groups:history"]);
    }
    scopes
}

/// Refuse a credential missing a read scope this deployment needs.
///
/// An EMPTY granted set is not treated as "missing everything". The scope list
/// arrives in a response header, and a proxy that strips it, or a fixture that
/// never sends it, would otherwise produce a confident refusal from an absence
/// of evidence. Unverifiable is reported and allowed; verifiably missing is
/// refused.
pub fn check_read_scopes(identity: &SlackIdentity, required: &[&str]) -> Result<()> {
    if identity.granted_scopes.is_empty() {
        tracing::warn!(
            "the workspace returned no granted-scope header; read scopes could not be verified"
        );
        return Ok(());
    }
    let missing: Vec<&str> = required
        .iter()
        .copied()
        .filter(|scope| !identity.has_scope(scope))
        .collect();
    if !missing.is_empty() {
        bail!(
            "the Slack token is missing required read scope(s): {}. Granted: {}",
            missing.join(", "),
            identity.granted_scopes.join(", ")
        );
    }
    Ok(())
}

/// Probe the workspace and present the facts the catalog onboarding needs.
///
/// The facts are PROBED, never asserted from config: the corpus host
/// revalidates kind and remote-authority agreement against the operator's
/// grant, and a config-asserted authority would defeat that check by
/// construction.
pub async fn run_onboarding(
    slack: &dyn SlackRead,
    sink: &dyn ConversationSink,
    config: &SatelliteConfig,
) -> Result<ConversationCatalogOnboardResponseV1> {
    let identity = slack.auth_test().await.context("probing the workspace")?;
    verify_workspace(config, &identity)?;
    check_read_scopes(
        &identity,
        &required_read_scopes(config.channels.private_channels),
    )?;
    let request = ConversationCatalogOnboardRequestV1 {
        schema_version: CATALOG_ONBOARD_SCHEMA_VERSION,
        scope: config.scope()?,
        probed_connector_kind: config.connector_kind.clone(),
        probed_remote_authority: identity
            .workspace_domain
            .clone()
            .unwrap_or_else(|| identity.workspace_id.clone()),
        probed_workspace_id: identity.workspace_id.clone(),
        probed_workspace_display_name: identity.workspace_name.clone(),
        display_name: config.display_name(),
    };
    request
        .validate()
        .map_err(|error| anyhow!("refusing to present an invalid onboard request: {error}"))?;
    sink.onboard(&request).await
}

/// Run one full publication cycle.
///
/// `now` is supplied rather than read from the clock inside so a test can pin
/// it; every window boundary in the cycle derives from it.
pub async fn run_publication_cycle(
    slack: &dyn SlackRead,
    sink: &dyn ConversationSink,
    config: &SatelliteConfig,
    journal_path: &Path,
    now: DateTime<Utc>,
) -> Result<CycleOutcome> {
    let scope = config.scope()?;
    let observed_at = now.to_rfc3339_opts(SecondsFormat::Secs, true);
    let now_secs = now.timestamp();
    let now_ts = ts_from_secs(now_secs);

    // -- identity, and the one scope refusal this posture supports -----------
    let identity = slack.auth_test().await.context("probing the workspace")?;
    verify_workspace(config, &identity)?;
    check_read_scopes(
        &identity,
        &required_read_scopes(config.channels.private_channels),
    )?;
    let workspace_id = identity.workspace_id.clone();

    let mut outcome = CycleOutcome {
        workspace_id: workspace_id.clone(),
        granted_scopes: identity.granted_scopes.clone(),
        write_scopes: identity.write_scopes(),
        ..CycleOutcome::default()
    };

    // -- enrollment ----------------------------------------------------------
    let policy =
        CompiledChannelPolicy::compile(&config.channels).context("compiling the channel policy")?;
    let roster = slack
        .list_channels(&ChannelListRequest {
            include_private: policy.include_private(),
            exclude_archived: !policy.include_archived(),
            page_limit: config.sweep.page_limit,
            max_pages: config.sweep.max_roster_pages,
        })
        .await
        .context("reading the channel roster")?;

    let mut skips = SkipCounters::default();
    let mut enrolled: BTreeMap<String, ChannelObservationV1> = BTreeMap::new();
    for channel in &roster {
        match policy.decide(channel) {
            ChannelDecision::Skip { reason } => skips.record(reason),
            ChannelDecision::Enroll { class } => {
                enrolled.insert(
                    channel.id.clone(),
                    ChannelObservationV1 {
                        channel_id: channel.id.clone(),
                        observed_name: channel
                            .name
                            .as_ref()
                            .map(|name| name.trim_start_matches('#').to_string()),
                        class,
                        // Under the ruled one-app posture membership IS the
                        // visibility bound, and the policy already refused
                        // every non-member, so this is always true here. It is
                        // sent explicitly rather than defaulted because the
                        // corpus host enforces class and visibility itself and
                        // must not have to infer either.
                        is_member: true,
                        observed_at: observed_at.clone(),
                    },
                );
            }
        }
    }
    outcome.channels_enrolled = enrolled.len() as u64;
    outcome.channels_skipped = skips.into_map();

    let roster_request = ChannelRosterRequestV1 {
        schema_version: SCHEMA_VERSION,
        conversation_policy_version: CONVERSATION_POLICY_VERSION.to_string(),
        scope: scope.clone(),
        workspace_id: workspace_id.clone(),
        // BTreeMap iteration gives the channel-id ordering the wire's roster
        // validation requires, and it deduplicates by construction.
        channels: enrolled.values().cloned().collect(),
    };
    roster_request
        .validate()
        .map_err(|error| anyhow!("refusing to publish an invalid roster: {error}"))?;
    sink.post_channels(&roster_request)
        .await
        .context("publishing the channel roster")?;

    // -- the server's cursors, which are the resume authority ----------------
    let cursors = sink
        .cursors(&scope)
        .await
        .context("reading the corpus cursors")?;
    let cursors: BTreeMap<String, ChannelCursorV1> = cursors
        .channels
        .into_iter()
        .map(|cursor| (cursor.channel_id.clone(), cursor))
        .collect();

    let mut journal = Journal::load(journal_path);
    journal.cycles = journal.cycles.saturating_add(1);
    let cycle_number = journal.cycles;
    let reconcile_now = config.reconciliation.enabled
        && cycle_number.is_multiple_of(u64::from(config.reconciliation.every_cycles.max(1)));
    outcome.reconciled = reconcile_now;

    for (channel_id, observation) in &enrolled {
        let cursor = cursors.get(channel_id).cloned().unwrap_or_default();
        let context = ChannelContext {
            channel_id,
            workspace_id: &workspace_id,
            scope: &scope,
            observed_at: &observed_at,
            now_secs,
            now_ts: &now_ts,
        };

        // Lane one: steady state.
        let steady = sweep_steady_state(slack, sink, config, &mut journal, &context, &cursor)
            .await
            .with_context(|| format!("sweeping channel {channel_id}"))?;
        outcome.absorb_sweep(&steady);

        // Lane one and a half: threads. Part of steady state rather than a
        // later phase, because an unbroadcast reply is not an enrichment of a
        // conversation, it IS the conversation (design 5.3).
        let threads = sweep_threads(
            slack,
            sink,
            config,
            &mut journal,
            &context,
            &cursor,
            &steady.discovered_parents,
            &steady.landed_ts,
        )
        .await
        .with_context(|| format!("sweeping threads in channel {channel_id}"))?;
        outcome.absorb_threads(&threads);

        // Lane two: reconciliation, on a slower cadence.
        if reconcile_now {
            let reconciled = reconcile(slack, sink, config, &mut journal, &context)
                .await
                .with_context(|| format!("reconciling channel {channel_id}"))?;
            outcome.revisions_emitted += reconciled.revisions;
            outcome.tombstones_emitted += reconciled.tombstones;
        }

        // Lane three: backfill, smallest budget, last.
        if config.backfill.enabled() {
            let backfilled = backfill(slack, sink, config, &mut journal, &context, &cursor)
                .await
                .with_context(|| format!("backfilling channel {channel_id}"))?;
            outcome.absorb_sweep(&backfilled);
            outcome.backfill_windows += backfilled.windows;
        }

        // Bound the journal, then publish it. Saving per channel rather than
        // once at the end means a crash costs one channel's redundant sweep
        // rather than the whole cycle's.
        {
            let window_start = ts_from_secs(now_secs - config.reconciliation.window_secs as i64);
            let channel = journal.channel_mut(channel_id);
            channel.prune_baseline(&window_start);
            channel.prune_threads(MAX_INFLIGHT_THREAD_PARENTS);
        }
        journal
            .save(journal_path)
            .context("publishing the producer journal")?;
        tracing::debug!(
            channel = context.channel_id,
            name = observation.observed_name.as_deref().unwrap_or("<unnamed>"),
            landed = steady.landed + threads.landed,
            threads_swept = threads.swept,
            "channel cycle complete"
        );
    }

    // -- report --------------------------------------------------------------
    let status = sink
        .status(&scope)
        .await
        .context("reading the corpus status")?;
    outcome.max_lag_seconds = status
        .channels
        .iter()
        .filter_map(|channel| channel.lag_seconds)
        .max();
    Ok(outcome)
}

/// The per-channel constants every lane needs.
struct ChannelContext<'a> {
    channel_id: &'a str,
    workspace_id: &'a str,
    scope: &'a ConnectorScope,
    observed_at: &'a str,
    now_secs: i64,
    now_ts: &'a str,
}

#[derive(Debug, Default)]
struct SweepResult {
    landed: u64,
    duplicates: u64,
    windows: u64,
    deferred: u64,
    skips: BTreeMap<String, u64>,
    /// Thread parents this sweep saw, mapped to their advertised latest reply.
    discovered_parents: BTreeMap<String, Option<String>>,
    /// Message timestamps landed this cycle, so a broadcast reply is not
    /// posted twice.
    landed_ts: BTreeSet<String>,
}

#[derive(Debug, Default)]
struct ThreadResult {
    landed: u64,
    duplicates: u64,
    swept: u64,
    skips: BTreeMap<String, u64>,
}

#[derive(Debug, Default)]
struct ReconcileResult {
    revisions: u64,
    tombstones: u64,
}

impl CycleOutcome {
    fn absorb_sweep(&mut self, sweep: &SweepResult) {
        self.messages_landed += sweep.landed;
        self.duplicates += sweep.duplicates;
        self.windows_deferred += sweep.deferred;
        merge_counts(&mut self.normalization_skips, &sweep.skips);
    }

    fn absorb_threads(&mut self, threads: &ThreadResult) {
        self.messages_landed += threads.landed;
        self.thread_replies_landed += threads.landed;
        self.duplicates += threads.duplicates;
        self.threads_swept += threads.swept;
        merge_counts(&mut self.normalization_skips, &threads.skips);
    }
}

fn merge_counts(into: &mut BTreeMap<String, u64>, from: &BTreeMap<String, u64>) {
    for (reason, count) in from {
        *into.entry(reason.clone()).or_default() += count;
    }
}

/// Lane one: walk forward from the resume mark in bounded windows.
async fn sweep_steady_state(
    slack: &dyn SlackRead,
    sink: &dyn ConversationSink,
    config: &SatelliteConfig,
    journal: &mut Journal,
    context: &ChannelContext<'_>,
    cursor: &ChannelCursorV1,
) -> Result<SweepResult> {
    let mut result = SweepResult::default();
    let window = config.sweep.window_secs.max(1) as i64;
    // A channel with no history anywhere starts one window back rather than at
    // the beginning of time. Reaching further back is BACKFILL, and backfill is
    // off unless an operator turned it on (design 5.1); a satellite that
    // silently swept all history on first run would be exactly the unbounded
    // rate-limited operation that ruling exists to prevent.
    let mut resume = journal
        .resume_from(context.channel_id, cursor.history_watermark.as_deref())
        .unwrap_or_else(|| ts_from_secs(context.now_secs - window));

    for _ in 0..config.sweep.windows_per_cycle.max(1) {
        let Some(start_secs) = secs_from_ts(&resume) else {
            bail!("cannot interpret resume mark {resume} as a timestamp");
        };
        if start_secs >= context.now_secs {
            break;
        }
        let end_secs = start_secs.saturating_add(window).min(context.now_secs);
        let end_ts = ts_from_secs(end_secs);

        let sweep = slack
            .history(&HistoryRequest {
                channel_id: context.channel_id.to_string(),
                oldest: Some(resume.clone()),
                latest: Some(end_ts.clone()),
                page_limit: config.sweep.page_limit,
                max_pages: config.sweep.max_pages_per_window,
            })
            .await?;
        if !sweep.complete {
            // THE line. A partial window holds the newest messages in the
            // range, not a contiguous run from the mark, so landing it would
            // advance the cursor over a hole nothing would ever return for.
            tracing::warn!(
                channel = context.channel_id,
                pages = sweep.pages,
                window_secs = window,
                "window enumeration did not complete; deferring it rather than landing a hole"
            );
            result.deferred += 1;
            break;
        }
        result.windows += 1;

        let mut counters = SkipCounters::default();
        let mut records: Vec<(ConversationMessageRecordV1, Option<String>, bool)> = Vec::new();
        for message in &sweep.messages {
            if message.ts.as_deref() == Some(resume.as_str()) {
                // The inclusive lower bound is the mark itself, which already
                // landed. Filtering it here is the other half of the
                // inclusive-bounds contract.
                continue;
            }
            if message.is_thread_parent()
                && let Some(ts) = &message.ts
            {
                result
                    .discovered_parents
                    .insert(ts.clone(), message.latest_reply.clone());
            }
            if let Some(record) = normalize(
                context.channel_id,
                message,
                context.observed_at,
                &mut counters,
            ) {
                let is_reply = record.thread_parent_ts.is_some();
                records.push((record, edit_stamp(message).map(str::to_string), is_reply));
            }
        }
        merge_counts(&mut result.skips, &counters.into_map());

        let posted = post_records(sink, context, records).await?;
        result.landed += posted.accepted;
        result.duplicates += posted.duplicates;

        // Marks advance only now, after the corpus acknowledged the batch.
        let channel = journal.channel_mut(context.channel_id);
        for landed in &posted.landed {
            channel.observe_landed(
                &landed.message_ts,
                landed.edited_ts.as_deref(),
                landed.is_reply,
            );
            result.landed_ts.insert(landed.message_ts.clone());
        }
        channel.advance_swept_through(&end_ts);
        resume = end_ts;
    }
    Ok(result)
}

/// Lane one and a half: per-parent reply sweeps.
#[allow(clippy::too_many_arguments)]
async fn sweep_threads(
    slack: &dyn SlackRead,
    sink: &dyn ConversationSink,
    config: &SatelliteConfig,
    journal: &mut Journal,
    context: &ChannelContext<'_>,
    cursor: &ChannelCursorV1,
    discovered: &BTreeMap<String, Option<String>>,
    already_landed: &BTreeSet<String>,
) -> Result<ThreadResult> {
    let mut result = ThreadResult::default();
    let mut candidates: BTreeMap<String, Option<String>> = discovered.clone();
    for parent in &cursor.inflight_thread_parents {
        // A parent the CORPUS knows about but this cycle's window did not
        // mention. This is what lets a restarted satellite resume a thread
        // sweep with no producer-side durable state at all.
        candidates.entry(parent.clone()).or_insert(None);
    }
    if candidates.is_empty() {
        return Ok(result);
    }

    let cycle = journal.cycles;
    let channel_journal = journal
        .channel(context.channel_id)
        .cloned()
        .unwrap_or_default();
    // Priority: threads with a real signal first (unknown, or an advertised
    // reply newer than what was swept), then the ROTATION by staleness. The
    // rotation is what keeps an old thread's unbroadcast reply from being
    // invisible forever, since nothing advertises it once the parent leaves the
    // sweep window.
    let rotation = u64::from(config.sweep.thread_rotation_cycles.max(1));
    let mut ordered: Vec<(u8, u64, String, Option<String>)> = candidates
        .into_iter()
        .filter_map(|(parent_ts, latest_reply)| {
            let signaled = channel_journal.thread_needs_sweep(&parent_ts, latest_reply.as_deref());
            let last_swept = channel_journal
                .thread(&parent_ts)
                .map(|mark| mark.last_swept_cycle)
                .unwrap_or(0);
            // An idle thread costs NOTHING until the rotation comes around,
            // which is the cheap test design 5.3 asks for. The rotation is what
            // keeps "cheap" from becoming "never": once a parent ages out of
            // the sweep window nothing advertises its new replies at all.
            let due = cycle.saturating_sub(last_swept) >= rotation;
            (signaled || due).then_some((u8::from(!signaled), last_swept, parent_ts, latest_reply))
        })
        .collect();
    ordered.sort_by(|left, right| {
        left.0
            .cmp(&right.0)
            .then(left.1.cmp(&right.1))
            .then(right.2.cmp(&left.2))
    });
    ordered.truncate(config.sweep.thread_parents_per_cycle.max(1) as usize);

    for (_, _, parent_ts, latest_reply) in ordered {
        let mark_from = channel_journal
            .thread(&parent_ts)
            .and_then(|mark| mark.swept_through_ts.clone())
            .unwrap_or_else(|| parent_ts.clone());
        let sweep = slack
            .replies(&RepliesRequest {
                channel_id: context.channel_id.to_string(),
                parent_ts: parent_ts.clone(),
                oldest: Some(mark_from.clone()),
                page_limit: config.sweep.page_limit,
                max_pages: config.sweep.max_pages_per_thread,
            })
            .await?;
        result.swept += 1;
        if !sweep.complete {
            tracing::warn!(
                channel = context.channel_id,
                parent = parent_ts,
                "thread enumeration did not complete; leaving its mark for the next cycle"
            );
            continue;
        }

        let mut counters = SkipCounters::default();
        let mut records: Vec<(ConversationMessageRecordV1, Option<String>, bool)> = Vec::new();
        let mut newest_reply: Option<String> = None;
        for message in &sweep.messages {
            let Some(ts) = message.ts.clone() else {
                continue;
            };
            // The parent itself comes back with every replies page, and the
            // inclusive lower bound returns the last swept reply again.
            if ts == parent_ts || ts == mark_from {
                continue;
            }
            if newest_reply
                .as_deref()
                .is_none_or(|current| message_ts_is_after(&ts, current))
            {
                newest_reply = Some(ts.clone());
            }
            if already_landed.contains(&ts) {
                // A broadcast reply already landed through the channel sweep
                // this cycle. Posting it again would be harmless and would make
                // every receipt report duplicates.
                continue;
            }
            if let Some(record) = normalize(
                context.channel_id,
                message,
                context.observed_at,
                &mut counters,
            ) {
                records.push((record, edit_stamp(message).map(str::to_string), true));
            }
        }
        merge_counts(&mut result.skips, &counters.into_map());

        let posted = post_records(sink, context, records).await?;
        result.landed += posted.accepted;
        result.duplicates += posted.duplicates;

        let channel = journal.channel_mut(context.channel_id);
        for landed in &posted.landed {
            channel.observe_landed(&landed.message_ts, landed.edited_ts.as_deref(), true);
        }
        let mark = channel.thread_mut(&parent_ts);
        if let Some(newest) = newest_reply.clone() {
            let advance = mark
                .swept_through_ts
                .as_deref()
                .is_none_or(|current| message_ts_is_after(&newest, current));
            if advance {
                mark.swept_through_ts = Some(newest);
            }
        } else if mark.swept_through_ts.is_none() {
            mark.swept_through_ts = Some(parent_ts.clone());
        }
        // Record what the channel advertised, so an idle thread costs nothing
        // until something moves. Falling back to the newest reply keeps the
        // cheap test meaningful for parents the channel sweep never mentioned.
        mark.latest_reply_seen = latest_reply
            .clone()
            .or_else(|| mark.swept_through_ts.clone());
        mark.last_swept_cycle = cycle;
    }
    Ok(result)
}

/// Lane two: the trailing reconciliation window (design 5.4).
///
/// Two honest limits, both stated rather than papered over:
///
/// - **Deletion detection is window-bounded.** A message deleted outside the
///   window is not detected and the corpus may retain text the workspace no
///   longer shows.
/// - **It covers channel-visible messages only.** A history resweep does not
///   return unbroadcast thread replies, so reply baselines are excluded from the
///   absence diff. Tombstoning them on absence would delete every thread reply
///   in the window on the first reconciliation pass, which is a far worse
///   failure than not detecting a reply deletion.
async fn reconcile(
    slack: &dyn SlackRead,
    sink: &dyn ConversationSink,
    config: &SatelliteConfig,
    journal: &mut Journal,
    context: &ChannelContext<'_>,
) -> Result<ReconcileResult> {
    let mut result = ReconcileResult::default();
    let window_start = ts_from_secs(context.now_secs - config.reconciliation.window_secs as i64);
    let baseline = journal
        .channel(context.channel_id)
        .map(|channel| channel.baseline.clone())
        .unwrap_or_default();
    if baseline.is_empty() {
        return Ok(result);
    }

    let sweep = slack
        .history(&HistoryRequest {
            channel_id: context.channel_id.to_string(),
            oldest: Some(window_start.clone()),
            latest: Some(context.now_ts.to_string()),
            page_limit: config.sweep.page_limit,
            max_pages: config.sweep.max_pages_per_window,
        })
        .await?;
    if !sweep.complete {
        // An incomplete resweep would read as mass deletion. Refusing to diff
        // it is the only safe response.
        tracing::warn!(
            channel = context.channel_id,
            "reconciliation resweep did not complete; skipping the diff this cycle"
        );
        return Ok(result);
    }

    let observed: BTreeMap<String, &RawMessage> = sweep
        .messages
        .iter()
        .filter_map(|message| message.ts.clone().map(|ts| (ts, message)))
        .collect();

    let mut revisions: Vec<ConversationRevisionV1> = Vec::new();
    let mut applied: Vec<(String, u64, Option<String>)> = Vec::new();
    let mut removed: Vec<String> = Vec::new();

    for (message_ts, entry) in &baseline {
        if message_ts_is_after(&window_start, message_ts) {
            continue;
        }
        match observed.get(message_ts) {
            Some(message) => {
                let stamp = edit_stamp(message).map(str::to_string);
                if stamp != entry.edited_ts {
                    let revision = entry.revision.saturating_add(1);
                    revisions.push(ConversationRevisionV1::Edit(ConversationRevisionRecordV1 {
                        channel_id: context.channel_id.to_string(),
                        message_ts: message_ts.clone(),
                        revision,
                        text: message.text.clone(),
                        edited_ts: stamp.clone(),
                        observed_at: context.observed_at.to_string(),
                    }));
                    applied.push((message_ts.clone(), revision, stamp));
                    result.revisions += 1;
                }
            }
            None if entry.thread_reply => {
                // Invisible to this read by construction, not deleted.
            }
            None => {
                let revision = entry.revision.saturating_add(1);
                revisions.push(ConversationRevisionV1::Tombstone(
                    ConversationTombstoneRecordV1 {
                        channel_id: context.channel_id.to_string(),
                        message_ts: message_ts.clone(),
                        revision,
                        observed_at: context.observed_at.to_string(),
                    },
                ));
                removed.push(message_ts.clone());
                result.tombstones += 1;
            }
        }
    }

    // Reseed baselines for messages this producer never recorded (the
    // journal-loss case). No revision is emitted: an unknown edit history is
    // not an observed edit, and claiming one would rewrite a landed record from
    // a guess.
    let mut reseed: Vec<(String, Option<String>)> = Vec::new();
    for (message_ts, message) in &observed {
        if !baseline.contains_key(message_ts) && !message_ts_is_after(&window_start, message_ts) {
            reseed.push((message_ts.clone(), edit_stamp(message).map(str::to_string)));
        }
    }

    if revisions.is_empty() && reseed.is_empty() {
        return Ok(result);
    }

    for chunk in revisions.chunks(MAX_REVISIONS_PER_REQUEST) {
        let request = ConversationRevisionsRequestV1 {
            schema_version: SCHEMA_VERSION,
            conversation_policy_version: CONVERSATION_POLICY_VERSION.to_string(),
            scope: context.scope.clone(),
            workspace_id: context.workspace_id.to_string(),
            channel_id: context.channel_id.to_string(),
            revisions: chunk.to_vec(),
            observed_at: context.observed_at.to_string(),
        };
        request
            .validate()
            .map_err(|error| anyhow!("refusing to publish invalid revisions: {error}"))?;
        sink.post_revisions(&request)
            .await
            .context("publishing revisions")?;
    }

    let channel = journal.channel_mut(context.channel_id);
    for (message_ts, revision, stamp) in applied {
        if let Some(entry) = channel.baseline.get_mut(&message_ts) {
            entry.revision = revision;
            entry.edited_ts = stamp;
        }
    }
    for message_ts in removed {
        // Dropped so the tombstone is emitted once rather than every pass.
        channel.baseline.remove(&message_ts);
    }
    for (message_ts, stamp) in reseed {
        channel.observe_landed(&message_ts, stamp.as_deref(), false);
    }
    Ok(result)
}

/// Lane three: bounded backward walk toward the operator's horizon.
async fn backfill(
    slack: &dyn SlackRead,
    sink: &dyn ConversationSink,
    config: &SatelliteConfig,
    journal: &mut Journal,
    context: &ChannelContext<'_>,
    cursor: &ChannelCursorV1,
) -> Result<SweepResult> {
    let mut result = SweepResult::default();
    let Some(floor_secs) = horizon_floor_secs(&config.backfill.horizon, context.now_secs) else {
        return Ok(result);
    };
    let window = config.sweep.window_secs.max(1) as i64;

    // Walk down from whichever is older: the corpus's oldest landed record or
    // this producer's own backfill mark. The mark exists because an EMPTY old
    // window never moves the corpus's oldest-observed, and without it the same
    // empty stretch would be re-walked every cycle forever.
    let mut end_ts = match (
        cursor.oldest_observed.clone(),
        journal
            .channel(context.channel_id)
            .and_then(|channel| channel.backfilled_to_ts.clone()),
    ) {
        (Some(server), Some(local)) => {
            if message_ts_is_after(&server, &local) {
                local
            } else {
                server
            }
        }
        (Some(server), None) => server,
        (None, Some(local)) => local,
        (None, None) => context.now_ts.to_string(),
    };

    for _ in 0..config.backfill.windows_per_cycle.max(1) {
        let Some(end_secs) = secs_from_ts(&end_ts) else {
            break;
        };
        if end_secs <= floor_secs {
            break;
        }
        let start_secs = end_secs.saturating_sub(window).max(floor_secs);
        let start_ts = ts_from_secs(start_secs);
        let sweep = slack
            .history(&HistoryRequest {
                channel_id: context.channel_id.to_string(),
                oldest: Some(start_ts.clone()),
                latest: Some(end_ts.clone()),
                page_limit: config.sweep.page_limit,
                max_pages: config.sweep.max_pages_per_window,
            })
            .await?;
        if !sweep.complete {
            result.deferred += 1;
            break;
        }
        result.windows += 1;

        let mut counters = SkipCounters::default();
        let mut records: Vec<(ConversationMessageRecordV1, Option<String>, bool)> = Vec::new();
        for message in &sweep.messages {
            if message.ts.as_deref() == Some(end_ts.as_str()) {
                // The inclusive upper bound is the oldest record already
                // landed.
                continue;
            }
            if message.is_thread_parent()
                && let Some(ts) = &message.ts
            {
                result
                    .discovered_parents
                    .insert(ts.clone(), message.latest_reply.clone());
            }
            if let Some(record) = normalize(
                context.channel_id,
                message,
                context.observed_at,
                &mut counters,
            ) {
                let is_reply = record.thread_parent_ts.is_some();
                records.push((record, edit_stamp(message).map(str::to_string), is_reply));
            }
        }
        merge_counts(&mut result.skips, &counters.into_map());

        let posted = post_records(sink, context, records).await?;
        result.landed += posted.accepted;
        result.duplicates += posted.duplicates;
        journal
            .channel_mut(context.channel_id)
            .lower_backfill_mark(&start_ts);
        end_ts = start_ts;
    }
    Ok(result)
}

/// What one message landed, for the journal's baseline.
struct LandedRecord {
    message_ts: String,
    edited_ts: Option<String>,
    is_reply: bool,
}

#[derive(Default)]
struct PostOutcome {
    accepted: u64,
    duplicates: u64,
    landed: Vec<LandedRecord>,
}

/// Order, deduplicate, chunk, digest, validate, and post.
///
/// The wire requires strict `message_ts` ordering within a batch and one channel
/// per batch, and it verifies the digest. Every one of those is enforced here
/// before the request leaves, so an invalid batch is a producer bug caught
/// producer-side rather than a 422 an operator has to decode.
async fn post_records(
    sink: &dyn ConversationSink,
    context: &ChannelContext<'_>,
    records: Vec<(ConversationMessageRecordV1, Option<String>, bool)>,
) -> Result<PostOutcome> {
    let mut outcome = PostOutcome::default();
    if records.is_empty() {
        // An empty sweep posts NOTHING. The wire accepts an empty batch, but
        // sending one per idle channel per cycle would turn "no news" into
        // write traffic and make the receipt log unreadable.
        return Ok(outcome);
    }

    let mut ordered: BTreeMap<String, (ConversationMessageRecordV1, Option<String>, bool)> =
        BTreeMap::new();
    for (record, edited_ts, is_reply) in records {
        let Some(key) = message_ts_order_key(&record.message_ts) else {
            continue;
        };
        ordered.insert(key, (record, edited_ts, is_reply));
    }
    let ordered: Vec<(ConversationMessageRecordV1, Option<String>, bool)> =
        ordered.into_values().collect();

    for chunk in ordered.chunks(MAX_BATCH_RECORDS) {
        let batch_records: Vec<ConversationMessageRecordV1> =
            chunk.iter().map(|(record, _, _)| record.clone()).collect();
        let batch = ConversationBatchV1 {
            schema_version: SCHEMA_VERSION,
            conversation_policy_version: CONVERSATION_POLICY_VERSION.to_string(),
            scope: context.scope.clone(),
            workspace_id: context.workspace_id.to_string(),
            channel_id: context.channel_id.to_string(),
            batch_digest: batch_digest(&batch_records),
            records: batch_records,
            observed_at: context.observed_at.to_string(),
        };
        batch
            .validate()
            .map_err(|error| anyhow!("refusing to publish an invalid batch: {error}"))?;
        let receipt = sink
            .post_batch(&batch)
            .await
            .context("publishing a batch")?;
        outcome.accepted += receipt.accepted;
        outcome.duplicates += receipt.duplicates;
        for (record, edited_ts, is_reply) in chunk {
            outcome.landed.push(LandedRecord {
                message_ts: record.message_ts.clone(),
                edited_ts: edited_ts.clone(),
                is_reply: *is_reply,
            });
        }
    }
    Ok(outcome)
}

/// Refuse a credential that reached a different workspace than configured.
///
/// A satellite pointed at the wrong workspace by a swapped credential would
/// land another workspace's messages under this scope's identity, and every
/// record it wrote would be durable and wrong.
fn verify_workspace(config: &SatelliteConfig, identity: &SlackIdentity) -> Result<()> {
    let Some(expected) = &config.expected_workspace_id else {
        return Ok(());
    };
    if expected != &identity.workspace_id {
        bail!(
            "the credential reached workspace {} but this satellite is configured for {expected}",
            identity.workspace_id
        );
    }
    Ok(())
}

/// The oldest second the backfill lane may reach, or `None` when it is off.
fn horizon_floor_secs(horizon: &BackfillHorizon, now_secs: i64) -> Option<i64> {
    match horizon {
        BackfillHorizon::Off => None,
        BackfillHorizon::Days { days } => Some(now_secs - i64::from(*days) * 86_400),
        BackfillHorizon::Since { ts } => secs_from_ts(ts),
        BackfillHorizon::All => Some(0),
    }
}

/// A provider-shaped timestamp for a whole second.
pub fn ts_from_secs(secs: i64) -> String {
    format!("{}.000000", secs.max(0))
}

/// The whole-second part of a provider timestamp.
pub fn secs_from_ts(ts: &str) -> Option<i64> {
    ts.split('.').next()?.parse::<i64>().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn identity(scopes: &[&str]) -> SlackIdentity {
        SlackIdentity {
            workspace_id: "T0FIXTURE".into(),
            workspace_domain: Some("fixture.slack.com".into()),
            workspace_name: Some("Fixture".into()),
            bot_user_id: Some("U0BOT".into()),
            bot_id: Some("B0BOT".into()),
            granted_scopes: scopes.iter().map(|scope| scope.to_string()).collect(),
        }
    }

    #[test]
    fn a_missing_read_scope_is_refused() {
        let granted = identity(&["channels:read", "chat:write"]);
        let error = check_read_scopes(&granted, &required_read_scopes(false))
            .unwrap_err()
            .to_string();
        assert!(error.contains("channels:history"), "{error}");
    }

    #[test]
    fn a_write_scope_is_never_a_reason_to_refuse() {
        // The one-app posture in one assertion: the collector reads with the
        // interactive bot's token, so write scopes are present and are NOT
        // grounds for refusal. They are recorded instead.
        let granted = identity(&[
            "channels:history",
            "channels:read",
            "chat:write",
            "reactions:write",
        ]);
        check_read_scopes(&granted, &required_read_scopes(false)).unwrap();
        assert_eq!(
            granted.write_scopes(),
            vec!["chat:write".to_string(), "reactions:write".to_string()]
        );
    }

    #[test]
    fn an_unverifiable_scope_list_is_reported_rather_than_refused() {
        let granted = identity(&[]);
        check_read_scopes(&granted, &required_read_scopes(true)).unwrap();
    }

    #[test]
    fn enabling_private_channels_widens_the_required_read_scopes() {
        assert_eq!(
            required_read_scopes(true),
            vec![
                "channels:read",
                "channels:history",
                "groups:read",
                "groups:history"
            ]
        );
    }

    #[test]
    fn timestamps_round_trip_through_whole_seconds() {
        assert_eq!(ts_from_secs(1_755_000_000), "1755000000.000000");
        assert_eq!(secs_from_ts("1755000000.123456"), Some(1_755_000_000));
        assert_eq!(secs_from_ts("garbage"), None);
    }

    #[test]
    fn the_backfill_horizon_decides_how_far_back_the_lane_may_reach() {
        let now = 1_755_000_000;
        assert_eq!(horizon_floor_secs(&BackfillHorizon::Off, now), None);
        assert_eq!(
            horizon_floor_secs(&BackfillHorizon::Days { days: 2 }, now),
            Some(now - 172_800)
        );
        assert_eq!(horizon_floor_secs(&BackfillHorizon::All, now), Some(0));
        assert_eq!(
            horizon_floor_secs(
                &BackfillHorizon::Since {
                    ts: "1754000000.000000".into()
                },
                now
            ),
            Some(1_754_000_000)
        );
    }
}
