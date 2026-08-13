//! `bbox-slack-collector`: the conversation satellite binary.
//!
//! Runs on a PRODUCER host, beside the interactive bridge and on the same
//! credential plane, not on the corpus host. It holds the Slack bot token,
//! observes the channels that bot is a member of, normalizes messages into wire
//! records, and publishes them. Everything downstream of observation --
//! chunking, embedding, edges, projection -- happens on the corpus host, which
//! is why this crate's dependency ceiling is enforced mechanically.
//!
//! Three subcommands, matching the three things an operator actually does:
//!
//! - `onboard` presents the probed workspace facts and find-or-creates the
//!   catalog project. Idempotent, so driving it before every cycle is safe.
//! - `run` executes exactly one cycle and exits. This is the cron shape.
//! - `watch` runs cycles on an interval until interrupted. This is the daemon
//!   shape, and a failed cycle logs and waits rather than exiting: a transient
//!   corpus restart or a workspace throttle must not take the satellite down.

use std::path::PathBuf;

use anyhow::{Context, Result};
use bbox_slack_collector::{
    ConversationSourceClient, SatelliteConfig, SlackClient, run_onboarding, run_publication_cycle,
};
use clap::{Parser, Subcommand};

#[derive(Parser, Debug)]
#[command(
    name = "bbox-slack-collector",
    about = "Publish a Slack workspace's visible conversations into a blackbox corpus"
)]
struct Cli {
    /// Path to the satellite config.
    #[arg(long, short)]
    config: PathBuf,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Probe the workspace and find-or-create its catalog project.
    Onboard,
    /// Run exactly one publication cycle.
    Run,
    /// Run publication cycles until interrupted.
    Watch {
        /// Seconds between cycles. Defaults to the config's own cadence.
        #[arg(long)]
        interval_secs: Option<u64>,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();
    let cli = Cli::parse();
    let config = SatelliteConfig::load(&cli.config)?;

    // Both credentials resolve HERE, once, at startup: the config carries
    // references and the literals never leave this process. A resolution
    // failure is a startup failure, because a satellite running on a credential
    // it cannot attribute is worse than one that did not start.
    let producer_bearer = config
        .producer_token
        .resolve("producer_token")
        .await
        .context("resolving the producer bearer")?;
    let slack_token = config
        .slack_token
        .resolve("slack_token")
        .await
        .context("resolving the Slack bot token")?;

    let sink = ConversationSourceClient::new(config.corpus_url.clone(), producer_bearer)?;
    let slack = SlackClient::new(
        config.slack_api_base_url.clone(),
        slack_token,
        config.rate.clone(),
    )?;

    match cli.command {
        Command::Onboard => {
            let receipt = run_onboarding(&slack, &sink, &config).await?;
            println!(
                "project {} ({}) at catalog epoch {}",
                receipt.project_id,
                if receipt.created_project {
                    "created"
                } else {
                    "existing"
                },
                receipt.epoch
            );
        }
        Command::Run => {
            let outcome =
                run_publication_cycle(&slack, &sink, &config, &config.journal_path, chrono::Utc::now())
                    .await?;
            report(&outcome, &slack);
        }
        Command::Watch { interval_secs } => {
            let interval = std::time::Duration::from_secs(
                interval_secs.unwrap_or(config.poll_interval_secs).max(1),
            );
            loop {
                match run_publication_cycle(
                    &slack,
                    &sink,
                    &config,
                    &config.journal_path,
                    chrono::Utc::now(),
                )
                .await
                {
                    Ok(outcome) => tracing::info!(
                        channels = outcome.channels_enrolled,
                        landed = outcome.messages_landed,
                        duplicates = outcome.duplicates,
                        thread_replies = outcome.thread_replies_landed,
                        revisions = outcome.revisions_emitted,
                        tombstones = outcome.tombstones_emitted,
                        deferred = outcome.windows_deferred,
                        lag_seconds = outcome.max_lag_seconds,
                        "publication cycle completed"
                    ),
                    // A cycle failure is not fatal. A corpus restart, a
                    // transient network fault, and a workspace throttle all
                    // resolve on a later cycle, and exiting here would turn
                    // every one of them into an operator page.
                    Err(error) => tracing::error!(error = %error, "publication cycle failed"),
                }
                tokio::select! {
                    _ = tokio::time::sleep(interval) => {}
                    _ = tokio::signal::ctrl_c() => {
                        tracing::info!("interrupted; stopping the satellite");
                        break;
                    }
                }
            }
        }
    }
    Ok(())
}

/// Print what one cycle did, including what the shared credential can do.
///
/// The granted-scope line is not decoration. Under the one-app posture the
/// collector cannot refuse a write scope, so the operator's compensating
/// control is SEEING the whole grant on every run.
fn report(outcome: &bbox_slack_collector::CycleOutcome, slack: &SlackClient) {
    let stats = slack.stats();
    println!(
        "workspace {} channels={} landed={} duplicates={} thread_replies={} revisions={} \
         tombstones={} deferred_windows={} backfill_windows={} reconciled={} requests={} \
         throttled={} lag_seconds={}",
        outcome.workspace_id,
        outcome.channels_enrolled,
        outcome.messages_landed,
        outcome.duplicates,
        outcome.thread_replies_landed,
        outcome.revisions_emitted,
        outcome.tombstones_emitted,
        outcome.windows_deferred,
        outcome.backfill_windows,
        outcome.reconciled,
        stats.requests,
        stats.throttled,
        outcome
            .max_lag_seconds
            .map(|lag| lag.to_string())
            .unwrap_or_else(|| "none".to_string()),
    );
    println!("granted scopes: {}", outcome.granted_scopes.join(", "));
    if !outcome.write_scopes.is_empty() {
        println!(
            "write scopes on the shared credential (reported, not used): {}",
            outcome.write_scopes.join(", ")
        );
    }
    if !outcome.channels_skipped.is_empty() {
        println!("channels not enrolled: {:?}", outcome.channels_skipped);
    }
    if !outcome.normalization_skips.is_empty() {
        println!("messages not recorded: {:?}", outcome.normalization_skips);
    }
}
