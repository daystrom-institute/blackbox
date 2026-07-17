//! OTLP telemetry: env-gated traces/metrics/logs export to a Grafana
//! otel-lgtm collector (design/... bbox-otel thread-f57d5dc8, stage 1).
//!
//! `OTEL_EXPORTER_OTLP_ENDPOINT` unset or empty keeps today's behavior
//! byte-for-byte: a plain fmt(stderr) + fmt(file) tracing subscriber, no
//! background export tasks, no otel SDK object ever constructed. Set it
//! (`http://host:4317` for the default grpc-tonic transport this repo
//! targets) to layer in a tracing-opentelemetry span exporter, an
//! opentelemetry-appender-tracing log bridge (events INFO and up
//! regardless of a more verbose `RUST_LOG`), and a periodic OTLP metrics
//! exporter reading the `bbox.*` instruments in `bbox_metrics`.
//!
//! Exporter build failures degrade to a logged warning and fall back to
//! the disabled (fmt-only) path - a misconfigured or unreachable collector
//! must never block or crash daemon startup. There is deliberately no
//! retry/backoff here: the OTLP SDK's own batch/periodic export machinery
//! already retries transient failures after the pipeline is up; a failure
//! at `build()` time means the exporter could not even be constructed
//! (bad URI, TLS config, ...), which retrying startup would not fix.

use std::time::Duration;

use opentelemetry::KeyValue;
use opentelemetry::global;
use opentelemetry::trace::TracerProvider as _;
use opentelemetry_appender_tracing::layer::OpenTelemetryTracingBridge;
use opentelemetry_otlp::{LogExporter, MetricExporter, SpanExporter, WithExportConfig};
use opentelemetry_sdk::Resource;
use opentelemetry_sdk::logs::SdkLoggerProvider;
use opentelemetry_sdk::metrics::{PeriodicReader, SdkMeterProvider};
use opentelemetry_sdk::trace::{Sampler, SdkTracerProvider};
use tracing_subscriber::filter::LevelFilter;
use tracing_subscriber::layer::{Layer, SubscriberExt};
use tracing_subscriber::util::SubscriberInitExt;

const ENDPOINT_ENV: &str = "OTEL_EXPORTER_OTLP_ENDPOINT";
const TRACES_SAMPLER_ARG_ENV: &str = "OTEL_TRACES_SAMPLER_ARG";
const METRIC_INTERVAL_ENV: &str = "OTEL_METRIC_EXPORT_INTERVAL";
const DEFAULT_METRIC_INTERVAL_SECS: u64 = 15;
const DEFAULT_TRACE_SAMPLE_RATIO: f64 = 1.0;

/// Holds the installed SDK providers so the daemon's shutdown path can
/// flush them before exit. `disabled()` (the default when the endpoint env
/// var is unset) carries nothing and `shutdown()` on it is a no-op.
pub(crate) struct TelemetryGuard {
    tracer_provider: Option<SdkTracerProvider>,
    meter_provider: Option<SdkMeterProvider>,
    logger_provider: Option<SdkLoggerProvider>,
}

impl TelemetryGuard {
    fn disabled() -> Self {
        Self {
            tracer_provider: None,
            meter_provider: None,
            logger_provider: None,
        }
    }

    /// Flush and shut down every installed provider. Best-effort: each
    /// provider gets its own logged-on-failure shutdown call rather than
    /// propagating an error, so a stuck collector cannot hang daemon
    /// shutdown.
    pub(crate) fn shutdown(&self) {
        if let Some(provider) = &self.tracer_provider
            && let Err(err) = provider.shutdown()
        {
            tracing::warn!(error = %err, "otel tracer provider shutdown failed");
        }
        if let Some(provider) = &self.meter_provider
            && let Err(err) = provider.shutdown()
        {
            tracing::warn!(error = %err, "otel meter provider shutdown failed");
        }
        if let Some(provider) = &self.logger_provider
            && let Err(err) = provider.shutdown()
        {
            tracing::warn!(error = %err, "otel logger provider shutdown failed");
        }
    }
}

/// `OTEL_EXPORTER_OTLP_ENDPOINT`, treating an empty/whitespace value the
/// same as unset - an operator clearing the env var in a shell profile
/// (`export OTEL_EXPORTER_OTLP_ENDPOINT=`) must land on the disabled path,
/// not a build error against an empty URI.
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

/// `OTEL_METRIC_EXPORT_INTERVAL` is milliseconds per the OTel spec.
/// Defaults to 15s (not the SDK's own 60s default) per the dashboard-first
/// emission spec - the ingest/mount dead-satellite panels want a tighter
/// default cadence.
fn metric_export_interval() -> Duration {
    std::env::var(METRIC_INTERVAL_ENV)
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .filter(|v| *v > 0)
        .map(Duration::from_millis)
        .unwrap_or(Duration::from_secs(DEFAULT_METRIC_INTERVAL_SECS))
}

fn build_resource(service_name: &str, service_version: &str) -> Resource {
    let host_name = hostname::get()
        .ok()
        .and_then(|h| h.into_string().ok())
        .unwrap_or_else(|| "unknown".to_string());
    Resource::builder()
        .with_service_name(service_name.to_string())
        .with_attributes([
            KeyValue::new(
                opentelemetry_semantic_conventions::attribute::SERVICE_VERSION,
                service_version.to_string(),
            ),
            // HOST_NAME is gated behind opentelemetry-semantic-conventions's
            // `semconv_experimental` feature in 0.32; "host.name" is the
            // same stable resource attribute key, spelled directly rather
            // than pulling in the experimental feature for one constant.
            KeyValue::new("host.name", host_name),
        ])
        .build()
}

struct EnabledTelemetry {
    tracer: opentelemetry_sdk::trace::Tracer,
    tracer_provider: SdkTracerProvider,
    meter_provider: SdkMeterProvider,
    logger_provider: SdkLoggerProvider,
}

fn build_otel(
    service_name: &str,
    service_version: &str,
    endpoint: &str,
) -> anyhow::Result<EnabledTelemetry> {
    use anyhow::Context;

    let resource = build_resource(service_name, service_version);

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
    let tracer = tracer_provider.tracer(service_name.to_string());

    let metric_exporter = MetricExporter::builder()
        .with_tonic()
        .with_endpoint(endpoint)
        .build()
        .context("building OTLP metric exporter")?;
    let reader = PeriodicReader::builder(metric_exporter)
        .with_interval(metric_export_interval())
        .build();
    let meter_provider = SdkMeterProvider::builder()
        .with_resource(resource.clone())
        .with_reader(reader)
        .build();

    let log_exporter = LogExporter::builder()
        .with_tonic()
        .with_endpoint(endpoint)
        .build()
        .context("building OTLP log exporter")?;
    let logger_provider = SdkLoggerProvider::builder()
        .with_resource(resource)
        .with_batch_exporter(log_exporter)
        .build();

    Ok(EnabledTelemetry {
        tracer,
        tracer_provider,
        meter_provider,
        logger_provider,
    })
}

fn build_env_filter() -> tracing_subscriber::EnvFilter {
    tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "blackbox=info".into())
}

/// Install the global tracing subscriber and, when
/// `OTEL_EXPORTER_OTLP_ENDPOINT` is set, the OTLP export pipeline.
/// `file_writer` is the caller-owned rolling file appender (or any other
/// `MakeWriter`); stderr is always the second sink, matching the pre-otel
/// behavior exactly.
///
/// Must be called at most once per process - like any `tracing_subscriber`
/// global-default install, a second call panics. Daemon startup calls this
/// exactly once, before any other subsystem starts.
pub(crate) fn init<W>(
    service_name: &'static str,
    service_version: &str,
    file_writer: W,
) -> TelemetryGuard
where
    W: for<'w> tracing_subscriber::fmt::MakeWriter<'w> + Send + Sync + 'static,
{
    let Some(endpoint) = otlp_endpoint_from_env() else {
        tracing_subscriber::registry()
            .with(build_env_filter())
            .with(tracing_subscriber::fmt::layer().with_writer(std::io::stderr))
            .with(
                tracing_subscriber::fmt::layer()
                    .with_writer(file_writer)
                    .with_ansi(false),
            )
            .init();
        return TelemetryGuard::disabled();
    };

    let enabled = match build_otel(service_name, service_version, &endpoint) {
        Ok(enabled) => enabled,
        Err(err) => {
            // The global subscriber is not installed yet, so `tracing::warn!`
            // here would be silently dropped (default noop dispatcher) -
            // stderr is the only reachable sink at this point in startup.
            eprintln!(
                "blackboxd: otel init failed against endpoint {endpoint:?}: {err:#}; \
                 falling back to fmt-only logging"
            );
            tracing_subscriber::registry()
                .with(build_env_filter())
                .with(tracing_subscriber::fmt::layer().with_writer(std::io::stderr))
                .with(
                    tracing_subscriber::fmt::layer()
                        .with_writer(file_writer)
                        .with_ansi(false),
                )
                .init();
            return TelemetryGuard::disabled();
        }
    };

    let stderr_layer = tracing_subscriber::fmt::layer()
        .with_writer(std::io::stderr)
        .with_filter(build_env_filter());
    let file_layer = tracing_subscriber::fmt::layer()
        .with_writer(file_writer)
        .with_ansi(false)
        .with_filter(build_env_filter());
    let span_layer = tracing_opentelemetry::layer()
        .with_tracer(enabled.tracer)
        .with_filter(build_env_filter());
    // INFO and up regardless of RUST_LOG verbosity: an operator debugging
    // locally with RUST_LOG=debug should not also flood the OTLP logs
    // backend with debug volume every session.
    let log_bridge_layer =
        OpenTelemetryTracingBridge::new(&enabled.logger_provider).with_filter(LevelFilter::INFO);

    tracing_subscriber::registry()
        .with(stderr_layer)
        .with(file_layer)
        .with(span_layer)
        .with(log_bridge_layer)
        .init();

    global::set_tracer_provider(enabled.tracer_provider.clone());
    global::set_meter_provider(enabled.meter_provider.clone());

    tracing::info!(
        endpoint = %endpoint,
        service = service_name,
        "otel export enabled (traces + metrics + logs)"
    );

    crate::server::bbox_metrics::register_global_observable_gauges();

    TelemetryGuard {
        tracer_provider: Some(enabled.tracer_provider),
        meter_provider: Some(enabled.meter_provider),
        logger_provider: Some(enabled.logger_provider),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::util::TestEnvGuard;

    #[test]
    fn endpoint_from_env_treats_blank_as_unset() {
        let mut env = TestEnvGuard::new();
        env.set(ENDPOINT_ENV, "   ");
        assert_eq!(otlp_endpoint_from_env(), None);
        env.remove(ENDPOINT_ENV);
        assert_eq!(otlp_endpoint_from_env(), None);
        env.set(
            ENDPOINT_ENV,
            "http://lgtm.observability.svc.cluster.local:4317",
        );
        assert_eq!(
            otlp_endpoint_from_env(),
            Some("http://lgtm.observability.svc.cluster.local:4317".to_string())
        );
    }

    #[test]
    fn trace_sample_ratio_defaults_and_parses() {
        let mut env = TestEnvGuard::new();
        env.remove(TRACES_SAMPLER_ARG_ENV);
        assert_eq!(trace_sample_ratio(), DEFAULT_TRACE_SAMPLE_RATIO);
        env.set(TRACES_SAMPLER_ARG_ENV, "0.25");
        assert_eq!(trace_sample_ratio(), 0.25);
        env.set(TRACES_SAMPLER_ARG_ENV, "not-a-number");
        assert_eq!(trace_sample_ratio(), DEFAULT_TRACE_SAMPLE_RATIO);
    }

    #[test]
    fn metric_export_interval_defaults_to_15s_and_respects_env() {
        let mut env = TestEnvGuard::new();
        env.remove(METRIC_INTERVAL_ENV);
        assert_eq!(
            metric_export_interval(),
            Duration::from_secs(DEFAULT_METRIC_INTERVAL_SECS)
        );
        env.set(METRIC_INTERVAL_ENV, "5000");
        assert_eq!(metric_export_interval(), Duration::from_millis(5000));
    }

    /// A bogus (unreachable/malformed) endpoint must still let the daemon
    /// boot: `init` degrades to the fmt-only path and returns a disabled
    /// guard rather than panicking or blocking. `with_endpoint` on the
    /// tonic builder validates the URI at `build()` time without any
    /// network I/O, so this exercises the fallback branch synchronously.
    #[test]
    fn init_with_malformed_endpoint_falls_back_without_panicking() {
        let bogus = "not a valid uri \u{0}";
        let result = std::panic::catch_unwind(|| build_otel("blackboxd-test", "0.0.0-test", bogus));
        match result {
            Ok(build_result) => assert!(
                build_result.is_err(),
                "malformed endpoint must fail to build, not silently succeed"
            ),
            Err(_) => panic!("build_otel must not panic on a malformed endpoint"),
        }
    }
}
