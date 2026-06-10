fn main() -> anyhow::Result<()> {
    let mut builder = tokio::runtime::Builder::new_multi_thread();

    builder.enable_all();

    // Preserve default worker-thread count (one per CPU core) and set a
    // descriptive thread name for observability.
    builder.thread_name("blackboxd-worker");

    // Under tokio_unstable, enable the poll-time histogram so that
    // tokio-metrics RuntimeMonitor can populate poll_time_histogram buckets
    // in RuntimeMetrics. Uses the default H2 log-scale histogram
    // (≈237 buckets, 100 ns – 68 s range, 25 % max error) — good resolution
    // across sub-millisecond to multi-second poll times without tuning.
    #[cfg(tokio_unstable)]
    {
        builder.enable_metrics_poll_time_histogram();
    }

    let runtime = builder.build()?;
    runtime.block_on(async { blackbox::server::run().await })
}
