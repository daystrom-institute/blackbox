use crate::server::BlackboxServer;

use rmcp::model::{CallToolResult, IntoContents};
use serde_json::Value;

impl BlackboxServer {
    const JSON_RESPONSE_PREVIEW_BYTES: usize = 1024;

    pub(crate) fn ok_text(text: &str) -> CallToolResult {
        CallToolResult::success(Self::cap_response_text(text).into_contents())
    }

    pub(crate) fn ok_json(value: &Value) -> CallToolResult {
        let text = serde_json::to_string_pretty(value).unwrap_or_default();
        CallToolResult::success(Self::cap_response_text(&text).into_contents())
    }

    pub(crate) fn err_text(msg: &str) -> CallToolResult {
        let mut r = CallToolResult::success(Self::cap_response_text(msg).into_contents());
        r.is_error = Some(true);
        r
    }

    /// Parse a tool-supplied spec field that nominally takes a JSON object
    /// but may arrive as a stringified JSON document (some MCP clients
    /// stringify nested objects when the schema doesn't pin `type: object`
    /// tightly). Accepts either form.
    pub(crate) fn parse_spec<T: serde::de::DeserializeOwned>(
        spec: Value,
        kind: &str,
    ) -> Result<T, CallToolResult> {
        let resolved = match spec {
            Value::String(s) => match serde_json::from_str::<Value>(&s) {
                Ok(v) => v,
                Err(e) => {
                    return Err(Self::err_text(&format!(
                        "{kind} spec parse failed: passed as string but not valid JSON: {e}"
                    )));
                }
            },
            other => other,
        };
        serde_json::from_value(resolved)
            .map_err(|e| Self::err_text(&format!("{kind} spec parse failed: {e}")))
    }

    pub(crate) fn cap_response_text(text: &str) -> String {
        if text.len() <= Self::MCP_RESPONSE_CAP_BYTES {
            return text.to_string();
        }
        let trimmed = text.trim_start();
        if trimmed.starts_with('{') || trimmed.starts_with('[') {
            // Transport invariant: never emit invalid JSON; size JSON at the producer.
            let preview = Self::prefix_at_char_boundary(text, Self::JSON_RESPONSE_PREVIEW_BYTES);
            return serde_json::json!({
                "error": "response_too_large",
                "bytes": text.len(),
                "cap_bytes": Self::MCP_RESPONSE_CAP_BYTES,
                "hint": "response exceeded the MCP cap; narrow the query (filters, limit, tail) or use a paginated variant",
                "preview": preview,
            })
            .to_string();
        }
        let suffix = "\n\n[... response truncated to 80KB by bbox response cap]";
        let target = Self::MCP_RESPONSE_CAP_BYTES.saturating_sub(suffix.len());
        let mut out = String::new();
        for ch in text.chars() {
            if out.len() + ch.len_utf8() > target {
                break;
            }
            out.push(ch);
        }
        out.push_str(suffix);
        out
    }

    fn prefix_at_char_boundary(text: &str, max_bytes: usize) -> String {
        let mut out = String::new();
        for ch in text.chars() {
            if out.len() + ch.len_utf8() > max_bytes {
                break;
            }
            out.push(ch);
        }
        out
    }

    /// Run a sync tool handler: time it, log at debug (ok) / warn (err),
    /// uniformly convert Result<String> into CallToolResult. Centralizes
    /// the match-ok-err boilerplate that used to repeat in every bbox_*
    /// handler and gives us per-call duration visibility in journald
    /// (filter: `journalctl --user -u blackbox | grep bbox_`).
    pub(crate) fn run<F>(tool: &'static str, op: F) -> CallToolResult
    where
        F: FnOnce() -> anyhow::Result<String>,
    {
        let start = std::time::Instant::now();
        match op() {
            Ok(text) => {
                let ms = start.elapsed().as_secs_f64() * 1000.0;
                tracing::info!(target: "blackbox::tool", tool, elapsed_ms = ms, bytes = text.len(), "ok");
                Self::ok_text(&text)
            }
            Err(e) => {
                let ms = start.elapsed().as_secs_f64() * 1000.0;
                tracing::warn!(target: "blackbox::tool", tool, elapsed_ms = ms, error = %e, "err");
                Self::err_text(&format!("Error: {e:#}"))
            }
        }
    }

    /// Run a blocking sync tool handler on tokio's blocking pool while
    /// preserving the same timing, tracing, and response conversion as `run`.
    pub(crate) async fn run_blocking<F>(tool: &'static str, op: F) -> CallToolResult
    where
        F: FnOnce() -> anyhow::Result<String> + Send + 'static,
    {
        let start = std::time::Instant::now();
        let result = tokio::task::spawn_blocking(op)
            .await
            .map_err(|e| anyhow::anyhow!("blocking task failed: {e}"))
            .and_then(std::convert::identity);

        match result {
            Ok(text) => {
                let ms = start.elapsed().as_secs_f64() * 1000.0;
                tracing::info!(target: "blackbox::tool", tool, elapsed_ms = ms, bytes = text.len(), "ok");
                Self::ok_text(&text)
            }
            Err(e) => {
                let ms = start.elapsed().as_secs_f64() * 1000.0;
                tracing::warn!(target: "blackbox::tool", tool, elapsed_ms = ms, error = %e, "err");
                Self::err_text(&format!("Error: {e:#}"))
            }
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mcp_response_cap_limits_large_text() {
        let huge = "x".repeat(BlackboxServer::MCP_RESPONSE_CAP_BYTES + 1024);
        let capped = BlackboxServer::cap_response_text(&huge);
        let suffix = "\n\n[... response truncated to 80KB by bbox response cap]";
        let target = BlackboxServer::MCP_RESPONSE_CAP_BYTES - suffix.len();
        let expected = format!("{}{}", "x".repeat(target), suffix);

        assert_eq!(capped, expected);
        assert_eq!(capped.len(), BlackboxServer::MCP_RESPONSE_CAP_BYTES);
        assert!(capped.contains("response truncated"));
    }

    #[test]
    fn oversized_json_returns_valid_error_envelope() {
        let inputs = [
            format!(
                "{{\"data\":\"{}\"}}",
                "x".repeat(BlackboxServer::MCP_RESPONSE_CAP_BYTES)
            ),
            format!(
                "[\"{}\"]",
                "x".repeat(BlackboxServer::MCP_RESPONSE_CAP_BYTES)
            ),
        ];

        for input in inputs {
            let capped = BlackboxServer::cap_response_text(&input);
            let parsed: Value = serde_json::from_str(&capped).expect("valid JSON error envelope");
            let preview = parsed["preview"].as_str().expect("preview string");

            assert_eq!(parsed["error"], "response_too_large");
            assert_eq!(parsed["bytes"], input.len());
            assert_eq!(parsed["cap_bytes"], BlackboxServer::MCP_RESPONSE_CAP_BYTES);
            assert!(input.starts_with(preview));
            assert!(preview.len() <= BlackboxServer::JSON_RESPONSE_PREVIEW_BYTES);
            assert!(capped.len() < BlackboxServer::MCP_RESPONSE_CAP_BYTES);
        }
    }

    #[test]
    fn under_cap_json_passthrough_is_unchanged() {
        let json = "{\"ok\":true,\"items\":[1,2,3]}";
        assert_eq!(BlackboxServer::cap_response_text(json), json);
    }
}
