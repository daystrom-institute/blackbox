//! `bbox.search.degraded` - one OTel counter, incremented at hybrid
//! search's degradation points (`mcp_tools::hybrid_search`).
//!
//! Like `bbox-embed`'s `metrics.rs`, this goes through the process-wide
//! `opentelemetry::global::meter()`, a documented no-op until blackboxd's
//! `server/telemetry.rs` installs a real `MeterProvider` - safe to call
//! unconditionally.

use std::sync::OnceLock;

use opentelemetry::KeyValue;
use opentelemetry::metrics::Counter;

const METER_NAME: &str = "blackboxd";

static DEGRADED: OnceLock<Counter<u64>> = OnceLock::new();

fn degraded_counter() -> &'static Counter<u64> {
    DEGRADED.get_or_init(|| {
        opentelemetry::global::meter(METER_NAME)
            .u64_counter("bbox.search.degraded")
            .with_description(
                "Hybrid search degradation events (rerank_unavailable|vector_unavailable|partition_busy).",
            )
            .build()
    })
}

/// `kind` is `rerank_unavailable` | `vector_unavailable` | `partition_busy`.
pub(crate) fn record_degraded(kind: &'static str) {
    degraded_counter().add(1, &[KeyValue::new("kind", kind)]);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn record_degraded_does_not_panic() {
        record_degraded("rerank_unavailable");
        record_degraded("vector_unavailable");
        record_degraded("partition_busy");
    }
}
