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
use std::sync::OnceLock;
use std::time::Duration;

#[derive(Deserialize, JsonSchema)]
struct WebFetchInput {
    /// URL to fetch (http/https).
    url: String,
    /// Max characters of extracted text to return (default 8000, clamped 500..=20000).
    max_chars: Option<usize>,
}

pub struct WebFetch;

#[async_trait]
impl Tool for WebFetch {
    fn name(&self) -> &str {
        "web_fetch"
    }
    fn description(&self) -> &str {
        "Fetch a web page and return its text content (HTML stripped, truncated to max_chars). Client-side; no external dependency."
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
        let resp = cx
            .http
            .get(&args.url)
            .header(
                "user-agent",
                "Mozilla/5.0 (compatible; bro-harness/0.1; +https://github.com)",
            )
            .timeout(Duration::from_secs(15))
            .send()
            .await;
        let body = match resp {
            Ok(r) => match r.error_for_status() {
                Ok(r) => match r.text().await {
                    Ok(t) => t,
                    Err(e) => return ToolResult::Error(format!("read body: {e}")),
                },
                Err(e) => return ToolResult::Error(format!("http status: {e}")),
            },
            Err(e) => return ToolResult::Error(format!("fetch {}: {e}", args.url)),
        };
        let text = strip_html(&body);
        let out: String = text.chars().take(max_chars).collect();
        ToolResult::Text(out)
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
