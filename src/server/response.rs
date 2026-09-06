use crate::server::BlackboxServer;

use rmcp::model::{CallToolResult, IntoContents};
use serde_json::Value;

impl BlackboxServer {
    pub(crate) fn ok_text(text: &str) -> CallToolResult {
        Self::bounded_response(text, None, false, None)
    }

    pub(crate) fn ok_json(value: &Value) -> CallToolResult {
        let text = serde_json::to_string(value).expect("JSON values serialize");
        Self::bounded_response(&text, None, false, None)
    }

    pub(crate) fn err_text(msg: &str) -> CallToolResult {
        Self::bounded_response(msg, None, true, None)
    }

    /// Bound the complete MCP result, including JSON escaping and both content
    /// representations. Oversized answers are producer errors, never implicit
    /// exports into a filesystem the caller may not share.
    fn bounded_response(
        text: &str,
        structured: Option<Value>,
        is_error: bool,
        tool: Option<&str>,
    ) -> CallToolResult {
        let typed_error = structured.as_ref().is_some_and(Self::is_invocation_error)
            || (text.len() <= Self::MCP_RESPONSE_CAP_BYTES
                && serde_json::from_str::<Value>(text)
                    .is_ok_and(|value| Self::is_invocation_error(&value)));
        let mut response = CallToolResult::success(text.into_contents());
        response.structured_content = structured;
        response.is_error = Some(is_error || typed_error);
        let bytes = Self::response_bytes(&response);
        if bytes <= Self::MCP_RESPONSE_CAP_BYTES {
            return response;
        }
        tracing::warn!(target: "blackbox::tool", tool, bytes, cap_bytes = Self::MCP_RESPONSE_CAP_BYTES, "response_too_large");
        let hint = match tool {
            Some("bbox_gaps") => {
                "Use a smaller limit or an exact id; request full detail only for selected gaps."
            }
            Some("bbox_hybrid_search" | "bbox_discover_seed_entities") => {
                "Use a smaller limit and narrower project/doc_type filters; omit debug detail."
            }
            Some("bbox_bundle_evidence") => {
                "Use fewer entity_refs/path_ids and property_mode=summary; inspect selected refs individually."
            }
            Some("bbox_inspect_entity") => {
                "Use targeted edge_types, a smaller per_type_limit, and property_mode=summary."
            }
            _ => {
                "Use the tool's documented filters, page controls, or summary view. If no bounded read exists, the producer needs a response-shaping fix."
            }
        };
        let mut error = serde_json::json!({
            "status": "error.response_too_large",
            "error": {
                "code": "response_too_large",
                "message": "The tool produced a result larger than its transport budget.",
            },
            "bytes": bytes,
            "cap_bytes": Self::MCP_RESPONSE_CAP_BYTES,
            "hint": hint,
            "retry_note": "The operation may have completed. Inspect current state before retrying a mutation.",
        });
        if let Some(tool) = tool {
            error["tool"] = Value::String(tool.into());
        }
        let mut result = CallToolResult::success(error.to_string().into_contents());
        result.structured_content = Some(error);
        result.is_error = Some(true);
        result
    }

    /// Only canonical invocation-error envelopes affect the MCP error signal.
    /// A successfully observed failed task or rejected proposal is still a
    /// domain outcome that workflow callers can inspect normally.
    fn is_invocation_error(value: &Value) -> bool {
        value
            .get("status")
            .and_then(Value::as_str)
            .is_some_and(|status| status.starts_with("error."))
    }

    fn response_bytes(response: &CallToolResult) -> usize {
        serde_json::to_vec(response)
            .expect("MCP results serialize")
            .len()
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
                let response = Self::bounded_response(&text, None, false, Some(tool));
                tracing::info!(target: "blackbox::tool", tool, elapsed_ms = ms, bytes = Self::response_bytes(&response), is_error = response.is_error, "response");
                response
            }
            Err(e) => {
                let ms = start.elapsed().as_secs_f64() * 1000.0;
                tracing::warn!(target: "blackbox::tool", tool, elapsed_ms = ms, error = %e, "err");
                Self::err_text(&format!("Error: {e:#}"))
            }
        }
    }

    /// Run a synchronous handler that preserves the established text content
    /// while additively publishing a machine-readable response envelope.
    pub(crate) fn run_with_structured<F>(tool: &'static str, op: F) -> CallToolResult
    where
        F: FnOnce() -> anyhow::Result<(String, Value)>,
    {
        let start = std::time::Instant::now();
        match op() {
            Ok((text, structured)) => {
                let ms = start.elapsed().as_secs_f64() * 1000.0;
                let response = Self::bounded_response(&text, Some(structured), false, Some(tool));
                tracing::info!(target: "blackbox::tool", tool, elapsed_ms = ms, bytes = Self::response_bytes(&response), is_error = response.is_error, "response");
                response
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
                let response = Self::bounded_response(&text, None, false, Some(tool));
                tracing::info!(target: "blackbox::tool", tool, elapsed_ms = ms, bytes = Self::response_bytes(&response), is_error = response.is_error, "response");
                response
            }
            Err(e) => {
                let ms = start.elapsed().as_secs_f64() * 1000.0;
                tracing::warn!(target: "blackbox::tool", tool, elapsed_ms = ms, error = %e, "err");
                Self::err_text(&format!("Error: {e:#}"))
            }
        }
    }

    /// Blocking-pool twin of [`Self::run_with_structured`].
    pub(crate) async fn run_blocking_with_structured<F>(tool: &'static str, op: F) -> CallToolResult
    where
        F: FnOnce() -> anyhow::Result<(String, Value)> + Send + 'static,
    {
        let start = std::time::Instant::now();
        let result = tokio::task::spawn_blocking(op)
            .await
            .map_err(|e| anyhow::anyhow!("blocking task failed: {e}"))
            .and_then(std::convert::identity);

        match result {
            Ok((text, structured)) => {
                let ms = start.elapsed().as_secs_f64() * 1000.0;
                let response = Self::bounded_response(&text, Some(structured), false, Some(tool));
                tracing::info!(target: "blackbox::tool", tool, elapsed_ms = ms, bytes = Self::response_bytes(&response), is_error = response.is_error, "response");
                response
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
    use crate::util::TestEnvGuard;

    #[test]
    fn oversized_results_fail_without_creating_dump_files() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let mut env = TestEnvGuard::new();
        env.set("BLACKBOX_STATE_DIR", &root);
        for text in [
            "x".repeat(BlackboxServer::MCP_RESPONSE_CAP_BYTES),
            serde_json::json!({"data": "界".repeat(30_000)}).to_string(),
        ] {
            let result = BlackboxServer::ok_text(&text);
            assert_eq!(result.is_error, Some(true));
            assert!(
                BlackboxServer::response_bytes(&result) < BlackboxServer::MCP_RESPONSE_CAP_BYTES
            );
            assert_eq!(
                result.structured_content.as_ref().unwrap()["error"]["code"],
                "response_too_large"
            );
            let wire = serde_json::to_string(&result).unwrap();
            assert!(!wire.contains("spilled_to"));
            assert!(!wire.contains("preview"));
            assert!(!root.join("response-dumps").exists());
        }
    }

    #[test]
    fn structured_content_and_json_escaping_count_toward_the_budget() {
        for (text, structured) in [
            (
                "small".to_string(),
                serde_json::json!({"data": "x".repeat(90_000)}),
            ),
            (
                "x".repeat(45_000),
                serde_json::json!({"data": "y".repeat(45_000)}),
            ),
            ("\n".repeat(45_000), serde_json::json!({})),
        ] {
            let result = BlackboxServer::run_with_structured("test", || Ok((text, structured)));
            assert_eq!(result.is_error, Some(true));
            assert!(
                BlackboxServer::response_bytes(&result) < BlackboxServer::MCP_RESPONSE_CAP_BYTES
            );
            assert_eq!(result.structured_content.unwrap()["tool"], "test");
        }
    }

    #[tokio::test]
    async fn blocking_structured_helper_uses_the_same_guard() {
        let result = BlackboxServer::run_blocking_with_structured("test", || {
            Ok((
                "small".into(),
                serde_json::json!({"data": "x".repeat(90_000)}),
            ))
        })
        .await;
        assert_eq!(result.is_error, Some(true));
        assert_eq!(
            result.structured_content.unwrap()["error"]["code"],
            "response_too_large"
        );
    }

    #[test]
    fn invocation_errors_and_domain_outcomes_remain_distinct() {
        let invalid =
            serde_json::json!({"status": "error.bad_input", "error": {"code": "error.bad_input"}});
        assert_eq!(BlackboxServer::ok_json(&invalid).is_error, Some(true));
        let structured =
            BlackboxServer::run_with_structured("test", || Ok(("Bad input".into(), invalid)));
        assert_eq!(structured.is_error, Some(true));
        for status in ["failed", "bad_input", "rejected", "completed"] {
            let result = BlackboxServer::ok_json(&serde_json::json!({"status": status}));
            assert_eq!(result.is_error, Some(false), "domain outcome {status}");
        }
    }

    #[test]
    fn small_success_preserves_both_views() {
        let value = serde_json::json!({"items": [1, 2]});
        let result =
            BlackboxServer::run_with_structured("test", || Ok(("Two items".into(), value.clone())));
        assert_eq!(result.is_error, Some(false));
        assert_eq!(result.structured_content, Some(value));
        assert!(
            serde_json::to_string(&result.content)
                .unwrap()
                .contains("Two items")
        );
    }
}
