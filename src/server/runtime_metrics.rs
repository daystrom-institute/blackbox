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
    let elapsed_micros = duration_micros(metrics.elapsed);
    let total_busy_duration_micros = duration_micros(metrics.total_busy_duration);
    let max_busy_duration_micros = duration_micros(metrics.max_busy_duration);
    let min_busy_duration_micros = duration_micros(metrics.min_busy_duration);

    let snapshot = json!({
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

    *snapshot_slot().write() = Some(snapshot.clone());

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
}

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
