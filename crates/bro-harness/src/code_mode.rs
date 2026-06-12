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

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use bro_capabilities::{ToolCapability, ToolInvocation};
use bro_code_mode::{
    CellId, CodeModeNestedToolCall, CodeModeService, CodeModeSessionDelegate, CodeModeToolKind,
    ExecuteRequest, FunctionCallOutputContentItem, NamespaceBinding, NotificationFuture,
    PUBLIC_TOOL_NAME, RuntimeResponse, ToolDefinition, ToolInvocationFuture, ToolName,
    ToolNamespaceDescription, WAIT_TOOL_NAME, WaitOutcome, WaitRequest,
    build_exec_tool_description, build_wait_tool_description, is_code_mode_nested_tool,
    parse_exec_source,
};
use bro_tools::{Tool, ToolCx, ToolResult};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

/// Tri-state code-mode selector — the harness mirror of Codex's `ToolMode`
/// (`Direct`/`CodeMode`/`CodeModeOnly`).
///
/// - [`Off`](CodeMode::Off): no `exec`/`wait`; the flat tool surface only.
/// - [`Optional`](CodeMode::Optional) (default): flat tools AND `exec`/`wait`
///   are both model-visible. The exec description does not claim "only".
/// - [`Only`](CodeMode::Only): flat builtins are demoted out of the wire array
///   (still `tool_search`-loadable); the model sees `exec`/`wait` as the
///   authorial surface and the exec description is rendered code-mode-only.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CodeMode {
    Off,
    #[default]
    Optional,
    Only,
}

impl std::str::FromStr for CodeMode {
    type Err = ();
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim().to_ascii_lowercase().as_str() {
            "off" | "direct" | "0" | "false" => Ok(CodeMode::Off),
            "optional" | "code_mode" | "codemode" | "on" => Ok(CodeMode::Optional),
            "only" | "code_mode_only" | "codemodeonly" => Ok(CodeMode::Only),
            _ => Err(()),
        }
    }
}

impl CodeMode {
    /// Parse a free-form value (CLI/env), warning and falling back to the
    /// default on an unrecognized token.
    pub fn parse_or_default(s: &str) -> Self {
        s.parse().unwrap_or_else(|()| {
            eprintln!(
                "bro-harness: unknown code-mode '{s}', using '{:?}'",
                CodeMode::default()
            );
            CodeMode::default()
        })
    }

    /// Whether `exec`/`wait` should be registered + pinned at all.
    pub fn enables_code_surface(self) -> bool {
        !matches!(self, CodeMode::Off)
    }

    /// Whether the flat builtins should be hidden from the wire array (deferred).
    pub fn defers_builtins(self) -> bool {
        matches!(self, CodeMode::Only)
    }

    /// The canonical CLI/serde token.
    pub fn as_str(self) -> &'static str {
        match self {
            CodeMode::Off => "off",
            CodeMode::Optional => "optional",
            CodeMode::Only => "only",
        }
    }
}

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

/// Per-session buffer of `notify(...)` payloads keyed by cell id.
///
/// There is no mid-turn injection hook in the harness turn loop, so notify()
/// is delivered model-visibly by riding the next `exec`/`wait` tool result for
/// the cell as a `[notifications]` section (drained on delivery).
type NotificationBuffer = Mutex<HashMap<String, Vec<String>>>;

/// Delegate that dispatches a cell's nested `tools.X(...)` call into the harness
/// tool seam. Holds the already-filtered [`ToolCapability`] (`HostTools`), so a
/// denied/unknown tool fails closed exactly as it does on the flat surface.
struct HarnessDelegate {
    seam: Arc<dyn ToolCapability>,
    notifications: Arc<NotificationBuffer>,
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
        // Buffered, not injected mid-turn: the payload is delivered as a
        // `[notifications]` section on the next `exec`/`wait` result for this
        // cell (drained by ExecTool/WaitTool). Non-fatal.
        tracing::debug!(cell = %cell_id, "code-mode notify queued: {text}");
        self.notifications
            .lock()
            .expect("code-mode notification buffer poisoned")
            .entry(cell_id.as_str().to_string())
            .or_default()
            .push(text);
        Box::pin(async { Ok(()) })
    }

    fn cell_closed(&self, _cell_id: &CellId) {}
}

/// Per-session code-mode surface: one [`CodeModeService`] shared by `exec`/`wait`
/// so `store`/`load` values and live cells persist across calls within a session.
struct CodeModeSurface {
    service: CodeModeService,
    catalog: Vec<ToolDefinition>,
    notifications: Arc<NotificationBuffer>,
}

impl CodeModeSurface {
    fn new(seam: Arc<dyn ToolCapability>, catalog: Vec<ToolDefinition>) -> Self {
        let notifications = Arc::new(NotificationBuffer::default());
        let delegate = Arc::new(HarnessDelegate {
            seam,
            notifications: Arc::clone(&notifications),
        });
        Self {
            service: CodeModeService::with_delegate(delegate),
            catalog,
            notifications,
        }
    }

    /// Remove and return the buffered `notify(...)` payloads for one cell.
    fn drain_notifications(&self, cell_id: &CellId) -> Vec<String> {
        self.notifications
            .lock()
            .expect("code-mode notification buffer poisoned")
            .remove(cell_id.as_str())
            .unwrap_or_default()
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

/// Render queued `notify(...)` payloads as a clearly-delimited section
/// appended to the cell output body. Empty when nothing was queued.
fn render_notifications(notifications: &[String]) -> String {
    if notifications.is_empty() {
        return String::new();
    }
    format!("\n\n[notifications]\n{}", notifications.join("\n"))
}

/// The cell a runtime response belongs to (every variant carries one).
fn response_cell_id(response: &RuntimeResponse) -> &CellId {
    match response {
        RuntimeResponse::Result { cell_id, .. }
        | RuntimeResponse::Yielded { cell_id, .. }
        | RuntimeResponse::Terminated { cell_id, .. } => cell_id,
    }
}

/// Map a runtime response into a harness tool result. A still-running cell
/// surfaces its `cell_id` so the model can `wait`. `notifications` are the
/// drained `notify(...)` payloads for the cell, delivered as a delimited
/// section alongside the cell output.
fn response_to_result(response: RuntimeResponse, notifications: Vec<String>) -> ToolResult {
    let notifications = render_notifications(&notifications);
    match response {
        RuntimeResponse::Result {
            content_items,
            error_text,
            ..
        } => {
            let body = format!("{}{notifications}", join_content(&content_items));
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
            let body = format!("{}{notifications}", join_content(&content_items));
            ToolResult::Text(format!(
                "{body}\n\nScript running with cell ID {cell_id}. Call `wait` with this cell_id for more output.",
            ))
        }
        RuntimeResponse::Terminated {
            cell_id,
            content_items,
        } => {
            let body = format!("{}{notifications}", join_content(&content_items));
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
            Ok(response) => {
                let notifications = self
                    .surface
                    .drain_notifications(response_cell_id(&response));
                response_to_result(response, notifications)
            }
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
            Ok(WaitOutcome::LiveCell(response)) | Ok(WaitOutcome::MissingCell(response)) => {
                let notifications = self
                    .surface
                    .drain_notifications(response_cell_id(&response));
                response_to_result(response, notifications)
            }
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
    mode: CodeMode,
    namespaces: &BTreeMap<String, ToolNamespaceDescription>,
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
            namespace_binding: t
                .namespace_binding()
                .map(|(namespace, method)| NamespaceBinding { namespace, method }),
        })
        .collect();

    let description = build_exec_tool_description(
        &catalog,
        namespaces,
        /*code_mode_only*/ mode == CodeMode::Only,
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
            edits: Arc::new(Mutex::new(bro_tools::EditSink::default())),
            session_env: Arc::new(BTreeMap::new()),
            tool_arg_defaults: Arc::new(bro_tools::ToolArgDefaults::default()),
            shell_env: Arc::new(Default::default()),
        }
    }

    fn exec_with(callable: Vec<Arc<dyn Tool>>) -> Arc<dyn Tool> {
        let seam: Arc<dyn ToolCapability> = Arc::new(crate::capabilities::HostTools::new(
            callable.clone(),
            test_cx(),
        ));
        code_mode_tools(&callable, seam, CodeMode::Only, &BTreeMap::new()).remove(0)
    }

    #[test]
    fn code_mode_parses_tristate_and_defaults() {
        assert_eq!("off".parse(), Ok(CodeMode::Off));
        assert_eq!("optional".parse(), Ok(CodeMode::Optional));
        assert_eq!("only".parse(), Ok(CodeMode::Only));
        assert_eq!(CodeMode::default(), CodeMode::Optional);
        assert_eq!(CodeMode::parse_or_default("bogus"), CodeMode::Optional);
        assert!(CodeMode::Optional.enables_code_surface());
        assert!(!CodeMode::Off.enables_code_surface());
        assert!(CodeMode::Only.defers_builtins());
        assert!(!CodeMode::Optional.defers_builtins());
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
    async fn cell_file_read_returns_file_contents() {
        // Reproduction: GLM's `Promise.all([tools.file_read(...)])` cell reported
        // empty file contents even though the files had content. A cell's
        // file_read must return the real file text.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("probe.txt"), "hello-codemode-123").unwrap();
        let cx = ToolCx {
            root: dir.path().to_path_buf(),
            safety: Arc::new(bro_tools::SafetyPolicy::new()),
            http: reqwest::Client::new(),
            todos: Arc::new(Mutex::new(bro_tools::TodoList::default())),
            shell_sessions: Arc::new(Mutex::new(bro_tools::ShellSessions::default())),
            edits: Arc::new(Mutex::new(bro_tools::EditSink::default())),
            session_env: Arc::new(BTreeMap::new()),
            tool_arg_defaults: Arc::new(bro_tools::ToolArgDefaults::default()),
            shell_env: Arc::new(Default::default()),
        };
        let callable: Vec<Arc<dyn Tool>> =
            vec![Arc::new(bro_tools::workspace::FileRead) as Arc<dyn Tool>];
        let seam: Arc<dyn ToolCapability> = Arc::new(crate::capabilities::HostTools::new(
            callable.clone(),
            cx.clone(),
        ));
        let exec = code_mode_tools(&callable, seam, CodeMode::Only, &BTreeMap::new()).remove(0);
        let result = exec
            .call(
                json!({ "source": "const r = await tools.file_read({ file_path: 'probe.txt' }); text(typeof r === 'string' ? r : JSON.stringify(r));" }),
                &cx,
            )
            .await;
        match result {
            ToolResult::Text(t) => {
                assert!(t.contains("hello-codemode-123"), "got: {t:?}")
            }
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
    async fn notify_payloads_ride_the_exec_result() {
        // notify() is buffered and delivered as a `[notifications]` section of
        // the exec result, not silently logged.
        let exec = exec_with(vec![Arc::new(Echo) as Arc<dyn Tool>]);
        let result = exec
            .call(
                json!({ "source": "notify('ping'); notify('pong'); text('done');" }),
                &test_cx(),
            )
            .await;
        match result {
            ToolResult::Text(t) => {
                assert!(t.contains("done"), "got: {t}");
                assert!(t.contains("[notifications]"), "got: {t}");
                assert!(t.contains("ping") && t.contains("pong"), "got: {t}");
            }
            other => panic!("expected text, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn notify_payloads_ride_a_yielded_exec_result() {
        // A still-running cell's queued notifications surface on the yield,
        // alongside the "Script running with cell ID" handle.
        let exec = exec_with(vec![Arc::new(Echo) as Arc<dyn Tool>]);
        let source =
            "// @exec: {\"yield_time_ms\": 200}\nnotify('bg-ping'); await new Promise(() => {});";
        let result = exec.call(json!({ "source": source }), &test_cx()).await;
        match result {
            ToolResult::Text(t) => {
                assert!(t.contains("Script running with cell ID"), "got: {t}");
                assert!(t.contains("[notifications]"), "got: {t}");
                assert!(t.contains("bg-ping"), "got: {t}");
            }
            other => panic!("expected text, got {other:?}"),
        }
    }

    /// Namespace-bound echo: canonical name `ns.echo`, projected into cells
    /// as the namespace global `ns.echo(...)` instead of `tools.*`.
    struct NamespacedEcho;

    #[async_trait]
    impl Tool for NamespacedEcho {
        fn name(&self) -> &str {
            "ns.echo"
        }
        fn description(&self) -> &str {
            "Echo the input back from a namespace global."
        }
        fn input_schema(&self) -> Value {
            json!({ "type": "object" })
        }
        fn namespace_binding(&self) -> Option<(String, String)> {
            Some(("ns".to_string(), "echo".to_string()))
        }
        async fn call(&self, input: Value, _cx: &ToolCx) -> ToolResult {
            ToolResult::Json(json!({ "echoed": input }))
        }
    }

    #[tokio::test]
    async fn namespace_binding_projects_as_namespace_global() {
        let exec = exec_with(vec![Arc::new(NamespacedEcho) as Arc<dyn Tool>]);
        let result = exec
            .call(
                json!({ "source": "const r = await ns.echo({ a: 1 }); text(JSON.stringify(r));" }),
                &test_cx(),
            )
            .await;
        match result {
            ToolResult::Text(t) => assert!(t.contains("\"a\":1"), "got: {t}"),
            other => panic!("expected text, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn namespace_binding_is_absent_from_flat_tools_object() {
        let exec = exec_with(vec![Arc::new(NamespacedEcho) as Arc<dyn Tool>]);
        let result = exec
            .call(
                json!({ "source": "text(typeof ns.echo); text(typeof tools['ns_echo']); text(typeof tools['ns.echo']);" }),
                &test_cx(),
            )
            .await;
        match result {
            ToolResult::Text(t) => {
                let lines: Vec<&str> = t.lines().collect();
                assert_eq!(lines[0], "function", "got: {t}");
                assert_eq!(lines[1], "undefined", "got: {t}");
                assert_eq!(lines[2], "undefined", "got: {t}");
            }
            other => panic!("expected text, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn namespace_binding_dispatches_through_seam_filter() {
        // The §4.5 deny-bypass guard holds for namespace globals too: in the
        // projected namespace but absent from the seam ⇒ fail closed.
        let callable = vec![Arc::new(NamespacedEcho) as Arc<dyn Tool>];
        let empty_seam: Arc<dyn ToolCapability> =
            Arc::new(crate::capabilities::HostTools::new(Vec::new(), test_cx()));
        let exec = code_mode_tools(&callable, empty_seam, CodeMode::Only, &BTreeMap::new()).remove(0);
        let result = exec
            .call(json!({ "source": "await ns.echo({ a: 1 });" }), &test_cx())
            .await;
        assert!(
            matches!(result, ToolResult::Error(_)),
            "denied namespace binding must fail closed, got {result:?}"
        );
    }

    #[tokio::test]
    async fn cell_composes_code_facts_bindings() {
        // Slice-1 proof: a cell chains code.items → code.read over real file
        // facts, values staying in the isolate, hash-anchored Spans intact.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("probe.rs"),
            "pub struct Alpha;\n\npub fn beta() -> u8 {\n    7\n}\n",
        )
        .unwrap();
        let cx = ToolCx {
            root: dir.path().to_path_buf(),
            safety: Arc::new(bro_tools::SafetyPolicy::new()),
            http: reqwest::Client::new(),
            todos: Arc::new(Mutex::new(bro_tools::TodoList::default())),
            shell_sessions: Arc::new(Mutex::new(bro_tools::ShellSessions::default())),
            edits: Arc::new(Mutex::new(bro_tools::EditSink::default())),
            session_env: Arc::new(BTreeMap::new()),
            tool_arg_defaults: Arc::new(bro_tools::ToolArgDefaults::default()),
            shell_env: Arc::new(Default::default()),
        };
        let callable = crate::bindings::binding_tools();
        let seam: Arc<dyn ToolCapability> = Arc::new(crate::capabilities::HostTools::new(
            callable.clone(),
            cx.clone(),
        ));
        let exec = code_mode_tools(
            &callable,
            seam,
            CodeMode::Only,
            &crate::bindings::namespace_descriptions(),
        )
        .remove(0);
        let source = r#"
const inv = await code.items({ file: "probe.rs" });
const beta = inv.items.find(i => i.name === "beta");
const body = await code.read({ span: beta.span });
text(`${inv.language}:${beta.kind}:${body.text.startsWith("pub fn beta")}`);
"#;
        let result = exec.call(json!({ "source": source }), &cx).await;
        match result {
            ToolResult::Text(t) => {
                assert!(t.contains("rust:function_item:true"), "got: {t}")
            }
            other => panic!("expected text, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn exec_description_documents_namespace_globals_in_optional_mode() {
        // D3 regression guard (refactor-v2-pressure-test.md §1): namespace
        // declarations render even when code_mode != only.
        let callable = vec![Arc::new(NamespacedEcho) as Arc<dyn Tool>];
        let seam: Arc<dyn ToolCapability> = Arc::new(crate::capabilities::HostTools::new(
            callable.clone(),
            test_cx(),
        ));
        let namespaces = BTreeMap::from([(
            "ns".to_string(),
            ToolNamespaceDescription {
                name: "ns".to_string(),
                description: "Namespace guidance.".to_string(),
                declarations: "declare const ns: { echo(args: {}): Promise<unknown>; };"
                    .to_string(),
            },
        )]);
        let exec = code_mode_tools(&callable, seam, CodeMode::Optional, &namespaces).remove(0);
        let description = exec.description();
        assert!(description.contains("## `ns` namespace"), "{description}");
        assert!(description.contains("Namespace guidance."), "{description}");
        assert!(
            description.contains("declare const ns: { echo(args: {}): Promise<unknown>; };"),
            "{description}"
        );
        assert!(
            description.contains("installed as globals beside `tools`"),
            "{description}"
        );
    }

    #[tokio::test]
    async fn denied_tool_fails_closed_in_cell() {
        // `echo` is in the projected namespace but NOT in the seam — the §4.5
        // deny-bypass guard: an in-cell call must reject, not bypass the filter.
        let callable = vec![Arc::new(Echo) as Arc<dyn Tool>];
        let empty_seam: Arc<dyn ToolCapability> =
            Arc::new(crate::capabilities::HostTools::new(Vec::new(), test_cx()));
        let exec = code_mode_tools(&callable, empty_seam, CodeMode::Only, &BTreeMap::new()).remove(0);
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
