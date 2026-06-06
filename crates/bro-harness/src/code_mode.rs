//! Codex code-mode adoption seam (`design/bro-harness/` — supersedes NARF).
//!
//! Projects the harness tool surface into the vendored [`bro_code_mode`] runtime
//! as a typed `tools.*` namespace, and exposes exactly two model-facing tools:
//! `exec` (run a JS/TS cell that composes `tools.*` calls, emits content via
//! `text()`/`image()`, and persists across cells via `store()`/`load()`) and
//! `wait` (resume or terminate a still-running cell by `cell_id`).
//!
//! A cell's nested `tools.X(...)` call dispatches back through the SAME filtered
//! [`bro_capabilities::ToolCapability`] seam (`HostTools`) the flat surface uses
//! and runs the real [`Tool`] against the session `ToolCx`. The deny-filter is
//! honored: a tool absent from the seam fails closed, with no in-cell bypass.

use std::collections::BTreeMap;
use std::sync::Arc;

use async_trait::async_trait;
use bro_capabilities::{ToolCapability, ToolInvocation};
use bro_code_mode::{
    CellId, CodeModeNestedToolCall, CodeModeService, CodeModeSessionDelegate, CodeModeToolKind,
    ExecuteRequest, FunctionCallOutputContentItem, NotificationFuture, PUBLIC_TOOL_NAME,
    RuntimeResponse, ToolDefinition, ToolInvocationFuture, ToolName, WAIT_TOOL_NAME, WaitOutcome,
    WaitRequest, build_exec_tool_description, build_wait_tool_description,
    is_code_mode_nested_tool, parse_exec_source,
};
use bro_tools::{Tool, ToolCx, ToolResult};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

// Vendored verbatim from openai/codex
// `codex-rs/core/src/tools/code_mode/execute_spec.rs`.
const CODE_MODE_FREEFORM_GRAMMAR: &str = r#"
start: pragma_source | plain_source
pragma_source: PRAGMA_LINE NEWLINE SOURCE
plain_source: SOURCE

PRAGMA_LINE: /[ \t]*\/\/ @exec:[^\r\n]*/
NEWLINE: /\r?\n/
SOURCE: /[\s\S]+/
"#;

/// Delegate that dispatches a cell's nested `tools.X(...)` call into the harness
/// tool seam. Holds the already-filtered [`ToolCapability`] (`HostTools`), so a
/// denied/unknown tool fails closed exactly as it does on the flat surface.
struct HarnessDelegate {
    seam: Arc<dyn ToolCapability>,
}

impl CodeModeSessionDelegate for HarnessDelegate {
    fn invoke_tool<'a>(
        &'a self,
        invocation: CodeModeNestedToolCall,
        cancellation_token: CancellationToken,
    ) -> ToolInvocationFuture<'a> {
        let seam = self.seam.clone();
        Box::pin(async move {
            let name = invocation.tool_name.to_string();
            let input_json = invocation.input.unwrap_or_else(|| json!({}));
            let call = ToolInvocation {
                name: name.clone(),
                input_json,
            };
            tokio::select! {
                _ = cancellation_token.cancelled() => {
                    Err(format!("{name}: cell cancelled before the tool returned"))
                }
                out = seam.call_tool(call) => match out {
                    Ok(out) if out.is_error => Err(out.content),
                    Ok(out) => {
                        // Match the flat seam's contract: JSON results parse into
                        // values; everything else is handed back as a string.
                        if out.content_type == "application/json" {
                            serde_json::from_str(&out.content)
                                .map_err(|e| format!("{name}: tool returned invalid JSON: {e}"))
                        } else {
                            Ok(Value::String(out.content))
                        }
                    }
                    Err(e) => Err(format!("{}: {}", e.code, e.message)),
                }
            }
        })
    }

    fn notify<'a>(
        &'a self,
        _call_id: String,
        cell_id: CellId,
        text: String,
        _cancellation_token: CancellationToken,
    ) -> NotificationFuture<'a> {
        // First landing: notifications are logged, not injected as extra
        // tool_call_output blocks (that requires a turn-loop hook). Non-fatal.
        Box::pin(async move {
            tracing::info!(cell = %cell_id, "code-mode notify: {text}");
            Ok(())
        })
    }

    fn cell_closed(&self, _cell_id: &CellId) {}
}

/// Per-session code-mode surface: one [`CodeModeService`] shared by `exec`/`wait`
/// so `store`/`load` values and live cells persist across calls within a session.
struct CodeModeSurface {
    service: CodeModeService,
    catalog: Vec<ToolDefinition>,
}

impl CodeModeSurface {
    fn new(seam: Arc<dyn ToolCapability>, catalog: Vec<ToolDefinition>) -> Self {
        let delegate = Arc::new(HarnessDelegate { seam });
        Self {
            service: CodeModeService::with_delegate(delegate),
            catalog,
        }
    }
}

/// Join the model-facing content items of a runtime response into tool-result
/// text. Images are noted but not yet forwarded (our `ToolResult` is single
/// text/json; image-block passthrough is a follow-on transport change).
fn join_content(items: &[FunctionCallOutputContentItem]) -> String {
    let mut out = String::new();
    for item in items {
        match item {
            FunctionCallOutputContentItem::InputText { text } => {
                if !out.is_empty() {
                    out.push('\n');
                }
                out.push_str(text);
            }
            FunctionCallOutputContentItem::InputImage { .. } => {
                if !out.is_empty() {
                    out.push('\n');
                }
                out.push_str("[image omitted — not yet forwarded by the harness]");
            }
        }
    }
    out
}

/// Map a runtime response into a harness tool result. A still-running cell
/// surfaces its `cell_id` so the model can `wait`.
fn response_to_result(response: RuntimeResponse) -> ToolResult {
    match response {
        RuntimeResponse::Result {
            content_items,
            error_text,
            ..
        } => {
            let body = join_content(&content_items);
            match error_text {
                Some(err) if body.is_empty() => ToolResult::Error(err),
                Some(err) => ToolResult::Error(format!("{body}\n{err}")),
                None => ToolResult::Text(body),
            }
        }
        RuntimeResponse::Yielded {
            cell_id,
            content_items,
        } => {
            let body = join_content(&content_items);
            ToolResult::Text(format!(
                "{body}\n\nScript running with cell ID {cell_id}. Call `wait` with this cell_id for more output.",
            ))
        }
        RuntimeResponse::Terminated {
            cell_id,
            content_items,
        } => {
            let body = join_content(&content_items);
            ToolResult::Text(format!("{body}\n\n[cell {cell_id} terminated]"))
        }
    }
}

/// `exec`: run a NARF-style composition cell in a fresh code-mode isolate.
struct ExecTool {
    surface: Arc<CodeModeSurface>,
    description: String,
}

#[async_trait]
impl Tool for ExecTool {
    fn name(&self) -> &str {
        PUBLIC_TOOL_NAME
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "source": {
                    "type": "string",
                    "description": "Raw JavaScript source text (an async module). Not JSON, not fenced."
                }
            },
            "required": ["source"]
        })
    }

    fn freeform_grammar(&self) -> Option<bro_tools::FreeformGrammar> {
        Some(bro_tools::FreeformGrammar {
            syntax: "lark".to_string(),
            definition: CODE_MODE_FREEFORM_GRAMMAR.to_string(),
        })
    }

    async fn call(&self, input: Value, _cx: &ToolCx) -> ToolResult {
        let Some(source) = input.get("source").and_then(Value::as_str) else {
            return ToolResult::Error("exec: `source` is required".into());
        };
        let parsed = match parse_exec_source(source) {
            Ok(p) => p,
            Err(e) => return ToolResult::Error(format!("exec: {e}")),
        };
        let request = ExecuteRequest {
            tool_call_id: "exec".to_string(),
            enabled_tools: self.surface.catalog.clone(),
            source: parsed.code,
            yield_time_ms: parsed.yield_time_ms,
            max_output_tokens: parsed.max_output_tokens,
        };
        let started = match self.surface.service.execute(request).await {
            Ok(s) => s,
            Err(e) => return ToolResult::Error(format!("exec failed: {e}")),
        };
        match started.initial_response().await {
            Ok(response) => response_to_result(response),
            Err(e) => ToolResult::Error(format!("exec failed: {e}")),
        }
    }
}

/// `wait`: resume or terminate a running `exec` cell by `cell_id`.
struct WaitTool {
    surface: Arc<CodeModeSurface>,
}

#[async_trait]
impl Tool for WaitTool {
    fn name(&self) -> &str {
        WAIT_TOOL_NAME
    }

    fn description(&self) -> &str {
        build_wait_tool_description()
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "cell_id": { "type": "string", "description": "Running exec cell id to resume." },
                "yield_time_ms": {
                    "type": "integer",
                    "description": "How long to wait for more output before yielding again (default 10000).",
                    "minimum": 0
                },
                "terminate": {
                    "type": "boolean",
                    "description": "Stop the running cell instead of waiting for output."
                }
            },
            "required": ["cell_id"]
        })
    }

    async fn call(&self, input: Value, _cx: &ToolCx) -> ToolResult {
        let Some(cell_id) = input.get("cell_id").and_then(Value::as_str) else {
            return ToolResult::Error("wait: `cell_id` is required".into());
        };
        let cell_id = CellId::new(cell_id.to_string());
        let terminate = input
            .get("terminate")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let outcome = if terminate {
            self.surface.service.terminate(cell_id).await
        } else {
            let yield_time_ms = input
                .get("yield_time_ms")
                .and_then(Value::as_u64)
                .unwrap_or(bro_code_mode::DEFAULT_WAIT_YIELD_TIME_MS);
            self.surface
                .service
                .wait(WaitRequest {
                    cell_id,
                    yield_time_ms,
                })
                .await
        };
        match outcome {
            Ok(WaitOutcome::LiveCell(response)) => response_to_result(response),
            Ok(WaitOutcome::MissingCell(response)) => response_to_result(response),
            Err(e) => ToolResult::Error(format!("wait failed: {e}")),
        }
    }
}

/// Build the `exec` + `wait` tools from the full callable tool set.
///
/// `callable` is every tool a cell may invoke — the same filtered set of
/// builtin, MCP, and capability tools the flat surface exposes. `seam` is the
/// [`ToolCapability`] that dispatches those tools (the harness `HostTools` over
/// the same set). The typed `tools.*` namespace is derived from `callable`,
/// excluding `exec`/`wait` so a cell cannot recursively launch the box.
pub fn code_mode_tools(
    callable: &[Arc<dyn Tool>],
    seam: Arc<dyn ToolCapability>,
) -> Vec<Arc<dyn Tool>> {
    let catalog: Vec<ToolDefinition> = callable
        .iter()
        .filter(|t| is_code_mode_nested_tool(t.name()))
        .map(|t| ToolDefinition {
            name: t.name().to_string(),
            tool_name: ToolName::plain(t.name()),
            description: t.description().to_string(),
            kind: CodeModeToolKind::Function,
            input_schema: Some(t.input_schema()),
            output_schema: None,
        })
        .collect();

    let description = build_exec_tool_description(
        &catalog,
        &BTreeMap::new(),
        /*code_mode_only*/ true,
        false,
    );
    let surface = Arc::new(CodeModeSurface::new(seam, catalog));

    vec![
        Arc::new(ExecTool {
            surface: surface.clone(),
            description,
        }) as Arc<dyn Tool>,
        Arc::new(WaitTool { surface }) as Arc<dyn Tool>,
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::sync::Mutex;

    /// Echoes its input back as JSON — proves a cell's `tools.echo(...)` reaches
    /// a real `Tool::call` through the seam.
    struct Echo;

    #[async_trait]
    impl Tool for Echo {
        fn name(&self) -> &str {
            "echo"
        }
        fn description(&self) -> &str {
            "Echo the input back."
        }
        fn input_schema(&self) -> Value {
            json!({ "type": "object" })
        }
        async fn call(&self, input: Value, _cx: &ToolCx) -> ToolResult {
            ToolResult::Json(input)
        }
    }

    fn test_cx() -> ToolCx {
        ToolCx {
            root: std::env::temp_dir(),
            safety: Arc::new(bro_tools::SafetyPolicy::new()),
            http: reqwest::Client::new(),
            todos: Arc::new(Mutex::new(bro_tools::TodoList::default())),
            shell_sessions: Arc::new(Mutex::new(bro_tools::ShellSessions::default())),
            promises: Arc::new(Mutex::new(bro_tools::PromiseStore::default())),
            edits: Arc::new(Mutex::new(bro_tools::EditSink::default())),
            session_env: Arc::new(BTreeMap::new()),
        }
    }

    fn exec_with(callable: Vec<Arc<dyn Tool>>) -> Arc<dyn Tool> {
        let seam: Arc<dyn ToolCapability> = Arc::new(crate::capabilities::HostTools::new(
            callable.clone(),
            test_cx(),
        ));
        code_mode_tools(&callable, seam).remove(0)
    }

    #[tokio::test]
    async fn cell_dispatches_nested_tool_through_seam() {
        let exec = exec_with(vec![Arc::new(Echo) as Arc<dyn Tool>]);
        let result = exec
            .call(
                json!({ "source": "const r = await tools.echo({ a: 1 }); text(JSON.stringify(r));" }),
                &test_cx(),
            )
            .await;
        match result {
            ToolResult::Text(t) => assert!(t.contains("\"a\":1"), "got: {t}"),
            other => panic!("expected text, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn store_load_persists_across_cells_in_a_session() {
        let exec = exec_with(vec![Arc::new(Echo) as Arc<dyn Tool>]);
        let _ = exec
            .call(json!({ "source": "store('k', { n: 7 });" }), &test_cx())
            .await;
        let result = exec
            .call(
                json!({ "source": "text(JSON.stringify(load('k')));" }),
                &test_cx(),
            )
            .await;
        match result {
            ToolResult::Text(t) => assert!(t.contains("\"n\":7"), "got: {t}"),
            other => panic!("expected text, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn denied_tool_fails_closed_in_cell() {
        // `echo` is in the projected namespace but NOT in the seam — the §4.5
        // deny-bypass guard: an in-cell call must reject, not bypass the filter.
        let callable = vec![Arc::new(Echo) as Arc<dyn Tool>];
        let empty_seam: Arc<dyn ToolCapability> =
            Arc::new(crate::capabilities::HostTools::new(Vec::new(), test_cx()));
        let exec = code_mode_tools(&callable, empty_seam).remove(0);
        let result = exec
            .call(
                json!({ "source": "await tools.echo({ a: 1 });" }),
                &test_cx(),
            )
            .await;
        assert!(
            matches!(result, ToolResult::Error(_)),
            "denied in-cell tool must fail closed, got {result:?}"
        );
    }
}
