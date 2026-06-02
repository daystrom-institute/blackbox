//! Shared HTTP robustness for the transport clients: transient-error retry
//! with capped exponential backoff, honoring `Retry-After`. Mirrors
//! pg_recon's `ClaudeChatClient` retry policy.
//!
//! Retryable: connection/timeout errors, and HTTP 408/425/429/5xx. Permanent
//! 4xx (auth, bad request) are returned to the caller unretried. Tunables:
//! `BRO_HARNESS_MAX_RETRIES` (default 3), `BRO_HARNESS_HTTP_TIMEOUT_SECS`
//! (default 600), `BRO_HARNESS_STREAM_IDLE_SECS` (default 300) — max gap
//! between two SSE events before a streaming turn is treated as hung.

use std::time::Duration;

const DEFAULT_MAX_RETRIES: u32 = 3;
const BASE_BACKOFF_MS: u64 = 500;
const MAX_BACKOFF_MS: u64 = 8_000;
const DEFAULT_STREAM_IDLE_SECS: u64 = 300;

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

pub fn backoff(attempt: u32) -> Duration {
    // attempt is 1-based; 500ms, 1s, 2s, 4s … capped.
    let ms = BASE_BACKOFF_MS
        .saturating_mul(1u64 << attempt.min(5).saturating_sub(1))
        .min(MAX_BACKOFF_MS);
    Duration::from_millis(ms)
}

fn status_retryable(status: reqwest::StatusCode) -> bool {
    matches!(status.as_u16(), 408 | 425 | 429) || status.is_server_error()
}

fn err_retryable(e: &reqwest::Error) -> bool {
    e.is_timeout() || e.is_connect() || e.is_request()
}

fn retry_after(resp: &reqwest::Response) -> Option<Duration> {
    let v = resp
        .headers()
        .get(reqwest::header::RETRY_AFTER)?
        .to_str()
        .ok()?;
    parse_retry_after(v.trim())
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
/// surface; retryable statuses/errors are retried up to the cap.
pub async fn send_with_retry<F, Fut>(label: &str, make: F) -> reqwest::Result<reqwest::Response>
where
    F: Fn() -> Fut,
    Fut: std::future::Future<Output = reqwest::Result<reqwest::Response>>,
{
    let max = max_retries();
    let mut attempt = 0u32;
    loop {
        attempt += 1;
        match make().await {
            Ok(resp) => {
                let status = resp.status();
                if status.is_success() || !status_retryable(status) || attempt > max {
                    return Ok(resp);
                }
                let wait = retry_after(&resp).unwrap_or_else(|| backoff(attempt));
                tracing::warn!(
                    label,
                    attempt,
                    status = status.as_u16(),
                    wait_ms = wait.as_millis() as u64,
                    "transient HTTP status; retrying"
                );
                tokio::time::sleep(wait).await;
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
    fn backoff_is_capped_and_monotonic() {
        assert_eq!(backoff(1), Duration::from_millis(500));
        assert_eq!(backoff(2), Duration::from_millis(1000));
        assert_eq!(backoff(3), Duration::from_millis(2000));
        assert!(backoff(20) <= Duration::from_millis(MAX_BACKOFF_MS));
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
