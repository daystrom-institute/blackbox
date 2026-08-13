//! `bbox-file-collector`: the connector satellite binary.
//!
//! Runs on a PRODUCER host, not the corpus host. It holds the vendor
//! credential, observes a remote store, applies operator policy, exports
//! provider-native documents, and publishes a manifest plus blobs over the
//! collector wire. Everything downstream of byte acquisition -- chunking,
//! embedding, edges, activation -- happens on the corpus host, which is why
//! this crate's dependency ceiling is enforced mechanically.
//!
//! Three subcommands, matching the three things an operator actually does:
//!
//! - `onboard` presents the probed remote facts and find-or-creates the
//!   catalog project. It is idempotent, so driving it before every cycle is
//!   safe.
//! - `publish` runs exactly one cycle and exits. This is the cron shape.
//! - `watch` runs cycles on an interval until interrupted. This is the daemon
//!   shape, and a failed cycle logs and waits rather than exiting: a
//!   transient daemon restart must not take the satellite down with it.

use std::path::PathBuf;

use anyhow::{Context, Result};
use bbox_file_collector::{
    FileSourceClient, FixtureConnector, RemoteSourceConnector, SatelliteConfig,
    policy::CompiledPolicy, run_publication_cycle,
};
use clap::{Parser, Subcommand};

#[derive(Parser, Debug)]
#[command(
    name = "bbox-file-collector",
    about = "Publish a remote document store into a blackbox corpus"
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
    /// Probe the remote store and find-or-create its catalog project.
    Onboard,
    /// Run exactly one publication cycle.
    Publish,
    /// Run publication cycles until interrupted.
    Watch {
        /// Seconds between cycles.
        #[arg(long, default_value_t = 300)]
        interval_secs: u64,
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
    let client = FileSourceClient::new(config.daemon_url.clone(), config.bearer()?)?;
    let connector = FixtureConnector::open(&config.connector_root)
        .with_context(|| format!("opening connector root {}", config.connector_root.display()))?;

    match cli.command {
        Command::Onboard => {
            let receipt = onboard(&config, &client, &connector).await?;
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
        Command::Publish => {
            let outcome = publish(&config, &client, &connector).await?;
            println!(
                "generation {} files={} fetched={} exported={} uploaded={} epoch={}",
                outcome.generation_id,
                outcome.file_count,
                outcome.blobs_fetched,
                outcome.documents_exported,
                outcome.blobs_uploaded,
                outcome.cursor_epoch
            );
        }
        Command::Watch { interval_secs } => {
            let interval = std::time::Duration::from_secs(interval_secs.max(1));
            loop {
                match publish(&config, &client, &connector).await {
                    Ok(outcome) => tracing::info!(
                        generation = %outcome.generation_id,
                        files = outcome.file_count,
                        fetched = outcome.blobs_fetched,
                        uploaded = outcome.blobs_uploaded,
                        "publication cycle completed"
                    ),
                    // A cycle failure is not fatal. The daemon restarting, a
                    // transient network fault, or a rejected manifest all
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

async fn onboard(
    config: &SatelliteConfig,
    client: &FileSourceClient,
    connector: &FixtureConnector,
) -> Result<bbox_file_source::FileCatalogOnboardResponseV1> {
    // The facts are PROBED from the remote, never asserted from config: the
    // daemon revalidates kind and remote-authority agreement against the
    // operator's grant, and a config-asserted authority would defeat that
    // check by construction.
    let info = connector.validate().await.context("probing the remote")?;
    let request = bbox_file_source::FileCatalogOnboardRequestV1 {
        schema_version: bbox_file_source::CATALOG_ONBOARD_SCHEMA_VERSION,
        scope: config.scope()?,
        probed_connector_kind: connector.kind().to_string(),
        probed_remote_authority: info.remote_authority,
        probed_remote_root_id: info.remote_root_id,
        probed_remote_display_name: info.remote_display_name,
        display_name: config.display_name(),
    };
    client.onboard(&request).await
}

async fn publish(
    config: &SatelliteConfig,
    client: &FileSourceClient,
    connector: &FixtureConnector,
) -> Result<bbox_file_collector::CycleOutcome> {
    let policy = CompiledPolicy::compile(&config.policy).context("compiling the source policy")?;
    let observed_at = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    run_publication_cycle(
        connector,
        &policy,
        &config.scope()?,
        &config.journal_path,
        client,
        &observed_at,
    )
    .await
}
