use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use parking_lot::RwLock;
use serde_json::{Value, json};
use tokio_metrics::{RuntimeMetrics, RuntimeMonitor};

const DEFAULT_INTERVAL_SECS: u64 = 60;
const INTERVAL_ENV: &str = "BLACKBOX_RUNTIME_METRICS_INTERVAL_SECS";
const LOG_TARGET: &str = "blackbox::runtime";

/// Scheduler-latency probe (design/daemon-runtime/healthz-ingest-starvation.md
/// §6). One second between probes: the health probe that starved in the
/// 2026-08-13 ingest incident had a 5s timeout, so the instrument has to
/// resolve well inside it. The 60s metrics interval above cannot.
const DEFAULT_PROBE_INTERVAL_MILLIS: u64 = 1_000;
const PROBE_INTERVAL_ENV: &str = "BLACKBOX_SCHED_PROBE_INTERVAL_MILLIS";
/// A ready task waiting longer than this to be polled means cheap HTTP
/// handlers are already degraded, well before an external probe times out.
const DEFAULT_PROBE_BUDGET_MILLIS: u64 = 250;
const PROBE_BUDGET_ENV: &str = "BLACKBOX_SCHED_PROBE_BUDGET_MILLIS";
/// Ceiling on how long one probe waits before recording the sample as
/// at-least-this-long. Generous relative to the budget on purpose: an
/// over-budget sample IS the finding, so truncating it early would discard
/// the number worth having.
const PROBE_CEILING: Duration = Duration::from_secs(30);
const PROBE_LOG_TARGET: &str = "blackbox::runtime::sched_probe";

static LATEST_SNAPSHOT: OnceLock<Arc<RwLock<Option<Value>>>> = OnceLock::new();

static PROBE_LAST_MICROS: AtomicU64 = AtomicU64::new(0);
static PROBE_MAX_MICROS: AtomicU64 = AtomicU64::new(0);
static PROBE_SAMPLE_COUNT: AtomicU64 = AtomicU64::new(0);
static PROBE_OVER_BUDGET_COUNT: AtomicU64 = AtomicU64::new(0);

pub(crate) fn spawn_runtime_metrics_sampler() {
    let interval_secs = runtime_metrics_interval_secs();
    if interval_secs == 0 {
        tracing::info!(
            target: LOG_TARGET,
            env = INTERVAL_ENV,
            interval_secs,
            "runtime metrics sampler disabled"
        );
        return;
    }

    let handle = tokio::runtime::Handle::current();
    let runtime_monitor = RuntimeMonitor::new(&handle);
    let mut intervals = runtime_monitor.intervals();

    tokio::spawn(async move {
        let interval = Duration::from_secs(interval_secs);
        loop {
            tokio::time::sleep(interval).await;
            let Some(metrics) = intervals.next() else {
                tracing::warn!(target: LOG_TARGET, "runtime metrics sampler ended");
                break;
            };
            publish_runtime_metrics_snapshot(&metrics, interval_secs);
        }
    });
}

pub(crate) fn latest_runtime_metrics_snapshot() -> Option<Value> {
    snapshot_slot().read().clone()
}

/// Measure how long the serving runtime takes to poll a task that is ready
/// the instant it is spawned.
///
/// This is the `/healthz` question minus the socket. `health_probe`
/// (src/server/mcp.rs) holds no lock, reads no state, and is `Ready` on its
/// first poll, so when it stops answering the cause is the runtime not
/// polling it, not anything on its path. Nothing else in the daemon measures
/// that; the 2026-08-13 ingest incident was unresolvable afterwards partly
/// because of it (design/daemon-runtime/healthz-ingest-starvation.md §5.2).
///
/// It runs on a dedicated OS thread, and that placement is the whole point.
/// [`spawn_runtime_metrics_sampler`] is a `tokio::spawn` on the runtime it
/// reports on, so runtime starvation delays the sampler by exactly the
/// amount it exists to record: it goes blind in the only window that
/// matters. This probe keeps its timekeeping (`std::thread::sleep`) and its
/// wait (`std::sync::mpsc`) off the runtime entirely, and reaches in only
/// through `Handle::spawn`. It shares no lock and no queue with ingest.
pub(crate) fn spawn_scheduler_latency_probe(handle: tokio::runtime::Handle) {
    let interval = scheduler_probe_interval();
    if interval.is_zero() {
        tracing::info!(
            target: PROBE_LOG_TARGET,
            env = PROBE_INTERVAL_ENV,
            "scheduler latency probe disabled"
        );
        return;
    }
    let budget = scheduler_probe_budget();

    if let Err(error) = std::thread::Builder::new()
        .name("blackbox-sched-probe".into())
        .spawn(move || scheduler_latency_probe_loop(&handle, interval, budget))
    {
        tracing::warn!(
            target: PROBE_LOG_TARGET,
            %error,
            "scheduler latency probe thread not started"
        );
    }
}

fn scheduler_latency_probe_loop(
    handle: &tokio::runtime::Handle,
    interval: Duration,
    budget: Duration,
) {
    loop {
        std::thread::sleep(interval);
        // `None` means the runtime dropped the task instead of running it,
        // i.e. it is shutting down. Stop probing rather than spin.
        let Some(latency) = probe_scheduler_latency_once(handle, PROBE_CEILING) else {
            return;
        };
        record_scheduler_latency(latency, budget);
    }
}

/// One round trip: spawn an always-ready task and time how long it takes to
/// run. Returns `None` when the runtime refuses the work (shutdown).
fn probe_scheduler_latency_once(
    handle: &tokio::runtime::Handle,
    ceiling: Duration,
) -> Option<Duration> {
    let (tx, rx) = std::sync::mpsc::sync_channel::<()>(1);
    let started = Instant::now();
    handle.spawn(async move {
        // Ready on first poll: everything measured is scheduling delay.
        let _ = tx.send(());
    });
    match rx.recv_timeout(ceiling) {
        Ok(()) => Some(started.elapsed()),
        // The task never ran within the ceiling. The sample is a floor, not
        // an exact figure, and a floor of 30s is finding enough.
        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => Some(started.elapsed()),
        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => None,
    }
}

fn record_scheduler_latency(latency: Duration, budget: Duration) {
    let micros = duration_micros(latency);
    PROBE_LAST_MICROS.store(micros, Ordering::Relaxed);
    PROBE_MAX_MICROS.fetch_max(micros, Ordering::Relaxed);
    PROBE_SAMPLE_COUNT.fetch_add(1, Ordering::Relaxed);

    if latency > budget {
        // Logged per sample, not per interval. The averaged 60s snapshot is
        // what made the original incident unreadable; an operator needs the
        // spike at the moment it happens.
        let over_budget_count = PROBE_OVER_BUDGET_COUNT.fetch_add(1, Ordering::Relaxed) + 1;
        tracing::warn!(
            target: PROBE_LOG_TARGET,
            latency_micros = micros,
            budget_micros = duration_micros(budget),
            over_budget_count,
            "serving runtime did not poll a ready task within budget; cheap \
             HTTP handlers such as /healthz are stalled for the same reason"
        );
    }
}

/// Live probe counters. Read straight from the atomics rather than from the
/// sampler's published snapshot, so this stays truthful even when the
/// sampler is itself starved.
pub(crate) fn scheduler_latency_snapshot() -> Value {
    json!({
        "source": "sched-probe",
        "budget_micros": duration_micros(scheduler_probe_budget()),
        "interval_millis": scheduler_probe_interval().as_millis() as u64,
        "last_micros": PROBE_LAST_MICROS.load(Ordering::Relaxed),
        "max_micros": PROBE_MAX_MICROS.load(Ordering::Relaxed),
        "sample_count": PROBE_SAMPLE_COUNT.load(Ordering::Relaxed),
        "over_budget_count": PROBE_OVER_BUDGET_COUNT.load(Ordering::Relaxed),
    })
}

fn publish_runtime_metrics_snapshot(metrics: &RuntimeMetrics, interval_secs: u64) {
    let sampled_at = chrono::Utc::now().to_rfc3339();
    let snapshot = build_runtime_metrics_snapshot(metrics, interval_secs, sampled_at.clone());
    *snapshot_slot().write() = Some(snapshot);

    log_runtime_metrics(metrics, interval_secs, &sampled_at);
}

fn build_runtime_metrics_snapshot(
    metrics: &RuntimeMetrics,
    interval_secs: u64,
    sampled_at: String,
) -> Value {
    let elapsed_micros = duration_micros(metrics.elapsed);
    let total_busy_duration_micros = duration_micros(metrics.total_busy_duration);
    let max_busy_duration_micros = duration_micros(metrics.max_busy_duration);
    let min_busy_duration_micros = duration_micros(metrics.min_busy_duration);

    let mut snapshot = json!({
        "sampled_at": sampled_at,
        "source": "tokio-metrics",
        "interval_secs": interval_secs,
        "elapsed_micros": elapsed_micros,
        "workers_count": metrics.workers_count,
        "live_tasks_count": metrics.live_tasks_count,
        "park": {
            "total_count": metrics.total_park_count,
            "max_count": metrics.max_park_count,
            "min_count": metrics.min_park_count,
        },
        "busy_duration_micros": {
            "total": total_busy_duration_micros,
            "max": max_busy_duration_micros,
            "min": min_busy_duration_micros,
        },
        "global_queue_depth": metrics.global_queue_depth,
    });

    add_unstable_snapshot_fields(&mut snapshot, metrics);

    snapshot
}

fn log_runtime_metrics(metrics: &RuntimeMetrics, interval_secs: u64, sampled_at: &str) {
    let elapsed_micros = duration_micros(metrics.elapsed);
    let total_busy_duration_micros = duration_micros(metrics.total_busy_duration);
    let max_busy_duration_micros = duration_micros(metrics.max_busy_duration);
    let min_busy_duration_micros = duration_micros(metrics.min_busy_duration);

    tracing::info!(
        target: LOG_TARGET,
        sampled_at = %sampled_at,
        interval_secs,
        elapsed_micros,
        workers_count = metrics.workers_count,
        live_tasks_count = metrics.live_tasks_count,
        global_queue_depth = metrics.global_queue_depth,
        total_busy_duration_micros,
        max_busy_duration_micros,
        min_busy_duration_micros,
        total_park_count = metrics.total_park_count,
        max_park_count = metrics.max_park_count,
        min_park_count = metrics.min_park_count,
        "runtime metrics sampled"
    );

    log_unstable_runtime_metrics(metrics);
}

#[cfg(tokio_unstable)]
fn add_unstable_snapshot_fields(snapshot: &mut Value, metrics: &RuntimeMetrics) {
    let histogram_buckets: Vec<Value> = metrics
        .poll_time_histogram
        .buckets()
        .iter()
        .map(|bucket| {
            json!({
                "range_start_micros": duration_micros(bucket.range_start()),
                "range_end_micros": duration_micros(bucket.range_end()),
                "count": bucket.count(),
            })
        })
        .collect();

    if let Some(object) = snapshot.as_object_mut() {
        object.insert(
            "unstable".to_string(),
            json!({
                "poll": {
                    "mean_duration_micros": duration_micros(metrics.mean_poll_duration),
                    "mean_duration_worker_min_micros": duration_micros(metrics.mean_poll_duration_worker_min),
                    "mean_duration_worker_max_micros": duration_micros(metrics.mean_poll_duration_worker_max),
                    "total_count": metrics.total_polls_count,
                    "max_count": metrics.max_polls_count,
                    "min_count": metrics.min_polls_count,
                    "budget_forced_yield_count": metrics.budget_forced_yield_count,
                    "time_histogram": histogram_buckets,
                },
                "queue_depth": {
                    "total_local": metrics.total_local_queue_depth,
                    "max_local": metrics.max_local_queue_depth,
                    "min_local": metrics.min_local_queue_depth,
                    "blocking": metrics.blocking_queue_depth,
                },
                "blocking_threads": {
                    "total": metrics.blocking_threads_count,
                    "idle": metrics.idle_blocking_threads_count,
                },
                "scheduler": {
                    "remote_schedule_count": metrics.num_remote_schedules,
                    "total_local_schedule_count": metrics.total_local_schedule_count,
                    "max_local_schedule_count": metrics.max_local_schedule_count,
                    "min_local_schedule_count": metrics.min_local_schedule_count,
                    "total_overflow_count": metrics.total_overflow_count,
                    "max_overflow_count": metrics.max_overflow_count,
                    "min_overflow_count": metrics.min_overflow_count,
                    "total_noop_count": metrics.total_noop_count,
                    "max_noop_count": metrics.max_noop_count,
                    "min_noop_count": metrics.min_noop_count,
                    "total_steal_count": metrics.total_steal_count,
                    "max_steal_count": metrics.max_steal_count,
                    "min_steal_count": metrics.min_steal_count,
                    "total_steal_operations": metrics.total_steal_operations,
                    "max_steal_operations": metrics.max_steal_operations,
                    "min_steal_operations": metrics.min_steal_operations,
                    "io_driver_ready_count": metrics.io_driver_ready_count,
                },
            }),
        );
    }
}

#[cfg(not(tokio_unstable))]
fn add_unstable_snapshot_fields(_snapshot: &mut Value, _metrics: &RuntimeMetrics) {}

#[cfg(tokio_unstable)]
fn log_unstable_runtime_metrics(metrics: &RuntimeMetrics) {
    tracing::info!(
        target: LOG_TARGET,
        mean_poll_duration_micros = duration_micros(metrics.mean_poll_duration),
        mean_poll_duration_worker_min_micros = duration_micros(metrics.mean_poll_duration_worker_min),
        mean_poll_duration_worker_max_micros = duration_micros(metrics.mean_poll_duration_worker_max),
        total_polls_count = metrics.total_polls_count,
        max_polls_count = metrics.max_polls_count,
        min_polls_count = metrics.min_polls_count,
        total_local_queue_depth = metrics.total_local_queue_depth,
        max_local_queue_depth = metrics.max_local_queue_depth,
        min_local_queue_depth = metrics.min_local_queue_depth,
        blocking_queue_depth = metrics.blocking_queue_depth,
        blocking_threads_count = metrics.blocking_threads_count,
        idle_blocking_threads_count = metrics.idle_blocking_threads_count,
        num_remote_schedules = metrics.num_remote_schedules,
        total_local_schedule_count = metrics.total_local_schedule_count,
        total_overflow_count = metrics.total_overflow_count,
        total_noop_count = metrics.total_noop_count,
        total_steal_count = metrics.total_steal_count,
        total_steal_operations = metrics.total_steal_operations,
        budget_forced_yield_count = metrics.budget_forced_yield_count,
        io_driver_ready_count = metrics.io_driver_ready_count,
        "runtime metrics unstable sampled"
    );
}

#[cfg(not(tokio_unstable))]
fn log_unstable_runtime_metrics(_metrics: &RuntimeMetrics) {}

fn snapshot_slot() -> &'static Arc<RwLock<Option<Value>>> {
    LATEST_SNAPSHOT.get_or_init(|| Arc::new(RwLock::new(None)))
}

fn runtime_metrics_interval_secs() -> u64 {
    std::env::var(INTERVAL_ENV)
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(DEFAULT_INTERVAL_SECS)
}

fn scheduler_probe_interval() -> Duration {
    Duration::from_millis(millis_from_env(
        PROBE_INTERVAL_ENV,
        DEFAULT_PROBE_INTERVAL_MILLIS,
    ))
}

fn scheduler_probe_budget() -> Duration {
    Duration::from_millis(millis_from_env(
        PROBE_BUDGET_ENV,
        DEFAULT_PROBE_BUDGET_MILLIS,
    ))
}

fn millis_from_env(name: &str, default: u64) -> u64 {
    std::env::var(name)
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(default)
}

fn duration_micros(duration: Duration) -> u64 {
    duration.as_micros().min(u128::from(u64::MAX)) as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn runtime_metrics_snapshot_contains_stable_keys() {
        let snapshot = build_runtime_metrics_snapshot(
            &sample_metrics(),
            60,
            "2026-06-09T00:00:00Z".to_string(),
        );

        let object = snapshot.as_object().expect("snapshot must be an object");
        for key in [
            "sampled_at",
            "source",
            "interval_secs",
            "elapsed_micros",
            "workers_count",
            "live_tasks_count",
            "park",
            "busy_duration_micros",
            "global_queue_depth",
        ] {
            assert!(object.contains_key(key), "missing stable key `{key}`");
        }

        assert_eq!(snapshot["source"], "tokio-metrics");
        assert_eq!(snapshot["interval_secs"], 60);
        assert_eq!(snapshot["workers_count"], 4);
        assert_eq!(snapshot["live_tasks_count"], 9);
        assert_eq!(snapshot["global_queue_depth"], 3);
        assert_eq!(snapshot["park"]["total_count"], 10);
        assert_eq!(snapshot["busy_duration_micros"]["total"], 2_000);

        #[cfg(not(tokio_unstable))]
        assert!(snapshot.get("unstable").is_none());
    }

    #[cfg(tokio_unstable)]
    #[test]
    fn runtime_metrics_snapshot_contains_unstable_keys() {
        let snapshot = build_runtime_metrics_snapshot(
            &sample_metrics(),
            60,
            "2026-06-09T00:00:00Z".to_string(),
        );

        let unstable = snapshot
            .get("unstable")
            .and_then(Value::as_object)
            .expect("unstable metrics must be present");
        for key in ["poll", "queue_depth", "blocking_threads", "scheduler"] {
            assert!(unstable.contains_key(key), "missing unstable key `{key}`");
        }

        assert_eq!(snapshot["unstable"]["poll"]["mean_duration_micros"], 11);
        assert_eq!(snapshot["unstable"]["poll"]["total_count"], 31);
        assert_eq!(snapshot["unstable"]["queue_depth"]["total_local"], 7);
        assert_eq!(snapshot["unstable"]["queue_depth"]["blocking"], 2);
        assert_eq!(snapshot["unstable"]["blocking_threads"]["total"], 6);
        assert_eq!(
            snapshot["unstable"]["scheduler"]["remote_schedule_count"],
            13
        );
        assert_eq!(
            snapshot["unstable"]["scheduler"]["io_driver_ready_count"],
            47
        );
    }

    /// The probe surface is stable-tokio only. Every field that discriminates
    /// the 2026-08-13 ingest starvation candidates used to sit behind
    /// `#[cfg(tokio_unstable)]`, which no build in this repo enables, so it
    /// was absent from the shipped binary exactly when it was needed
    /// (design/daemon-runtime/healthz-ingest-starvation.md §5.2).
    #[test]
    fn scheduler_latency_snapshot_exposes_probe_counters_on_stable_tokio() {
        let snapshot = scheduler_latency_snapshot();

        let object = snapshot
            .as_object()
            .expect("probe snapshot must be an object");
        for key in [
            "source",
            "budget_micros",
            "interval_millis",
            "last_micros",
            "max_micros",
            "sample_count",
            "over_budget_count",
        ] {
            assert!(object.contains_key(key), "missing probe key `{key}`");
        }
        assert_eq!(snapshot["source"], "sched-probe");
    }

    #[test]
    fn scheduler_probe_measures_an_idle_runtime_as_prompt() {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("probe test runtime");

        let latency = probe_scheduler_latency_once(runtime.handle(), Duration::from_secs(5))
            .expect("an idle runtime must answer the probe");

        assert!(
            latency < Duration::from_secs(1),
            "idle runtime probe took {latency:?}"
        );
    }

    /// The laboratory version of the incident: the task is ready, and nothing
    /// polls it because the only worker is parked in blocking code. This is
    /// what the probe exists to catch, so if this assertion ever stops
    /// holding the instrument is worthless.
    #[test]
    fn scheduler_probe_detects_a_runtime_whose_worker_is_blocked() {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .expect("probe test runtime");
        let handle = runtime.handle().clone();

        handle.spawn(async {
            std::thread::sleep(Duration::from_millis(300));
        });
        // Let the blocking task claim the single worker before probing;
        // otherwise the probe can win the race and measure nothing.
        std::thread::sleep(Duration::from_millis(50));

        let latency = probe_scheduler_latency_once(&handle, Duration::from_secs(5))
            .expect("a live runtime must answer the probe eventually");

        assert!(
            latency >= Duration::from_millis(100),
            "probe must observe the stall, saw {latency:?}"
        );
    }

    /// A probe thread that cannot tell "shutting down" from "stalled" would
    /// spin against a dead runtime for the life of the process.
    #[test]
    fn scheduler_probe_reports_none_once_the_runtime_is_gone() {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .expect("probe test runtime");
        let handle = runtime.handle().clone();
        runtime.shutdown_timeout(Duration::from_secs(1));

        assert!(
            probe_scheduler_latency_once(&handle, Duration::from_millis(200)).is_none(),
            "a shut-down runtime must end the probe loop, not stall it"
        );
    }

    /// One test owns the probe statics so the delta assertions cannot race a
    /// sibling test in a shared-process run.
    #[test]
    fn over_budget_samples_are_counted_rather_than_averaged_away() {
        let budget = Duration::from_millis(250);
        let samples_before = PROBE_SAMPLE_COUNT.load(Ordering::Relaxed);
        let over_before = PROBE_OVER_BUDGET_COUNT.load(Ordering::Relaxed);

        record_scheduler_latency(Duration::from_millis(900), budget);

        assert_eq!(PROBE_LAST_MICROS.load(Ordering::Relaxed), 900_000);
        assert!(PROBE_MAX_MICROS.load(Ordering::Relaxed) >= 900_000);
        assert!(PROBE_SAMPLE_COUNT.load(Ordering::Relaxed) > samples_before);
        assert_eq!(
            PROBE_OVER_BUDGET_COUNT.load(Ordering::Relaxed),
            over_before + 1,
            "a sample past budget must be counted"
        );

        let over_after_spike = PROBE_OVER_BUDGET_COUNT.load(Ordering::Relaxed);
        record_scheduler_latency(Duration::from_millis(1), budget);

        assert_eq!(
            PROBE_OVER_BUDGET_COUNT.load(Ordering::Relaxed),
            over_after_spike,
            "a healthy sample must not be counted as over budget"
        );
        // The peak survives a later healthy sample; `max` is the incident
        // evidence and must not be reset by recovery.
        assert!(PROBE_MAX_MICROS.load(Ordering::Relaxed) >= 900_000);
    }

    #[test]
    fn probe_tuning_falls_back_to_defaults_on_unset_or_unparseable_env() {
        let mut env = crate::util::TestEnvGuard::new();

        env.remove(PROBE_INTERVAL_ENV);
        assert_eq!(
            millis_from_env(PROBE_INTERVAL_ENV, DEFAULT_PROBE_INTERVAL_MILLIS),
            DEFAULT_PROBE_INTERVAL_MILLIS
        );

        env.set(PROBE_INTERVAL_ENV, "not-a-number");
        assert_eq!(
            millis_from_env(PROBE_INTERVAL_ENV, DEFAULT_PROBE_INTERVAL_MILLIS),
            DEFAULT_PROBE_INTERVAL_MILLIS
        );

        // Zero is a deliberate value, not a parse failure: it disables the
        // probe in spawn_scheduler_latency_probe.
        env.set(PROBE_INTERVAL_ENV, "0");
        assert_eq!(
            millis_from_env(PROBE_INTERVAL_ENV, DEFAULT_PROBE_INTERVAL_MILLIS),
            0
        );
        assert!(scheduler_probe_interval().is_zero());
    }

    fn sample_metrics() -> RuntimeMetrics {
        let mut metrics = RuntimeMetrics::default();
        metrics.workers_count = 4;
        metrics.live_tasks_count = 9;
        metrics.total_park_count = 10;
        metrics.max_park_count = 6;
        metrics.min_park_count = 4;
        metrics.total_busy_duration = Duration::from_micros(2_000);
        metrics.max_busy_duration = Duration::from_micros(1_200);
        metrics.min_busy_duration = Duration::from_micros(800);
        metrics.global_queue_depth = 3;
        metrics.elapsed = Duration::from_secs(60);

        fill_unstable_sample_metrics(&mut metrics);

        metrics
    }

    #[cfg(tokio_unstable)]
    fn fill_unstable_sample_metrics(metrics: &mut RuntimeMetrics) {
        metrics.mean_poll_duration = Duration::from_micros(11);
        metrics.mean_poll_duration_worker_min = Duration::from_micros(5);
        metrics.mean_poll_duration_worker_max = Duration::from_micros(17);
        metrics.total_noop_count = 19;
        metrics.max_noop_count = 8;
        metrics.min_noop_count = 1;
        metrics.total_steal_count = 23;
        metrics.max_steal_count = 12;
        metrics.min_steal_count = 2;
        metrics.total_steal_operations = 29;
        metrics.max_steal_operations = 13;
        metrics.min_steal_operations = 3;
        metrics.num_remote_schedules = 13;
        metrics.total_local_schedule_count = 17;
        metrics.max_local_schedule_count = 9;
        metrics.min_local_schedule_count = 4;
        metrics.total_overflow_count = 5;
        metrics.max_overflow_count = 3;
        metrics.min_overflow_count = 1;
        metrics.total_polls_count = 31;
        metrics.max_polls_count = 21;
        metrics.min_polls_count = 10;
        metrics.total_local_queue_depth = 7;
        metrics.max_local_queue_depth = 5;
        metrics.min_local_queue_depth = 2;
        metrics.blocking_queue_depth = 2;
        metrics.blocking_threads_count = 6;
        metrics.idle_blocking_threads_count = 4;
        metrics.budget_forced_yield_count = 37;
        metrics.io_driver_ready_count = 47;
    }

    #[cfg(not(tokio_unstable))]
    fn fill_unstable_sample_metrics(_metrics: &mut RuntimeMetrics) {}
}
