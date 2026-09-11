//! Codex code-mode adoption seam (`design/bro-harness/` — supersedes NARF).
//!
//! Projects the harness tool surface into the vendored [`bro_code_mode`] runtime
//! as a typed `tools.*` namespace, and exposes exactly two model-facing tools:
//! `exec` (run a JS/TS cell that composes `tools.*` calls, emits content via
//! `text()`, and persists across cells via `store()`/`load()`) and
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
    ExecuteRequest, NamespaceBinding, NotificationFuture, PUBLIC_TOOL_NAME, RuntimeResponse,
    ToolDefinition, ToolInvocationFuture, ToolName, ToolNamespaceDescription, WAIT_TOOL_NAME,
    WaitOutcome, WaitRequest, build_exec_tool_description, build_wait_tool_description,
    is_code_mode_nested_tool, parse_exec_source,
};
use bro_tools::{Tool, ToolCx, ToolResult};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

mod output;
use output::response_to_result;

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
            // Admission is cancellation-aware at the host gate. Once admitted,
            // await its real outcome, including blocking mutations and cleanup.
            match seam
                .call_tool_with_context(call, cancellation_token, invocation.context_id)
                .await
            {
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

/// Local addition: owned `exec`/`wait` pair plus the session lifecycle hook.
///
/// Most harness callers only need the tool vector returned by
/// [`code_mode_tools`]. Standalone drivers such as `isolate --cell` also need a
/// deliberate shutdown point so live cells and delegated child work are stopped
/// before the process exits.
#[derive(Clone)]
pub struct CodeModeToolSession {
    tools: Vec<Arc<dyn Tool>>,
    surface: Arc<CodeModeSurface>,
}

impl CodeModeToolSession {
    pub fn new(
        callable: &[Arc<dyn Tool>],
        seam: Arc<dyn ToolCapability>,
        mode: CodeMode,
        namespaces: &BTreeMap<String, ToolNamespaceDescription>,
    ) -> Self {
        let mut seen: std::collections::HashMap<String, Arc<dyn Tool>> =
            std::collections::HashMap::new();
        let mut catalog: Vec<ToolDefinition> = callable
            .iter()
            .filter(|t| is_code_mode_nested_tool(t.name()))
            .filter(|tool| {
                if seen
                    .get(tool.name())
                    .is_some_and(|previous| Arc::ptr_eq(previous, tool))
                {
                    return false;
                }
                seen.entry(tool.name().to_owned())
                    .or_insert_with(|| (*tool).clone());
                true
            })
            .map(|t| ToolDefinition {
                name: t.name().to_string(),
                tool_name: ToolName::plain(t.name()),
                description: t.description().to_string(),
                kind: if t.freeform_grammar().is_some() {
                    CodeModeToolKind::Freeform
                } else {
                    CodeModeToolKind::Function
                },
                input_schema: Some(t.input_schema()),
                output_schema: t.output_schema(),
                namespace_binding: t
                    .namespace_binding()
                    .map(|(namespace, method)| NamespaceBinding { namespace, method }),
            })
            .collect();

        // One tool admitted through multiple visibility lanes is one callable.
        // Distinct implementations remain for runtime admission to reject,
        // even if they happen to advertise identical metadata.
        catalog.sort_by(|left, right| left.name.cmp(&right.name));

        let description = build_exec_tool_description(
            &catalog,
            namespaces,
            /*code_mode_only*/ mode == CodeMode::Only,
            false,
        );
        let surface = Arc::new(CodeModeSurface::new(seam, catalog));
        let tools = vec![
            Arc::new(ExecTool {
                surface: surface.clone(),
                description,
            }) as Arc<dyn Tool>,
            Arc::new(WaitTool {
                surface: surface.clone(),
            }) as Arc<dyn Tool>,
        ];

        Self { tools, surface }
    }

    pub fn tools(&self) -> Vec<Arc<dyn Tool>> {
        self.tools.clone()
    }

    pub async fn shutdown(&self) -> Result<(), String> {
        self.surface.service.shutdown().await
    }

    /// Includes completed cells whose terminal output has not been consumed.
    pub async fn has_outstanding_work(&self) -> bool {
        self.surface.service.has_outstanding_work().await
    }

    /// Close yielded cells at an interrupted turn boundary without closing the
    /// reusable session. Call after active exec/wait invocations have drained.
    pub async fn cancel_all(&self) -> Vec<ToolResult> {
        self.surface
            .service
            .cancel_all()
            .await
            .into_iter()
            .map(|response| {
                let cell_id = response_cell_id(&response).clone();
                let notifications = self.surface.drain_notifications(&cell_id);
                let result =
                    response_to_result(response, notifications, None, crate::bound::cap_bytes());
                let (content, is_error) = result.into_content();
                let content = format!("Code-mode cell {cell_id}:\n{content}");
                if is_error {
                    ToolResult::Error(content)
                } else {
                    ToolResult::Text(content)
                }
            })
            .collect()
    }
}

/// The cell a runtime response belongs to (every variant carries one).
fn response_cell_id(response: &RuntimeResponse) -> &CellId {
    match response {
        RuntimeResponse::Result { cell_id, .. }
        | RuntimeResponse::Yielded { cell_id, .. }
        | RuntimeResponse::Terminated { cell_id, .. } => cell_id,
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

    async fn call(&self, input: Value, cx: &ToolCx) -> ToolResult {
        if cx.cancellation.is_cancelled() {
            return ToolResult::Error("exec cancelled before cell admission".into());
        }
        let Some(source) = input.get("source").and_then(Value::as_str) else {
            return ToolResult::Error("exec: `source` is required".into());
        };
        let parsed = match parse_exec_source(source) {
            Ok(p) => p,
            Err(e) => return ToolResult::Error(format!("exec: {e}")),
        };
        let max_output_tokens = parsed.max_output_tokens;
        let request = ExecuteRequest {
            context_id: Some(cx.instruction_generation),
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
        // Startup itself is never dropped: once execute allocates a cell, its
        // identifier is known and any raced cancellation can close it fully.
        let cell_id = started.cell_id.clone();
        let initial_response = started.initial_response();
        tokio::pin!(initial_response);
        let outcome = tokio::select! {
            biased;
            response = &mut initial_response => response,
            _ = cx.cancellation.cancelled() => {
                match self.surface.service.terminate(cell_id).await {
                    Ok(WaitOutcome::LiveCell(response)) => Ok(response),
                    // Completion can race cell lookup. Its original receiver
                    // owns the real outcome even after the cell map is empty.
                    Ok(WaitOutcome::MissingCell(missing)) => initial_response.await.or(Ok(missing)),
                    Err(error) => Err(error),
                }
            }
        };
        match outcome {
            Ok(response) => {
                let notifications = self
                    .surface
                    .drain_notifications(response_cell_id(&response));
                response_to_result(response, notifications, max_output_tokens, cx.output_budget)
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
                "max_tokens": {
                    "type": "integer",
                    "description": "Output token budget for this wait call, estimated at four bytes per token. Defaults to 10000; the shared host result limit also applies (normally 16 KiB, configurable by the operator), with space reserved for status and diagnostics.",
                    "minimum": 0,
                    "maximum": 9007199254740991_u64
                },
                "terminate": {
                    "type": "boolean",
                    "description": "Stop the running cell instead of waiting for output."
                }
            },
            "required": ["cell_id"]
        })
    }

    async fn call(&self, input: Value, cx: &ToolCx) -> ToolResult {
        let Some(cell_id) = input.get("cell_id").and_then(Value::as_str) else {
            return ToolResult::Error("wait: `cell_id` is required".into());
        };
        if cell_id.len() > 128 {
            return ToolResult::Error("wait: `cell_id` exceeds 128 bytes".into());
        }
        let max_tokens = match input.get("max_tokens") {
            None | Some(Value::Null) => None,
            Some(value) => match value
                .as_u64()
                .filter(|tokens| *tokens <= 9_007_199_254_740_991)
                .and_then(|tokens| usize::try_from(tokens).ok())
            {
                Some(tokens) => Some(tokens),
                None => {
                    return ToolResult::Error(
                        "wait: `max_tokens` must be a non-negative safe integer".into(),
                    );
                }
            },
        };
        let cell_id = CellId::new(cell_id.to_string());
        let terminate = input
            .get("terminate")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let outcome = if terminate || cx.cancellation.is_cancelled() {
            self.surface.service.terminate(cell_id).await
        } else {
            let yield_time_ms = input
                .get("yield_time_ms")
                .and_then(Value::as_u64)
                .unwrap_or(bro_code_mode::DEFAULT_WAIT_YIELD_TIME_MS);
            let waiting = self.surface.service.wait(WaitRequest {
                cell_id: cell_id.clone(),
                yield_time_ms,
            });
            tokio::pin!(waiting);
            tokio::select! {
                biased;
                response = &mut waiting => response,
                _ = cx.cancellation.cancelled() => {
                    match self.surface.service.terminate(cell_id.clone()).await {
                        Ok(WaitOutcome::MissingCell(_)) => waiting.await,
                        response => response,
                    }
                }
            }
        };
        match outcome {
            Ok(WaitOutcome::LiveCell(response)) | Ok(WaitOutcome::MissingCell(response)) => {
                let notifications = self
                    .surface
                    .drain_notifications(response_cell_id(&response));
                response_to_result(response, notifications, max_tokens, cx.output_budget)
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
    CodeModeToolSession::new(callable, seam, mode, namespaces).tools()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::sync::Mutex;
    use std::time::Duration;

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
            tool_observations: Default::default(),
            instruction_generation: 0,
            instruction_policy: None,
            root: std::env::temp_dir(),
            safety: Arc::new(bro_tools::SafetyPolicy::new()),
            http: reqwest::Client::new(),
            todos: Arc::new(Mutex::new(bro_tools::TodoList::default())),
            shell_sessions: Arc::new(Mutex::new(bro_tools::ShellSessions::default())),
            edits: Arc::new(Mutex::new(bro_tools::EditSink::default())),
            cancellation: Default::default(),
            output_budget: 16 * 1024,
            child_env: Arc::new(Default::default()),
            session_env: Arc::new(BTreeMap::new()),
            tool_arg_defaults: Arc::new(bro_tools::ToolArgDefaults::default()),
            shell_env: Arc::new(Default::default()),
        }
    }

    fn code_mode_pair(callable: Vec<Arc<dyn Tool>>) -> (Arc<dyn Tool>, Arc<dyn Tool>) {
        let seam: Arc<dyn ToolCapability> = Arc::new(crate::capabilities::HostTools::new(
            callable.clone(),
            test_cx(),
        ));
        let mut tools = code_mode_tools(&callable, seam, CodeMode::Only, &BTreeMap::new());
        let exec = tools.remove(0);
        let wait = tools.remove(0);
        (exec, wait)
    }

    fn exec_with(callable: Vec<Arc<dyn Tool>>) -> Arc<dyn Tool> {
        code_mode_pair(callable).0
    }

    fn yielded_cell_id(text: &str) -> String {
        let marker = "Script running with cell ID ";
        let start = text.find(marker).expect("yielded cell id missing") + marker.len();
        text[start..]
            .split('.')
            .next()
            .expect("yielded cell id terminator missing")
            .to_string()
    }

    struct BlockingMutation {
        started: tokio::sync::Semaphore,
        release: tokio::sync::Semaphore,
        finished: std::sync::atomic::AtomicBool,
    }

    #[async_trait]
    impl Tool for BlockingMutation {
        fn name(&self) -> &str {
            "mutation"
        }
        fn description(&self) -> &str {
            "Controlled mutation fixture"
        }
        fn input_schema(&self) -> Value {
            json!({"type":"object"})
        }
        async fn call(&self, _: Value, _: &ToolCx) -> ToolResult {
            self.started.add_permits(1);
            self.release.acquire().await.unwrap().forget();
            self.finished
                .store(true, std::sync::atomic::Ordering::SeqCst);
            ToolResult::Json(json!({"mutation":"finished"}))
        }
    }

    fn cancellation_fixture() -> (CodeModeToolSession, Arc<BlockingMutation>) {
        let mutation = Arc::new(BlockingMutation {
            started: tokio::sync::Semaphore::new(0),
            release: tokio::sync::Semaphore::new(0),
            finished: std::sync::atomic::AtomicBool::new(false),
        });
        let callable: Vec<Arc<dyn Tool>> = vec![mutation.clone()];
        let seam = Arc::new(crate::capabilities::HostTools::new(
            callable.clone(),
            test_cx(),
        ));
        (
            CodeModeToolSession::new(&callable, seam, CodeMode::Only, &BTreeMap::new()),
            mutation,
        )
    }

    #[tokio::test]
    async fn exec_cancellation_drains_mutation_and_keeps_receipt_at_zero_output_budget() {
        let (session, mutation) = cancellation_fixture();
        let exec = session.tools()[0].clone();
        let cx = test_cx();
        let cancellation = cx.cancellation.clone();
        let call = tokio::spawn(async move {
            exec.call(json!({"source":"// @exec: {\"yield_time_ms\": 60000, \"max_output_tokens\": 0}\nnotify('callback delivered'); text(await tools.mutation({}));"}), &cx).await
        });
        tokio::time::timeout(Duration::from_secs(2), mutation.started.acquire())
            .await
            .unwrap()
            .unwrap()
            .forget();
        cancellation.cancel();
        tokio::time::sleep(Duration::from_millis(30)).await;
        assert!(
            !call.is_finished(),
            "cancelled exec must wait for actual mutation completion"
        );
        mutation.release.add_permits(1);
        let result = tokio::time::timeout(Duration::from_secs(2), call)
            .await
            .unwrap()
            .unwrap();
        let text = result.into_content().0;
        assert!(mutation.finished.load(std::sync::atomic::Ordering::SeqCst));
        assert!(text.contains("Script terminated"), "{text}");
        assert!(
            text.contains("nested tool outcomes") && text.contains("finished"),
            "{text}"
        );
        assert!(text.contains("callback delivered"), "{text}");
    }

    #[tokio::test]
    async fn cancel_all_drains_yielded_cells_and_session_accepts_next_turn() {
        let (session, mutation) = cancellation_fixture();
        let exec = session.tools()[0].clone();
        let result = exec.call(json!({"source":"// @exec: {\"yield_time_ms\": 20}\ntext(await tools.mutation({}));"}), &test_cx()).await;
        assert!(
            result
                .into_content()
                .0
                .contains("Script running with cell ID")
        );
        tokio::time::timeout(Duration::from_secs(2), mutation.started.acquire())
            .await
            .unwrap()
            .unwrap()
            .forget();
        let cancelled = session.cancel_all();
        tokio::pin!(cancelled);
        assert!(
            tokio::time::timeout(Duration::from_millis(30), &mut cancelled)
                .await
                .is_err()
        );
        mutation.release.add_permits(1);
        let results = tokio::time::timeout(Duration::from_secs(2), cancelled)
            .await
            .unwrap();
        assert_eq!(results.len(), 1);
        let text = results.into_iter().next().unwrap().into_content().0;
        assert!(
            text.contains("Code-mode cell") && text.contains("finished"),
            "{text}"
        );
        let result = exec
            .call(json!({"source":"text('next turn');"}), &test_cx())
            .await;
        assert!(result.into_content().0.contains("next turn"));
    }

    #[tokio::test]
    async fn cancelled_exec_never_allocates_a_cell() {
        let (session, _) = cancellation_fixture();
        let cx = test_cx();
        cx.cancellation.cancel();
        let result = session.tools()[0]
            .call(json!({"source":"while (true) {}"}), &cx)
            .await;
        assert!(result.is_error());
        assert!(session.cancel_all().await.is_empty());
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
    async fn exec_and_wait_enforce_separate_output_budgets() {
        let (exec, wait) = code_mode_pair(vec![]);
        let source = r#"// @exec: {"max_output_tokens": 1}
text('AB' + 'x'.repeat(20000) + 'YZ');
await yield_control();
text('CD' + 'z'.repeat(20000) + 'UV');
"#;
        let initial = exec.call(json!({ "source": source }), &test_cx()).await;
        let cell_id = match initial {
            ToolResult::Text(text) => {
                assert!(text.starts_with("Script running with cell ID"));
                assert!(text.contains("Output:\nAB\n[output truncated;"));
                assert!(text.ends_with("\nYZ"));
                yielded_cell_id(&text)
            }
            other => panic!("expected yielded text, got {other:?}"),
        };
        assert_eq!(
            wait.input_schema()["properties"]["max_tokens"]["type"],
            "integer"
        );
        let result = wait
            .call(
                json!({ "cell_id": cell_id, "max_tokens": 2, "yield_time_ms": 5000 }),
                &test_cx(),
            )
            .await;
        match result {
            ToolResult::Text(text) => {
                assert!(text.starts_with("Script completed\n"), "{text}");
                assert!(text.contains("Output:\nCDzz\n[output truncated;"), "{text}");
                assert!(text.ends_with("\nzzUV"), "{text}");
                assert!(
                    !text.contains("AB"),
                    "wait must not replay prior output: {text}"
                );
            }
            other => panic!("expected completed text, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn invalid_wait_budget_does_not_consume_or_terminate_the_cell() {
        let (exec, wait) = code_mode_pair(vec![]);
        let result = exec
            .call(
                json!({ "source": "await yield_control(); text('still available');" }),
                &test_cx(),
            )
            .await;
        let ToolResult::Text(text) = result else {
            panic!("expected yielded result, got {result:?}");
        };
        let cell_id = yielded_cell_id(&text);
        for budget in [
            json!(-1),
            json!(1.5),
            json!("2"),
            json!(9_007_199_254_740_992_u64),
        ] {
            let result = wait
                .call(
                    json!({ "cell_id": cell_id, "max_tokens": budget, "terminate": true }),
                    &test_cx(),
                )
                .await;
            assert!(
                matches!(result, ToolResult::Error(ref text) if text.contains("non-negative safe integer"))
            );
        }
        let result = wait
            .call(
                json!({ "cell_id": cell_id, "yield_time_ms": 5000 }),
                &test_cx(),
            )
            .await;
        assert!(matches!(result, ToolResult::Text(ref text) if text.contains("still available")));
    }

    #[tokio::test]
    async fn emitted_image_reports_unsupported_transport() {
        let exec = exec_with(vec![]);
        assert!(exec.description().contains("no image is delivered"));
        let result = exec
            .call(
                json!({ "source": "image('data:image/png;base64,AA=='); text('other output');" }),
                &test_cx(),
            )
            .await;
        match result {
            ToolResult::Error(text) => {
                assert!(text.starts_with("Script failed\n"));
                assert!(text.contains("image() output is unsupported"));
                assert!(text.contains("other output"));
                assert!(!text.contains("AA=="));
            }
            other => panic!("expected unsupported-image error, got {other:?}"),
        }
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
    async fn cell_shell_children_keep_session_environment_policy() {
        let dir = tempfile::tempdir().unwrap();
        let mut cx = test_cx();
        cx.root = dir.path().canonicalize().unwrap();
        // ShellRun sets NO_COLOR before applying the session policy. Use this
        // harmless known value to test the real V8 -> spawned delegate -> shell
        // path without reading credentials or changing process environment.
        cx.child_env = Arc::new(bro_tools::ChildEnvironment::new(["NO_COLOR".into()]));
        let callable: Vec<Arc<dyn Tool>> = vec![Arc::new(bro_tools::ShellRun)];
        let seam: Arc<dyn ToolCapability> = Arc::new(crate::capabilities::HostTools::new(
            callable.clone(),
            cx.clone(),
        ));
        let session = CodeModeToolSession::new(&callable, seam, CodeMode::Only, &BTreeMap::new());
        let result = session.tools()[0]
            .call(
                json!({"source": r#"text(await tools.shell_run({command: "printf '%s' \"${NO_COLOR-unset}\"", yield_time_ms: 0}));"#}),
                &cx,
            )
            .await;
        session.shutdown().await.unwrap();
        let ToolResult::Text(text) = result else {
            panic!("expected shell result, got {result:?}");
        };
        assert!(text.contains("\"stdout\":\"unset\""), "{text}");
        assert!(text.contains("\"exit_code\":0"), "{text}");
    }

    #[tokio::test]
    async fn cell_shell_run_output_filter_arrays_return_terminal_result() {
        let exec = exec_with(vec![Arc::new(bro_tools::ShellRun) as Arc<dyn Tool>]);
        let source = r#"
const result = await tools.shell_run({
  command: "printf 'noise\nBUILD SUCCESSFUL\nerror: keep\n'; printf 'warning: keep\nignore\n' >&2; exit 7",
  yield_time_ms: 0,
  timeout_ms: 300_000,
  max_output_tokens: 12_000,
  output_filter: {
    stdout: ["BUILD SUCCESSFUL", "error:"],
    stderr: ["warning:"]
  }
});
text(JSON.stringify(result));
"#;
        let result = tokio::time::timeout(
            Duration::from_secs(10),
            exec.call(json!({ "source": source }), &test_cx()),
        )
        .await
        .expect("code-mode shell_run with output_filter arrays must not hang");
        match result {
            ToolResult::Text(t) => {
                assert!(t.contains("\"exit_code\":7"), "got: {t}");
                assert!(t.contains("BUILD SUCCESSFUL"), "got: {t}");
                assert!(t.contains("error: keep"), "got: {t}");
                assert!(t.contains("warning: keep"), "got: {t}");
                assert!(t.contains("\"output_filter\""), "got: {t}");
                assert!(!t.contains("noise"), "got: {t}");
                assert!(!t.contains("ignore"), "got: {t}");
            }
            other => panic!("expected text, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn cell_shell_run_output_filter_can_yield_then_wait() {
        let (exec, wait) = code_mode_pair(vec![Arc::new(bro_tools::ShellRun) as Arc<dyn Tool>]);
        let source = r#"// @exec: {"yield_time_ms": 50}
const result = await tools.shell_run({
  command: "sleep 0.2; printf 'noise\nBUILD SUCCESSFUL\n'",
  yield_time_ms: 0,
  timeout_ms: 300_000,
  max_output_tokens: 12_000,
  output_filter: { stdout: "BUILD SUCCESSFUL" }
});
text(JSON.stringify(result));
"#;

        let initial = exec.call(json!({ "source": source }), &test_cx()).await;
        let cell_id = match initial {
            ToolResult::Text(t) => {
                assert!(t.contains("Script running with cell ID"), "got: {t}");
                assert!(!t.contains("BUILD SUCCESSFUL"), "got: {t}");
                yielded_cell_id(&t)
            }
            other => panic!("expected yielded text, got {other:?}"),
        };

        let result = tokio::time::timeout(
            Duration::from_secs(10),
            wait.call(
                json!({ "cell_id": cell_id, "yield_time_ms": 5000 }),
                &test_cx(),
            ),
        )
        .await
        .expect("waiting on yielded shell_run cell must complete");
        match result {
            ToolResult::Text(t) => {
                assert!(t.contains("\"exit_code\":0"), "got: {t}");
                assert!(t.contains("BUILD SUCCESSFUL"), "got: {t}");
                assert!(t.contains("\"output_filter\""), "got: {t}");
                assert!(!t.contains("noise"), "got: {t}");
            }
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
            tool_observations: Default::default(),
            instruction_generation: 0,
            instruction_policy: None,
            root: dir.path().to_path_buf(),
            safety: Arc::new(bro_tools::SafetyPolicy::new()),
            http: reqwest::Client::new(),
            todos: Arc::new(Mutex::new(bro_tools::TodoList::default())),
            shell_sessions: Arc::new(Mutex::new(bro_tools::ShellSessions::default())),
            edits: Arc::new(Mutex::new(bro_tools::EditSink::default())),
            cancellation: Default::default(),
            output_budget: 16 * 1024,
            child_env: Arc::new(Default::default()),
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
    async fn function_store_round_trips_callables_across_cells() {
        // The function store: store(key, fn) persists source, load(key)
        // revives a callable in a later (fresh) isolate.
        let exec = exec_with(vec![Arc::new(Echo) as Arc<dyn Tool>]);
        let _ = exec
            .call(
                json!({ "source": "store('helpers.double', (n) => n * 2);" }),
                &test_cx(),
            )
            .await;
        let result = exec
            .call(
                json!({ "source": "const double = load('helpers.double'); text(String(double(21)));" }),
                &test_cx(),
            )
            .await;
        match result {
            ToolResult::Text(t) => assert!(t.contains("42"), "got: {t}"),
            other => panic!("expected text, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn revived_function_with_lost_closure_fails_loudly_at_call() {
        // The self-contained constraint is enforced by reality, not a rule:
        // a captured variable is gone after source round-trip, and calling
        // the revived function throws ReferenceError.
        let exec = exec_with(vec![Arc::new(Echo) as Arc<dyn Tool>]);
        let _ = exec
            .call(
                json!({ "source": "const factor = 3; store('helpers.scaled', (n) => n * factor);" }),
                &test_cx(),
            )
            .await;
        let result = exec
            .call(
                json!({ "source": "const scaled = load('helpers.scaled'); text(String(scaled(2)));" }),
                &test_cx(),
            )
            .await;
        match result {
            ToolResult::Error(e) => assert!(e.contains("factor is not defined"), "got: {e}"),
            other => panic!("expected ReferenceError, got {other:?}"),
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
    async fn full_binding_catalog_has_discoverable_schemas_without_default_manuals() {
        let binding_session = crate::bindings::BindingToolSession::new();
        let callable = binding_session.tools();
        let count = callable.len();
        let seam: Arc<dyn ToolCapability> = Arc::new(crate::capabilities::HostTools::new(
            callable.clone(),
            test_cx(),
        ));
        let tools = code_mode_tools(
            &callable,
            seam,
            CodeMode::Optional,
            &crate::bindings::namespace_descriptions(),
        );
        assert!(
            tools[0].description().len() < 8_000,
            "default exec description grew to {} bytes",
            tools[0].description().len()
        );
        let result = tools[0].call(json!({"source": r#"
            for (const entry of ALL_TOOLS) {
                if (!entry.input_schema || !entry.declaration) throw Error("missing schema: " + entry.canonical_name);
                if (!entry.namespace || typeof globalThis[entry.namespace][entry.method] !== "function") throw Error("missing method: " + entry.canonical_name);
            }
            const assist = ALL_TOOLS.find(t => t.canonical_name === "lsp.assist");
            if (!assist || !assist.declaration.includes("assist(args:")) throw Error("missing assist declaration");
            text({count: ALL_TOOLS.length, assist: assist.callable});
        "#}), &test_cx()).await;
        let ToolResult::Text(text) = result else {
            panic!("binding discovery failed: {result:?}");
        };
        assert!(text.contains(&format!("\"count\":{count}")), "{text}");
        assert!(text.contains("\"assist\":\"lsp.assist\""), "{text}");
        binding_session.shutdown().await;
    }

    #[tokio::test]
    async fn discovery_metadata_drives_flat_and_namespace_calls_in_both_modes() {
        for mode in [CodeMode::Optional, CodeMode::Only] {
            let callable: Vec<Arc<dyn Tool>> = vec![Arc::new(Echo), Arc::new(NamespacedEcho)];
            let seam: Arc<dyn ToolCapability> = Arc::new(crate::capabilities::HostTools::new(
                callable.clone(),
                test_cx(),
            ));
            let mut tools = code_mode_tools(&callable, seam, mode, &BTreeMap::new());
            let result = tools.remove(0).call(json!({"source": r#"
                for (const canonical of ["echo", "ns.echo"]) {
                    const entry = ALL_TOOLS.find(t => t.canonical_name === canonical);
                    if (!entry || entry.input_schema.type !== "object" || entry.kind !== "function") throw Error("missing schema");
                    if (!entry.declaration.includes("echo(args:")) throw Error("missing generated declaration");
                    const owner = entry.namespace === null ? tools : globalThis[entry.namespace];
                    if (typeof owner[entry.method] !== "function") throw Error("uncallable discovery result");
                    text({canonical: entry.canonical_name, callable: entry.callable, value: await owner[entry.method]({probe: canonical})});
                }
            "#}), &test_cx()).await;
            let ToolResult::Text(text) = result else {
                panic!("discovery execution failed: {result:?}");
            };
            assert!(text.contains("\"callable\":\"tools.echo\""), "{text}");
            assert!(text.contains("\"callable\":\"ns.echo\""), "{text}");
            assert!(text.contains("\"probe\":\"ns.echo\""), "{text}");
        }
    }

    #[tokio::test]
    async fn duplicate_visibility_of_same_tool_does_not_duplicate_discovery() {
        let echo: Arc<dyn Tool> = Arc::new(Echo);
        let exec = exec_with(vec![echo.clone(), echo]);
        let result = exec.call(json!({"source":"text(ALL_TOOLS.filter(t => t.canonical_name === 'echo').length); text(await tools.echo({ok:true}));"}), &test_cx()).await;
        let ToolResult::Text(text) = result else {
            panic!("duplicate visibility failed: {result:?}");
        };
        assert!(text.contains("Output:\n1\n"), "{text}");
        assert!(text.contains("\"ok\":true"), "{text}");
    }

    struct CatalogProbe {
        name: &'static str,
        binding: Option<(&'static str, &'static str)>,
        calls: Arc<std::sync::atomic::AtomicUsize>,
    }

    #[async_trait]
    impl Tool for CatalogProbe {
        fn name(&self) -> &str {
            self.name
        }
        fn description(&self) -> &str {
            "Collision fixture"
        }
        fn input_schema(&self) -> Value {
            json!({"type":"object"})
        }
        fn namespace_binding(&self) -> Option<(String, String)> {
            self.binding
                .map(|(namespace, method)| (namespace.into(), method.into()))
        }
        async fn call(&self, _: Value, _: &ToolCx) -> ToolResult {
            self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            ToolResult::Json(json!({"called":true}))
        }
    }

    #[tokio::test]
    async fn catalog_collisions_fail_before_any_nested_call() {
        for definitions in [
            vec![("foo-bar", None), ("foo_bar", None)],
            vec![("same", None), ("same", None)],
            vec![
                ("one", Some(("ns", "foo-bar"))),
                ("two", Some(("ns", "foo_bar"))),
            ],
            vec![("one", Some(("JSON", "call")))],
            vec![("one", Some(("ns-x", "a"))), ("two", Some(("ns_x", "b")))],
            vec![("__proto__", None)],
        ] {
            let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
            let tools = definitions
                .into_iter()
                .map(|(name, binding)| {
                    Arc::new(CatalogProbe {
                        name,
                        binding,
                        calls: calls.clone(),
                    }) as Arc<dyn Tool>
                })
                .collect();
            let exec = exec_with(tools);
            let result = exec.call(json!({"source":"for (const entry of ALL_TOOLS) { const owner = entry.namespace === null ? tools : globalThis[entry.namespace]; await owner[entry.method]({}); }"}), &test_cx()).await;
            assert!(result.is_error(), "ambiguous catalog executed: {result:?}");
            assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 0);
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
                let lines: Vec<&str> = t.split_once("Output:\n").unwrap().1.lines().collect();
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
        let exec =
            code_mode_tools(&callable, empty_seam, CodeMode::Only, &BTreeMap::new()).remove(0);
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
            tool_observations: Default::default(),
            instruction_generation: 0,
            instruction_policy: None,
            root: dir.path().to_path_buf(),
            safety: Arc::new(bro_tools::SafetyPolicy::new()),
            http: reqwest::Client::new(),
            todos: Arc::new(Mutex::new(bro_tools::TodoList::default())),
            shell_sessions: Arc::new(Mutex::new(bro_tools::ShellSessions::default())),
            edits: Arc::new(Mutex::new(bro_tools::EditSink::default())),
            cancellation: Default::default(),
            output_budget: 16 * 1024,
            child_env: Arc::new(Default::default()),
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
    async fn cell_composes_facts_algebra_and_choke_point() {
        // Full mutation slice in one cell: query a span, queue a replacement,
        // apply — and the bytes land on disk with the EditSet consumed.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("probe.rs"),
            "pub fn answer() -> u8 {\n    41\n}\n",
        )
        .unwrap();
        let cx = ToolCx {
            tool_observations: Default::default(),
            instruction_generation: 0,
            instruction_policy: None,
            root: dir.path().to_path_buf(),
            safety: Arc::new(bro_tools::SafetyPolicy::new()),
            http: reqwest::Client::new(),
            todos: Arc::new(Mutex::new(bro_tools::TodoList::default())),
            shell_sessions: Arc::new(Mutex::new(bro_tools::ShellSessions::default())),
            edits: Arc::new(Mutex::new(bro_tools::EditSink::default())),
            cancellation: Default::default(),
            output_budget: 16 * 1024,
            child_env: Arc::new(Default::default()),
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
const fn41 = inv.items.find(i => i.name === "answer");
const es = await edits.begin();
await edits.replace({ es, span: fn41.span, text: "pub fn answer() -> u8 {\n    42\n}" });
const r = await edits.apply({ es });
text(`${r.applied}:${r.semantic_status}:${r.validations[0].status}`);
"#;
        let result = exec.call(json!({ "source": source }), &cx).await;
        match result {
            ToolResult::Text(t) => assert!(t.contains("true:syntax_only:passed"), "got: {t}"),
            other => panic!("expected text, got {other:?}"),
        }
        let on_disk = std::fs::read_to_string(dir.path().join("probe.rs")).unwrap();
        assert!(on_disk.contains("42"), "{on_disk}");
    }

    #[tokio::test]
    async fn exec_description_documents_namespace_globals_in_optional_mode() {
        // Only admitted methods appear in the default namespace index.
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
        assert!(description.contains("`ns`: echo"), "{description}");
        assert!(
            !description.contains("Namespace guidance."),
            "{description}"
        );
        assert!(!description.contains("declare const ns:"), "{description}");
        assert!(
            description.contains("installed beside `tools`"),
            "{description}"
        );
        assert!(description.contains("input_schema"), "{description}");
    }

    #[tokio::test]
    async fn denied_tool_fails_closed_in_cell() {
        // `echo` is in the projected namespace but NOT in the seam — the §4.5
        // deny-bypass guard: an in-cell call must reject, not bypass the filter.
        let callable = vec![Arc::new(Echo) as Arc<dyn Tool>];
        let empty_seam: Arc<dyn ToolCapability> =
            Arc::new(crate::capabilities::HostTools::new(Vec::new(), test_cx()));
        let exec =
            code_mode_tools(&callable, empty_seam, CodeMode::Only, &BTreeMap::new()).remove(0);
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
    struct AuthoringGeneration;
    #[async_trait]
    impl Tool for AuthoringGeneration {
        fn name(&self) -> &str {
            "authoring_generation"
        }
        fn description(&self) -> &str {
            "Observe immutable authoring context"
        }
        fn input_schema(&self) -> Value {
            json!({"type":"object"})
        }
        async fn call(&self, _: Value, cx: &ToolCx) -> ToolResult {
            ToolResult::Json(json!(cx.instruction_generation))
        }
    }

    #[tokio::test]
    async fn yielded_cell_retains_authoring_generation_across_newer_exec_and_wait() {
        let callable = vec![Arc::new(AuthoringGeneration) as Arc<dyn Tool>];
        let (exec, wait) = code_mode_pair(callable);
        let mut old = test_cx();
        old.instruction_generation = 7;
        let yielded = exec.call(json!({"source":"// @exec: {\"yield_time_ms\": 1}\nawait new Promise(resolve => setTimeout(resolve, 80)); text(await tools.authoring_generation({}));"}), &old).await;
        let ToolResult::Text(yielded) = yielded else {
            panic!("expected yielded cell")
        };
        let cell_id = yielded_cell_id(&yielded);
        let mut newer = old.clone();
        newer.instruction_generation = 8;
        let fresh = exec
            .call(
                json!({"source":"text(await tools.authoring_generation({}));"}),
                &newer,
            )
            .await;
        assert!(
            matches!(fresh, ToolResult::Text(ref text) if text.lines().last() == Some("8")),
            "{fresh:?}"
        );
        let result = wait
            .call(json!({"cell_id":cell_id,"yield_time_ms":1000}), &newer)
            .await;
        assert!(
            matches!(result, ToolResult::Text(ref text) if text.lines().last() == Some("7")),
            "{result:?}"
        );
    }
}
