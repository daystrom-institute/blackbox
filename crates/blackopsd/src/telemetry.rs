//! Minimal OTLP telemetry mirror of `transcript-search/src/server/telemetry.rs`
//! (bbox-otel thread-f57d5dc8, stage 1). blackopsd is a separate crate from
//! blackboxd - it does not depend on `blackbox` - so this duplicates the
//! same env-gated contract rather than sharing code:
//!
//! `OTEL_EXPORTER_OTLP_ENDPOINT` unset or empty keeps today's behavior
//! byte-for-byte (plain `tracing_subscriber::fmt()` to stderr, exactly what
//! `main.rs` did before this change). Set it to layer in a
//! tracing-opentelemetry span exporter and an opentelemetry-appender-tracing
//! log bridge (events INFO and up). No metrics pipeline: blackopsd has no
//! `bbox.*` domain instruments defined yet (those are blackboxd's), so a
//! metrics exporter here would have nothing to export - add one alongside
//! the first blackopsd-side instrument.
//!
//! Exporter build failures degrade to a logged warning and fall back to the
//! disabled (fmt-only) path, same as blackboxd's mirror.

use opentelemetry::trace::TracerProvider as _;
use opentelemetry_appender_tracing::layer::OpenTelemetryTracingBridge;
use opentelemetry_otlp::{LogExporter, SpanExporter, WithExportConfig};
use opentelemetry_sdk::Resource;
use opentelemetry_sdk::logs::SdkLoggerProvider;
use opentelemetry_sdk::trace::{Sampler, SdkTracerProvider};
use tracing_subscriber::filter::LevelFilter;
use tracing_subscriber::layer::{Layer, SubscriberExt};
use tracing_subscriber::util::SubscriberInitExt;

const ENDPOINT_ENV: &str = "OTEL_EXPORTER_OTLP_ENDPOINT";
const TRACES_SAMPLER_ARG_ENV: &str = "OTEL_TRACES_SAMPLER_ARG";
const DEFAULT_TRACE_SAMPLE_RATIO: f64 = 1.0;

pub(crate) struct TelemetryGuard {
    tracer_provider: Option<SdkTracerProvider>,
    logger_provider: Option<SdkLoggerProvider>,
}

impl TelemetryGuard {
    fn disabled() -> Self {
        Self {
            tracer_provider: None,
            logger_provider: None,
        }
    }

    pub(crate) fn shutdown(&self) {
        if let Some(provider) = &self.tracer_provider
            && let Err(err) = provider.shutdown()
        {
            tracing::warn!(error = %err, "otel tracer provider shutdown failed");
        }
        if let Some(provider) = &self.logger_provider
            && let Err(err) = provider.shutdown()
        {
            tracing::warn!(error = %err, "otel logger provider shutdown failed");
        }
    }
}

fn otlp_endpoint_from_env() -> Option<String> {
    std::env::var(ENDPOINT_ENV)
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

fn trace_sample_ratio() -> f64 {
    std::env::var(TRACES_SAMPLER_ARG_ENV)
        .ok()
        .and_then(|v| v.trim().parse::<f64>().ok())
        .filter(|v| v.is_finite())
        .unwrap_or(DEFAULT_TRACE_SAMPLE_RATIO)
}

fn build_resource() -> Resource {
    let host_name = hostname::get()
        .ok()
        .and_then(|h| h.into_string().ok())
        .unwrap_or_else(|| "unknown".to_string());
    Resource::builder()
        .with_service_name("blackopsd")
        .with_attributes([
            opentelemetry::KeyValue::new(
                opentelemetry_semantic_conventions::attribute::SERVICE_VERSION,
                env!("CARGO_PKG_VERSION"),
            ),
            // HOST_NAME is gated behind opentelemetry-semantic-conventions's
            // `semconv_experimental` feature in 0.32; "host.name" is the
            // same stable resource attribute key, spelled directly rather
            // than pulling in the experimental feature for one constant.
            opentelemetry::KeyValue::new("host.name", host_name),
        ])
        .build()
}

struct Enabled {
    tracer: opentelemetry_sdk::trace::Tracer,
    tracer_provider: SdkTracerProvider,
    logger_provider: SdkLoggerProvider,
}

fn build_otel(endpoint: &str) -> anyhow::Result<Enabled> {
    use anyhow::Context;

    let resource = build_resource();

    let span_exporter = SpanExporter::builder()
        .with_tonic()
        .with_endpoint(endpoint)
        .build()
        .context("building OTLP span exporter")?;
    let tracer_provider = SdkTracerProvider::builder()
        .with_resource(resource.clone())
        .with_sampler(Sampler::ParentBased(Box::new(Sampler::TraceIdRatioBased(
            trace_sample_ratio(),
        ))))
        .with_batch_exporter(span_exporter)
        .build();
    let tracer = tracer_provider.tracer("blackopsd".to_string());

    let log_exporter = LogExporter::builder()
        .with_tonic()
        .with_endpoint(endpoint)
        .build()
        .context("building OTLP log exporter")?;
    let logger_provider = SdkLoggerProvider::builder()
        .with_resource(resource)
        .with_batch_exporter(log_exporter)
        .build();

    Ok(Enabled {
        tracer,
        tracer_provider,
        logger_provider,
    })
}

fn build_env_filter() -> tracing_subscriber::EnvFilter {
    tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| "blackopsd=info".into())
}

/// Install the global tracing subscriber. Must be called at most once per
/// process, before any other subsystem starts - mirrors the call site
/// `main.rs` used for the plain `tracing_subscriber::fmt()` init this
/// replaces.
pub(crate) fn init() -> TelemetryGuard {
    let Some(endpoint) = otlp_endpoint_from_env() else {
        tracing_subscriber::fmt()
            .with_env_filter(build_env_filter())
            .init();
        return TelemetryGuard::disabled();
    };

    let enabled = match build_otel(&endpoint) {
        Ok(enabled) => enabled,
        Err(err) => {
            eprintln!(
                "blackopsd: otel init failed against endpoint {endpoint:?}: {err:#}; falling back to fmt-only logging"
            );
            tracing_subscriber::fmt()
                .with_env_filter(build_env_filter())
                .init();
            return TelemetryGuard::disabled();
        }
    };

    let fmt_layer = tracing_subscriber::fmt::layer().with_filter(build_env_filter());
    let span_layer = tracing_opentelemetry::layer()
        .with_tracer(enabled.tracer)
        .with_filter(build_env_filter());
    let log_bridge_layer =
        OpenTelemetryTracingBridge::new(&enabled.logger_provider).with_filter(LevelFilter::INFO);

    tracing_subscriber::registry()
        .with(fmt_layer)
        .with(span_layer)
        .with(log_bridge_layer)
        .init();

    opentelemetry::global::set_tracer_provider(enabled.tracer_provider.clone());

    tracing::info!(endpoint = %endpoint, "otel export enabled (traces + logs)");

    TelemetryGuard {
        tracer_provider: Some(enabled.tracer_provider),
        logger_provider: Some(enabled.logger_provider),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn init_with_malformed_endpoint_falls_back_without_panicking() {
        let bogus = "not a valid uri \u{0}";
        let result = std::panic::catch_unwind(|| build_otel(bogus));
        match result {
            Ok(build_result) => assert!(build_result.is_err()),
            Err(_) => panic!("build_otel must not panic on a malformed endpoint"),
        }
    }
}
