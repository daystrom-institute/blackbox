//! The conversation satellite: a producer-host binary that observes a Slack
//! workspace from a bot's own perspective and publishes message records over
//! the conversation-source wire.
//!
//! Two properties are the point of the design, and both are properties of WHERE
//! this code runs rather than of what it does:
//!
//! - **The daemon never talks to Slack.** It holds no workspace credential and
//!   opens no socket to a chat provider. The only new network trust edge is
//!   producer-host-to-vendor, on the host where the token has to live anyway.
//! - **The daemon owns the cursor.** This satellite asks where to resume rather
//!   than asserting where it left off, so a producer restart, reinstall, or host
//!   move needs no producer-side durable state to be correct.
//!
//! # Write-safety under the one-app posture
//!
//! The deployed posture is ONE Slack app: the operator's existing interactive
//! bot, whose channel membership defines exactly what gets indexed (design 3.1,
//! ruled 2026-08-13). A Slack app holds one bot token per install carrying ALL
//! its granted scopes, so a read-only credential for that identity does not
//! exist and the two-app design's startup assertion ("this grant carries no
//! write scope, refuse otherwise") is unavailable here.
//!
//! Write-safety therefore moves into the code path, in three layers:
//!
//! 1. **No write call sites.** Nothing in this crate composes a mutation.
//! 2. **A closed read-method enum.** [`slack::SlackReadMethod`] is the only
//!    thing the client will build a request path from, there is no
//!    string-taking entry point, and there is no vendor SDK underneath that
//!    would supply the write surface as ordinary functions.
//! 3. **A dependency ceiling**, enforced by
//!    `scripts/acceptance-slack-collector-deps.sh` over the resolved graph.
//!
//! What the collector CAN do about scopes it records: `auth.test` captures the
//! granted set, the write subset is reported on every cycle outcome, and a
//! MISSING read scope is refused at startup. The agents-never-post rule is
//! untouched; it binds agents, and this is an observer that structurally cannot
//! compose a write.
//!
//! # Producer discipline
//!
//! This crate ships RECORDS, not documents. It does not concatenate messages
//! into synthetic documents, choose thread windows, summarize, render markdown,
//! resolve mentions, or embed. Every document-shaping decision stays
//! corpus-side, so exactly one chunker version exists in the system and a
//! satellite deploy can never skew against the index.
//!
//! Module map:
//!
//! - [`slack`] -- the closed read-method set, the allowlist client, the vendor
//!   response shapes, and the rate pacer;
//! - [`policy`] -- channel enrollment: classes, membership, include and exclude
//!   globs, counted refusals;
//! - [`normalize`] -- Slack message JSON to the wire record, with every drop
//!   counted;
//! - [`journal`] -- producer working state: reconciliation baselines, forward
//!   and backfill marks, thread rotation;
//! - [`cycle`] -- one publication cycle across all four lanes, written against
//!   a sink trait so its decisions are testable without a socket;
//! - [`client`] -- the `/internal/conversation-source/v1/*` wire client;
//! - [`config`] -- the operator-declared satellite configuration;
//! - [`secret`] -- credential references, resolved at startup and never
//!   rendered.

pub mod client;
pub mod config;
pub mod cycle;
pub mod journal;
pub mod normalize;
pub mod policy;
pub mod secret;
pub mod slack;

pub use client::ConversationSourceClient;
pub use config::{
    BackfillHorizon, BackfillPolicy, ReconciliationPolicy, SatelliteConfig, SweepPolicy,
};
pub use cycle::{
    ConversationSink, CycleOutcome, ScopeObservation, Shutdown, check_read_scopes,
    required_read_scopes, run_onboarding, run_publication_cycle,
    run_publication_cycle_with_shutdown, scope_observation,
};
pub use journal::{ChannelJournal, Journal, MessageBaseline, ThreadMark};
pub use policy::{
    ChannelDecision, ChannelPolicy, CompiledChannelPolicy, EnrollmentMode, SkipCounters,
};
pub use secret::SecretRef;
pub use slack::{RatePolicy, SlackClient, SlackRead, SlackReadMethod};
