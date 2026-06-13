use crate::server::BlackboxServer;

use rmcp::model::{CallToolResult, IntoContents};
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

impl BlackboxServer {
    const JSON_RESPONSE_PREVIEW_BYTES: usize = 1024;
    /// Spilled response dumps older than this are pruned on the next spill.
    const DUMP_RETENTION: Duration = Duration::from_secs(7 * 24 * 60 * 60);

    /// Directory oversized MCP responses are spilled to.
    fn response_dump_dir() -> PathBuf {
        let home = dirs::home_dir().unwrap_or_else(std::env::temp_dir);
        crate::util::blackbox_state_dir(&home).join("response-dumps")
    }

    /// Write an over-cap payload to a dump file so the inline cap never
    /// destroys data: the cap is context hygiene, not data policy, and every
    /// client of this localhost daemon has file-read tools to recover the
    /// full payload. Best-effort: `None` means the write failed and the
    /// caller falls back to inline truncation.
    // Spill is rare (>80KB responses only); blocking IO here mirrors the
    // harness-side spill in bro-harness bound.rs.
    #[allow(clippy::disallowed_methods)]
    fn spill_oversized_response(text: &str) -> Option<PathBuf> {
        let dir = Self::response_dump_dir();
        std::fs::create_dir_all(&dir).ok()?;
        Self::prune_old_dumps(&dir);
        let millis = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0);
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        std::hash::Hash::hash(text, &mut hasher);
        let digest = std::hash::Hasher::finish(&hasher);
        let trimmed = text.trim_start();
        let ext = if trimmed.starts_with('{') || trimmed.starts_with('[') {
            "json"
        } else {
            "txt"
        };
        let path = dir.join(format!("resp-{millis}-{digest:016x}.{ext}"));
        std::fs::write(&path, text).ok()?;
        Some(path)
    }

    #[allow(clippy::disallowed_methods)]
    fn prune_old_dumps(dir: &Path) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        let now = SystemTime::now();
        for entry in entries.flatten() {
            let stale = entry
                .metadata()
                .and_then(|m| m.modified())
                .ok()
                .and_then(|modified| now.duration_since(modified).ok())
                .is_some_and(|age| age > Self::DUMP_RETENTION);
            if stale {
                let _ = std::fs::remove_file(entry.path());
            }
        }
    }

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
        let spilled = Self::spill_oversized_response(text);
        let trimmed = text.trim_start();
        if trimmed.starts_with('{') || trimmed.starts_with('[') {
            // Transport invariant: never emit invalid JSON; size JSON at the producer.
            let preview = Self::prefix_at_char_boundary(text, Self::JSON_RESPONSE_PREVIEW_BYTES);
            let mut envelope = serde_json::json!({
                "error": "response_too_large",
                "bytes": text.len(),
                "cap_bytes": Self::MCP_RESPONSE_CAP_BYTES,
                "hint": "response exceeded the MCP cap; narrow the query (filters, limit, tail) or use a paginated variant",
                "preview": preview,
            });
            if let Some(path) = &spilled {
                envelope["spilled_to"] = Value::String(path.display().to_string());
                envelope["hint"] = Value::String(
                    "response exceeded the MCP cap; the full payload is at spilled_to (read it \
                     with file tools), or narrow the query (filters, limit, tail)"
                        .to_string(),
                );
            }
            return envelope.to_string();
        }
        let suffix = match &spilled {
            Some(path) => format!(
                "\n\n[... inline view truncated to 80KB by bbox response cap; full {} kB \
                 response written to {}]",
                text.len().div_ceil(1024),
                path.display()
            ),
            None => "\n\n[... response truncated to 80KB by bbox response cap]".to_string(),
        };
        let target = Self::MCP_RESPONSE_CAP_BYTES.saturating_sub(suffix.len());
        let mut out = String::new();
        for ch in text.chars() {
            if out.len() + ch.len_utf8() > target {
                break;
            }
            out.push(ch);
        }
        out.push_str(&suffix);
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
    use crate::util::TestEnvGuard;

    #[test]
    fn mcp_response_cap_spills_large_text_losslessly() {
        let tmp = tempfile::tempdir().unwrap();
        let mut env = TestEnvGuard::new();
        env.set("BLACKBOX_STATE_DIR", tmp.path());

        let huge = "x".repeat(BlackboxServer::MCP_RESPONSE_CAP_BYTES + 1024);
        let capped = BlackboxServer::cap_response_text(&huge);

        assert_eq!(capped.len(), BlackboxServer::MCP_RESPONSE_CAP_BYTES);
        assert!(capped.contains("inline view truncated"));
        assert!(capped.contains("response written to"));

        // The full payload landed in <state_dir>/response-dumps, lossless.
        let dump_dir = tmp.path().join("response-dumps");
        let dumps: Vec<_> = std::fs::read_dir(&dump_dir).unwrap().flatten().collect();
        assert_eq!(dumps.len(), 1);
        assert_eq!(std::fs::read_to_string(dumps[0].path()).unwrap(), huge);
        assert!(capped.contains(&dumps[0].path().display().to_string()));
    }

    #[test]
    fn oversized_json_returns_valid_error_envelope_with_spill_path() {
        let tmp = tempfile::tempdir().unwrap();
        let mut env = TestEnvGuard::new();
        env.set("BLACKBOX_STATE_DIR", tmp.path());

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

            // Lossless: the envelope points at a dump holding the full payload.
            let spilled = parsed["spilled_to"].as_str().expect("spilled_to path");
            assert!(spilled.ends_with(".json"));
            assert_eq!(std::fs::read_to_string(spilled).unwrap(), input);
        }
    }

    #[test]
    fn spill_failure_falls_back_to_inline_truncation() {
        let tmp = tempfile::tempdir().unwrap();
        // Point the state dir at a file so create_dir_all fails.
        let blocker = tmp.path().join("not-a-dir");
        std::fs::write(&blocker, b"x").unwrap();
        let mut env = TestEnvGuard::new();
        env.set("BLACKBOX_STATE_DIR", &blocker);

        let huge = "x".repeat(BlackboxServer::MCP_RESPONSE_CAP_BYTES + 1024);
        let capped = BlackboxServer::cap_response_text(&huge);
        assert_eq!(capped.len(), BlackboxServer::MCP_RESPONSE_CAP_BYTES);
        assert!(capped.contains("response truncated to 80KB"));
        assert!(!capped.contains("written to"));

        let json = format!(
            "{{\"data\":\"{}\"}}",
            "x".repeat(BlackboxServer::MCP_RESPONSE_CAP_BYTES)
        );
        let parsed: Value =
            serde_json::from_str(&BlackboxServer::cap_response_text(&json)).unwrap();
        assert_eq!(parsed["error"], "response_too_large");
        assert!(parsed.get("spilled_to").is_none());
    }

    #[test]
    fn old_dumps_are_pruned_on_spill() {
        let tmp = tempfile::tempdir().unwrap();
        let mut env = TestEnvGuard::new();
        env.set("BLACKBOX_STATE_DIR", tmp.path());

        let dump_dir = tmp.path().join("response-dumps");
        std::fs::create_dir_all(&dump_dir).unwrap();
        let stale = dump_dir.join("resp-0-deadbeef.txt");
        std::fs::write(&stale, b"old").unwrap();
        let old_mtime =
            SystemTime::now() - BlackboxServer::DUMP_RETENTION - Duration::from_secs(60);
        let f = std::fs::File::open(&stale).unwrap();
        f.set_modified(old_mtime).unwrap();

        let huge = "y".repeat(BlackboxServer::MCP_RESPONSE_CAP_BYTES + 1);
        let _ = BlackboxServer::cap_response_text(&huge);

        assert!(!stale.exists(), "stale dump pruned");
        let count = std::fs::read_dir(&dump_dir).unwrap().flatten().count();
        assert_eq!(count, 1, "only the fresh spill remains");
    }

    #[test]
    fn under_cap_json_passthrough_is_unchanged() {
        let json = "{\"ok\":true,\"items\":[1,2,3]}";
        assert_eq!(BlackboxServer::cap_response_text(json), json);
    }
}
