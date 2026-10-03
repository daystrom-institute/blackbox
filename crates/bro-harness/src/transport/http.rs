//! Shared HTTP robustness for the transport clients: transient-error retry
//! with capped exponential backoff, honoring `Retry-After`. Mirrors
//! pg_recon's `ClaudeChatClient` retry policy.
//!
//! Server retry advice (`Retry-After`) is captured as a *deadline* measured
//! from receipt (codex `RetryAfter`), so passing it between callers (a
//! rejected WebSocket handshake, a WS retry, the WS→HTTP fallback, an in-band
//! stream failure) cannot restart or shorten the server's window. Advice
//! only sets the timing of the next attempt; it never extends the configured
//! retry budget, and quota/policy/auth failure codes stay terminal even when
//! they arrive on an otherwise-retryable status (OpenAI serves quota
//! exhaustion as 429).
//!
//! Retryable: connection/timeout errors, and HTTP 408/425/429/5xx. Permanent
//! 4xx (auth, bad request) are returned to the caller unretried. Tunables:
//! `BRO_HARNESS_MAX_RETRIES` (default 3), `BRO_HARNESS_HTTP_TIMEOUT_SECS`
//! (default 600), `BRO_HARNESS_STREAM_IDLE_SECS` (default 300) — max gap
//! between two SSE events before a streaming turn is treated as hung.

use std::time::Duration;

use reqwest::header::{HeaderMap, RETRY_AFTER};
use tokio::time::Instant;

const DEFAULT_MAX_RETRIES: u32 = 3;
const BASE_BACKOFF_MS: u64 = 500;
const MAX_BACKOFF_MS: u64 = 8_000;
const DEFAULT_STREAM_IDLE_SECS: u64 = 300;

/// The earliest time a server advised making a follow-up request. Captured
/// once at receipt so the advice survives being handed to another caller; a
/// scheduling pause between capture and use can only lengthen the observed
/// wait, never shorten the server's deadline.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetryAfter(Instant);

impl RetryAfter {
    /// Captures an interval measured from receipt of the server's advice.
    pub fn from_delay(delay: Duration) -> Option<Self> {
        Instant::now().checked_add(delay).map(Self)
    }

    /// Reads a `Retry-After` value (delay seconds or an HTTP-date).
    pub fn from_header(value: &str) -> Option<Self> {
        Self::from_delay(parse_retry_after(value.trim())?)
    }

    /// Reads the `Retry-After` header from a response, including the rejected
    /// handshake response of a WebSocket upgrade.
    pub fn from_headers(headers: &HeaderMap) -> Option<Self> {
        let value = headers.get(RETRY_AFTER)?.to_str().ok()?;
        Self::from_header(value)
    }

    /// The original deadline, for callers that pass the advice on or sleep
    /// until it.
    pub fn deadline(self) -> Instant {
        self.0
    }

    /// Remaining delay, or zero once the deadline has passed.
    pub fn remaining_delay(self) -> Duration {
        self.0.saturating_duration_since(Instant::now())
    }
}

/// Sleep until the server's advised deadline. A deadline already in the past
/// (or `Retry-After: 0`) returns immediately, so honoring stale advice never
/// adds latency.
pub async fn sleep_until_retry(after: RetryAfter) {
    tokio::time::sleep_until(after.deadline()).await;
}

/// Provider failure codes that are terminal no matter which HTTP status or
/// stream envelope carried them. Quota and spend-limit codes arrive on 429,
/// which the status table alone would happily retry until the budget is gone.
pub fn terminal_failure_code(code: &str) -> bool {
    matches!(
        code,
        "insufficient_quota"
            | "credit_balance_exhausted"
            | "organization_spend_limit_exceeded"
            | "project_spend_limit_exceeded"
            | "usage_not_included"
            | "invalid_request"
            | "invalid_prompt"
            | "invalid_api_key"
            | "unauthorized"
            | "cyber_policy"
            | "bio_policy"
            | "misalignment_policy_violation"
    )
}

/// The machine-readable failure code of a JSON error envelope body, if any.
/// Considers `error.type` when `error.code` is absent or null, matching the
/// HTTP error classifier, so a quota/policy envelope cannot retry just
/// because it spells its code in the `type` field.
pub fn json_error_code(body: &str) -> Option<String> {
    let parsed = serde_json::from_str::<serde_json::Value>(body).ok()?;
    let error = parsed.get("error")?;
    if let Some(code) = error.get("code").and_then(|code| code.as_str()) {
        return Some(code.to_string());
    }
    error
        .get("type")?
        .as_str()
        .filter(|kind| !kind.is_empty())
        .map(str::to_string)
}

/// Buffer a response body so a retry decision can read it without taking the
/// body away from the caller: returns a rebuilt response (same status,
/// headers and bytes) plus the decoded text. Only worth calling on responses
/// whose body is needed for classification, never on success paths.
async fn buffer_body(resp: reqwest::Response) -> reqwest::Result<(reqwest::Response, String)> {
    let status = resp.status();
    let headers = resp.headers().clone();
    let bytes = resp.bytes().await?;
    let text = String::from_utf8_lossy(&bytes).into_owned();
    let mut builder = http::Response::builder().status(status);
    for (name, value) in &headers {
        builder = builder.header(name.clone(), value.clone());
    }
    let rebuilt = reqwest::Response::from(
        builder
            .body(bytes.to_vec())
            .expect("status and headers came from a live response"),
    );
    Ok((rebuilt, text))
}

pub fn request_timeout() -> Duration {
    let secs = std::env::var("BRO_HARNESS_HTTP_TIMEOUT_SECS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(600);
    Duration::from_secs(secs)
}

/// Max idle gap between two SSE events before a streaming turn is abandoned as
/// hung. Codex uses a 5-minute idle timeout between Responses stream events; we
/// match that default. A whole-request timeout (`request_timeout`) can't catch a
/// connection that stays open but stops producing events.
pub fn stream_idle_timeout() -> Duration {
    let secs = std::env::var("BRO_HARNESS_STREAM_IDLE_SECS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(DEFAULT_STREAM_IDLE_SECS);
    Duration::from_secs(secs)
}

pub fn max_retries() -> u32 {
    std::env::var("BRO_HARNESS_MAX_RETRIES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(DEFAULT_MAX_RETRIES)
}

/// Deterministic capped exponential backoff: 500ms, 1s, 2s, 4s, 8s … capped at
/// `MAX_BACKOFF_MS`. `attempt` is 1-based. The public [`backoff`] adds jitter on
/// top of this.
fn backoff_base(attempt: u32) -> Duration {
    let ms = BASE_BACKOFF_MS
        .saturating_mul(1u64 << attempt.min(5).saturating_sub(1))
        .min(MAX_BACKOFF_MS);
    Duration::from_millis(ms)
}

/// Capped exponential backoff with ±20% jitter. Without jitter, a fleet that
/// trips a shared rate limit (429) at the same instant retries in lockstep and
/// re-thunders the provider on every wave. The jitter source is wall-clock
/// sub-second nanos — its only job is to decorrelate concurrent retriers, so
/// cryptographic quality is irrelevant and no `rand` dependency is needed. The
/// jittered value is re-capped at `MAX_BACKOFF_MS` so the hard ceiling holds.
pub fn backoff(attempt: u32) -> Duration {
    let base = backoff_base(attempt).as_millis() as f64;
    let factor = 0.8 + 0.4 * jitter_frac(); // [0.8, 1.2)
    let ms = ((base * factor) as u64).min(MAX_BACKOFF_MS);
    Duration::from_millis(ms)
}

/// A `[0.0, 1.0)` spread from wall-clock sub-second nanos — a dependency-free
/// jitter source for [`backoff`]. Not suitable for anything needing real
/// randomness; adjacent calls within the same nanosecond collide harmlessly.
fn jitter_frac() -> f64 {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    (nanos % 1_000) as f64 / 1_000.0
}

fn status_retryable(status: reqwest::StatusCode) -> bool {
    matches!(status.as_u16(), 408 | 425 | 429) || status.is_server_error()
}

fn err_retryable(e: &reqwest::Error) -> bool {
    e.is_timeout() || e.is_connect() || e.is_request()
}

/// `Retry-After` is either a non-negative number of seconds or an HTTP-date
/// (RFC 7231 §7.1.3). Parse both; clamp the date form to a non-negative delay.
fn parse_retry_after(v: &str) -> Option<Duration> {
    if let Ok(secs) = v.parse::<u64>() {
        return Some(Duration::from_secs(secs));
    }
    // HTTP-date form, e.g. "Wed, 21 Oct 2026 07:28:00 GMT".
    let when = chrono::DateTime::parse_from_rfc2822(v).ok()?;
    let delta = when.timestamp() - chrono::Utc::now().timestamp();
    Some(Duration::from_secs(delta.max(0) as u64))
}

/// Send a request with retry. `make` rebuilds + sends the request on each
/// attempt (request bodies are consumed per send). A non-success response
/// with a *non-retryable* status is returned as `Ok` for the caller to
/// surface; retryable statuses/errors are retried up to the cap, and a
/// retryable status whose body carries a terminal failure code (quota,
/// policy, auth) is returned unretried. `Retry-After` sets the sleep deadline
/// but never buys extra attempts.
pub async fn send_with_retry<F, Fut>(label: &str, make: F) -> reqwest::Result<reqwest::Response>
where
    F: Fn() -> Fut,
    Fut: std::future::Future<Output = reqwest::Result<reqwest::Response>>,
{
    send_with_retry_observed(label, make, |_| {}).await
}

/// Publish response deadlines before any body read or retry wait so callers
/// can retain server advice when the request future is cancelled.
pub async fn send_with_retry_observed<F, Fut, O>(
    label: &str,
    make: F,
    mut observe: O,
) -> reqwest::Result<reqwest::Response>
where
    F: Fn() -> Fut,
    Fut: std::future::Future<Output = reqwest::Result<reqwest::Response>>,
    O: FnMut(Option<RetryAfter>),
{
    let max = max_retries();
    let mut attempt = 0u32;
    loop {
        attempt += 1;
        match make().await {
            Ok(resp) => {
                observe(RetryAfter::from_headers(resp.headers()));
                let status = resp.status();
                if status.is_success() || !status_retryable(status) || attempt > max {
                    return Ok(resp);
                }
                // Classify the envelope before spending another attempt: a 429
                // carrying `insufficient_quota` is terminal.
                let (resp, body) = buffer_body(resp).await?;
                if let Some(code) = json_error_code(&body)
                    && terminal_failure_code(&code)
                {
                    tracing::warn!(
                        label,
                        attempt,
                        status = status.as_u16(),
                        code = %code,
                        "terminal provider failure code; not retrying"
                    );
                    return Ok(resp);
                }
                let advice = RetryAfter::from_headers(resp.headers());
                let wait = advice
                    .map(RetryAfter::remaining_delay)
                    .unwrap_or_else(|| backoff(attempt));
                tracing::warn!(
                    label,
                    attempt,
                    status = status.as_u16(),
                    wait_ms = wait.as_millis() as u64,
                    "transient HTTP status; retrying"
                );
                match advice {
                    Some(advice) => sleep_until_retry(advice).await,
                    None => tokio::time::sleep(wait).await,
                }
            }
            Err(e) => {
                if !err_retryable(&e) || attempt > max {
                    return Err(e);
                }
                let wait = backoff(attempt);
                tracing::warn!(
                    label, attempt, error = %e, wait_ms = wait.as_millis() as u64,
                    "transient HTTP error; retrying"
                );
                tokio::time::sleep(wait).await;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_base_is_capped_and_monotonic() {
        assert_eq!(backoff_base(1), Duration::from_millis(500));
        assert_eq!(backoff_base(2), Duration::from_millis(1000));
        assert_eq!(backoff_base(3), Duration::from_millis(2000));
        assert_eq!(backoff_base(4), Duration::from_millis(4000));
        assert!(backoff_base(20) <= Duration::from_millis(MAX_BACKOFF_MS));
    }

    #[test]
    fn backoff_jitter_stays_within_bounds_and_under_cap() {
        // Every jittered draw lands within ±20% of the base and never above the
        // hard ceiling. Sampled repeatedly since the jitter source is the clock.
        for attempt in 1..=6u32 {
            let base = backoff_base(attempt).as_millis() as u64;
            let lo = (base * 8) / 10; // 0.8x
            let hi = (base * 12) / 10; // 1.2x
            for _ in 0..50 {
                let b = backoff(attempt).as_millis() as u64;
                assert!(b >= lo.min(MAX_BACKOFF_MS), "attempt {attempt}: {b} < {lo}");
                assert!(b <= hi, "attempt {attempt}: {b} > {hi}");
                assert!(b <= MAX_BACKOFF_MS, "attempt {attempt}: {b} over cap");
            }
        }
    }

    #[test]
    fn retry_after_parses_seconds_and_http_date() {
        assert_eq!(parse_retry_after("12"), Some(Duration::from_secs(12)));
        // A far-past date clamps to zero rather than panicking/underflowing.
        assert_eq!(
            parse_retry_after("Wed, 21 Oct 2015 07:28:00 GMT"),
            Some(Duration::from_secs(0))
        );
        // A future date yields a positive delay.
        let future = parse_retry_after("Wed, 21 Oct 2099 07:28:00 GMT").unwrap();
        assert!(future > Duration::from_secs(0));
        assert_eq!(parse_retry_after("garbage"), None);
    }

    #[test]
    fn retry_after_deadline_is_captured_at_receipt_and_only_shrinks() {
        let advice = RetryAfter::from_header("2").expect("valid advice");
        let first = advice.remaining_delay();
        assert!(first <= Duration::from_secs(2));
        // Re-reading the same advice can only observe a shorter remaining
        // delay: the deadline is fixed at receipt, so handing the value to
        // another caller (WS retry → HTTP fallback) cannot restart the window.
        assert!(advice.remaining_delay() <= first);
        assert_eq!(advice.deadline(), advice.deadline());
        // Zero and expired advice stay immediately satisfiable.
        assert_eq!(
            RetryAfter::from_header("0").map(RetryAfter::remaining_delay),
            Some(Duration::from_secs(0))
        );
        assert_eq!(
            RetryAfter::from_header("Wed, 21 Oct 2015 07:28:00 GMT")
                .map(RetryAfter::remaining_delay),
            Some(Duration::from_secs(0))
        );
        assert!(RetryAfter::from_header("garbage").is_none());
    }

    #[test]
    fn quota_and_policy_codes_are_terminal_while_overload_is_not() {
        for code in [
            "insufficient_quota",
            "credit_balance_exhausted",
            "organization_spend_limit_exceeded",
            "project_spend_limit_exceeded",
            "usage_not_included",
            "invalid_request",
            "invalid_api_key",
            "cyber_policy",
            "bio_policy",
            "misalignment_policy_violation",
        ] {
            assert!(terminal_failure_code(code), "{code} must be terminal");
        }
        for code in [
            "server_is_overloaded",
            "rate_limit_exceeded",
            "slow_down",
            "",
        ] {
            assert!(!terminal_failure_code(code), "{code} stays retryable");
        }
    }

    #[test]
    fn json_error_code_reads_the_envelope_code() {
        assert_eq!(
            json_error_code(r#"{"error":{"code":"insufficient_quota","message":"nope"}}"#)
                .as_deref(),
            Some("insufficient_quota")
        );
        // `error.type` is honored when `error.code` is absent or null.
        assert_eq!(
            json_error_code(r#"{"error":{"type":"insufficient_quota","message":"nope"}}"#)
                .as_deref(),
            Some("insufficient_quota")
        );
        assert_eq!(
            json_error_code(r#"{"error":{"code":null,"type":"usage_not_included"}}"#).as_deref(),
            Some("usage_not_included")
        );
        assert_eq!(json_error_code(r#"{"error":{"message":"nope"}}"#), None);
        assert_eq!(json_error_code(r#"{"error":{"type":""}}"#), None);
        assert_eq!(json_error_code("not json"), None);
        assert!(
            terminal_failure_code(
                json_error_code(r#"{"error":{"type":"insufficient_quota"}}"#)
                    .as_deref()
                    .unwrap_or_default()
            ),
            "a type-only quota envelope must classify terminal"
        );
    }

    #[tokio::test]
    async fn buffered_body_preserves_status_headers_and_bytes_for_the_caller() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            let mut request = Vec::new();
            let mut chunk = [0; 4096];
            loop {
                let count = socket.read(&mut chunk).await.unwrap();
                if count == 0 {
                    break;
                }
                request.extend_from_slice(&chunk[..count]);
                if request.windows(4).any(|part| part == b"\r\n\r\n") {
                    break;
                }
            }
            let body = r#"{"error":{"code":"insufficient_quota","message":"spent"}}"#;
            let response = format!(
                "HTTP/1.1 429 Too Many Requests\r\nRetry-After: 3\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            socket.write_all(response.as_bytes()).await.unwrap();
        });
        let resp = reqwest::Client::new()
            .post(format!("http://{address}/responses"))
            .json(&serde_json::json!({}))
            .send()
            .await
            .unwrap();
        server.await.unwrap();
        assert_eq!(resp.status(), reqwest::StatusCode::TOO_MANY_REQUESTS);
        let (rebuilt, text) = buffer_body(resp).await.unwrap();
        assert_eq!(rebuilt.status(), reqwest::StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(
            RetryAfter::from_headers(rebuilt.headers()).map(|advice| advice.remaining_delay()),
            Some(Duration::from_secs(3))
        );
        assert_eq!(rebuilt.text().await.unwrap(), text);
        assert_eq!(
            json_error_code(&text).as_deref(),
            Some("insufficient_quota")
        );
    }

    #[test]
    fn classifies_retryable_statuses() {
        use reqwest::StatusCode;
        assert!(status_retryable(StatusCode::TOO_MANY_REQUESTS));
        assert!(status_retryable(StatusCode::BAD_GATEWAY));
        assert!(status_retryable(StatusCode::SERVICE_UNAVAILABLE));
        assert!(!status_retryable(StatusCode::UNAUTHORIZED));
        assert!(!status_retryable(StatusCode::BAD_REQUEST));
        assert!(!status_retryable(StatusCode::NOT_FOUND));
    }
}
