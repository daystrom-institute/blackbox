// Phase 4 concurrency enforcement (concurrency-model §5): the clippy.toml
// disallowed_methods list warns crate-wide by default. The store / index /
// boot layers legitimately do blocking fs on actor threads and blocking-pool
// contexts, so the crate root allows the lint and the enforcement surfaces
// re-deny it: src/tools/mod.rs (MCP handlers) — plus scripts/
// lint-concurrency.sh as the syntactic backstop for handler bodies.
#![allow(clippy::disallowed_methods)]

const HELP: &str = concat!(
    "blackboxd ",
    env!("CARGO_PKG_VERSION"),
    " - Blackbox MCP daemon

USAGE:
    blackboxd            start the daemon (foreground)
    blackboxd --help     print this help and exit
    blackboxd --version  print the version and exit

blackboxd takes no other flags. Configuration comes from
$XDG_CONFIG_HOME/blackbox/config.toml (override with BLACKBOX_CONFIG)
plus explicit env overrides - BBOX_PORT, BBOX_BIND, BLACKBOX_STATE_DIR,
and friends. See docs/operating-blackbox.md and
docs/operations-isolated-dev-daemon.md in the repo for the full list.
");

fn main() -> anyhow::Result<()> {
    // Help/version probes must be side-effect-free: no store opens, no
    // background workers, no port bind, not even a tokio runtime
    // (gap-663baff0). Unknown flags error out rather than silently
    // starting a daemon — a typo'd flag booting a second daemon is
    // exactly the footgun class this guards.
    if let Some(arg) = std::env::args().nth(1) {
        match arg.as_str() {
            "--help" | "-h" => {
                print!("{HELP}");
                return Ok(());
            }
            "--version" | "-V" => {
                println!("blackboxd {}", env!("CARGO_PKG_VERSION"));
                return Ok(());
            }
            other => {
                eprintln!("blackboxd: unknown argument '{other}' (see --help)");
                std::process::exit(2);
            }
        }
    }

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
