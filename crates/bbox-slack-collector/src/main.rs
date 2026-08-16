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
    ConversationSourceClient, SatelliteConfig, Shutdown, SlackClient, run_onboarding,
    run_publication_cycle, run_publication_cycle_with_shutdown,
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
            let outcome = run_publication_cycle(
                &slack,
                &sink,
                &config,
                &config.journal_path,
                chrono::Utc::now(),
            )
            .await?;
            report(&outcome, &slack);
        }
        Command::Watch { interval_secs } => {
            let interval = std::time::Duration::from_secs(
                interval_secs.unwrap_or(config.poll_interval_secs).max(1),
            );
            // Kubernetes sends SIGTERM on pod stop, and the container runtime's
            // kill escalates on a deadline. Watching for it here is what turns
            // "the process dies mid-cycle" into "the cycle finishes the current
            // channel, the journal is saved, and the process exits 0".
            #[cfg(unix)]
            let mut sigterm =
                tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                    .context("installing the SIGTERM listener")?;
            let shutdown = Shutdown::default();
            // The one-app posture cannot refuse a write scope, so watch mode
            // states the whole grant at startup and re-states it whenever the
            // observed set moves between cycles: a scope granted or revoked
            // under a running satellite is exactly the change an operator
            // must see without going looking for it.
            let mut observed_scopes: Option<(Vec<String>, Vec<String>)> = None;
            loop {
                let mut cycle = std::pin::pin!(run_publication_cycle_with_shutdown(
                    &slack,
                    &sink,
                    &config,
                    &config.journal_path,
                    chrono::Utc::now(),
                    &shutdown,
                ));
                // A signal during a cycle does NOT cancel it: the select arm
                // raises the flag, then awaits the cycle to completion, so the
                // current channel's journal save happens before exit.
                let outcome = tokio::select! {
                    outcome = &mut cycle => outcome,
                    signal = interrupt(&mut sigterm) => {
                        shutdown.request();
                        tracing::info!(
                            signal,
                            "terminating; finishing current channel"
                        );
                        cycle.await
                    }
                };
                match outcome {
                    Ok(outcome) => {
                        let snapshot =
                            (outcome.granted_scopes.clone(), outcome.write_scopes.clone());
                        match bbox_slack_collector::scope_observation(
                            observed_scopes
                                .as_ref()
                                .map(|(granted, writes)| (granted.as_slice(), writes.as_slice())),
                            &snapshot.0,
                            &snapshot.1,
                        ) {
                            bbox_slack_collector::ScopeObservation::Initial => tracing::info!(
                                granted = snapshot.0.join(", "),
                                writes = snapshot.1.join(", "),
                                "granted scopes observed"
                            ),
                            bbox_slack_collector::ScopeObservation::Changed => tracing::info!(
                                granted = snapshot.0.join(", "),
                                writes = snapshot.1.join(", "),
                                "granted scopes changed between cycles"
                            ),
                            bbox_slack_collector::ScopeObservation::Unchanged => {}
                        }
                        observed_scopes = Some(snapshot);
                        tracing::info!(
                            channels = outcome.channels_enrolled,
                            landed = outcome.messages_landed,
                            duplicates = outcome.duplicates,
                            thread_replies = outcome.thread_replies_landed,
                            revisions = outcome.revisions_emitted,
                            tombstones = outcome.tombstones_emitted,
                            deferred = outcome.windows_deferred,
                            // Honest key: this is the corpus's worst NEWEST-record
                            // staleness (the quietest channel), not backfill depth.
                            // Backfill progress is the fields below it.
                            max_channel_staleness_seconds = outcome.max_lag_seconds,
                            backfill_windows = outcome.backfill_windows,
                            channels_backfilling = outcome.channels_backfilling,
                            oldest_backfilled_to = outcome.oldest_backfilled_to,
                            "publication cycle completed"
                        )
                    }
                    // A cycle failure is not fatal. A corpus restart, a
                    // transient network fault, and a workspace throttle all
                    // resolve on a later cycle, and exiting here would turn
                    // every one of them into an operator page. The Debug
                    // rendering is deliberate: it carries the whole anyhow
                    // chain, and the cause was the part the log used to drop.
                    Err(error) => tracing::error!(error = ?error, "publication cycle failed"),
                }
                if shutdown.is_requested() {
                    tracing::info!("interrupted; stopping the satellite");
                    break;
                }
                tokio::select! {
                    _ = tokio::time::sleep(interval) => {}
                    signal = interrupt(&mut sigterm) => {
                        tracing::info!(signal, "interrupted; stopping the satellite");
                        break;
                    }
                }
            }
        }
    }
    Ok(())
}

/// Wait for whichever interrupt this platform offers, returning its name.
///
/// On unix both Ctrl-C and SIGTERM count; anywhere else only Ctrl-C does,
/// because `signal::unix` does not exist there.
#[cfg(unix)]
async fn interrupt(sigterm: &mut tokio::signal::unix::Signal) -> &'static str {
    tokio::select! {
        _ = tokio::signal::ctrl_c() => "ctrl_c",
        _ = sigterm.recv() => "SIGTERM",
    }
}

#[cfg(not(unix))]
async fn interrupt() -> &'static str {
    tokio::signal::ctrl_c().await.ok();
    "ctrl_c"
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
         tombstones={} deferred_windows={} backfill_windows={} channels_backfilling={} \
         oldest_backfilled_to={} reconciled={} requests={} throttled={} \
         max_channel_staleness_seconds={}",
        outcome.workspace_id,
        outcome.channels_enrolled,
        outcome.messages_landed,
        outcome.duplicates,
        outcome.thread_replies_landed,
        outcome.revisions_emitted,
        outcome.tombstones_emitted,
        outcome.windows_deferred,
        outcome.backfill_windows,
        outcome.channels_backfilling,
        outcome.oldest_backfilled_to.as_deref().unwrap_or("none"),
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
