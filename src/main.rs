// Phase 4 concurrency enforcement (concurrency-model §5): the clippy.toml
// disallowed_methods list warns crate-wide by default. The store / index /
// boot layers legitimately do blocking fs on actor threads and blocking-pool
// contexts, so the crate root allows the lint and the enforcement surfaces
// re-deny it: src/tools/mod.rs (MCP handlers) — plus scripts/
// lint-concurrency.sh as the syntactic backstop for handler bodies.
#![allow(clippy::disallowed_methods)]
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
