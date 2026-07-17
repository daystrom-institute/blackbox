//! OTel instrumentation for embedding provider calls
//! (`bbox.embed.provider.duration` / `bbox.embed.provider.errors`).
//!
//! This crate is a daemon-internal leaf: it depends on external crates
//! only, never on `blackbox` internals. `opentelemetry` (the API crate,
//! not the SDK/exporter) is exactly such an external dependency, and
//! `opentelemetry::global::meter()` is a documented no-op until
//! blackboxd's `server/telemetry.rs` installs a real `MeterProvider` on
//! the process-wide global registry - see that module's doc comment for
//! the full contract. Every function here is therefore safe to call
//! unconditionally, OTLP export enabled or not.

use std::sync::OnceLock;
use std::time::Duration;

use opentelemetry::KeyValue;
use opentelemetry::metrics::{Counter, Histogram};

const METER_NAME: &str = "blackboxd";

struct Instruments {
    duration: Histogram<f64>,
    errors: Counter<u64>,
}

static INSTRUMENTS: OnceLock<Instruments> = OnceLock::new();

fn instruments() -> &'static Instruments {
    INSTRUMENTS.get_or_init(|| {
        let meter = opentelemetry::global::meter(METER_NAME);
        Instruments {
            duration: meter
                .f64_histogram("bbox.embed.provider.duration")
                .with_unit("s")
                .with_description("Embedding provider HTTP round-trip time, one call per batch.")
                .build(),
            errors: meter
                .u64_counter("bbox.embed.provider.errors")
                .with_description("Embedding provider call failures.")
                .build(),
        }
    })
}

/// Record one embedding provider call outcome (document batch or single
/// query embed). On error, `kind` distinguishes HTTP 429 (rate limit -
/// expected under load, wants backoff not alerting) from everything else.
/// Provider error types (voyage.rs et al) surface 429 as a formatted "HTTP
/// 429" string rather than a typed variant - only genuinely non-retryable
/// 4xx payload rejections get a typed marker
/// (`queue::NonRetryableBatchError`) - so classification here is a
/// substring match on the rendered error chain rather than a downcast.
pub(crate) fn record_provider_call(
    provider: &str,
    model: &str,
    elapsed: Duration,
    error: Option<&anyhow::Error>,
) {
    let inst = instruments();
    inst.duration.record(
        elapsed.as_secs_f64(),
        &[
            KeyValue::new("provider", provider.to_string()),
            KeyValue::new("model", model.to_string()),
        ],
    );
    if let Some(err) = error {
        let kind = if format!("{err:#}").contains("429") {
            "http_429"
        } else {
            "other"
        };
        inst.errors.add(
            1,
            &[
                KeyValue::new("provider", provider.to_string()),
                KeyValue::new("model", model.to_string()),
                KeyValue::new("kind", kind),
            ],
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn record_provider_call_does_not_panic_without_or_with_error() {
        record_provider_call("voyage", "voyage-code-3", Duration::from_millis(5), None);
        let err = anyhow::anyhow!(
            "voyage embedding request failed: HTTP 429 batch_size=1 body=slow down"
        );
        record_provider_call(
            "voyage",
            "voyage-code-3",
            Duration::from_millis(5),
            Some(&err),
        );
        let err =
            anyhow::anyhow!("voyage embedding request failed: HTTP 500 batch_size=1 body=oops");
        record_provider_call(
            "voyage",
            "voyage-code-3",
            Duration::from_millis(5),
            Some(&err),
        );
    }
}
