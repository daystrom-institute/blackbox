// Vendored from openai/codex codex-rs/code-mode (Apache-2.0); see crate NOTICE.
use std::collections::HashMap;
use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::time::Duration;

use serde::Deserialize;
use serde::Serialize;
use serde_json::Value as JsonValue;
use tokio::sync::Mutex;
use tokio::sync::mpsc;
use tokio::sync::oneshot;
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;
use tracing::warn;

use crate::FunctionCallOutputContentItem;
use crate::runtime::CodeModeNestedToolCall;
use crate::runtime::DEFAULT_EXEC_YIELD_TIME_MS;
use crate::runtime::ExecuteRequest;
use crate::runtime::ExecuteToPendingOutcome;
use crate::runtime::PendingRuntimeMode;
use crate::runtime::RuntimeCommand;
use crate::runtime::RuntimeControlCommand;
use crate::runtime::RuntimeEvent;
use crate::runtime::RuntimeResponse;
use crate::runtime::WaitOutcome;
use crate::runtime::WaitRequest;
use crate::runtime::WaitToPendingOutcome;
use crate::runtime::WaitToPendingRequest;
use crate::runtime::spawn_runtime;

pub type CodeModeSessionResultFuture<'a, T> =
    Pin<Box<dyn Future<Output = Result<T, String>> + Send + 'a>>;
pub type CodeModeSessionProviderFuture<'a> =
    CodeModeSessionResultFuture<'a, Arc<dyn CodeModeSession>>;
pub type ToolInvocationFuture<'a> =
    Pin<Box<dyn Future<Output = Result<JsonValue, String>> + Send + 'a>>;
pub type NotificationFuture<'a> = Pin<Box<dyn Future<Output = Result<(), String>> + Send + 'a>>;

#[derive(Clone, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
pub struct CellId(String);

impl CellId {
    pub fn new(value: String) -> Self {
        Self(value)
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl AsRef<str> for CellId {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

impl fmt::Display for CellId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

pub struct StartedCell {
    pub cell_id: CellId,
    initial_response_rx: oneshot::Receiver<RuntimeResponse>,
}

impl StartedCell {
    pub async fn initial_response(self) -> Result<RuntimeResponse, String> {
        self.initial_response_rx
            .await
            .map_err(|_| "exec runtime ended unexpectedly".to_string())
    }
}

/// Host callbacks used by a code-mode session while cells are executing.
pub trait CodeModeSessionDelegate: Send + Sync {
    fn invoke_tool<'a>(
        &'a self,
        invocation: CodeModeNestedToolCall,
        cancellation_token: CancellationToken,
    ) -> ToolInvocationFuture<'a>;

    fn notify<'a>(
        &'a self,
        call_id: String,
        cell_id: CellId,
        text: String,
        cancellation_token: CancellationToken,
    ) -> NotificationFuture<'a>;

    /// Releases delegate state associated with a cell after it reaches a terminal state.
    fn cell_closed(&self, cell_id: &CellId);
}

pub struct NoopCodeModeSessionDelegate;

impl CodeModeSessionDelegate for NoopCodeModeSessionDelegate {
    fn invoke_tool<'a>(
        &'a self,
        _invocation: CodeModeNestedToolCall,
        cancellation_token: CancellationToken,
    ) -> ToolInvocationFuture<'a> {
        Box::pin(async move {
            cancellation_token.cancelled().await;
            Err("code mode nested tools are unavailable".to_string())
        })
    }

    fn notify<'a>(
        &'a self,
        _call_id: String,
        _cell_id: CellId,
        _text: String,
        _cancellation_token: CancellationToken,
    ) -> NotificationFuture<'a> {
        Box::pin(async { Ok(()) })
    }

    fn cell_closed(&self, _cell_id: &CellId) {}
}

/// A durable code-mode session owned by one Codex thread.
///
/// Cells executed in the same session share stored values. Separate sessions
/// must keep those values isolated. Implementations may execute cells
/// in-process or remotely.
pub trait CodeModeSession: Send + Sync {
    fn execute<'a>(
        &'a self,
        request: ExecuteRequest,
    ) -> CodeModeSessionResultFuture<'a, StartedCell>;

    fn wait<'a>(&'a self, request: WaitRequest) -> CodeModeSessionResultFuture<'a, WaitOutcome>;

    fn terminate<'a>(&'a self, cell_id: CellId) -> CodeModeSessionResultFuture<'a, WaitOutcome>;

    fn shutdown<'a>(&'a self) -> CodeModeSessionResultFuture<'a, ()>;
}

/// Creates code-mode sessions for one Codex thread.
///
/// Providers choose where a session executes and receive the host delegate that
/// the session should use for nested tool calls and notifications.
pub trait CodeModeSessionProvider: Send + Sync {
    fn create_session<'a>(
        &'a self,
        delegate: Arc<dyn CodeModeSessionDelegate>,
    ) -> CodeModeSessionProviderFuture<'a>;
}

#[derive(Default)]
pub struct InProcessCodeModeSessionProvider;

impl CodeModeSessionProvider for InProcessCodeModeSessionProvider {
    fn create_session<'a>(
        &'a self,
        delegate: Arc<dyn CodeModeSessionDelegate>,
    ) -> CodeModeSessionProviderFuture<'a> {
        Box::pin(async move {
            let session: Arc<dyn CodeModeSession> =
                Arc::new(CodeModeService::with_delegate(delegate));
            Ok(session)
        })
    }
}

#[derive(Clone)]
struct CellHandle {
    control_tx: mpsc::UnboundedSender<CellControlCommand>,
    runtime_tx: std::sync::mpsc::Sender<RuntimeCommand>,
    cancellation_token: CancellationToken,
}

struct Inner {
    stored_values: Mutex<HashMap<String, JsonValue>>,
    cells: Mutex<HashMap<CellId, CellHandle>>,
    delegate: Arc<dyn CodeModeSessionDelegate>,
    shutting_down: AtomicBool,
    next_cell_id: AtomicU64,
}

pub struct CodeModeService {
    inner: Arc<Inner>,
}

impl CodeModeService {
    pub fn new() -> Self {
        Self::with_delegate(Arc::new(NoopCodeModeSessionDelegate))
    }

    pub fn with_delegate(delegate: Arc<dyn CodeModeSessionDelegate>) -> Self {
        Self {
            inner: Arc::new(Inner {
                stored_values: Mutex::new(HashMap::new()),
                cells: Mutex::new(HashMap::new()),
                delegate,
                shutting_down: AtomicBool::new(false),
                next_cell_id: AtomicU64::new(1),
            }),
        }
    }

    fn allocate_cell_id(&self) -> CellId {
        CellId::new(
            self.inner
                .next_cell_id
                .fetch_add(1, Ordering::Relaxed)
                .to_string(),
        )
    }

    pub async fn execute(&self, request: ExecuteRequest) -> Result<StartedCell, String> {
        if self.inner.shutting_down.load(Ordering::Acquire) {
            return Err("code mode session is shutting down".to_string());
        }
        let initial_yield_time_ms = request.yield_time_ms.unwrap_or(DEFAULT_EXEC_YIELD_TIME_MS);
        let (response_tx, response_rx) = oneshot::channel();
        let cell_id = self.allocate_cell_id();
        self.start_cell(
            cell_id.clone(),
            request,
            CellResponseSender::Runtime(response_tx),
            Some(initial_yield_time_ms),
            PendingRuntimeMode::Continue,
        )
        .await?;

        Ok(StartedCell {
            cell_id,
            initial_response_rx: response_rx,
        })
    }

    pub async fn execute_to_pending(
        &self,
        request: ExecuteRequest,
    ) -> Result<ExecuteToPendingOutcome, String> {
        let (response_tx, response_rx) = oneshot::channel();
        let cell_id = self.allocate_cell_id();
        self.start_cell(
            cell_id,
            request,
            CellResponseSender::ExecuteToPending(response_tx),
            /*initial_yield_time_ms*/ None,
            PendingRuntimeMode::PauseUntilResumed,
        )
        .await?;

        response_rx
            .await
            .map_err(|_| "exec runtime ended unexpectedly".to_string())
    }

    async fn start_cell(
        &self,
        cell_id: CellId,
        request: ExecuteRequest,
        initial_response_tx: CellResponseSender,
        initial_yield_time_ms: Option<u64>,
        pending_mode: PendingRuntimeMode,
    ) -> Result<(), String> {
        let (event_tx, event_rx) = mpsc::unbounded_channel();
        let (control_tx, control_rx) = mpsc::unbounded_channel();
        let stored_values = self.inner.stored_values.lock().await.clone();
        let cancellation_token = CancellationToken::new();
        let (runtime_tx, runtime_control_tx, runtime_terminate_handle) = {
            let mut cells = self.inner.cells.lock().await;
            if self.inner.shutting_down.load(Ordering::Acquire) {
                return Err("code mode session is shutting down".to_string());
            }
            if cells.contains_key(&cell_id) {
                return Err(format!("exec cell {cell_id} already exists"));
            }

            let (runtime_tx, runtime_control_tx, runtime_terminate_handle) =
                spawn_runtime(stored_values, request, event_tx, pending_mode)?;

            cells.insert(
                cell_id.clone(),
                CellHandle {
                    control_tx,
                    runtime_tx: runtime_tx.clone(),
                    cancellation_token: cancellation_token.clone(),
                },
            );
            (runtime_tx, runtime_control_tx, runtime_terminate_handle)
        };

        tokio::spawn(run_cell_control(
            Arc::clone(&self.inner),
            CellControlContext {
                cell_id,
                runtime_tx,
                runtime_control_tx,
                pending_mode,
                runtime_terminate_handle,
                cancellation_token,
            },
            event_rx,
            control_rx,
            initial_response_tx,
            initial_yield_time_ms,
        ));

        Ok(())
    }

    pub async fn wait(&self, request: WaitRequest) -> Result<WaitOutcome, String> {
        let WaitRequest {
            cell_id,
            yield_time_ms,
        } = request;
        let handle = self.inner.cells.lock().await.get(&cell_id).cloned();
        let Some(handle) = handle else {
            return Ok(WaitOutcome::MissingCell(missing_cell_response(cell_id)));
        };
        let (response_tx, response_rx) = oneshot::channel();
        let control_message = CellControlCommand::Poll {
            yield_time_ms,
            response_tx,
        };
        if handle.control_tx.send(control_message).is_err() {
            return Ok(WaitOutcome::MissingCell(missing_cell_response(cell_id)));
        }
        match response_rx.await {
            Ok(response) => Ok(WaitOutcome::LiveCell(response)),
            Err(_) => Ok(WaitOutcome::MissingCell(missing_cell_response(cell_id))),
        }
    }

    pub async fn terminate(&self, cell_id: CellId) -> Result<WaitOutcome, String> {
        let handle = self.inner.cells.lock().await.get(&cell_id).cloned();
        let Some(handle) = handle else {
            return Ok(WaitOutcome::MissingCell(missing_cell_response(cell_id)));
        };
        let (response_tx, response_rx) = oneshot::channel();
        if handle
            .control_tx
            .send(CellControlCommand::Terminate { response_tx })
            .is_err()
        {
            return Ok(WaitOutcome::MissingCell(missing_cell_response(cell_id)));
        }
        match response_rx.await {
            Ok(response) => Ok(WaitOutcome::LiveCell(response)),
            Err(_) => Ok(WaitOutcome::MissingCell(missing_cell_response(cell_id))),
        }
    }

    pub async fn wait_to_pending(
        &self,
        request: WaitToPendingRequest,
    ) -> Result<WaitToPendingOutcome, String> {
        let cell_id = request.cell_id;
        let handle = self.inner.cells.lock().await.get(&cell_id).cloned();
        let Some(handle) = handle else {
            return Ok(WaitToPendingOutcome::MissingCell(missing_cell_response(
                cell_id,
            )));
        };
        let (response_tx, response_rx) = oneshot::channel();
        if handle
            .control_tx
            .send(CellControlCommand::PollToPending { response_tx })
            .is_err()
        {
            return Ok(WaitToPendingOutcome::MissingCell(missing_cell_response(
                cell_id,
            )));
        }
        match response_rx.await {
            Ok(response) => Ok(WaitToPendingOutcome::LiveCell(response)),
            Err(_) => Ok(WaitToPendingOutcome::MissingCell(missing_cell_response(
                cell_id,
            ))),
        }
    }

    pub async fn shutdown(&self) -> Result<(), String> {
        self.inner.shutting_down.store(true, Ordering::Release);
        self.cancel_all().await;
        Ok(())
    }

    /// Local addition (not vendored): close current cells without closing the
    /// session. Every reply follows callback completion and remains available
    /// to the host for transcript delivery.
    pub async fn cancel_all(&self) -> Vec<RuntimeResponse> {
        let handles = self
            .inner
            .cells
            .lock()
            .await
            .values()
            .cloned()
            .collect::<Vec<_>>();
        let mut responses = Vec::new();
        for handle in handles {
            handle.cancellation_token.cancel();
            let (response_tx, response_rx) = oneshot::channel();
            let _ = handle
                .control_tx
                .send(CellControlCommand::Terminate { response_tx });
            responses.push(response_rx);
        }
        let mut outcomes = Vec::new();
        for response in responses {
            if let Ok(response) = response.await {
                outcomes.push(response);
            }
        }
        outcomes
    }
}

impl Default for CodeModeService {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for CodeModeService {
    fn drop(&mut self) {
        self.inner.shutting_down.store(true, Ordering::Release);
        if let Ok(cells) = self.inner.cells.try_lock() {
            for handle in cells.values() {
                handle.cancellation_token.cancel();
                let (response_tx, _response_rx) = oneshot::channel();
                let _ = handle
                    .control_tx
                    .send(CellControlCommand::Terminate { response_tx });
                let _ = handle.runtime_tx.send(RuntimeCommand::Terminate);
            }
        }
    }
}

impl CodeModeSession for CodeModeService {
    fn execute<'a>(
        &'a self,
        request: ExecuteRequest,
    ) -> CodeModeSessionResultFuture<'a, StartedCell> {
        Box::pin(CodeModeService::execute(self, request))
    }

    fn wait<'a>(&'a self, request: WaitRequest) -> CodeModeSessionResultFuture<'a, WaitOutcome> {
        Box::pin(CodeModeService::wait(self, request))
    }

    fn terminate<'a>(&'a self, cell_id: CellId) -> CodeModeSessionResultFuture<'a, WaitOutcome> {
        Box::pin(CodeModeService::terminate(self, cell_id))
    }

    fn shutdown<'a>(&'a self) -> CodeModeSessionResultFuture<'a, ()> {
        Box::pin(CodeModeService::shutdown(self))
    }
}

enum CellControlCommand {
    Poll {
        yield_time_ms: u64,
        response_tx: oneshot::Sender<RuntimeResponse>,
    },
    PollToPending {
        response_tx: oneshot::Sender<ExecuteToPendingOutcome>,
    },
    Terminate {
        response_tx: oneshot::Sender<RuntimeResponse>,
    },
}

enum CellResponseSender {
    Runtime(oneshot::Sender<RuntimeResponse>),
    ExecuteToPending(oneshot::Sender<ExecuteToPendingOutcome>),
}

struct PendingResult {
    content_items: Vec<FunctionCallOutputContentItem>,
    error_text: Option<String>,
}

struct CellControlContext {
    cell_id: CellId,
    runtime_tx: std::sync::mpsc::Sender<RuntimeCommand>,
    runtime_control_tx: std::sync::mpsc::Sender<RuntimeControlCommand>,
    pending_mode: PendingRuntimeMode,
    runtime_terminate_handle: v8::IsolateHandle,
    cancellation_token: CancellationToken,
}

fn missing_cell_response(cell_id: CellId) -> RuntimeResponse {
    RuntimeResponse::Result {
        error_text: Some(format!("exec cell {cell_id} not found")),
        cell_id,
        content_items: Vec::new(),
    }
}

fn pending_result_response(cell_id: &CellId, result: PendingResult) -> RuntimeResponse {
    RuntimeResponse::Result {
        cell_id: cell_id.clone(),
        content_items: result.content_items,
        error_text: result.error_text,
    }
}

fn send_terminal_response(response_tx: CellResponseSender, response: RuntimeResponse) {
    match response_tx {
        CellResponseSender::Runtime(response_tx) => {
            let _ = response_tx.send(response);
        }
        CellResponseSender::ExecuteToPending(response_tx) => {
            let _ = response_tx.send(ExecuteToPendingOutcome::Completed(response));
        }
    }
}

fn send_or_buffer_result(
    cell_id: &CellId,
    result: PendingResult,
    response_tx: &mut Option<CellResponseSender>,
    pending_result: &mut Option<PendingResult>,
) -> bool {
    if let Some(response_tx) = response_tx.take() {
        let response = pending_result_response(cell_id, result);
        send_terminal_response(response_tx, response);
        return true;
    }

    *pending_result = Some(result);
    false
}

fn send_yield_response(
    cell_id: &CellId,
    content_items: &mut Vec<FunctionCallOutputContentItem>,
    response_tx: &mut Option<CellResponseSender>,
) {
    let Some(current_response_tx) = response_tx.take() else {
        return;
    };
    match current_response_tx {
        CellResponseSender::Runtime(response_tx) => {
            let _ = response_tx.send(RuntimeResponse::Yielded {
                cell_id: cell_id.clone(),
                content_items: std::mem::take(content_items),
            });
        }
        CellResponseSender::ExecuteToPending(execute_to_pending_tx) => {
            *response_tx = Some(CellResponseSender::ExecuteToPending(execute_to_pending_tx));
        }
    }
}

// Local addition (not vendored): retain bounded, actual nested outcomes when
// JavaScript is terminated before it can print a tool's completed response.
#[derive(Default)]
struct NestedOutcomeLog {
    records: Vec<(String, String)>,
    bytes: usize,
    omitted: usize,
}

// Local addition (not vendored).
struct NestedOutcome {
    id: String,
    name: String,
    result: Result<serde_json::Value, String>,
}

impl NestedOutcomeLog {
    // Local addition (not vendored): omission counts are explicit; no output
    // files or new recovery tools are created by cancellation.
    fn record(&mut self, outcome: NestedOutcome) {
        let id = outcome.id.clone();
        let is_error = outcome.result.is_err();
        let output = match outcome.result {
            Ok(value) => value,
            Err(error) => serde_json::Value::String(error),
        };
        let rendered = output.to_string();
        let mut record = serde_json::json!({
            "call_id": outcome.id,
            "tool": outcome.name,
            "is_error": is_error,
        });
        if rendered.len() > 1024 {
            let mut end = 1024;
            while !rendered.is_char_boundary(end) {
                end -= 1;
            }
            record["output_preview"] = serde_json::json!(&rendered[..end]);
            record["omitted_bytes"] = serde_json::json!(rendered.len() - end);
        } else {
            record["output"] = output;
        }
        let record = record.to_string();
        if self.bytes + record.len() > 8 * 1024 {
            self.omitted += 1;
        } else {
            self.bytes += record.len();
            self.records.push((id, record));
        }
    }

    // Local addition (not vendored).
    fn append_to(&self, content: &mut Vec<FunctionCallOutputContentItem>) {
        self.append_selected_to(content, None);
    }

    // Local addition (not vendored): normal terminal replies only need results
    // the isolate had not consumed, as identified by its resolver map.
    fn append_selected_to(
        &self,
        content: &mut Vec<FunctionCallOutputContentItem>,
        ids: Option<&[String]>,
    ) {
        if ids.is_some_and(|ids| ids.is_empty()) {
            return;
        }
        let records = self
            .records
            .iter()
            .filter(|(id, _)| ids.is_none_or(|ids| ids.contains(id)))
            .map(|(_, record)| record.as_str())
            .collect::<Vec<_>>();
        if records.is_empty() && self.omitted == 0 {
            return;
        }
        let mut text = format!("[nested tool outcomes]\n{}", records.join("\n"));
        if self.omitted > 0 {
            text.push_str(&format!(
                "\n[{} additional nested outcomes omitted]",
                self.omitted
            ));
        }
        content.push(FunctionCallOutputContentItem::InputText { text });
    }
}

// Local addition (not vendored): an admitted host call owns its actual
// completion. Never abort its task merely because V8 has stopped.
async fn drain_tool_call_tasks(
    tasks: &mut JoinSet<NestedOutcome>,
    outcomes: &mut NestedOutcomeLog,
) {
    while let Some(result) = tasks.join_next().await {
        match result {
            Ok(outcome) => outcomes.record(outcome),
            Err(error) => outcomes.record(NestedOutcome {
                id: "unknown".into(),
                name: "unknown".into(),
                result: Err(format!("nested tool task failed: {error}")),
            }),
        }
    }
}

async fn run_cell_control(
    inner: Arc<Inner>,
    context: CellControlContext,
    mut event_rx: mpsc::UnboundedReceiver<RuntimeEvent>,
    mut control_rx: mpsc::UnboundedReceiver<CellControlCommand>,
    initial_response_tx: CellResponseSender,
    initial_yield_time_ms: Option<u64>,
) {
    let CellControlContext {
        cell_id,
        runtime_tx,
        runtime_control_tx,
        pending_mode,
        runtime_terminate_handle,
        cancellation_token,
    } = context;
    let mut content_items = Vec::new();
    let mut pending_tool_call_ids = Vec::new();
    let mut pending_result: Option<PendingResult> = None;
    let mut response_tx = Some(initial_response_tx);
    let mut termination_requested = false;
    let mut runtime_closed = false;
    let mut yield_timer: Option<std::pin::Pin<Box<tokio::time::Sleep>>> = None;
    // Most recently requested yield window. A delegated tool invocation may
    // outlive the outer cell yield window (notably a blocking shell_run inside
    // code-mode). The cell is allowed to yield while a nested call is still in
    // flight; a later wait observes the eventual tool response and runtime result.
    // When a nested call returns before the cell yields, re-arm a fresh window
    // from tool-return so post-tool async work still gets a fair yield boundary.
    let mut yield_window_ms: Option<u64> = initial_yield_time_ms;
    let mut tool_call_tasks = JoinSet::new();
    let mut notification_tasks = JoinSet::new();
    let mut nested_outcomes = NestedOutcomeLog::default();

    loop {
        tokio::select! {
            maybe_event = async {
                if runtime_closed {
                    std::future::pending::<Option<RuntimeEvent>>().await
                } else {
                    event_rx.recv().await
                }
            } => {
                let Some(event) = maybe_event else {
                    runtime_closed = true;
                    if termination_requested {
                        // Local addition (not vendored): callback results must
                        // be settled before any terminal reply is observable.
                        drain_notification_tasks(&mut notification_tasks).await;
                        drain_tool_call_tasks(&mut tool_call_tasks, &mut nested_outcomes).await;
                        nested_outcomes.append_to(&mut content_items);
                        if let Some(response_tx) = response_tx.take() {
                            let response = RuntimeResponse::Terminated {
                                cell_id: cell_id.clone(),
                                content_items: std::mem::take(&mut content_items),
                            };
                            send_terminal_response(response_tx, response);
                        }
                        break;
                    }
                    if pending_result.is_none() {
                        cancellation_token.cancel();
                        drain_notification_tasks(&mut notification_tasks).await;
                        drain_tool_call_tasks(&mut tool_call_tasks, &mut nested_outcomes).await;
                        nested_outcomes.append_to(&mut content_items);
                        let result = PendingResult {
                            content_items: std::mem::take(&mut content_items),
                            error_text: Some("exec runtime ended unexpectedly".to_string()),
                        };
                        if send_or_buffer_result(
                            &cell_id,
                            result,
                            &mut response_tx,
                            &mut pending_result,
                        ) {
                            break;
                        }
                    }
                    continue;
                };
                match event {
                    RuntimeEvent::Started => {
                        yield_timer = initial_yield_time_ms.map(|initial_yield_time_ms| {
                            Box::pin(tokio::time::sleep(Duration::from_millis(initial_yield_time_ms)))
                        });
                    }
                    RuntimeEvent::Pending => {
                        if let Some(current_response_tx) = response_tx.take() {
                            match current_response_tx {
                                CellResponseSender::Runtime(runtime_response_tx) => {
                                    response_tx =
                                        Some(CellResponseSender::Runtime(runtime_response_tx));
                                }
                                CellResponseSender::ExecuteToPending(response_tx) => {
                                    let _ = response_tx.send(ExecuteToPendingOutcome::Pending {
                                        cell_id: cell_id.clone(),
                                        content_items: std::mem::take(&mut content_items),
                                        pending_tool_call_ids: std::mem::take(
                                            &mut pending_tool_call_ids,
                                        ),
                                    });
                                }
                            }
                        }
                    }
                    RuntimeEvent::ContentItem(item) => {
                        content_items.push(item);
                    }
                    RuntimeEvent::YieldRequested => {
                        yield_timer = None;
                        send_yield_response(&cell_id, &mut content_items, &mut response_tx);
                    }
                    RuntimeEvent::Notify { call_id, text } => {
                        let delegate = Arc::clone(&inner.delegate);
                        let cell_id = cell_id.clone();
                        let cancellation_token = cancellation_token.child_token();
                        notification_tasks.spawn(async move {
                            // Local addition (not vendored): callbacks already
                            // emitted by V8 are drained, never raced and dropped.
                            if let Err(err) = delegate.notify(
                                    call_id,
                                    cell_id.clone(),
                                    text,
                                    cancellation_token,
                                ).await {
                                warn!("failed to deliver code mode notification for cell {cell_id}: {err}");
                            }
                        });
                    }
                    RuntimeEvent::ToolCall {
                        id,
                        name,
                        kind,
                        input,
                    } => {
                        // Local addition (not vendored): queued runtime events
                        // cannot start fresh host calls after cancellation.
                        if termination_requested || cancellation_token.is_cancelled() {
                            nested_outcomes.record(NestedOutcome {
                                id,
                                name: name.to_string(),
                                result: Err("cancelled before tool admission".into()),
                            });
                            continue;
                        }
                        if pending_mode == PendingRuntimeMode::PauseUntilResumed {
                            pending_tool_call_ids.push(id.clone());
                        }
                        let tool_call = CodeModeNestedToolCall {
                            cell_id: cell_id.clone(),
                            runtime_tool_call_id: id.clone(),
                            tool_name: name,
                            tool_kind: kind,
                            input,
                        };
                        let delegate = Arc::clone(&inner.delegate);
                        let runtime_tx = runtime_tx.clone();
                        let cancellation_token = cancellation_token.child_token();
                        tool_call_tasks.spawn(async move {
                            // Local addition (not vendored): the host controls
                            // admission and cancellation of admitted operations.
                            let name = tool_call.tool_name.to_string();
                            let response = delegate.invoke_tool(tool_call, cancellation_token).await;
                            let command = match &response {
                                Ok(result) => RuntimeCommand::ToolResponse { id: id.clone(), result: result.clone() },
                                Err(error_text) => RuntimeCommand::ToolError { id: id.clone(), error_text: error_text.clone() },
                            };
                            let _ = runtime_tx.send(command);
                            NestedOutcome { id, name, result: response }
                        });
                    }
                    RuntimeEvent::Result {
                        stored_value_writes,
                        error_text,
                        pending_tool_call_ids,
                    } => {
                        yield_timer = None;
                        if termination_requested {
                            drain_notification_tasks(&mut notification_tasks).await;
                            drain_tool_call_tasks(&mut tool_call_tasks, &mut nested_outcomes).await;
                            nested_outcomes.append_to(&mut content_items);
                            if let Some(response_tx) = response_tx.take() {
                                let response = RuntimeResponse::Terminated {
                                    cell_id: cell_id.clone(),
                                    content_items: std::mem::take(&mut content_items),
                                };
                                send_terminal_response(response_tx, response);
                            }
                            break;
                        }
                        drain_notification_tasks(&mut notification_tasks).await;
                        // Local addition (not vendored): even fire-and-forget
                        // calls that began executing must finish before success.
                        cancellation_token.cancel();
                        drain_tool_call_tasks(&mut tool_call_tasks, &mut nested_outcomes).await;
                        nested_outcomes.append_selected_to(&mut content_items, Some(&pending_tool_call_ids));
                        inner
                            .stored_values
                            .lock()
                            .await
                            .extend(stored_value_writes);
                        let result = PendingResult {
                            content_items: std::mem::take(&mut content_items),
                            error_text,
                        };
                        if send_or_buffer_result(
                            &cell_id,
                            result,
                            &mut response_tx,
                            &mut pending_result,
                        ) {
                            break;
                        }
                    }
                }
            }
            task_result = notification_tasks.join_next(), if !notification_tasks.is_empty() => {
                if let Some(Err(err)) = task_result
                    && !err.is_cancelled()
                {
                    warn!("code mode notification task failed: {err}");
                }
            }
            task_result = tool_call_tasks.join_next(), if !tool_call_tasks.is_empty() => {
                match task_result {
                    Some(Ok(outcome)) => nested_outcomes.record(outcome),
                    Some(Err(err)) => nested_outcomes.record(NestedOutcome {
                        id: "unknown".into(), name: "unknown".into(),
                        result: Err(format!("nested tool task failed: {err}")),
                    }),
                    None => {}
                }
                // The nested call is back: re-arm a fresh yield window from
                // tool-return (only if a window is active — i.e. the cell has
                // not already yielded and is not in pause-until-resumed mode).
                if tool_call_tasks.is_empty() && yield_timer.is_some() {
                    yield_timer = yield_window_ms.map(|window_ms| {
                        Box::pin(tokio::time::sleep(Duration::from_millis(window_ms)))
                    });
                }
            }
            maybe_command = control_rx.recv() => {
                let Some(command) = maybe_command else {
                    break;
                };
                match command {
                    CellControlCommand::Poll {
                        yield_time_ms,
                        response_tx: next_response_tx,
                    } => {
                        if let Some(result) = pending_result.take() {
                            let _ = next_response_tx.send(pending_result_response(&cell_id, result));
                            break;
                        }
                        response_tx = Some(CellResponseSender::Runtime(next_response_tx));
                        yield_window_ms = Some(yield_time_ms);
                        yield_timer = Some(Box::pin(tokio::time::sleep(Duration::from_millis(yield_time_ms))));
                        resume_paused_runtime(&runtime_control_tx, pending_mode);
                    }
                    CellControlCommand::PollToPending {
                        response_tx: next_response_tx,
                    } => {
                        if let Some(result) = pending_result.take() {
                            let response = pending_result_response(&cell_id, result);
                            let _ = next_response_tx
                                .send(ExecuteToPendingOutcome::Completed(response));
                            break;
                        }
                        response_tx =
                            Some(CellResponseSender::ExecuteToPending(next_response_tx));
                        yield_window_ms = None;
                        yield_timer = None;
                        resume_paused_runtime(&runtime_control_tx, pending_mode);
                    }
                    CellControlCommand::Terminate { response_tx: next_response_tx } => {
                        if let Some(result) = pending_result.take() {
                            let _ = next_response_tx.send(pending_result_response(&cell_id, result));
                            break;
                        }

                        response_tx = Some(CellResponseSender::Runtime(next_response_tx));
                        termination_requested = true;
                        cancellation_token.cancel();
                        yield_timer = None;
                        let _ = runtime_tx.send(RuntimeCommand::Terminate);
                        terminate_paused_runtime(&runtime_control_tx, pending_mode);
                        let _ = runtime_terminate_handle.terminate_execution();
                        if runtime_closed {
                            drain_notification_tasks(&mut notification_tasks).await;
                            drain_tool_call_tasks(&mut tool_call_tasks, &mut nested_outcomes).await;
                            nested_outcomes.append_to(&mut content_items);
                            if let Some(response_tx) = response_tx.take() {
                                let response = RuntimeResponse::Terminated {
                                    cell_id: cell_id.clone(),
                                    content_items: std::mem::take(&mut content_items),
                                };
                                send_terminal_response(response_tx, response);
                            }
                            break;
                        } else {
                            continue;
                        }
                    }
                }
            }
            _ = async {
                if let Some(yield_timer) = yield_timer.as_mut() {
                    yield_timer.await;
                } else {
                    std::future::pending::<()>().await;
                }
            } => {
                yield_timer = None;
                send_yield_response(&cell_id, &mut content_items, &mut response_tx);
            }
        }
    }

    let _ = runtime_tx.send(RuntimeCommand::Terminate);
    cancellation_token.cancel();
    drain_notification_tasks(&mut notification_tasks).await;
    drain_tool_call_tasks(&mut tool_call_tasks, &mut nested_outcomes).await;
    terminate_paused_runtime(&runtime_control_tx, pending_mode);
    inner.cells.lock().await.remove(&cell_id);
    inner.delegate.cell_closed(&cell_id);
}

async fn drain_notification_tasks(notification_tasks: &mut JoinSet<()>) {
    while let Some(result) = notification_tasks.join_next().await {
        if let Err(err) = result
            && !err.is_cancelled()
        {
            warn!("code mode notification task failed: {err}");
        }
    }
}

fn resume_paused_runtime(
    runtime_control_tx: &std::sync::mpsc::Sender<RuntimeControlCommand>,
    pending_mode: PendingRuntimeMode,
) {
    if pending_mode == PendingRuntimeMode::PauseUntilResumed {
        let _ = runtime_control_tx.send(RuntimeControlCommand::Resume);
    }
}

fn terminate_paused_runtime(
    runtime_control_tx: &std::sync::mpsc::Sender<RuntimeControlCommand>,
    pending_mode: PendingRuntimeMode,
) {
    if pending_mode == PendingRuntimeMode::PauseUntilResumed {
        let _ = runtime_control_tx.send(RuntimeControlCommand::Terminate);
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::Arc;
    use std::sync::atomic::AtomicU64;
    use std::sync::atomic::Ordering;
    use std::time::Duration;

    use crate::tool_name::ToolName;
    use pretty_assertions::assert_eq;
    use tokio::sync::Mutex;
    use tokio::sync::mpsc;
    use tokio::sync::oneshot;

    use super::CellControlCommand;
    use super::CellControlContext;
    use super::CellId;
    use super::CellResponseSender;
    use super::CodeModeService;
    use super::Inner;
    use super::NoopCodeModeSessionDelegate;
    use super::PendingRuntimeMode;
    use super::RuntimeCommand;
    use super::RuntimeResponse;
    use super::WaitOutcome;
    use super::WaitRequest;
    use super::WaitToPendingOutcome;
    use super::WaitToPendingRequest;
    use super::run_cell_control;
    use crate::CodeModeToolKind;
    use crate::FunctionCallOutputContentItem;
    use crate::ToolDefinition;
    use crate::runtime::ExecuteRequest;
    use crate::runtime::ExecuteToPendingOutcome;
    use crate::runtime::RuntimeEvent;
    use crate::runtime::spawn_runtime;

    fn execute_request(source: &str) -> ExecuteRequest {
        ExecuteRequest {
            tool_call_id: "call_1".to_string(),
            enabled_tools: Vec::new(),
            source: source.to_string(),
            yield_time_ms: Some(1),
            max_output_tokens: None,
        }
    }

    fn cell_id(value: &str) -> CellId {
        CellId::new(value.to_string())
    }

    async fn execute(service: &CodeModeService, request: ExecuteRequest) -> RuntimeResponse {
        service
            .execute(request)
            .await
            .unwrap()
            .initial_response()
            .await
            .unwrap()
    }

    fn test_inner() -> Arc<Inner> {
        Arc::new(Inner {
            stored_values: Mutex::new(HashMap::new()),
            cells: Mutex::new(HashMap::new()),
            delegate: Arc::new(NoopCodeModeSessionDelegate),
            shutting_down: std::sync::atomic::AtomicBool::new(false),
            next_cell_id: AtomicU64::new(1),
        })
    }

    /// Delegate whose nested tool calls block for `delay` before returning
    /// `"slow-result"`. Used to exercise code-mode yield behavior while a
    /// delegated invocation is still in flight.
    struct SlowToolDelegate {
        delay: Duration,
    }

    impl super::CodeModeSessionDelegate for SlowToolDelegate {
        fn invoke_tool<'a>(
            &'a self,
            _invocation: crate::runtime::CodeModeNestedToolCall,
            _cancellation_token: tokio_util::sync::CancellationToken,
        ) -> super::ToolInvocationFuture<'a> {
            let delay = self.delay;
            Box::pin(async move {
                tokio::time::sleep(delay).await;
                Ok(serde_json::Value::String("slow-result".to_string()))
            })
        }

        fn notify<'a>(
            &'a self,
            _call_id: String,
            _cell_id: CellId,
            _text: String,
            _cancellation_token: tokio_util::sync::CancellationToken,
        ) -> super::NotificationFuture<'a> {
            Box::pin(async { Ok(()) })
        }

        fn cell_closed(&self, _cell_id: &CellId) {}
    }

    fn slow_tool_definition() -> ToolDefinition {
        ToolDefinition {
            name: "slow".to_string(),
            tool_name: ToolName::plain("slow"),
            description: String::new(),
            kind: CodeModeToolKind::Function,
            input_schema: None,
            output_schema: None,
            namespace_binding: None,
        }
    }

    // Local addition (not vendored): explicit barriers prove completion order,
    // without relying on a real subprocess or filesystem mutation.
    struct CompletionDelegate {
        started: tokio::sync::Semaphore,
        release: tokio::sync::Semaphore,
        tool_finished: std::sync::atomic::AtomicBool,
        notification_finished: std::sync::atomic::AtomicBool,
    }

    impl CompletionDelegate {
        fn new() -> Self {
            Self {
                started: tokio::sync::Semaphore::new(0),
                release: tokio::sync::Semaphore::new(0),
                tool_finished: std::sync::atomic::AtomicBool::new(false),
                notification_finished: std::sync::atomic::AtomicBool::new(false),
            }
        }
    }

    impl super::CodeModeSessionDelegate for CompletionDelegate {
        fn invoke_tool<'a>(
            &'a self,
            _: crate::runtime::CodeModeNestedToolCall,
            _: tokio_util::sync::CancellationToken,
        ) -> super::ToolInvocationFuture<'a> {
            Box::pin(async move {
                self.started.add_permits(1);
                self.release.acquire().await.unwrap().forget();
                self.tool_finished.store(true, Ordering::SeqCst);
                Ok(serde_json::json!({"mutation": "finished"}))
            })
        }

        fn notify<'a>(
            &'a self,
            _: String,
            _: CellId,
            _: String,
            _: tokio_util::sync::CancellationToken,
        ) -> super::NotificationFuture<'a> {
            Box::pin(async move {
                self.started.add_permits(1);
                self.release.acquire().await.unwrap().forget();
                self.notification_finished.store(true, Ordering::SeqCst);
                Ok(())
            })
        }

        fn cell_closed(&self, _: &CellId) {}
    }

    #[tokio::test]
    async fn termination_drains_admitted_tool_and_notification_before_reply() {
        let delegate = Arc::new(CompletionDelegate::new());
        let service = CodeModeService::with_delegate(delegate.clone());
        let started = service
            .execute(ExecuteRequest {
                enabled_tools: vec![slow_tool_definition()],
                source: "notify('record this'); text(await tools.slow({}));".into(),
                yield_time_ms: Some(60_000),
                ..execute_request("")
            })
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(2), delegate.started.acquire_many(2))
            .await
            .unwrap()
            .unwrap()
            .forget();
        let termination = service.terminate(started.cell_id.clone());
        tokio::pin!(termination);
        assert!(
            tokio::time::timeout(Duration::from_millis(30), &mut termination)
                .await
                .is_err()
        );
        assert!(!delegate.tool_finished.load(Ordering::SeqCst));
        delegate.release.add_permits(2);
        let response = tokio::time::timeout(Duration::from_secs(2), termination)
            .await
            .unwrap()
            .unwrap();
        assert!(delegate.tool_finished.load(Ordering::SeqCst));
        assert!(delegate.notification_finished.load(Ordering::SeqCst));
        let WaitOutcome::LiveCell(RuntimeResponse::Terminated { content_items, .. }) = response
        else {
            panic!("expected terminated cell, got {response:?}");
        };
        assert!(content_items.iter().any(|item| matches!(item,
            FunctionCallOutputContentItem::InputText { text }
            if text.contains("nested tool outcomes") && text.contains("finished") && text.contains("slow"))));
    }

    #[tokio::test]
    async fn normal_completion_drains_unawaited_nested_work() {
        let delegate = Arc::new(CompletionDelegate::new());
        let service = CodeModeService::with_delegate(delegate.clone());
        let started = service.execute(ExecuteRequest {
            enabled_tools: vec![slow_tool_definition()],
            source: "tools.slow({}); await new Promise(resolve => setTimeout(resolve, 20)); text('done');".into(),
            yield_time_ms: Some(60_000),
            ..execute_request("")
        }).await.unwrap();
        tokio::time::timeout(Duration::from_secs(2), delegate.started.acquire())
            .await
            .unwrap()
            .unwrap()
            .forget();
        let response = started.initial_response();
        tokio::pin!(response);
        assert!(
            tokio::time::timeout(Duration::from_millis(60), &mut response)
                .await
                .is_err()
        );
        delegate.release.add_permits(1);
        let response = tokio::time::timeout(Duration::from_secs(2), response)
            .await
            .unwrap()
            .unwrap();
        assert!(delegate.tool_finished.load(Ordering::SeqCst));
        let RuntimeResponse::Result {
            content_items,
            error_text: None,
            ..
        } = response
        else {
            panic!("expected completion, got {response:?}");
        };
        assert!(content_items.iter().any(|item| matches!(item,
            FunctionCallOutputContentItem::InputText { text } if text == "done")));
        assert!(content_items.iter().any(|item| matches!(item,
            FunctionCallOutputContentItem::InputText { text }
            if text.contains("nested tool outcomes") && text.contains("finished"))));
    }

    #[tokio::test]
    async fn normal_terminal_retains_completed_response_not_consumed_by_v8() {
        let inner = Arc::new(Inner {
            stored_values: Mutex::new(HashMap::new()),
            cells: Mutex::new(HashMap::new()),
            delegate: Arc::new(SlowToolDelegate {
                delay: Duration::ZERO,
            }),
            shutting_down: std::sync::atomic::AtomicBool::new(false),
            next_cell_id: AtomicU64::new(1),
        });
        let (event_tx, event_rx) = mpsc::unbounded_channel();
        let (_control_tx, control_rx) = mpsc::unbounded_channel();
        let (response_tx, response_rx) = oneshot::channel();
        let (runtime_event_tx, _runtime_event_rx) = mpsc::unbounded_channel();
        let (_runtime_tx, runtime_control_tx, runtime_terminate_handle) = spawn_runtime(
            HashMap::new(),
            execute_request(""),
            runtime_event_tx,
            PendingRuntimeMode::Continue,
        )
        .unwrap();
        // Observe the host response separately from V8. This establishes that
        // it completed BEFORE the controller sees the runtime terminal event.
        let (tool_response_tx, tool_response_rx) = std::sync::mpsc::channel();
        let actor = tokio::spawn(run_cell_control(
            inner,
            CellControlContext {
                cell_id: cell_id("delayed-result"),
                runtime_tx: tool_response_tx,
                runtime_control_tx,
                pending_mode: PendingRuntimeMode::Continue,
                runtime_terminate_handle,
                cancellation_token: tokio_util::sync::CancellationToken::new(),
            },
            event_rx,
            control_rx,
            CellResponseSender::Runtime(response_tx),
            None,
        ));
        event_tx
            .send(RuntimeEvent::ToolCall {
                id: "tool-1".into(),
                name: ToolName::plain("slow"),
                kind: CodeModeToolKind::Function,
                input: None,
            })
            .unwrap();
        let response = tokio::task::spawn_blocking(move || {
            tool_response_rx
                .recv_timeout(Duration::from_secs(2))
                .unwrap()
        })
        .await
        .unwrap();
        assert!(matches!(response, RuntimeCommand::ToolResponse { .. }));
        event_tx
            .send(RuntimeEvent::Result {
                stored_value_writes: HashMap::new(),
                error_text: None,
                pending_tool_call_ids: vec!["tool-1".into()],
            })
            .unwrap();
        let response = tokio::time::timeout(Duration::from_secs(2), response_rx)
            .await
            .unwrap()
            .unwrap();
        let RuntimeResponse::Result {
            content_items,
            error_text: None,
            ..
        } = response
        else {
            panic!("expected normal completion");
        };
        assert!(content_items.iter().any(|item| matches!(item,
            FunctionCallOutputContentItem::InputText { text }
            if text.contains("slow-result") && text.contains("tool-1"))));
        actor.await.unwrap();
    }

    #[tokio::test]
    async fn nested_tool_call_in_flight_can_yield_and_later_complete() {
        // The nested tool call (500 ms) outlives the cell yield window
        // (100 ms). The cell should yield promptly instead of hiding a long
        // nested call; a follow-up wait then receives the eventual result.
        let service = CodeModeService::with_delegate(Arc::new(SlowToolDelegate {
            delay: Duration::from_millis(500),
        }));

        let response = tokio::time::timeout(
            Duration::from_secs(5),
            execute(
                &service,
                ExecuteRequest {
                    enabled_tools: vec![slow_tool_definition()],
                    source: "const r = await tools.slow({}); text(String(r));".to_string(),
                    yield_time_ms: Some(100),
                    ..execute_request("")
                },
            ),
        )
        .await
        .unwrap();

        assert_eq!(
            response,
            RuntimeResponse::Yielded {
                cell_id: cell_id("1"),
                content_items: Vec::new(),
            }
        );

        let wait = service
            .wait(WaitRequest {
                cell_id: cell_id("1"),
                yield_time_ms: 1_000,
            })
            .await
            .unwrap();
        assert_eq!(
            wait,
            WaitOutcome::LiveCell(RuntimeResponse::Result {
                cell_id: cell_id("1"),
                content_items: vec![FunctionCallOutputContentItem::InputText {
                    text: "slow-result".to_string(),
                }],
                error_text: None,
            })
        );
    }

    #[tokio::test]
    async fn yield_window_rearms_after_nested_tool_returns() {
        // The initial cell yield can now fire while the nested call is still
        // running. After the nested call (300 ms) returns, a follow-up wait sees
        // the output produced after tool-return before yielding again.
        let service = CodeModeService::with_delegate(Arc::new(SlowToolDelegate {
            delay: Duration::from_millis(300),
        }));

        let response = tokio::time::timeout(
            Duration::from_secs(5),
            execute(
                &service,
                ExecuteRequest {
                    enabled_tools: vec![slow_tool_definition()],
                    source: "await tools.slow({}); text(\"tool-returned\"); await new Promise(() => {});"
                        .to_string(),
                    yield_time_ms: Some(100),
                    ..execute_request("")
                },
            ),
        )
        .await
        .unwrap();

        assert_eq!(
            response,
            RuntimeResponse::Yielded {
                cell_id: cell_id("1"),
                content_items: Vec::new(),
            }
        );

        let wait = service
            .wait(WaitRequest {
                cell_id: cell_id("1"),
                yield_time_ms: 1_000,
            })
            .await
            .unwrap();
        assert_eq!(
            wait,
            WaitOutcome::LiveCell(RuntimeResponse::Yielded {
                cell_id: cell_id("1"),
                content_items: vec![FunctionCallOutputContentItem::InputText {
                    text: "tool-returned".to_string(),
                }],
            })
        );

        let _ = service.terminate(cell_id("1")).await;
    }

    #[tokio::test]
    async fn synchronous_exit_returns_successfully() {
        let service = CodeModeService::new();

        let response = execute(
            &service,
            ExecuteRequest {
                source: r#"text("before"); exit(); text("after");"#.to_string(),
                yield_time_ms: None,
                ..execute_request("")
            },
        )
        .await;

        assert_eq!(
            response,
            RuntimeResponse::Result {
                cell_id: cell_id("1"),
                content_items: vec![FunctionCallOutputContentItem::InputText {
                    text: "before".to_string(),
                }],
                error_text: None,
            }
        );
    }

    #[tokio::test]
    async fn stored_values_are_shared_between_cells_but_not_sessions() {
        let first_session = CodeModeService::new();
        let second_session = CodeModeService::new();

        let write_response = execute(
            &first_session,
            ExecuteRequest {
                source: r#"store("key", "visible");"#.to_string(),
                yield_time_ms: None,
                ..execute_request("")
            },
        )
        .await;

        let same_session = execute(
            &first_session,
            ExecuteRequest {
                source: r#"text(String(load("key")));"#.to_string(),
                yield_time_ms: None,
                ..execute_request("")
            },
        )
        .await;
        let other_session = execute(
            &second_session,
            ExecuteRequest {
                source: r#"text(String(load("key")));"#.to_string(),
                yield_time_ms: None,
                ..execute_request("")
            },
        )
        .await;

        assert_eq!(
            write_response,
            RuntimeResponse::Result {
                cell_id: cell_id("1"),
                content_items: Vec::new(),
                error_text: None,
            }
        );
        assert_eq!(
            same_session,
            RuntimeResponse::Result {
                cell_id: cell_id("2"),
                content_items: vec![FunctionCallOutputContentItem::InputText {
                    text: "visible".to_string(),
                }],
                error_text: None,
            }
        );
        assert_eq!(
            other_session,
            RuntimeResponse::Result {
                cell_id: cell_id("1"),
                content_items: vec![FunctionCallOutputContentItem::InputText {
                    text: "undefined".to_string(),
                }],
                error_text: None,
            }
        );
    }

    #[tokio::test]
    async fn stored_bare_function_round_trips_between_cells() {
        let service = CodeModeService::new();

        let write_response = execute(
            &service,
            ExecuteRequest {
                source: r#"store("helper", function double(n) { return n * 2; });"#.to_string(),
                yield_time_ms: None,
                ..execute_request("")
            },
        )
        .await;
        let read_response = execute(
            &service,
            ExecuteRequest {
                source: r#"const double = load("helper"); text(String(double(21)));"#.to_string(),
                yield_time_ms: None,
                ..execute_request("")
            },
        )
        .await;

        assert_eq!(
            write_response,
            RuntimeResponse::Result {
                cell_id: cell_id("1"),
                content_items: Vec::new(),
                error_text: None,
            }
        );
        assert_eq!(
            read_response,
            RuntimeResponse::Result {
                cell_id: cell_id("2"),
                content_items: vec![FunctionCallOutputContentItem::InputText {
                    text: "42".to_string(),
                }],
                error_text: None,
            }
        );
    }

    #[tokio::test]
    async fn stored_object_preserves_nested_function_and_siblings() {
        let service = CodeModeService::new();

        let write_response = execute(
            &service,
            ExecuteRequest {
                source: r#"store("helpers", { n: 42, gate: function gate(n) { return n > 10; }, label: "ok" });"#.to_string(),
                yield_time_ms: None,
                ..execute_request("")
            },
        )
        .await;
        let read_response = execute(
            &service,
            ExecuteRequest {
                source: r#"const helpers = load("helpers"); text(JSON.stringify({ n: helpers.n, label: helpers.label, gate: helpers.gate(11) }));"#.to_string(),
                yield_time_ms: None,
                ..execute_request("")
            },
        )
        .await;

        assert_eq!(
            write_response,
            RuntimeResponse::Result {
                cell_id: cell_id("1"),
                content_items: Vec::new(),
                error_text: None,
            }
        );
        assert_eq!(
            read_response,
            RuntimeResponse::Result {
                cell_id: cell_id("2"),
                content_items: vec![FunctionCallOutputContentItem::InputText {
                    text: r#"{"n":42,"label":"ok","gate":true}"#.to_string(),
                }],
                error_text: None,
            }
        );
    }

    #[tokio::test]
    async fn stored_array_preserves_nested_function_and_siblings() {
        let service = CodeModeService::new();

        let write_response = execute(
            &service,
            ExecuteRequest {
                source:
                    r#"store("items", [3, function triple(n) { return n * 3; }, { tag: "kept" }]);"#
                        .to_string(),
                yield_time_ms: None,
                ..execute_request("")
            },
        )
        .await;
        let read_response = execute(
            &service,
            ExecuteRequest {
                source: r#"const items = load("items"); text(JSON.stringify([items[0], items[1](14), items[2].tag]));"#.to_string(),
                yield_time_ms: None,
                ..execute_request("")
            },
        )
        .await;

        assert_eq!(
            write_response,
            RuntimeResponse::Result {
                cell_id: cell_id("1"),
                content_items: Vec::new(),
                error_text: None,
            }
        );
        assert_eq!(
            read_response,
            RuntimeResponse::Result {
                cell_id: cell_id("2"),
                content_items: vec![FunctionCallOutputContentItem::InputText {
                    text: r#"[3,42,"kept"]"#.to_string(),
                }],
                error_text: None,
            }
        );
    }

    #[tokio::test]
    async fn shutdown_interrupts_cpu_bound_cells() {
        let service = CodeModeService::new();

        let cell = service
            .execute(ExecuteRequest {
                source: "while (true) {}".to_string(),
                ..execute_request("")
            })
            .await
            .unwrap();
        assert_eq!(
            cell.initial_response().await.unwrap(),
            RuntimeResponse::Yielded {
                cell_id: cell_id("1"),
                content_items: Vec::new(),
            }
        );

        tokio::time::timeout(Duration::from_secs(1), service.shutdown())
            .await
            .unwrap()
            .unwrap();
    }

    #[tokio::test]
    async fn start_cell_rejects_new_cell_after_shutdown_begins() {
        let service = CodeModeService::new();
        service.inner.shutting_down.store(true, Ordering::Release);
        let (response_tx, _response_rx) = oneshot::channel();

        let error = service
            .start_cell(
                cell_id("late-cell"),
                execute_request(""),
                CellResponseSender::Runtime(response_tx),
                Some(/*initial_yield_time_ms*/ 1),
                PendingRuntimeMode::Continue,
            )
            .await
            .unwrap_err();

        assert_eq!(error, "code mode session is shutting down".to_string());
        assert!(service.inner.cells.lock().await.is_empty());
    }

    #[tokio::test]
    async fn execute_to_pending_returns_completed_for_synchronous_results() {
        let service = CodeModeService::new();

        let response = service
            .execute_to_pending(ExecuteRequest {
                source: r#"text("done");"#.to_string(),
                yield_time_ms: Some(60_000),
                ..execute_request("")
            })
            .await
            .unwrap();

        assert_eq!(
            response,
            ExecuteToPendingOutcome::Completed(RuntimeResponse::Result {
                cell_id: cell_id("1"),
                content_items: vec![FunctionCallOutputContentItem::InputText {
                    text: "done".to_string(),
                }],
                error_text: None,
            })
        );
    }

    #[tokio::test]
    async fn execute_to_pending_returns_once_the_runtime_is_quiescent() {
        let service = CodeModeService::new();

        let response = tokio::time::timeout(
            Duration::from_secs(1),
            service.execute_to_pending(ExecuteRequest {
                source: r#"text("before"); await new Promise(() => {});"#.to_string(),
                yield_time_ms: Some(60_000),
                ..execute_request("")
            }),
        )
        .await
        .unwrap()
        .unwrap();

        assert_eq!(
            response,
            ExecuteToPendingOutcome::Pending {
                cell_id: cell_id("1"),
                content_items: vec![FunctionCallOutputContentItem::InputText {
                    text: "before".to_string(),
                }],
                pending_tool_call_ids: Vec::new(),
            }
        );

        let termination = service.terminate(cell_id("1")).await.unwrap();

        assert_eq!(
            termination,
            WaitOutcome::LiveCell(RuntimeResponse::Terminated {
                cell_id: cell_id("1"),
                content_items: Vec::new(),
            })
        );
    }

    #[tokio::test]
    async fn execute_to_pending_identifies_tool_calls_in_paused_frontier() {
        let service = CodeModeService::new();

        let response = service
            .execute_to_pending(ExecuteRequest {
                enabled_tools: vec![ToolDefinition {
                    name: "echo".to_string(),
                    tool_name: ToolName::plain("echo"),
                    description: String::new(),
                    kind: CodeModeToolKind::Function,
                    input_schema: None,
                    output_schema: None,
                    namespace_binding: None,
                }],
                source: r#"
await Promise.all([
  tools.echo({ value: "first" }),
  tools.echo({ value: "second" }),
]);
"#
                .to_string(),
                yield_time_ms: Some(60_000),
                ..execute_request("")
            })
            .await
            .unwrap();

        assert_eq!(
            response,
            ExecuteToPendingOutcome::Pending {
                cell_id: cell_id("1"),
                content_items: Vec::new(),
                pending_tool_call_ids: vec!["tool-1".to_string(), "tool-2".to_string()],
            }
        );

        let termination = service.terminate(cell_id("1")).await.unwrap();

        let WaitOutcome::LiveCell(RuntimeResponse::Terminated {
            cell_id: id,
            content_items,
        }) = termination
        else {
            panic!("expected terminated cell");
        };
        assert_eq!(id, cell_id("1"));
        let outcomes = content_items
            .iter()
            .filter_map(|item| match item {
                FunctionCallOutputContentItem::InputText { text } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert_eq!(outcomes.matches("\"tool\":\"echo\"").count(), 2);
        assert!(outcomes.contains("unavailable"));
    }

    #[tokio::test]
    async fn execute_to_pending_excludes_delayed_timeout_tool_calls_until_wait() {
        let service = CodeModeService::new();

        let initial_response = service
            .execute_to_pending(ExecuteRequest {
                enabled_tools: vec![ToolDefinition {
                    name: "echo".to_string(),
                    tool_name: ToolName::plain("echo"),
                    description: String::new(),
                    kind: CodeModeToolKind::Function,
                    input_schema: None,
                    output_schema: None,
                    namespace_binding: None,
                }],
                source: r#"
setTimeout(() => {
  tools.echo({ value: "delayed" });
}, 1000);
await Promise.all([
  tools.echo({ value: "second" }),
  tools.echo({ value: "third" }),
]);
"#
                .to_string(),
                yield_time_ms: Some(60_000),
                ..execute_request("")
            })
            .await
            .unwrap();

        assert_eq!(
            initial_response,
            ExecuteToPendingOutcome::Pending {
                cell_id: cell_id("1"),
                content_items: Vec::new(),
                pending_tool_call_ids: vec!["tool-1".to_string(), "tool-2".to_string()],
            }
        );

        let runtime_tx = service
            .inner
            .cells
            .lock()
            .await
            .get(&cell_id("1"))
            .unwrap()
            .runtime_tx
            .clone();
        runtime_tx
            .send(RuntimeCommand::TimeoutFired { id: 1 })
            .unwrap();

        let resumed_response = tokio::time::timeout(
            Duration::from_secs(1),
            service.wait_to_pending(WaitToPendingRequest {
                cell_id: cell_id("1"),
            }),
        )
        .await
        .unwrap()
        .unwrap();

        assert_eq!(
            resumed_response,
            WaitToPendingOutcome::LiveCell(ExecuteToPendingOutcome::Pending {
                cell_id: cell_id("1"),
                content_items: Vec::new(),
                pending_tool_call_ids: vec!["tool-3".to_string()],
            })
        );

        let termination = service.terminate(cell_id("1")).await.unwrap();

        let WaitOutcome::LiveCell(RuntimeResponse::Terminated {
            cell_id: id,
            content_items,
        }) = termination
        else {
            panic!("expected terminated cell");
        };
        assert_eq!(id, cell_id("1"));
        let outcomes = content_items
            .iter()
            .filter_map(|item| match item {
                FunctionCallOutputContentItem::InputText { text } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert_eq!(outcomes.matches("\"tool\":\"echo\"").count(), 3);
        assert!(outcomes.contains("unavailable"));
    }

    #[tokio::test]
    async fn wait_to_pending_returns_after_resumed_runtime_becomes_quiescent_again() {
        let service = CodeModeService::new();

        let initial_response = service
            .execute_to_pending(ExecuteRequest {
                source: r#"
await new Promise((resolve) => setTimeout(resolve, 60_000));
text("after");
await new Promise(() => {});
"#
                .to_string(),
                yield_time_ms: Some(60_000),
                ..execute_request("")
            })
            .await
            .unwrap();

        assert_eq!(
            initial_response,
            ExecuteToPendingOutcome::Pending {
                cell_id: cell_id("1"),
                content_items: Vec::new(),
                pending_tool_call_ids: Vec::new(),
            }
        );

        let runtime_tx = service
            .inner
            .cells
            .lock()
            .await
            .get(&cell_id("1"))
            .unwrap()
            .runtime_tx
            .clone();
        runtime_tx
            .send(RuntimeCommand::TimeoutFired { id: 1 })
            .unwrap();

        let resumed_response = tokio::time::timeout(
            Duration::from_secs(1),
            service.wait_to_pending(WaitToPendingRequest {
                cell_id: cell_id("1"),
            }),
        )
        .await
        .unwrap()
        .unwrap();

        assert_eq!(
            resumed_response,
            WaitToPendingOutcome::LiveCell(ExecuteToPendingOutcome::Pending {
                cell_id: cell_id("1"),
                content_items: vec![FunctionCallOutputContentItem::InputText {
                    text: "after".to_string(),
                }],
                pending_tool_call_ids: Vec::new(),
            })
        );

        let termination = service.terminate(cell_id("1")).await.unwrap();

        assert_eq!(
            termination,
            WaitOutcome::LiveCell(RuntimeResponse::Terminated {
                cell_id: cell_id("1"),
                content_items: Vec::new(),
            })
        );
    }

    #[tokio::test]
    async fn wait_to_pending_returns_completed_after_resumed_runtime_finishes() {
        let service = CodeModeService::new();

        let initial_response = service
            .execute_to_pending(ExecuteRequest {
                source: r#"
await new Promise((resolve) => setTimeout(resolve, 60_000));
text("done");
"#
                .to_string(),
                yield_time_ms: Some(60_000),
                ..execute_request("")
            })
            .await
            .unwrap();

        assert_eq!(
            initial_response,
            ExecuteToPendingOutcome::Pending {
                cell_id: cell_id("1"),
                content_items: Vec::new(),
                pending_tool_call_ids: Vec::new(),
            }
        );

        let runtime_tx = service
            .inner
            .cells
            .lock()
            .await
            .get(&cell_id("1"))
            .unwrap()
            .runtime_tx
            .clone();
        runtime_tx
            .send(RuntimeCommand::TimeoutFired { id: 1 })
            .unwrap();

        let resumed_response = tokio::time::timeout(
            Duration::from_secs(1),
            service.wait_to_pending(WaitToPendingRequest {
                cell_id: cell_id("1"),
            }),
        )
        .await
        .unwrap()
        .unwrap();

        assert_eq!(
            resumed_response,
            WaitToPendingOutcome::LiveCell(ExecuteToPendingOutcome::Completed(
                RuntimeResponse::Result {
                    cell_id: cell_id("1"),
                    content_items: vec![FunctionCallOutputContentItem::InputText {
                        text: "done".to_string(),
                    }],
                    error_text: None,
                }
            ))
        );
    }

    #[tokio::test]
    async fn v8_console_is_not_exposed_on_global_this() {
        let service = CodeModeService::new();

        let response = execute(
            &service,
            ExecuteRequest {
                source: r#"text(String(Object.hasOwn(globalThis, "console")));"#.to_string(),
                yield_time_ms: None,
                ..execute_request("")
            },
        )
        .await;

        assert_eq!(
            response,
            RuntimeResponse::Result {
                cell_id: cell_id("1"),
                content_items: vec![FunctionCallOutputContentItem::InputText {
                    text: "false".to_string(),
                }],
                error_text: None,
            }
        );
    }

    #[tokio::test]
    async fn date_locale_string_formats_with_icu_data() {
        let service = CodeModeService::new();

        let response = execute(
            &service,
            ExecuteRequest {
                source: r#"
const value = new Date("2025-01-02T03:04:05Z")
  .toLocaleString("fr-FR", {
    weekday: "long",
    month: "long",
    day: "numeric",
    hour: "2-digit",
    minute: "2-digit",
    second: "2-digit",
    hour12: false,
    timeZone: "UTC",
  });
text(value);
"#
                .to_string(),
                yield_time_ms: None,
                ..execute_request("")
            },
        )
        .await;

        assert_eq!(
            response,
            RuntimeResponse::Result {
                cell_id: cell_id("1"),
                content_items: vec![FunctionCallOutputContentItem::InputText {
                    text: "jeudi 2 janvier \u{e0} 03:04:05".to_string(),
                }],
                error_text: None,
            }
        );
    }

    #[tokio::test]
    async fn intl_date_time_format_formats_with_icu_data() {
        let service = CodeModeService::new();

        let response = execute(
            &service,
            ExecuteRequest {
                source: r#"
const formatter = new Intl.DateTimeFormat("fr-FR", {
  weekday: "long",
  month: "long",
  day: "numeric",
  hour: "2-digit",
  minute: "2-digit",
  second: "2-digit",
  hour12: false,
  timeZone: "UTC",
});
text(formatter.format(new Date("2025-01-02T03:04:05Z")));
"#
                .to_string(),
                yield_time_ms: None,
                ..execute_request("")
            },
        )
        .await;

        assert_eq!(
            response,
            RuntimeResponse::Result {
                cell_id: cell_id("1"),
                content_items: vec![FunctionCallOutputContentItem::InputText {
                    text: "jeudi 2 janvier \u{e0} 03:04:05".to_string(),
                }],
                error_text: None,
            }
        );
    }

    #[tokio::test]
    async fn output_helpers_return_undefined() {
        let service = CodeModeService::new();

        let response = execute(
            &service,
            ExecuteRequest {
                source: r#"
const returnsUndefined = [
  text("first"),
  image("https://example.com/image.jpg"),
  notify("ping"),
].map((value) => value === undefined);
text(JSON.stringify(returnsUndefined));
"#
                .to_string(),
                yield_time_ms: None,
                ..execute_request("")
            },
        )
        .await;

        assert_eq!(
            response,
            RuntimeResponse::Result {
                cell_id: cell_id("1"),
                content_items: vec![
                    FunctionCallOutputContentItem::InputText {
                        text: "first".to_string(),
                    },
                    FunctionCallOutputContentItem::InputImage {
                        image_url: "https://example.com/image.jpg".to_string(),
                        detail: Some(crate::DEFAULT_IMAGE_DETAIL),
                    },
                    FunctionCallOutputContentItem::InputText {
                        text: "[true,true,true]".to_string(),
                    },
                ],
                error_text: None,
            }
        );
    }

    #[tokio::test]
    async fn image_helper_accepts_raw_mcp_image_block_with_original_detail() {
        let service = CodeModeService::new();

        let response = execute(
            &service,
            ExecuteRequest {
                source: r#"
image({
  type: "image",
  data: "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR4nGP4z8DwHwAFAAH/iZk9HQAAAABJRU5ErkJggg==",
  mimeType: "image/png",
  _meta: { "codex/imageDetail": "original" },
});
"#
                .to_string(),
                yield_time_ms: None,
                ..execute_request("")
            },
        )
        .await;

        assert_eq!(
            response,
            RuntimeResponse::Result {
                cell_id: cell_id("1"),
                content_items: vec![FunctionCallOutputContentItem::InputImage {
                    image_url: "data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR4nGP4z8DwHwAFAAH/iZk9HQAAAABJRU5ErkJggg==".to_string(),
                    detail: Some(crate::ImageDetail::Original),
                }],
                error_text: None,
            }
        );
    }

    #[tokio::test]
    async fn image_helper_second_arg_overrides_explicit_object_detail() {
        let service = CodeModeService::new();

        let response = execute(
            &service,
            ExecuteRequest {
                source: r#"
image(
  {
    image_url: "https://example.com/image.jpg",
    detail: "high",
  },
  "original",
);
"#
                .to_string(),
                yield_time_ms: None,
                ..execute_request("")
            },
        )
        .await;

        assert_eq!(
            response,
            RuntimeResponse::Result {
                cell_id: cell_id("1"),
                content_items: vec![FunctionCallOutputContentItem::InputImage {
                    image_url: "https://example.com/image.jpg".to_string(),
                    detail: Some(crate::ImageDetail::Original),
                }],
                error_text: None,
            }
        );
    }

    #[tokio::test]
    async fn image_helper_second_arg_overrides_raw_mcp_image_detail() {
        let service = CodeModeService::new();

        let response = execute(
            &service,
            ExecuteRequest {
                source: r#"
image(
  {
    type: "image",
    data: "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR4nGP4z8DwHwAFAAH/iZk9HQAAAABJRU5ErkJggg==",
    mimeType: "image/png",
    _meta: { "codex/imageDetail": "original" },
  },
  "high",
);
"#
                .to_string(),
                yield_time_ms: None,
                ..execute_request("")
            },
        )
        .await;

        assert_eq!(
            response,
            RuntimeResponse::Result {
                cell_id: cell_id("1"),
                content_items: vec![FunctionCallOutputContentItem::InputImage {
                    image_url: "data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR4nGP4z8DwHwAFAAH/iZk9HQAAAABJRU5ErkJggg==".to_string(),
                    detail: Some(crate::ImageDetail::High),
                }],
                error_text: None,
            }
        );
    }

    #[tokio::test]
    async fn image_helper_accepts_low_detail() {
        let service = CodeModeService::new();

        let response = execute(
            &service,
            ExecuteRequest {
                source: r#"
image({
  image_url: "https://example.com/image.jpg",
  detail: "low",
});
"#
                .to_string(),
                yield_time_ms: None,
                ..execute_request("")
            },
        )
        .await;

        assert_eq!(
            response,
            RuntimeResponse::Result {
                cell_id: cell_id("1"),
                content_items: vec![FunctionCallOutputContentItem::InputImage {
                    image_url: "https://example.com/image.jpg".to_string(),
                    detail: Some(crate::ImageDetail::Low),
                }],
                error_text: None,
            }
        );
    }

    #[tokio::test]
    async fn image_helper_rejects_unsupported_detail() {
        let service = CodeModeService::new();

        let response = execute(
            &service,
            ExecuteRequest {
                source: r#"
image({
  image_url: "https://example.com/image.jpg",
  detail: "medium",
});
"#
                .to_string(),
                yield_time_ms: None,
                ..execute_request("")
            },
        )
        .await;

        assert_eq!(
            response,
            RuntimeResponse::Result {
                cell_id: cell_id("1"),
                content_items: Vec::new(),
                error_text: Some(
                    "image detail must be one of: auto, low, high, original".to_string()
                ),
            }
        );
    }

    #[tokio::test]
    async fn image_helper_rejects_raw_mcp_result_container() {
        let service = CodeModeService::new();

        let response = execute(
            &service,
            ExecuteRequest {
                source: r#"
image({
  content: [
    {
      type: "image",
      data: "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR4nGP4z8DwHwAFAAH/iZk9HQAAAABJRU5ErkJggg==",
      mimeType: "image/png",
      _meta: { "codex/imageDetail": "original" },
    },
  ],
  isError: false,
});
"#
                .to_string(),
                yield_time_ms: None,
                ..execute_request("")
            },
        )
        .await;

        assert_eq!(
            response,
            RuntimeResponse::Result {
                cell_id: cell_id("1"),
                content_items: Vec::new(),
                error_text: Some(
                    "image expects a non-empty image URL string, an object with image_url and optional detail, or a raw MCP image block".to_string(),
                ),
            }
        );
    }

    #[tokio::test]
    async fn wait_reports_missing_cell_separately_from_runtime_results() {
        let service = CodeModeService::new();

        let response = service
            .wait(WaitRequest {
                cell_id: cell_id("missing"),
                yield_time_ms: 1,
            })
            .await
            .unwrap();

        assert_eq!(
            response,
            WaitOutcome::MissingCell(RuntimeResponse::Result {
                cell_id: cell_id("missing"),
                content_items: Vec::new(),
                error_text: Some("exec cell missing not found".to_string()),
            })
        );
    }

    #[tokio::test]
    async fn terminate_waits_for_runtime_shutdown_before_responding() {
        let inner = test_inner();
        let (event_tx, event_rx) = mpsc::unbounded_channel();
        let (control_tx, control_rx) = mpsc::unbounded_channel();
        let (initial_response_tx, initial_response_rx) = oneshot::channel();
        let (runtime_event_tx, _runtime_event_rx) = mpsc::unbounded_channel();
        let (runtime_tx, runtime_control_tx, runtime_terminate_handle) = spawn_runtime(
            HashMap::new(),
            ExecuteRequest {
                source: "await new Promise(() => {})".to_string(),
                yield_time_ms: None,
                ..execute_request("")
            },
            runtime_event_tx,
            PendingRuntimeMode::Continue,
        )
        .unwrap();

        tokio::spawn(run_cell_control(
            inner,
            CellControlContext {
                cell_id: cell_id("cell-1"),
                runtime_tx: runtime_tx.clone(),
                runtime_control_tx,
                pending_mode: PendingRuntimeMode::Continue,
                runtime_terminate_handle,
                cancellation_token: tokio_util::sync::CancellationToken::new(),
            },
            event_rx,
            control_rx,
            CellResponseSender::Runtime(initial_response_tx),
            Some(/*initial_yield_time_ms*/ 60_000),
        ));

        event_tx.send(RuntimeEvent::Started).unwrap();
        event_tx.send(RuntimeEvent::YieldRequested).unwrap();
        assert_eq!(
            initial_response_rx.await.unwrap(),
            RuntimeResponse::Yielded {
                cell_id: cell_id("cell-1"),
                content_items: Vec::new(),
            }
        );

        let (terminate_response_tx, terminate_response_rx) = oneshot::channel();
        control_tx
            .send(CellControlCommand::Terminate {
                response_tx: terminate_response_tx,
            })
            .unwrap();
        let terminate_response = async { terminate_response_rx.await.unwrap() };
        tokio::pin!(terminate_response);
        assert!(
            tokio::time::timeout(Duration::from_millis(100), terminate_response.as_mut())
                .await
                .is_err()
        );

        drop(event_tx);

        assert_eq!(
            terminate_response.await,
            RuntimeResponse::Terminated {
                cell_id: cell_id("cell-1"),
                content_items: Vec::new(),
            }
        );

        let _ = runtime_tx.send(RuntimeCommand::Terminate);
    }
}
