//! Authenticated HTTP-fetch primitive.
//!
//! Shared by the workflow `http_json` op (per-node, per-tick) AND
//! the daemon-level poller (scheduled, out-of-band). Same shape
//! parses from the same `Value` argv — anything that can configure
//! one can configure the other. Composition over duplication.
//!
//! The primitive is *just* a fetch: build a request, send it, classify
//! the response. No templating, no var capture, no scheduling — those
//! are concerns of the caller (workflow runner does template render +
//! `OpEffect::SetVar`; poller does interval scheduling + dispatch).

use std::collections::HashMap;
use std::time::Duration;

use anyhow::{Result, anyhow, bail};
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum ResponseKind {
    /// Parse body as JSON; non-JSON body is an error.
    #[default]
    Json,
    /// Capture body verbatim as a `Value::String` (e.g. `.diff` URLs).
    Text,
    /// Try JSON, fall back to text on parse failure.
    Auto,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HttpFetchSpec {
    #[serde(default = "default_method")]
    pub method: String,
    pub url: String,
    #[serde(default)]
    pub headers: HashMap<String, String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body: Option<Value>,
    #[serde(default = "default_timeout_secs")]
    pub timeout_secs: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expect_status: Option<Vec<u16>>,
    #[serde(default)]
    pub response_kind: ResponseKind,
    #[serde(default = "default_allow_empty")]
    pub allow_empty_body: bool,
    /// Status-classified retry policy. Defaults are conservative
    /// (3 attempts, 500ms base backoff, 30s cap) and apply even when
    /// the caller doesn't mention `retry` at all; set `attempts: 1`
    /// to opt out entirely and preserve pre-retry behavior.
    #[serde(default)]
    pub retry: RetrySpec,
}

fn default_method() -> String {
    "GET".into()
}
fn default_timeout_secs() -> u64 {
    30
}
fn default_allow_empty() -> bool {
    true
}

fn default_retry_attempts() -> u32 {
    3
}
fn default_retry_base_ms() -> u64 {
    500
}
fn default_retry_max_ms() -> u64 {
    30_000
}

/// Retry policy for [`HttpFetchSpec::execute`]. `attempts` counts the
/// total number of tries (1 = no retry). Backoff is capped exponential:
/// `base_ms * 2^n`, clamped to `max_ms`. A `Retry-After` response header
/// (seconds form) overrides the computed backoff on 429/503, also
/// clamped to `max_ms`.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub struct RetrySpec {
    #[serde(default = "default_retry_attempts")]
    pub attempts: u32,
    #[serde(default = "default_retry_base_ms")]
    pub base_ms: u64,
    #[serde(default = "default_retry_max_ms")]
    pub max_ms: u64,
}

impl Default for RetrySpec {
    fn default() -> Self {
        RetrySpec {
            attempts: default_retry_attempts(),
            base_ms: default_retry_base_ms(),
            max_ms: default_retry_max_ms(),
        }
    }
}

/// The outcome of one fetch attempt, as seen by the retry classifier.
/// No network types here on purpose: keeps [`classify_retry`] a pure
/// function that unit tests can drive without a server.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FetchOutcome {
    /// The request never got a response (connect/send/timeout failure).
    Transport,
    /// A response came back with this HTTP status.
    Status(u16),
}

/// Decide whether attempt number `attempt` (0-indexed, already made)
/// should be followed by a retry, and if so, after how long.
///
/// Retryable outcomes: transport errors, and HTTP 429/502/503/504.
/// Everything else (including any status not in that set) returns
/// `None` unconditionally, non-retryable statuses keep exact
/// pre-retry behavior. `retry_after_secs`, when present, overrides the
/// computed backoff on 429/503 (per RFC 9110 semantics for those two
/// statuses); it is ignored for 502/504 and transport errors.
pub fn classify_retry(
    outcome: FetchOutcome,
    attempt: u32,
    retry: &RetrySpec,
    retry_after_secs: Option<u64>,
) -> Option<Duration> {
    if attempt + 1 >= retry.attempts.max(1) {
        return None;
    }
    let retryable = match outcome {
        FetchOutcome::Transport => true,
        FetchOutcome::Status(s) => matches!(s, 429 | 502 | 503 | 504),
    };
    if !retryable {
        return None;
    }
    let honors_retry_after = matches!(outcome, FetchOutcome::Status(429) | FetchOutcome::Status(503));
    let delay_ms = match (honors_retry_after, retry_after_secs) {
        (true, Some(secs)) => secs.saturating_mul(1000).min(retry.max_ms),
        _ => backoff_ms(attempt, retry.base_ms, retry.max_ms),
    };
    Some(Duration::from_millis(delay_ms))
}

/// `base_ms * 2^attempt`, clamped to `max_ms`. Shift is capped so a
/// pathologically large `attempt` can't overflow `u64`.
fn backoff_ms(attempt: u32, base_ms: u64, max_ms: u64) -> u64 {
    let shift = attempt.min(32);
    let factor = 1u64.checked_shl(shift).unwrap_or(u64::MAX);
    base_ms.saturating_mul(factor).min(max_ms)
}

#[derive(Debug, Clone)]
pub struct HttpFetchResult {
    #[allow(dead_code)] // Debug-formatted in log output
    pub status: u16,
    pub value: Value,
}

impl HttpFetchSpec {
    /// Parse from the loose `Value` argv shape used by both the workflow
    /// `http_json` op AND the poller spec's `source` field.
    /// Method defaults to GET; everything else is per-field default.
    /// Strict on field types — a header value that isn't a string fails
    /// loudly here rather than at fetch time.
    pub fn from_args(args: &Value) -> Result<Self> {
        let url = args
            .get("url")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow!("http fetch requires args.url"))?
            .to_string();
        let method = args
            .get("method")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
            .unwrap_or_else(default_method);
        let timeout_secs = args
            .get("timeout_secs")
            .and_then(|v| v.as_u64())
            .unwrap_or_else(default_timeout_secs);
        let expect_status = args
            .get("expect_status")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_u64().map(|n| n as u16))
                    .collect()
            });
        let allow_empty_body = args
            .get("allow_empty_body")
            .and_then(|v| v.as_bool())
            .unwrap_or_else(default_allow_empty);
        let response_kind = match args.get("response_kind").and_then(|v| v.as_str()) {
            Some("text") => ResponseKind::Text,
            Some("auto") => ResponseKind::Auto,
            Some("json") | None => ResponseKind::Json,
            Some(other) => {
                bail!("http fetch: invalid response_kind '{other}' (expected json|text|auto)")
            }
        };
        let mut headers = HashMap::new();
        if let Some(h) = args.get("headers").and_then(|v| v.as_object()) {
            for (k, v) in h {
                let vs = v
                    .as_str()
                    .ok_or_else(|| anyhow!("http fetch header '{k}' must be string"))?;
                headers.insert(k.clone(), vs.to_string());
            }
        }
        let body = args.get("body").cloned();
        let retry = match args.get("retry") {
            None | Some(Value::Null) => RetrySpec::default(),
            Some(Value::Object(_)) => {
                let v = args.get("retry").expect("checked Some above");
                let attempts = match v.get("attempts") {
                    None => default_retry_attempts(),
                    Some(x) => x.as_u64().ok_or_else(|| {
                        anyhow!("http fetch retry.attempts must be a non-negative integer")
                    })? as u32,
                };
                let base_ms = match v.get("base_ms") {
                    None => default_retry_base_ms(),
                    Some(x) => x
                        .as_u64()
                        .ok_or_else(|| anyhow!("http fetch retry.base_ms must be an integer"))?,
                };
                let max_ms = match v.get("max_ms") {
                    None => default_retry_max_ms(),
                    Some(x) => x
                        .as_u64()
                        .ok_or_else(|| anyhow!("http fetch retry.max_ms must be an integer"))?,
                };
                RetrySpec {
                    attempts: attempts.max(1),
                    base_ms,
                    max_ms,
                }
            }
            Some(_) => bail!("http fetch retry must be an object, e.g. {{\"attempts\": 3}}"),
        };
        Ok(HttpFetchSpec {
            method,
            url,
            headers,
            body,
            timeout_secs,
            expect_status,
            response_kind,
            allow_empty_body,
            retry,
        })
    }

    /// Execute the fetch; returns `(status, parsed value)`. Errors:
    /// invalid method, network failure, body-read failure, status
    /// outside the configured allow set, or a `Json`-mode response
    /// that wasn't actually JSON.
    ///
    /// Retries per `self.retry` on transport failure and on 429/502/
    /// 503/504 responses, with capped exponential backoff (honoring a
    /// seconds-form `Retry-After` header on 429/503). Every other
    /// failure mode (bad method, non-retryable status, non-JSON body,
    /// empty-body policy violation) returns on the first attempt,
    /// exactly as before retry support existed.
    pub async fn execute(&self) -> Result<HttpFetchResult> {
        let parsed_method = reqwest::Method::from_bytes(self.method.as_bytes())
            .map_err(|e| anyhow!("http fetch invalid method '{}': {e}", self.method))?;
        let mut attempt: u32 = 0;
        loop {
            match self.try_once(&parsed_method).await {
                Ok(result) => return Ok(result),
                Err(AttemptError::Fatal(e)) => return Err(e),
                Err(AttemptError::Transport(e)) => {
                    match classify_retry(FetchOutcome::Transport, attempt, &self.retry, None) {
                        Some(delay) => {
                            tokio::time::sleep(delay).await;
                            attempt += 1;
                        }
                        None => return Err(e),
                    }
                }
                Err(AttemptError::Status {
                    status,
                    retry_after_secs,
                    err,
                }) => {
                    match classify_retry(
                        FetchOutcome::Status(status),
                        attempt,
                        &self.retry,
                        retry_after_secs,
                    ) {
                        Some(delay) => {
                            tokio::time::sleep(delay).await;
                            attempt += 1;
                        }
                        None => return Err(err),
                    }
                }
            }
        }
    }

    /// One request attempt. Classifies the failure shape so `execute`'s
    /// retry loop can decide without re-parsing an anyhow message.
    async fn try_once(&self, parsed_method: &reqwest::Method) -> Result<HttpFetchResult, AttemptError> {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(self.timeout_secs))
            .build()
            .map_err(|e| AttemptError::Fatal(anyhow!("http client build: {e}")))?;
        let mut req = client.request(parsed_method.clone(), &self.url);
        for (k, v) in &self.headers {
            req = req.header(k, v);
        }
        if let Some(body) = &self.body {
            req = req.json(body);
        }
        let method = self.method.clone();
        let url = self.url.clone();
        let resp = req
            .send()
            .await
            .map_err(|e| AttemptError::Transport(anyhow!("http fetch {method} {url}: send: {e}")))?;
        let status = resp.status().as_u16();
        let allow = match &self.expect_status {
            Some(arr) => arr.contains(&status),
            None => (200..300).contains(&status),
        };
        let retry_after_secs = resp
            .headers()
            .get(reqwest::header::RETRY_AFTER)
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.trim().parse::<u64>().ok());
        let text = resp.text().await.map_err(|e| {
            AttemptError::Transport(anyhow!("http fetch {method} {url}: body: {e}"))
        })?;
        if !allow {
            let preview: String = text.chars().take(500).collect();
            let err = anyhow!("http fetch {method} {url}: HTTP {status}: {preview}");
            return Err(AttemptError::Status {
                status,
                retry_after_secs,
                err,
            });
        }
        let value = if text.trim().is_empty() {
            if !self.allow_empty_body {
                return Err(AttemptError::Fatal(anyhow!(
                    "http fetch {method} {url}: empty body but allow_empty_body=false"
                )));
            }
            Value::Null
        } else {
            match self.response_kind {
                ResponseKind::Text => Value::String(text),
                ResponseKind::Auto => serde_json::from_str(&text).unwrap_or(Value::String(text)),
                ResponseKind::Json => serde_json::from_str(&text).map_err(|e| {
                    let preview: String = text.chars().take(200).collect();
                    AttemptError::Fatal(anyhow!(
                        "http fetch {method} {url}: response not JSON: {e}: {preview}"
                    ))
                })?,
            }
        };
        Ok(HttpFetchResult { status, value })
    }
}

/// Internal per-attempt failure classification. Not exposed; callers
/// see the plain `anyhow::Error` `execute()` ultimately returns.
enum AttemptError {
    /// Never eligible for retry regardless of `retry` policy (bad
    /// method, non-JSON body, empty-body policy violation, …).
    Fatal(anyhow::Error),
    /// No response was received at all.
    Transport(anyhow::Error),
    /// A response came back but its status wasn't in the allow set.
    Status {
        status: u16,
        retry_after_secs: Option<u64>,
        err: anyhow::Error,
    },
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn from_args_defaults() {
        let s = HttpFetchSpec::from_args(&json!({"url": "http://x"})).unwrap();
        assert_eq!(s.method, "GET");
        assert_eq!(s.timeout_secs, 30);
        assert!(s.allow_empty_body);
        assert_eq!(s.response_kind, ResponseKind::Json);
        assert!(s.headers.is_empty());
        assert!(s.body.is_none());
        assert_eq!(s.retry, RetrySpec::default());
        assert_eq!(s.retry.attempts, 3);
        assert_eq!(s.retry.base_ms, 500);
        assert_eq!(s.retry.max_ms, 30_000);
    }

    #[test]
    fn from_args_full() {
        let s = HttpFetchSpec::from_args(&json!({
            "method": "POST",
            "url": "http://x",
            "headers": {"Authorization": "token abc"},
            "body": {"k": "v"},
            "timeout_secs": 5,
            "expect_status": [200, 201, 409],
            "response_kind": "text",
            "allow_empty_body": false
        }))
        .unwrap();
        assert_eq!(s.method, "POST");
        assert_eq!(s.timeout_secs, 5);
        assert!(!s.allow_empty_body);
        assert_eq!(s.response_kind, ResponseKind::Text);
        assert_eq!(s.expect_status, Some(vec![200, 201, 409]));
        assert_eq!(s.headers.get("Authorization").unwrap(), "token abc");
        assert_eq!(s.body, Some(json!({"k": "v"})));
    }

    #[test]
    fn from_args_url_required() {
        let err = HttpFetchSpec::from_args(&json!({"method": "GET"})).unwrap_err();
        assert!(format!("{err}").contains("args.url"));
    }

    #[test]
    fn from_args_invalid_response_kind() {
        let err = HttpFetchSpec::from_args(&json!({"url": "http://x", "response_kind": "yaml"}))
            .unwrap_err();
        assert!(format!("{err}").contains("response_kind"));
    }

    #[test]
    fn from_args_non_string_header_rejected() {
        let err =
            HttpFetchSpec::from_args(&json!({"url": "http://x", "headers": {"X": 7}})).unwrap_err();
        assert!(format!("{err}").contains("must be string"));
    }

    #[test]
    fn from_args_retry_partial_override() {
        let s = HttpFetchSpec::from_args(&json!({
            "url": "http://x",
            "retry": {"attempts": 5}
        }))
        .unwrap();
        assert_eq!(s.retry.attempts, 5);
        // Unspecified fields keep their conservative defaults.
        assert_eq!(s.retry.base_ms, 500);
        assert_eq!(s.retry.max_ms, 30_000);
    }

    #[test]
    fn from_args_retry_full_override() {
        let s = HttpFetchSpec::from_args(&json!({
            "url": "http://x",
            "retry": {"attempts": 1, "base_ms": 100, "max_ms": 1_000}
        }))
        .unwrap();
        assert_eq!(s.retry.attempts, 1);
        assert_eq!(s.retry.base_ms, 100);
        assert_eq!(s.retry.max_ms, 1_000);
    }

    #[test]
    fn from_args_retry_attempts_zero_clamped_to_one() {
        let s = HttpFetchSpec::from_args(&json!({
            "url": "http://x",
            "retry": {"attempts": 0}
        }))
        .unwrap();
        assert_eq!(s.retry.attempts, 1);
    }

    #[test]
    fn from_args_retry_not_object_rejected() {
        let err = HttpFetchSpec::from_args(&json!({"url": "http://x", "retry": "yes"}))
            .unwrap_err();
        assert!(format!("{err}").contains("retry must be an object"));
    }

    #[test]
    fn from_args_retry_bad_field_type_rejected() {
        let err = HttpFetchSpec::from_args(&json!({
            "url": "http://x",
            "retry": {"attempts": "three"}
        }))
        .unwrap_err();
        assert!(format!("{err}").contains("retry.attempts"));
    }

    // ── classify_retry: pure, no network ─────────────────────────

    fn retry(attempts: u32) -> RetrySpec {
        RetrySpec {
            attempts,
            base_ms: 500,
            max_ms: 30_000,
        }
    }

    #[test]
    fn classify_retry_transport_error_retries() {
        let d = classify_retry(FetchOutcome::Transport, 0, &retry(3), None);
        assert_eq!(d, Some(Duration::from_millis(500)));
    }

    #[test]
    fn classify_retry_status_429_retries() {
        let d = classify_retry(FetchOutcome::Status(429), 0, &retry(3), None);
        assert_eq!(d, Some(Duration::from_millis(500)));
    }

    #[test]
    fn classify_retry_status_502_503_504_retry() {
        for status in [502u16, 503, 504] {
            let d = classify_retry(FetchOutcome::Status(status), 0, &retry(3), None);
            assert!(d.is_some(), "status {status} should be retryable");
        }
    }

    #[test]
    fn classify_retry_non_retryable_status_never_retries() {
        for status in [200u16, 400, 401, 404, 409, 500, 501] {
            let d = classify_retry(FetchOutcome::Status(status), 0, &retry(3), None);
            assert_eq!(d, None, "status {status} must not retry");
        }
    }

    #[test]
    fn classify_retry_exhausted_attempts_stops() {
        // attempts=3 means generations 0,1,2 are used; after generation 1
        // (the 2nd attempt, 0-indexed) there is exactly one attempt left,
        // and after generation 2 (the 3rd attempt) none remain.
        assert!(classify_retry(FetchOutcome::Status(503), 0, &retry(3), None).is_some());
        assert!(classify_retry(FetchOutcome::Status(503), 1, &retry(3), None).is_some());
        assert_eq!(
            classify_retry(FetchOutcome::Status(503), 2, &retry(3), None),
            None
        );
    }

    #[test]
    fn classify_retry_attempts_one_opts_out() {
        assert_eq!(
            classify_retry(FetchOutcome::Status(503), 0, &retry(1), None),
            None
        );
        assert_eq!(
            classify_retry(FetchOutcome::Transport, 0, &retry(1), None),
            None
        );
    }

    #[test]
    fn classify_retry_backoff_doubles_and_caps() {
        let r = RetrySpec {
            attempts: 10,
            base_ms: 500,
            max_ms: 5_000,
        };
        assert_eq!(
            classify_retry(FetchOutcome::Status(503), 0, &r, None),
            Some(Duration::from_millis(500))
        );
        assert_eq!(
            classify_retry(FetchOutcome::Status(503), 1, &r, None),
            Some(Duration::from_millis(1_000))
        );
        assert_eq!(
            classify_retry(FetchOutcome::Status(503), 2, &r, None),
            Some(Duration::from_millis(2_000))
        );
        // 4th generation would be 4000ms uncapped, still under 5000 cap.
        assert_eq!(
            classify_retry(FetchOutcome::Status(503), 3, &r, None),
            Some(Duration::from_millis(4_000))
        );
        // 5th generation would be 8000ms uncapped; clamped to max_ms.
        assert_eq!(
            classify_retry(FetchOutcome::Status(503), 4, &r, None),
            Some(Duration::from_millis(5_000))
        );
    }

    #[test]
    fn classify_retry_honors_retry_after_on_429_and_503() {
        let r = retry(3);
        assert_eq!(
            classify_retry(FetchOutcome::Status(429), 0, &r, Some(2)),
            Some(Duration::from_millis(2_000))
        );
        assert_eq!(
            classify_retry(FetchOutcome::Status(503), 0, &r, Some(7)),
            Some(Duration::from_millis(7_000))
        );
    }

    #[test]
    fn classify_retry_ignores_retry_after_on_502_and_504() {
        let r = retry(3);
        // 502/504 don't carry Retry-After semantics per the spec; even if
        // a caller somehow passed a value, backoff (not the header) wins.
        assert_eq!(
            classify_retry(FetchOutcome::Status(502), 0, &r, Some(99)),
            Some(Duration::from_millis(500))
        );
        assert_eq!(
            classify_retry(FetchOutcome::Status(504), 0, &r, Some(99)),
            Some(Duration::from_millis(500))
        );
    }

    #[test]
    fn classify_retry_after_capped_at_max_ms() {
        let r = RetrySpec {
            attempts: 3,
            base_ms: 500,
            max_ms: 10_000,
        };
        assert_eq!(
            classify_retry(FetchOutcome::Status(429), 0, &r, Some(3_600)),
            Some(Duration::from_millis(10_000))
        );
    }
}
