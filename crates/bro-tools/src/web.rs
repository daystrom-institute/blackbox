//! Web tools.
//!
//! `web_search` is deliberately NOT here: it is a provider-executed
//! server-side tool (verified for GLM + DeepSeek — see the design doc) that
//! the harness passes through rather than implementing. `web_fetch` IS
//! client-side here — it is the genuine fix for the `claude` CLI's
//! base-URL-breaking `claude.ai` preflight (claude-code #24921): it just
//! fetches the URL and strips it to text, with no Anthropic dependency.
//!
//! Ported from pg_recon `WebToolFunctions.WebFetch`.

use crate::tool::{Tool, ToolAnnotations, ToolCx, ToolResult, schema_for};
use async_trait::async_trait;
use regex::Regex;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::sync::OnceLock;
use std::time::Duration;

#[derive(Deserialize, JsonSchema)]
struct WebFetchInput {
    /// URL to fetch (http/https).
    url: String,
    /// Max characters of extracted text to return (default 8000, clamped 500..=20000).
    max_chars: Option<usize>,
    /// Start character in the returned text representation (default 0). HTML is
    /// reduced to text; other textual media preserve their original UTF-8 content.
    start_char: Option<usize>,
    /// SHA-256 from a previous page. Refuse if the fetched text changed before
    /// continuing; use with start_char to avoid joining different versions.
    expected_sha256: Option<String>,
}

pub struct WebFetch;

#[async_trait]
impl Tool for WebFetch {
    fn name(&self) -> &str {
        "web_fetch"
    }
    fn description(&self) -> &str {
        "Fetch UTF-8 text over HTTP(S). HTML is reduced to text; plain text, code and JSON preserve whitespace and angle brackets. Bodies over 2 MiB and binary media are refused. Pages are bounded by max_chars and 8000 output bytes; follow start_char and expected_sha256 from the continuation."
    }
    fn input_schema(&self) -> Value {
        schema_for::<WebFetchInput>()
    }
    fn annotations(&self) -> ToolAnnotations {
        ToolAnnotations {
            read_only: true,
            ..Default::default()
        }
    }
    async fn call(&self, input: Value, cx: &ToolCx) -> ToolResult {
        let args: WebFetchInput = match serde_json::from_value(input) {
            Ok(a) => a,
            Err(e) => return ToolResult::Error(format!("bad input: {e}")),
        };
        let max_chars = args.max_chars.unwrap_or(8000).clamp(500, 20_000);
        let request = cx
            .http
            .get(&args.url)
            .header(
                "user-agent",
                "Mozilla/5.0 (compatible; bro-harness/0.1; +https://github.com)",
            )
            .timeout(Duration::from_secs(15));
        let resp = tokio::select! {
            biased;
            _ = cx.cancellation.cancelled() => return ToolResult::Error("web_fetch: cancelled before response headers".into()),
            response = request.send() => response,
        };
        let mut response = match resp {
            Ok(response) => match response.error_for_status() {
                Ok(response) => response,
                Err(error) => return ToolResult::Error(format!("http status: {error}")),
            },
            Err(error) => return ToolResult::Error(format!("fetch {}: {error}", args.url)),
        };
        let media = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .unwrap_or("")
            .split(';')
            .next()
            .unwrap_or("")
            .trim()
            .to_ascii_lowercase();
        let html = matches!(media.as_str(), "text/html" | "application/xhtml+xml");
        if !media.is_empty()
            && !media.starts_with("text/")
            && !matches!(
                media.as_str(),
                "application/json" | "application/xml" | "application/javascript"
            )
            && !media.ends_with("+json")
            && !media.ends_with("+xml")
        {
            return ToolResult::Error(format!(
                "web_fetch: unsupported media type {media}; use a client for that format"
            ));
        }
        const MAX_BODY_BYTES: usize = 2 * 1024 * 1024;
        if response
            .content_length()
            .is_some_and(|length| length > MAX_BODY_BYTES as u64)
        {
            return ToolResult::Error(
                "web_fetch: response exceeds 2 MiB; request a narrower source".into(),
            );
        }
        let mut body = Vec::new();
        loop {
            let chunk = tokio::select! {
                _ = cx.cancellation.cancelled() => return ToolResult::Error("web_fetch: cancelled before complete response".into()),
                chunk = response.chunk() => chunk,
            };
            match chunk {
                Ok(Some(chunk)) => {
                    if body.len().saturating_add(chunk.len()) > MAX_BODY_BYTES {
                        return ToolResult::Error(
                            "web_fetch: response exceeds 2 MiB; request a narrower source".into(),
                        );
                    }
                    body.extend_from_slice(&chunk);
                }
                Ok(None) => break,
                Err(error) => return ToolResult::Error(format!("read body: {error}")),
            }
        }
        let body =
            match String::from_utf8(body) {
                Ok(body) => body,
                Err(_) => return ToolResult::Error(
                    "web_fetch: response is not UTF-8 text; use a client supporting its encoding"
                        .into(),
                ),
            };
        let text = if html { strip_html(&body) } else { body };
        let digest = format!("{:x}", Sha256::digest(text.as_bytes()));
        if args
            .expected_sha256
            .as_deref()
            .is_some_and(|expected| expected != digest)
        {
            return ToolResult::Error("web_fetch: source text changed since the previous page; restart from start_char=0 without expected_sha256".into());
        }
        let start = args.start_char.unwrap_or(0);
        let total = text.chars().count();
        if start > total {
            return ToolResult::Error(format!(
                "web_fetch: start_char {start} exceeds text length {total}"
            ));
        }
        // Reserve continuation metadata before selecting a contiguous prefix.
        // The final backstop must never clip a page whose continuation skips it.
        let budget = if cx.output_budget == 0 {
            crate::output::DEFAULT_OUTPUT_BYTES
        } else {
            cx.output_budget.min(crate::output::DEFAULT_OUTPUT_BYTES)
        };
        let mut page = String::new();
        let mut count = 0usize;
        for ch in text.chars().skip(start).take(max_chars) {
            if page.len() + ch.len_utf8() > budget.saturating_sub(256) {
                break;
            }
            page.push(ch);
            count += 1;
        }
        if start + count < total {
            if count == 0 {
                return ToolResult::Error(
                    "web_fetch: output budget is too small for a text page".into(),
                );
            }
            page.push_str(&format!("\n[page truncated; continue SAME url with start_char={} and expected_sha256=\"{digest}\"]", start + count));
        }
        ToolResult::Text(page)
    }
}

/// Crude HTML → text: drop script/style, strip tags, collapse whitespace.
/// Mirrors pg_recon's regex pipeline; a markdown converter (e.g. an
/// html2text/turndown-equivalent) is a later refinement.
fn strip_html(html: &str) -> String {
    fn re(p: &str, cell: &'static OnceLock<Regex>) -> &'static Regex {
        cell.get_or_init(|| Regex::new(p).expect("valid html regex"))
    }
    static SCRIPT: OnceLock<Regex> = OnceLock::new();
    static TAG: OnceLock<Regex> = OnceLock::new();
    static WS: OnceLock<Regex> = OnceLock::new();

    let no_script = re(
        r"(?is)<(script|style)[^>]*>.*?</\s*(script|style)\s*>",
        &SCRIPT,
    )
    .replace_all(html, " ");
    let no_tags = re(r"(?s)<[^>]+>", &TAG).replace_all(&no_script, " ");
    let decoded = no_tags
        .replace("&nbsp;", " ")
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'");
    re(r"\s+", &WS)
        .replace_all(&decoded, " ")
        .trim()
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_tags_scripts_and_entities() {
        let html = "<html><head><style>x{}</style></head><body><p>Hello &amp; <b>world</b></p><script>evil()</script></body></html>";
        assert_eq!(strip_html(html), "Hello & world");
    }
}
