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
    /// Explicit retry policy override. `None` (the default when the
    /// caller doesn't mention `retry` at all) means "apply the
    /// method-based default" - see [`default_retry_for_method`]: GET/
    /// HEAD/PUT/DELETE (idempotent) get conservative retries (3
    /// attempts, 500ms base backoff, 30s cap); POST/PATCH/anything else
    /// (not safely retryable - a transient failure after the server
    /// already applied the side effect must not replay it) get
    /// `attempts: 1` (no retry) UNLESS the caller sets this field
    /// explicitly, which is treated as an informed opt-in and honored
    /// exactly regardless of method.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retry: Option<RetrySpec>,
}

impl HttpFetchSpec {
    /// Never expose resolved credentials, opaque URL components, or request bodies.
    pub fn response_view(&self, detail: bool) -> Value {
        let origin = reqwest::Url::parse(&self.url)
            .ok()
            .filter(|url| matches!(url.scheme(), "http" | "https"))
            .map(|url| url.origin().ascii_serialization());
        let mut row = serde_json::json!({"method": self.method, "endpoint_origin": origin});
        if detail {
            let mut header_names: Vec<_> = self.headers.keys().collect();
            header_names.sort();
            row["header_names"] = serde_json::json!(header_names);
            row["body_configured"] = serde_json::json!(self.body.is_some());
            row["timeout_secs"] = serde_json::json!(self.timeout_secs);
            row["response_kind"] = serde_json::json!(self.response_kind);
            row["allow_empty_body"] = serde_json::json!(self.allow_empty_body);
            if let Some(status) = &self.expect_status {
                row["expect_status"] = serde_json::json!(status);
            }
            if let Some(retry) = &self.retry {
                row["retry"] = serde_json::json!(retry);
            }
        }
        row
    }
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

/// Hard ceiling on `retry.attempts`, independent of what a caller asks
/// for. Guards against a runaway retry loop from a typo'd or malicious
/// spec (`"attempts": 999999999`).
const MAX_RETRY_ATTEMPTS: u32 = 10;

/// Hard cap on any single computed delay (backoff step or honored
/// `Retry-After`), in milliseconds. Also the per-request TOTAL delay
/// budget enforced by [`HttpFetchSpec::execute`]'s retry loop - once
/// cumulative sleep across all retries would exceed this, the loop
/// stops and surfaces the last error instead of continuing to wait.
const MAX_TOTAL_DELAY_MS: u64 = 5 * 60 * 1000; // 5 minutes

/// HTTP methods considered idempotent for retry-default purposes (safe
/// to replay a transient failure without risking a duplicate side
/// effect). Deliberately conservative: only methods every popular
/// HTTP client already retries by default.
fn is_idempotent_method(method: &str) -> bool {
    matches!(
        method.to_ascii_uppercase().as_str(),
        "GET" | "HEAD" | "PUT" | "DELETE"
    )
}

/// The retry policy applied when a caller doesn't set `HttpFetchSpec.retry`
/// at all. Idempotent methods get the conservative default (3/500ms/30s);
/// everything else (POST, PATCH, and any method this primitive doesn't
/// recognize) gets `attempts: 1` - no implicit retry, because retrying a
/// non-idempotent request on a transient failure can execute it twice.
fn default_retry_for_method(method: &str) -> RetrySpec {
    if is_idempotent_method(method) {
        RetrySpec::default()
    } else {
        RetrySpec {
            attempts: 1,
            base_ms: default_retry_base_ms(),
            max_ms: default_retry_max_ms(),
        }
    }
}

/// Clamp a caller-supplied `retry.attempts` value into `[1, MAX_RETRY_ATTEMPTS]`.
/// `raw` comes from a JSON `u64`; an out-of-`u32`-range value saturates to
/// `u32::MAX` before clamping down, so an absurd number (`"attempts":
/// 99999999999`) degrades to the ceiling instead of silently truncating
/// via `as u32` (which could wrap to something small and misleading).
fn clamp_retry_attempts(raw: u64) -> u32 {
    u32::try_from(raw)
        .unwrap_or(u32::MAX)
        .clamp(1, MAX_RETRY_ATTEMPTS)
}

/// Clamp a caller-supplied `retry.base_ms` / `retry.max_ms` value into
/// `[1, MAX_TOTAL_DELAY_MS]`. Rejects zero (which would defeat backoff -
/// every retry firing immediately) by flooring to 1ms, and rejects an
/// absurdly large single delay by capping it at the same ceiling as the
/// total-delay budget (a single step can't need to exceed the total cap).
fn clamp_retry_delay_ms(raw: u64) -> u64 {
    raw.clamp(1, MAX_TOTAL_DELAY_MS)
}

/// Retry policy for [`HttpFetchSpec::execute`]. `attempts` counts the
/// total number of tries (1 = no retry), clamped to
/// `[1, MAX_RETRY_ATTEMPTS]`. Backoff is capped exponential:
/// `base_ms * 2^n`, clamped to `max_ms`. A `Retry-After` response header
/// (delta-seconds or HTTP-date form) overrides the computed backoff on
/// 429/503, also clamped to `max_ms`. `base_ms` and `max_ms` are each
/// clamped to `[1, MAX_TOTAL_DELAY_MS]`.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub struct RetrySpec {
    #[serde(default = "default_retry_attempts")]
    pub attempts: u32,
    #[serde(default = "default_retry_base_ms")]
    pub base_ms: u64,
    #[serde(default = "default_retry_max_ms")]
    pub max_ms: u64,
}

impl RetrySpec {
    /// Clamp every field into operational bounds. `execute` applies
    /// this to the EFFECTIVE policy regardless of how the spec was
    /// constructed: `from_args` clamps at parse time, but serde paths
    /// (pollers deserialize `HttpFetchSpec` directly inside
    /// `PollerSpec`) would otherwise carry `attempts: u32::MAX` /
    /// `base_ms: 0` straight into the retry loop.
    pub fn normalized(&self) -> RetrySpec {
        RetrySpec {
            attempts: clamp_retry_attempts(u64::from(self.attempts)),
            base_ms: clamp_retry_delay_ms(self.base_ms),
            max_ms: clamp_retry_delay_ms(self.max_ms),
        }
    }
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
    let honors_retry_after = matches!(
        outcome,
        FetchOutcome::Status(429) | FetchOutcome::Status(503)
    );
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

/// Whether adding `next_delay` to `total_so_far_ms` would exceed the
/// [`MAX_TOTAL_DELAY_MS`] cumulative retry budget. Pure so it's unit
/// testable without driving the async retry loop.
fn exceeds_total_delay_budget(total_so_far_ms: u64, next_delay: Duration) -> bool {
    let next_ms = next_delay.as_millis().min(u128::from(u64::MAX)) as u64;
    total_so_far_ms.saturating_add(next_ms) > MAX_TOTAL_DELAY_MS
}

/// Parse a `Retry-After` header value in either accepted HTTP form:
/// delta-seconds (`"120"`) or an HTTP-date (RFC 7231 IMF-fixdate, e.g.
/// `"Wed, 21 Oct 2015 07:28:00 GMT"`). A date in the past clamps to `0`
/// (the retry is due immediately) rather than being rejected. A value
/// that parses as neither form returns `None` so the caller falls back
/// to computed backoff - a malformed header must degrade to "ignore it",
/// never to "wait zero seconds" (that would turn a broken header into a
/// retry storm) or "reject the response" (that would turn cosmetic
/// header noise into a hard failure).
fn parse_retry_after(value: &str) -> Option<u64> {
    let trimmed = value.trim();
    if let Ok(secs) = trimmed.parse::<u64>() {
        return Some(secs);
    }
    let target = chrono::DateTime::parse_from_rfc2822(trimmed).ok()?;
    let now = chrono::Utc::now();
    let delta_secs = target
        .with_timezone(&chrono::Utc)
        .signed_duration_since(now)
        .num_seconds();
    Some(delta_secs.max(0) as u64)
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
        // `retry` stays `None` unless the caller explicitly declares the
        // key: `None` means "apply the method-based default at execute
        // time" (see `default_retry_for_method`), while `Some(_)` - even
        // one that just repeats the conservative defaults - is an
        // explicit, honored-as-given policy. This is what lets a POST
        // opt into retries: declaring `"retry": {}` (or any subset of
        // fields) is the informed-consent signal, independent of method.
        let retry: Option<RetrySpec> = match args.get("retry") {
            None | Some(Value::Null) => None,
            Some(Value::Object(v)) => {
                let attempts = match v.get("attempts") {
                    None => default_retry_attempts(),
                    Some(x) => {
                        let raw = x.as_u64().ok_or_else(|| {
                            anyhow!("http fetch retry.attempts must be a non-negative integer")
                        })?;
                        clamp_retry_attempts(raw)
                    }
                };
                let base_ms = match v.get("base_ms") {
                    None => default_retry_base_ms(),
                    Some(x) => {
                        let raw = x.as_u64().ok_or_else(|| {
                            anyhow!("http fetch retry.base_ms must be an integer")
                        })?;
                        clamp_retry_delay_ms(raw)
                    }
                };
                let max_ms = match v.get("max_ms") {
                    None => default_retry_max_ms(),
                    Some(x) => {
                        let raw = x
                            .as_u64()
                            .ok_or_else(|| anyhow!("http fetch retry.max_ms must be an integer"))?;
                        clamp_retry_delay_ms(raw)
                    }
                };
                Some(RetrySpec {
                    attempts,
                    base_ms,
                    max_ms,
                })
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
    /// Retries per the effective retry policy (`self.retry` if the
    /// caller set it explicitly, else [`default_retry_for_method`]) on
    /// transport failure and on 429/502/503/504 responses, with capped
    /// exponential backoff (honoring a `Retry-After` header, delta-
    /// seconds or HTTP-date, on 429/503). Cumulative sleep across all
    /// retries is capped at [`MAX_TOTAL_DELAY_MS`]; once adding the next
    /// delay would exceed it, the loop stops and surfaces the triggering
    /// error instead of continuing to wait. Every other failure mode
    /// (bad method, non-retryable status, non-JSON body, empty-body
    /// policy violation) returns on the first attempt, exactly as before
    /// retry support existed.
    pub async fn execute(&self) -> Result<HttpFetchResult> {
        let parsed_method = reqwest::Method::from_bytes(self.method.as_bytes())
            .map_err(|e| anyhow!("http fetch invalid method '{}': {e}", self.method))?;
        // normalized(): the single choke point for retry bounds. Specs
        // built through from_args are already clamped, but serde-built
        // specs (poller installs) are not, and unbounded attempts with
        // zero delay would grind a failing endpoint without ever
        // consuming the delay budget.
        let retry = self
            .retry
            .unwrap_or_else(|| default_retry_for_method(&self.method))
            .normalized();
        let mut attempt: u32 = 0;
        let mut total_delay_ms: u64 = 0;
        loop {
            let (outcome, retry_after_secs, err) = match self.try_once(&parsed_method).await {
                Ok(result) => return Ok(result),
                Err(AttemptError::Fatal(e)) => return Err(e),
                Err(AttemptError::Transport(e)) => (FetchOutcome::Transport, None, e),
                Err(AttemptError::Status {
                    status,
                    retry_after_secs,
                    err,
                }) => (FetchOutcome::Status(status), retry_after_secs, err),
            };
            let Some(delay) = classify_retry(outcome, attempt, &retry, retry_after_secs) else {
                return Err(err);
            };
            if exceeds_total_delay_budget(total_delay_ms, delay) {
                return Err(err.context(format!(
                    "retry budget exhausted: cumulative backoff would exceed the {MAX_TOTAL_DELAY_MS}ms cap"
                )));
            }
            total_delay_ms += delay.as_millis() as u64;
            tokio::time::sleep(delay).await;
            attempt += 1;
        }
    }

    /// One request attempt. Classifies the failure shape so `execute`'s
    /// retry loop can decide without re-parsing an anyhow message.
    async fn try_once(
        &self,
        parsed_method: &reqwest::Method,
    ) -> Result<HttpFetchResult, AttemptError> {
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
        let resp = req.send().await.map_err(|e| {
            AttemptError::Transport(anyhow!("http fetch {method} {url}: send: {e}"))
        })?;
        let status = resp.status().as_u16();
        let allow = match &self.expect_status {
            Some(arr) => arr.contains(&status),
            None => (200..300).contains(&status),
        };
        let retry_after_secs = resp
            .headers()
            .get(reqwest::header::RETRY_AFTER)
            .and_then(|v| v.to_str().ok())
            .and_then(parse_retry_after);
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
        // No explicit `retry` key: stays None, meaning "apply the
        // method-based default at execute() time" rather than baking in
        // a policy at parse time.
        assert_eq!(s.retry, None);
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
        let r = s.retry.expect("explicit retry key must produce Some");
        assert_eq!(r.attempts, 5);
        // Unspecified fields keep their conservative defaults.
        assert_eq!(r.base_ms, 500);
        assert_eq!(r.max_ms, 30_000);
    }

    #[test]
    fn from_args_retry_full_override() {
        let s = HttpFetchSpec::from_args(&json!({
            "url": "http://x",
            "retry": {"attempts": 1, "base_ms": 100, "max_ms": 1_000}
        }))
        .unwrap();
        let r = s.retry.unwrap();
        assert_eq!(r.attempts, 1);
        assert_eq!(r.base_ms, 100);
        assert_eq!(r.max_ms, 1_000);
    }

    #[test]
    fn from_args_retry_empty_object_is_explicit_opt_in() {
        // `"retry": {}` is still an explicit declaration (Some), distinct
        // from omitting the key entirely (None) - this is the mechanism
        // a POST/PATCH caller uses to opt into the conservative defaults.
        let s = HttpFetchSpec::from_args(&json!({
            "url": "http://x",
            "method": "POST",
            "retry": {}
        }))
        .unwrap();
        assert_eq!(s.retry, Some(RetrySpec::default()));
    }

    #[test]
    fn from_args_retry_attempts_zero_clamped_to_one() {
        let s = HttpFetchSpec::from_args(&json!({
            "url": "http://x",
            "retry": {"attempts": 0}
        }))
        .unwrap();
        assert_eq!(s.retry.unwrap().attempts, 1);
    }

    #[test]
    fn from_args_retry_attempts_clamped_to_ceiling() {
        let s = HttpFetchSpec::from_args(&json!({
            "url": "http://x",
            "retry": {"attempts": 999_999}
        }))
        .unwrap();
        assert_eq!(s.retry.unwrap().attempts, MAX_RETRY_ATTEMPTS);
    }

    #[test]
    fn from_args_retry_delay_ms_clamped_to_ceiling() {
        let s = HttpFetchSpec::from_args(&json!({
            "url": "http://x",
            "retry": {"base_ms": 999_999_999, "max_ms": 999_999_999}
        }))
        .unwrap();
        let r = s.retry.unwrap();
        assert_eq!(r.base_ms, MAX_TOTAL_DELAY_MS);
        assert_eq!(r.max_ms, MAX_TOTAL_DELAY_MS);
    }

    #[test]
    fn from_args_retry_delay_ms_zero_clamped_to_one() {
        let s = HttpFetchSpec::from_args(&json!({
            "url": "http://x",
            "retry": {"base_ms": 0, "max_ms": 0}
        }))
        .unwrap();
        let r = s.retry.unwrap();
        assert_eq!(r.base_ms, 1);
        assert_eq!(r.max_ms, 1);
    }

    #[test]
    fn from_args_retry_not_object_rejected() {
        let err =
            HttpFetchSpec::from_args(&json!({"url": "http://x", "retry": "yes"})).unwrap_err();
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

    // ── method-based retry defaults ───────────────────────────────

    #[test]
    fn idempotent_methods_get_conservative_default() {
        for method in ["GET", "HEAD", "PUT", "DELETE", "get", "Put"] {
            let r = default_retry_for_method(method);
            assert_eq!(r, RetrySpec::default(), "method {method}");
            assert_eq!(r.attempts, 3);
        }
    }

    #[test]
    fn non_idempotent_methods_default_to_no_retry() {
        for method in ["POST", "PATCH", "post", "CONNECT", "TRACE", "WEIRD"] {
            let r = default_retry_for_method(method);
            assert_eq!(r.attempts, 1, "method {method} must default to no retry");
        }
    }

    #[test]
    fn execute_uses_method_default_when_retry_unset() {
        // No `.retry` set at all (None) - unwrap_or_else must compute
        // the method-appropriate default, not silently retry a POST.
        let get_spec =
            HttpFetchSpec::from_args(&json!({"url": "http://x", "method": "GET"})).unwrap();
        assert_eq!(get_spec.retry, None);
        assert_eq!(
            get_spec
                .retry
                .unwrap_or_else(|| default_retry_for_method(&get_spec.method)),
            RetrySpec::default()
        );

        let post_spec =
            HttpFetchSpec::from_args(&json!({"url": "http://x", "method": "POST"})).unwrap();
        assert_eq!(post_spec.retry, None);
        assert_eq!(
            post_spec
                .retry
                .unwrap_or_else(|| default_retry_for_method(&post_spec.method))
                .attempts,
            1
        );
    }

    #[test]
    fn explicit_retry_on_post_overrides_the_no_retry_default() {
        let s = HttpFetchSpec::from_args(&json!({
            "url": "http://x",
            "method": "POST",
            "retry": {"attempts": 4}
        }))
        .unwrap();
        assert_eq!(
            s.retry
                .unwrap_or_else(|| default_retry_for_method(&s.method))
                .attempts,
            4
        );
    }

    // ── Retry-After parsing: pure, no network ──────────────────────

    #[test]
    fn parse_retry_after_delta_seconds() {
        assert_eq!(parse_retry_after("120"), Some(120));
        assert_eq!(parse_retry_after("  7 "), Some(7));
        assert_eq!(parse_retry_after("0"), Some(0));
    }

    #[test]
    fn parse_retry_after_http_date_future_and_past() {
        let now = chrono::Utc::now();
        let future = now + chrono::Duration::seconds(60);
        let future_str = future.to_rfc2822();
        let secs = parse_retry_after(&future_str).expect("future HTTP-date should parse");
        // Allow slack for wall-clock drift between constructing `future`
        // and `parse_retry_after` calling `Utc::now()` again.
        assert!((55..=65).contains(&secs), "got {secs}");

        let past = now - chrono::Duration::seconds(60);
        let past_str = past.to_rfc2822();
        assert_eq!(
            parse_retry_after(&past_str),
            Some(0),
            "a past HTTP-date must clamp to zero, not go negative"
        );
    }

    #[test]
    fn parse_retry_after_malformed_falls_back_to_none() {
        assert_eq!(parse_retry_after("not-a-date-or-number"), None);
        assert_eq!(parse_retry_after(""), None);
        assert_eq!(parse_retry_after("-5"), None);
    }

    // ── total delay budget: pure, no network ───────────────────────

    #[test]
    fn exceeds_total_delay_budget_under_cap_is_false() {
        assert!(!exceeds_total_delay_budget(0, Duration::from_millis(1_000)));
        assert!(!exceeds_total_delay_budget(
            MAX_TOTAL_DELAY_MS - 1,
            Duration::from_millis(1)
        ));
    }

    #[test]
    fn exceeds_total_delay_budget_over_cap_is_true() {
        assert!(exceeds_total_delay_budget(
            MAX_TOTAL_DELAY_MS,
            Duration::from_millis(1)
        ));
        assert!(exceeds_total_delay_budget(
            0,
            Duration::from_millis(MAX_TOTAL_DELAY_MS + 1)
        ));
    }

    #[test]
    fn exceeds_total_delay_budget_does_not_overflow_on_huge_delay() {
        assert!(exceeds_total_delay_budget(0, Duration::from_secs(u64::MAX)));
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

    // ── live counting-server tests: proves the method-based retry
    // default end to end, not just via the pure helpers above ─────

    /// Spawn an axum server on an ephemeral port that answers every
    /// request (any method) from a fixed response script, indexed by
    /// hit count (the last entry repeats once the script is exhausted).
    /// Returns the base URL, a shared hit counter, and the server task
    /// handle (aborted on drop by the caller going out of scope is NOT
    /// automatic — callers hold `_handle` for the test's lifetime so the
    /// task stays alive, and the process exiting reclaims it; these are
    /// short-lived unit tests, not long-running services).
    async fn spawn_scripted_server(
        script: Vec<(u16, Option<String>)>,
    ) -> (
        String,
        std::sync::Arc<std::sync::atomic::AtomicUsize>,
        tokio::task::JoinHandle<()>,
    ) {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicUsize, Ordering};

        let hits = Arc::new(AtomicUsize::new(0));
        let script = Arc::new(script);
        let hits_for_handler = hits.clone();
        let handler = move || {
            let hits = hits_for_handler.clone();
            let script = script.clone();
            async move {
                let n = hits.fetch_add(1, Ordering::SeqCst);
                let (status, retry_after) = script
                    .get(n)
                    .cloned()
                    .unwrap_or_else(|| script.last().cloned().unwrap());
                let code = axum::http::StatusCode::from_u16(status)
                    .unwrap_or(axum::http::StatusCode::INTERNAL_SERVER_ERROR);
                let mut builder = axum::http::Response::builder().status(code);
                if let Some(ra) = retry_after {
                    builder = builder.header("retry-after", ra);
                }
                builder
                    .body(axum::body::Body::from("{}"))
                    .expect("response builds")
            }
        };
        let app = axum::Router::new().route("/probe", axum::routing::any(handler));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server_handle = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        (
            format!("http://127.0.0.1:{}/probe", addr.port()),
            hits,
            server_handle,
        )
    }

    #[tokio::test]
    async fn execute_post_default_does_not_retry_on_transient_failure() {
        let (url, hits, _handle) = spawn_scripted_server(vec![(503, None)]).await;
        let spec = HttpFetchSpec::from_args(&json!({"url": url, "method": "POST"})).unwrap();
        let err = spec.execute().await.unwrap_err();
        assert!(format!("{err}").contains("503"));
        assert_eq!(
            hits.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "a non-idempotent POST must not retry without explicit opt-in"
        );
    }

    #[tokio::test]
    async fn execute_post_explicit_retry_opts_in_and_retries() {
        // Retry-After: 0 on the failing responses keeps the test fast
        // regardless of base_ms/max_ms.
        let (url, hits, _handle) = spawn_scripted_server(vec![
            (503, Some("0".to_string())),
            (503, Some("0".to_string())),
            (200, None),
        ])
        .await;
        let spec = HttpFetchSpec::from_args(&json!({
            "url": url,
            "method": "POST",
            "retry": {"attempts": 3}
        }))
        .unwrap();
        let result = spec.execute().await.unwrap();
        assert_eq!(result.status, 200);
        assert_eq!(
            hits.load(std::sync::atomic::Ordering::SeqCst),
            3,
            "explicit retry opt-in on POST must actually retry"
        );
    }

    #[tokio::test]
    async fn execute_get_default_retries_via_retry_after_header_then_succeeds() {
        // No `retry` key at all: exercises the TRUE method-based default
        // for an idempotent method (GET). Retry-After: 0 keeps it fast.
        let (url, hits, _handle) = spawn_scripted_server(vec![
            (503, Some("0".to_string())),
            (503, Some("0".to_string())),
            (200, None),
        ])
        .await;
        let spec = HttpFetchSpec::from_args(&json!({"url": url, "method": "GET"})).unwrap();
        assert_eq!(spec.retry, None);
        let result = spec.execute().await.unwrap();
        assert_eq!(result.status, 200);
        assert_eq!(hits.load(std::sync::atomic::Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn execute_get_default_gives_up_after_exhausting_attempts() {
        let (url, hits, _handle) = spawn_scripted_server(vec![(503, Some("0".to_string()))]).await;
        let spec = HttpFetchSpec::from_args(&json!({"url": url, "method": "GET"})).unwrap();
        let err = spec.execute().await.unwrap_err();
        assert!(format!("{err}").contains("503"));
        // Default attempts=3 for an idempotent method: 1 initial try + 2 retries.
        assert_eq!(hits.load(std::sync::atomic::Ordering::SeqCst), 3);
    }
}

#[cfg(test)]
mod serde_path_clamp_tests {
    use super::*;

    #[test]
    fn serde_built_retry_spec_is_normalized_at_execute_bounds() {
        // Pollers deserialize HttpFetchSpec directly, bypassing
        // from_args clamping; normalized() is the execute-time choke
        // point that bounds them anyway.
        let spec: RetrySpec = serde_json::from_str(
            r#"{"attempts": 4294967295, "base_ms": 0, "max_ms": 999999999999}"#,
        )
        .unwrap();
        assert_eq!(spec.attempts, u32::MAX, "serde carries raw values");
        let n = spec.normalized();
        assert_eq!(n.attempts, MAX_RETRY_ATTEMPTS);
        assert!(n.base_ms >= 1, "zero base delay clamps up: {}", n.base_ms);
        assert!(
            n.max_ms <= MAX_TOTAL_DELAY_MS,
            "max_ms bounded: {}",
            n.max_ms
        );
    }
}
