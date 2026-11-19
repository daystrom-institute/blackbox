use anyhow::Result;
use bbox_transcript_collector::{CaptureMemo, Client, Config, error_diagnostic, publish_cycle};
use clap::{Parser, Subcommand};
#[derive(Parser)]
struct Cli {
    #[arg(long)]
    config: std::path::PathBuf,
    #[command(subcommand)]
    command: Command,
}
#[derive(Subcommand)]
enum Command {
    Onboard,
    Publish,
    Watch {
        #[arg(long, default_value_t = 300)]
        interval_secs: u64,
    },
}
#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let config = Config::load(&cli.config)?;
    let client = Client::new(&config)?;
    match cli.command {
        Command::Onboard => println!("{}", client.onboard(&config).await?),
        Command::Publish => {
            let report = publish_cycle(&config, &client, &mut CaptureMemo::default()).await?;
            println!("{}", serde_json::to_string(&report)?);
            anyhow::ensure!(
                report.failed == 0,
                "some native transcript streams failed; review the cycle report and retry"
            );
        }
        Command::Watch { interval_secs } => {
            // One memo for the life of the watch: later cycles read only the
            // streams whose files changed since their last capture.
            let mut memo = CaptureMemo::default();
            loop {
                match publish_cycle(&config, &client, &mut memo).await {
                    Ok(report) => println!("{}", serde_json::to_string(&report)?),
                    Err(error) => eprintln!(
                        "native transcript cycle failed: {}",
                        error_diagnostic(&error)
                    ),
                }
                tokio::select! { _ = tokio::signal::ctrl_c() => break, _ = tokio::time::sleep(std::time::Duration::from_secs(interval_secs.max(1))) => {} }
            }
        }
    }
    Ok(())
}
