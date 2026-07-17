//! OTel instrumentation for the periodic reindex pass
//! (`index::reindex::execute_reindex_pass` /
//! `index::reindex::scheduled_reindex_tick`): `bbox.reindex.duration`,
//! `bbox.reindex.last_age_seconds`, and the index-size gauges
//! `bbox.index.documents` / `bbox.index.segments`.
//!
//! Same pattern as `bbox-embed`'s `metrics.rs`: goes through the
//! process-wide `opentelemetry::global::meter()`, a documented no-op until
//! blackboxd's `server/telemetry.rs` installs a real `MeterProvider` - safe
//! to call unconditionally. `bbox.index.bytes` (tantivy space usage) is
//! deliberately not wired here - computing it needs a `Searcher` the
//! reindex pass does not already hold, and opening one mid-pass to answer
//! a gauge was judged not worth the extra reader churn for stage 1.

use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use opentelemetry::metrics::{Gauge, Histogram};

const METER_NAME: &str = "blackboxd";

struct Instruments {
    duration: Histogram<f64>,
    documents: Gauge<u64>,
    segments: Gauge<u64>,
}

static INSTRUMENTS: OnceLock<Instruments> = OnceLock::new();

fn instruments() -> &'static Instruments {
    INSTRUMENTS.get_or_init(|| {
        let meter = opentelemetry::global::meter(METER_NAME);
        Instruments {
            duration: meter
                .f64_histogram("bbox.reindex.duration")
                .with_unit("s")
                .with_description("Full reindex pass wall time (execute_reindex_pass).")
                .build(),
            documents: meter
                .u64_gauge("bbox.index.documents")
                .with_description("Tantivy index document count as of the last reindex commit.")
                .build(),
            segments: meter
                .u64_gauge("bbox.index.segments")
                .with_description("Tantivy searchable segment count as of the last reindex commit.")
                .build(),
        }
    })
}

/// Unix seconds of the last successful pass completion, for the
/// `bbox.reindex.last_age_seconds` observable gauge below. `0` means "no
/// successful pass yet this process" (a fresh daemon start before its
/// first tick, or an all-failed run) - the gauge callback skips observing
/// in that case rather than reporting a huge bogus age.
static LAST_SUCCESS_UNIX_SECS: AtomicU64 = AtomicU64::new(0);

/// Record one completed reindex pass: duration, and the resulting index
/// document/segment counts. Call once per pass, after `writer.commit()`
/// succeeds.
pub fn record_reindex_pass(elapsed: Duration, documents: u64, segments: u64) {
    let inst = instruments();
    inst.duration.record(elapsed.as_secs_f64(), &[]);
    inst.documents.record(documents, &[]);
    inst.segments.record(segments, &[]);
    let now_secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    LAST_SUCCESS_UNIX_SECS.store(now_secs, Ordering::Relaxed);
    register_last_age_gauge_once();
}

fn register_last_age_gauge_once() {
    static REGISTERED: OnceLock<opentelemetry::metrics::ObservableGauge<f64>> = OnceLock::new();
    REGISTERED.get_or_init(|| {
        opentelemetry::global::meter(METER_NAME)
            .f64_observable_gauge("bbox.reindex.last_age_seconds")
            .with_unit("s")
            .with_description("Seconds since the last successful reindex pass completed.")
            .with_callback(|observer| {
                let last = LAST_SUCCESS_UNIX_SECS.load(Ordering::Relaxed);
                if last == 0 {
                    return;
                }
                let now = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .map(|d| d.as_secs())
                    .unwrap_or(last);
                observer.observe(now.saturating_sub(last) as f64, &[]);
            })
            .build()
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn record_reindex_pass_does_not_panic() {
        record_reindex_pass(Duration::from_millis(250), 1234, 12);
    }
}
