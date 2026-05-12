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
        Ok(HttpFetchSpec {
            method,
            url,
            headers,
            body,
            timeout_secs,
            expect_status,
            response_kind,
            allow_empty_body,
        })
    }

    /// Execute the fetch; returns `(status, parsed value)`. Errors:
    /// invalid method, network failure, body-read failure, status
    /// outside the configured allow set, or a `Json`-mode response
    /// that wasn't actually JSON.
    pub async fn execute(&self) -> Result<HttpFetchResult> {
        let parsed_method = reqwest::Method::from_bytes(self.method.as_bytes())
            .map_err(|e| anyhow!("http fetch invalid method '{}': {e}", self.method))?;
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(self.timeout_secs))
            .build()
            .map_err(|e| anyhow!("http client build: {e}"))?;
        let mut req = client.request(parsed_method, &self.url);
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
            .map_err(|e| anyhow!("http fetch {method} {url}: send: {e}"))?;
        let status = resp.status().as_u16();
        let allow = match &self.expect_status {
            Some(arr) => arr.contains(&status),
            None => (200..300).contains(&status),
        };
        let text = resp
            .text()
            .await
            .map_err(|e| anyhow!("http fetch {method} {url}: body: {e}"))?;
        if !allow {
            let preview: String = text.chars().take(500).collect();
            bail!("http fetch {method} {url}: HTTP {status}: {preview}");
        }
        let value = if text.trim().is_empty() {
            if !self.allow_empty_body {
                bail!("http fetch {method} {url}: empty body but allow_empty_body=false");
            }
            Value::Null
        } else {
            match self.response_kind {
                ResponseKind::Text => Value::String(text),
                ResponseKind::Auto => serde_json::from_str(&text).unwrap_or(Value::String(text)),
                ResponseKind::Json => serde_json::from_str(&text).map_err(|e| {
                    let preview: String = text.chars().take(200).collect();
                    anyhow!("http fetch {method} {url}: response not JSON: {e}: {preview}")
                })?,
            }
        };
        Ok(HttpFetchResult { status, value })
    }
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
}
