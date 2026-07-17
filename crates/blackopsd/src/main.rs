mod telemetry;

use clap::Parser;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let telemetry_guard = telemetry::init();
    let result = blackopsd::run(blackopsd::BlackopsdConfig::parse()).await;
    // Flush spans/logs still buffered in the OTLP batch exporters before
    // the process exits. No-op when telemetry is disabled.
    telemetry_guard.shutdown();
    result
}
