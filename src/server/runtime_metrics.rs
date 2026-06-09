use std::sync::{Arc, OnceLock};
use std::time::Duration;

use parking_lot::RwLock;
use serde_json::{Value, json};
use tokio_metrics::{RuntimeMetrics, RuntimeMonitor};

const DEFAULT_INTERVAL_SECS: u64 = 60;
const INTERVAL_ENV: &str = "BLACKBOX_RUNTIME_METRICS_INTERVAL_SECS";
const LOG_TARGET: &str = "blackbox::runtime";

static LATEST_SNAPSHOT: OnceLock<Arc<RwLock<Option<Value>>>> = OnceLock::new();

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
