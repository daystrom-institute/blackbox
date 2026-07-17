//! `bbox.*` OTel instruments - the dashboard-first emission spec in
//! bbox-otel thread-f57d5dc8. Every instrument is created through
//! `opentelemetry::global::meter("blackboxd")`, which is a documented
//! no-op before `telemetry::init` installs a real `MeterProvider`: every
//! function here is safe to call unconditionally, OTLP export enabled or
//! not, and costs nothing beyond a no-op call when it is disabled.
//!
//! `bbox.search.degraded` is NOT here: hybrid search lives in the
//! `bbox-mcp-tools` peeled crate, which cannot depend back on this daemon
//! crate, so it self-instruments through the same global-meter pattern in
//! its own `bbox_search_metrics.rs` (mirrored again in `bbox-embed`'s
//! `metrics.rs` for provider duration/errors, and `bbox-indexing`'s
//! `metrics.rs` for reindex duration). All four land on the same
//! `opentelemetry::global::meter("blackboxd")` process-wide registry, so
//! they show up as one instrument set regardless of which crate defined
//! them.
//!
//! Two shapes:
//! - Synchronous instruments (counters/histograms) recorded at the call
//!   site - the tool seam, record ingest, mount sync, storage GC.
//!   `instruments()` builds them once behind a `OnceLock`.
//! - Observable (async) gauges/counters registered once, here, when OTLP
//!   is enabled (`register_global_observable_gauges`, called from
//!   `telemetry::init`). Each reads existing process state at collection
//!   time - the tokio-metrics sampler's snapshot, the embed queue's
//!   `status_response()`, the vector store's `metrics_nonblocking()` - the
//!   non-blocking accessors already used elsewhere in this daemon, never a
//!   blocking `metrics()` rebuild call.
//!
//! Mount and ingest "age" gauges have no persisted timestamp to read
//! (`MountRecord`/`RecordStore` carry no last-success instant - see
//! `sync_one_mount` / `internal_record_ingest_handler`), so this module
//! keeps its own process-local last-seen maps, populated at the same call
//! sites that record the counters. They reset on daemon restart: the age
//! reads as "no data" until the first sync/ingest after a restart, not the
//! true pre-restart age. Acceptable for a liveness panel - a restart is
//! itself an operationally visible event.

use std::collections::BTreeMap;
use std::sync::OnceLock;
use std::time::Instant;

use opentelemetry::KeyValue;
use opentelemetry::metrics::{Counter, Histogram, ObservableCounter, ObservableGauge};
use parking_lot::RwLock;

const METER_NAME: &str = "blackboxd";

struct Instruments {
    tool_duration: Histogram<f64>,
    tool_errors: Counter<u64>,
    tool_response_bytes: Histogram<f64>,
    ingest_records: Counter<u64>,
    ingest_bytes: Counter<u64>,
    ingest_batches: Counter<u64>,
    mount_syncs: Counter<u64>,
    mount_sync_duration: Histogram<f64>,
    mount_fetched: Counter<u64>,
    mount_deleted: Counter<u64>,
    storage_gc_bytes_pruned: Counter<u64>,
}

static INSTRUMENTS: OnceLock<Instruments> = OnceLock::new();

fn instruments() -> &'static Instruments {
    INSTRUMENTS.get_or_init(|| {
        let meter = opentelemetry::global::meter(METER_NAME);
        Instruments {
            tool_duration: meter
                .f64_histogram("bbox.tool.duration")
                .with_unit("s")
                .with_description("MCP tool handler wall time, from the run/run_blocking seam.")
                .build(),
            tool_errors: meter
                .u64_counter("bbox.tool.errors")
                .with_description("MCP tool handler error outcomes.")
                .build(),
            tool_response_bytes: meter
                .f64_histogram("bbox.tool.response_bytes")
                .with_unit("By")
                .with_description("MCP tool handler response payload size.")
                .build(),
            ingest_records: meter
                .u64_counter("bbox.ingest.records")
                .with_description("Records accepted by POST /internal/records.")
                .build(),
            ingest_bytes: meter
                .u64_counter("bbox.ingest.bytes")
                .with_unit("By")
                .build(),
            ingest_batches: meter
                .u64_counter("bbox.ingest.batches")
                .with_description("Ingest batch outcomes.")
                .build(),
            mount_syncs: meter
                .u64_counter("bbox.mount.syncs")
                .with_description("Mount sync pass outcomes (ok|busy|error).")
                .build(),
            mount_sync_duration: meter
                .f64_histogram("bbox.mount.sync.duration")
                .with_unit("s")
                .build(),
            mount_fetched: meter.u64_counter("bbox.mount.fetched").build(),
            mount_deleted: meter.u64_counter("bbox.mount.deleted").build(),
            storage_gc_bytes_pruned: meter
                .u64_counter("bbox.storage_gc.bytes_pruned")
                .with_unit("By")
                .build(),
        }
    })
}

/// Tool-call seam instrumentation. Called from `server::response::run` and
/// `run_blocking` - the shared helper every `#[tool]` handler routes
/// through - so every tool reports without per-tool edits. `bytes` is the
/// serialized response length on success; `None` on error (there is no
/// response body to size).
pub(crate) fn record_tool_call(tool: &str, elapsed_secs: f64, bytes: Option<usize>, ok: bool) {
    let attrs = [KeyValue::new("tool", tool.to_string())];
    let inst = instruments();
    inst.tool_duration.record(elapsed_secs, &attrs);
    if let Some(bytes) = bytes {
        inst.tool_response_bytes.record(bytes as f64, &attrs);
    }
    if !ok {
        inst.tool_errors.add(1, &attrs);
    }
}

/// One accepted `POST /internal/records` batch. `outcome` is `"ok"` or a
/// `bro_core::BroError` code (`"idempotency_conflict"`,
/// `"persistence_failed"`, ...). `records`/`bytes` count only on `"ok"` -
/// ingestion is batch-atomic (RecordStore commits the whole batch or
/// none of it, per `crates/bbox-indexing/CLAUDE.md` "Record ingest commits
/// are synchronous"), so counting them on a rejected/failed batch would
/// claim records that never landed. `batches` always increments so the
/// per-outcome breakdown stays visible regardless.
pub(crate) fn record_ingest_batch(host_id: &str, records: u64, bytes: u64, outcome: &str) {
    let inst = instruments();
    inst.ingest_batches.add(
        1,
        &[
            KeyValue::new("host_id", host_id.to_string()),
            KeyValue::new("outcome", outcome.to_string()),
        ],
    );
    if outcome == "ok" {
        let host_attr = [KeyValue::new("host_id", host_id.to_string())];
        inst.ingest_records.add(records, &host_attr);
        inst.ingest_bytes.add(bytes, &host_attr);
        ingest_last_seen()
            .write()
            .insert(host_id.to_string(), Instant::now());
    }
}

/// One mount sync pass outcome (`sync_one_mount` / the periodic loop).
pub(crate) fn record_mount_sync(
    mount_id: &str,
    outcome: &str,
    elapsed_secs: f64,
    fetched: u64,
    deleted: u64,
) {
    let inst = instruments();
    inst.mount_syncs.add(
        1,
        &[
            KeyValue::new("mount_id", mount_id.to_string()),
            KeyValue::new("outcome", outcome.to_string()),
        ],
    );
    let mount_attr = [KeyValue::new("mount_id", mount_id.to_string())];
    inst.mount_sync_duration.record(elapsed_secs, &mount_attr);
    if fetched > 0 {
        inst.mount_fetched.add(fetched, &mount_attr);
    }
    if deleted > 0 {
        inst.mount_deleted.add(deleted, &mount_attr);
    }
    if outcome == "ok" {
        mount_last_success()
            .write()
            .insert(mount_id.to_string(), Instant::now());
    }
}

/// Bytes actually reclaimed by one storage GC maintenance pass.
pub(crate) fn record_storage_gc_bytes_pruned(bytes: u64) {
    if bytes > 0 {
        instruments().storage_gc_bytes_pruned.add(bytes, &[]);
    }
}

// ---------------------------------------------------------------------
// Process-local "last seen" maps backing the two age gauges below. See
// the module doc for why these are ephemeral rather than reading a
// persisted timestamp.
// ---------------------------------------------------------------------

fn ingest_last_seen() -> &'static RwLock<BTreeMap<String, Instant>> {
    static MAP: OnceLock<RwLock<BTreeMap<String, Instant>>> = OnceLock::new();
    MAP.get_or_init(Default::default)
}

fn mount_last_success() -> &'static RwLock<BTreeMap<String, Instant>> {
    static MAP: OnceLock<RwLock<BTreeMap<String, Instant>>> = OnceLock::new();
    MAP.get_or_init(Default::default)
}

/// Register every observable (async) gauge/counter that reads existing
/// process state instead of being recorded at a call site. Called exactly
/// once, from `telemetry::init`, after the real `MeterProvider` is
/// installed - the built instrument handles are retained in `OnceLock`s
/// (never dropped) so their callbacks keep firing for the process
/// lifetime; `get_or_init` also makes this function itself idempotent if
/// ever called twice.
pub(crate) fn register_global_observable_gauges() {
    register_runtime_gauges();
    register_embed_gauges();
    register_vector_gauges();
    register_age_gauges();
}

fn register_runtime_gauges() {
    static WORKERS: OnceLock<ObservableGauge<u64>> = OnceLock::new();
    static TASKS: OnceLock<ObservableGauge<u64>> = OnceLock::new();
    static QUEUE_DEPTH: OnceLock<ObservableGauge<u64>> = OnceLock::new();
    static BUSY_RATIO: OnceLock<ObservableGauge<f64>> = OnceLock::new();
    let meter = opentelemetry::global::meter(METER_NAME);
    WORKERS.get_or_init(|| {
        meter
            .u64_observable_gauge("bbox.runtime.workers")
            .with_description("tokio runtime worker thread count (tokio-metrics sampler).")
            .with_callback(|observer| {
                if let Some(snapshot) = super::runtime_metrics::latest_runtime_metrics_snapshot()
                    && let Some(v) = snapshot.get("workers_count").and_then(|v| v.as_u64())
                {
                    observer.observe(v, &[]);
                }
            })
            .build()
    });
    TASKS.get_or_init(|| {
        meter
            .u64_observable_gauge("bbox.runtime.tasks")
            .with_description("tokio runtime live task count (tokio-metrics sampler).")
            .with_callback(|observer| {
                if let Some(snapshot) = super::runtime_metrics::latest_runtime_metrics_snapshot()
                    && let Some(v) = snapshot.get("live_tasks_count").and_then(|v| v.as_u64())
                {
                    observer.observe(v, &[]);
                }
            })
            .build()
    });
    QUEUE_DEPTH.get_or_init(|| {
        meter
            .u64_observable_gauge("bbox.runtime.queue_depth")
            .with_description("tokio runtime global injection queue depth (tokio-metrics sampler).")
            .with_callback(|observer| {
                if let Some(snapshot) = super::runtime_metrics::latest_runtime_metrics_snapshot()
                    && let Some(v) = snapshot.get("global_queue_depth").and_then(|v| v.as_u64())
                {
                    observer.observe(v, &[]);
                }
            })
            .build()
    });
    BUSY_RATIO.get_or_init(|| {
        meter
            .f64_observable_gauge("bbox.runtime.busy_ratio")
            .with_description("tokio runtime busy_duration/elapsed over the last sampler interval.")
            .with_callback(|observer| {
                let Some(snapshot) = super::runtime_metrics::latest_runtime_metrics_snapshot()
                else {
                    return;
                };
                let elapsed = snapshot.get("elapsed_micros").and_then(|v| v.as_u64());
                let busy = snapshot
                    .get("busy_duration_micros")
                    .and_then(|v| v.get("total"))
                    .and_then(|v| v.as_u64());
                if let (Some(elapsed), Some(busy)) = (elapsed, busy)
                    && elapsed > 0
                {
                    observer.observe(busy as f64 / elapsed as f64, &[]);
                }
            })
            .build()
    });
}

fn register_embed_gauges() {
    static QUEUE_DEPTH: OnceLock<ObservableGauge<u64>> = OnceLock::new();
    static QUEUE_BYTES: OnceLock<ObservableGauge<u64>> = OnceLock::new();
    static EMBEDDED: OnceLock<ObservableCounter<u64>> = OnceLock::new();
    static DROPPED: OnceLock<ObservableCounter<u64>> = OnceLock::new();
    static CAPPED: OnceLock<ObservableCounter<u64>> = OnceLock::new();
    let meter = opentelemetry::global::meter(METER_NAME);
    QUEUE_DEPTH.get_or_init(|| {
        meter
            .u64_observable_gauge("bbox.embed.queue_depth")
            .with_description("Pending embed requests per route (embed_queue::status_response).")
            .with_callback(|observer| {
                for (route, status) in crate::embed_queue::status_response().routes {
                    observer.observe(status.queue_depth, &[KeyValue::new("route", route)]);
                }
            })
            .build()
    });
    QUEUE_BYTES.get_or_init(|| {
        meter
            .u64_observable_gauge("bbox.embed.queue_bytes")
            .with_unit("By")
            .with_callback(|observer| {
                for (route, status) in crate::embed_queue::status_response().routes {
                    observer.observe(status.queue_bytes, &[KeyValue::new("route", route)]);
                }
            })
            .build()
    });
    // `indexed_count` / `dropped_count` / `capped_count` are already
    // durable cumulative counters on `RouteStatus` (never decrease within
    // a process - see queue.rs `mark_success`/`mark_dropped`/enqueue-cap
    // doc comments), so they map directly onto OTel's async-counter
    // contract instead of needing a call-site increment on every embed
    // worker outcome.
    EMBEDDED.get_or_init(|| {
        meter
            .u64_observable_counter("bbox.embed.embedded")
            .with_description("Cumulative successfully embedded items per route.")
            .with_callback(|observer| {
                for (route, status) in crate::embed_queue::status_response().routes {
                    observer.observe(status.indexed_count, &[KeyValue::new("route", route)]);
                }
            })
            .build()
    });
    DROPPED.get_or_init(|| {
        meter
            .u64_observable_counter("bbox.embed.dropped")
            .with_description("Cumulative permanently-dropped (poison) items per route.")
            .with_callback(|observer| {
                for (route, status) in crate::embed_queue::status_response().routes {
                    observer.observe(status.dropped_count, &[KeyValue::new("route", route)]);
                }
            })
            .build()
    });
    CAPPED.get_or_init(|| {
        meter
            .u64_observable_counter("bbox.embed.capped")
            .with_description("Cumulative enqueue attempts rejected by the per-route queue cap.")
            .with_callback(|observer| {
                for (route, status) in crate::embed_queue::status_response().routes {
                    observer.observe(status.capped_count, &[KeyValue::new("route", route)]);
                }
            })
            .build()
    });
}

fn register_vector_gauges() {
    static ACTIVE: OnceLock<ObservableGauge<u64>> = OnceLock::new();
    static DELETED_RATIO: OnceLock<ObservableGauge<f64>> = OnceLock::new();
    static DISCONNECTED: OnceLock<ObservableGauge<u64>> = OnceLock::new();
    let meter = opentelemetry::global::meter(METER_NAME);
    ACTIVE.get_or_init(|| {
        meter
            .u64_observable_gauge("bbox.vectors.active_count")
            .with_callback(|observer| {
                let Some(partitions) = crate::vectors::metrics_nonblocking() else {
                    return;
                };
                for (partition, metrics) in partitions {
                    observer.observe(
                        metrics.active_count as u64,
                        &[KeyValue::new("partition", partition)],
                    );
                }
            })
            .build()
    });
    DELETED_RATIO.get_or_init(|| {
        meter
            .f64_observable_gauge("bbox.vectors.deleted_ratio")
            .with_callback(|observer| {
                let Some(partitions) = crate::vectors::metrics_nonblocking() else {
                    return;
                };
                for (partition, metrics) in partitions {
                    observer.observe(
                        metrics.deleted_ratio as f64,
                        &[KeyValue::new("partition", partition)],
                    );
                }
            })
            .build()
    });
    DISCONNECTED.get_or_init(|| {
        meter
            .u64_observable_gauge("bbox.vectors.disconnected_nodes")
            .with_description(
                "HNSW nodes unreachable by graph traversal (vector-recall risk, gap-1168b0bd).",
            )
            .with_callback(|observer| {
                let Some(partitions) = crate::vectors::metrics_nonblocking() else {
                    return;
                };
                for (partition, metrics) in partitions {
                    let disconnected = metrics
                        .hnsw
                        .as_ref()
                        .map(|h| h.disconnected_nodes)
                        .unwrap_or(0);
                    observer.observe(
                        disconnected as u64,
                        &[KeyValue::new("partition", partition)],
                    );
                }
            })
            .build()
    });
}

fn register_age_gauges() {
    static INGEST_AGE: OnceLock<ObservableGauge<f64>> = OnceLock::new();
    static MOUNT_AGE: OnceLock<ObservableGauge<f64>> = OnceLock::new();
    let meter = opentelemetry::global::meter(METER_NAME);
    INGEST_AGE.get_or_init(|| {
        meter
            .f64_observable_gauge("bbox.ingest.last_record_age_seconds")
            .with_unit("s")
            .with_description("Seconds since the last accepted record-ingest batch, per host_id.")
            .with_callback(|observer| {
                let now = Instant::now();
                for (host_id, seen) in ingest_last_seen().read().iter() {
                    observer.observe(
                        now.saturating_duration_since(*seen).as_secs_f64(),
                        &[KeyValue::new("host_id", host_id.clone())],
                    );
                }
            })
            .build()
    });
    MOUNT_AGE.get_or_init(|| {
        meter
            .f64_observable_gauge("bbox.mount.last_success_age_seconds")
            .with_unit("s")
            .with_callback(|observer| {
                let now = Instant::now();
                for (mount_id, seen) in mount_last_success().read().iter() {
                    observer.observe(
                        now.saturating_duration_since(*seen).as_secs_f64(),
                        &[KeyValue::new("mount_id", mount_id.clone())],
                    );
                }
            })
            .build()
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn record_tool_call_and_ingest_batch_do_not_panic_when_otel_disabled() {
        // No telemetry::init call in this test process (nextest is
        // process-per-test): global::meter() is the documented no-op
        // provider. Every recording call must be a harmless no-op, never a
        // panic, so the tool/ingest/mount/search/storage-gc call sites stay
        // safe to leave unconditional.
        record_tool_call("bbox_test_tool", 0.001, Some(42), true);
        record_tool_call("bbox_test_tool", 0.002, None, false);
        record_ingest_batch("test-producer", 3, 1024, "ok");
        record_mount_sync("test-mount", "ok", 0.5, 10, 2);
        record_storage_gc_bytes_pruned(4096);
        assert!(ingest_last_seen().read().contains_key("test-producer"));
        assert!(mount_last_success().read().contains_key("test-mount"));
    }

    #[test]
    fn register_global_observable_gauges_is_idempotent() {
        register_global_observable_gauges();
        register_global_observable_gauges();
    }
}
