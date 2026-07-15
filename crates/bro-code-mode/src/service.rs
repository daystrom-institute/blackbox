// Vendored from openai/codex codex-rs/code-mode (Apache-2.0); see crate NOTICE.
use std::collections::HashMap;
use std::collections::VecDeque;
use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::OnceLock;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::time::Duration;

use serde::Deserialize;
use serde::Serialize;
use serde_json::Value as JsonValue;
use tokio::sync::Mutex;
use tokio::sync::Notify;
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

// Local addition (not vendored): retain enough bounded lifecycle history for
// late wait/terminate calls without keeping isolates or actor tasks alive.
const MAX_CELL_TOMBSTONES: usize = 256;

// Local addition (not vendored): session shutdown is a bounded library
// operation. The worker may treat expiry as a fatal shutdown failure.
const SESSION_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);

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

/// Local addition (not vendored): a reserved or active cell handle. The
/// runtime sender is installed after admission so shutdown never waits behind
/// V8 startup while holding the admission lock.
#[derive(Clone)]
struct CellHandle {
    control_tx: mpsc::UnboundedSender<CellControlCommand>,
    runtime_tx: Arc<OnceLock<std::sync::mpsc::Sender<RuntimeCommand>>>,
}

// Local addition (not vendored): direct runtime termination is a best-effort
// supplement to the actor's isolate termination path.
impl CellHandle {
    fn terminate_runtime(&self) {
        if let Some(runtime_tx) = self.runtime_tx.get() {
            let _ = runtime_tx.send(RuntimeCommand::Terminate);
        }
    }

    #[cfg(test)]
    fn runtime_tx(&self) -> Option<std::sync::mpsc::Sender<RuntimeCommand>> {
        self.runtime_tx.get().cloned()
    }
}

/// Local addition (not vendored): the stable cause retained in a terminal
/// tombstone. Existing `RuntimeResponse` variants remain the wire surface.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CellTerminalCause {
    Completed,
    ExplicitTermination,
    SessionShutdown,
    RuntimeFailure,
    InternalProtocolFailure,
}

/// Local addition (not vendored): immutable terminal output plus any yield
/// boundaries whose observers disappeared before receiving them.
#[derive(Clone, Debug)]
struct CellTerminal {
    cause: CellTerminalCause,
    content_items: Vec<FunctionCallOutputContentItem>,
    error_text: Option<String>,
    pending_yields: VecDeque<Vec<FunctionCallOutputContentItem>>,
    terminal_claimed: bool,
}

// Local addition (not vendored): terminal response reconstruction preserves
// the existing exec/wait response schema.
impl CellTerminal {
    fn completed(
        content_items: Vec<FunctionCallOutputContentItem>,
        error_text: Option<String>,
        pending_yields: VecDeque<Vec<FunctionCallOutputContentItem>>,
    ) -> Self {
        let cause = if error_text.is_some() {
            CellTerminalCause::RuntimeFailure
        } else {
            CellTerminalCause::Completed
        };
        Self {
            cause,
            content_items,
            error_text,
            pending_yields,
            terminal_claimed: false,
        }
    }

    fn terminated(
        cause: CellTerminalCause,
        content_items: Vec<FunctionCallOutputContentItem>,
        pending_yields: VecDeque<Vec<FunctionCallOutputContentItem>>,
    ) -> Self {
        debug_assert!(matches!(
            cause,
            CellTerminalCause::ExplicitTermination | CellTerminalCause::SessionShutdown
        ));
        Self {
            cause,
            content_items,
            error_text: None,
            pending_yields,
            terminal_claimed: false,
        }
    }

    fn internal_failure(
        content_items: Vec<FunctionCallOutputContentItem>,
        pending_yields: VecDeque<Vec<FunctionCallOutputContentItem>>,
    ) -> Self {
        Self {
            cause: CellTerminalCause::InternalProtocolFailure,
            content_items,
            error_text: Some("exec runtime ended unexpectedly".to_string()),
            pending_yields,
            terminal_claimed: false,
        }
    }

    fn response(&self, cell_id: &CellId) -> RuntimeResponse {
        match self.cause {
            CellTerminalCause::ExplicitTermination | CellTerminalCause::SessionShutdown => {
                RuntimeResponse::Terminated {
                    cell_id: cell_id.clone(),
                    content_items: self.content_items.clone(),
                }
            }
            CellTerminalCause::Completed
            | CellTerminalCause::RuntimeFailure
            | CellTerminalCause::InternalProtocolFailure => RuntimeResponse::Result {
                cell_id: cell_id.clone(),
                content_items: self.content_items.clone(),
                error_text: self.error_text.clone(),
            },
        }
    }

    fn next_wait_response(&mut self, cell_id: &CellId) -> RuntimeResponse {
        if let Some(content_items) = self.pending_yields.pop_front() {
            RuntimeResponse::Yielded {
                cell_id: cell_id.clone(),
                content_items,
            }
        } else {
            self.claim_response(cell_id)
        }
    }

    fn claim_response(&mut self, cell_id: &CellId) -> RuntimeResponse {
        self.terminal_claimed = true;
        self.response(cell_id)
    }
}

/// Local addition (not vendored): admission, active ownership, and retained
/// terminal state share one serialized registry.
struct CellRegistry {
    accepting: bool,
    active: HashMap<CellId, CellHandle>,
    tombstones: HashMap<CellId, CellTerminal>,
    tombstone_order: VecDeque<CellId>,
}

// Local addition (not vendored): registry operations enforce bounded retention
// and atomically replace active ownership with a tombstone.
impl CellRegistry {
    fn new() -> Self {
        Self {
            accepting: true,
            active: HashMap::new(),
            tombstones: HashMap::new(),
            tombstone_order: VecDeque::new(),
        }
    }

    fn reserve(&mut self, cell_id: CellId, handle: CellHandle) -> Result<(), String> {
        if !self.accepting {
            return Err("code mode session is shutting down".to_string());
        }
        if self.active.contains_key(&cell_id) {
            return Err(format!("exec cell {cell_id} already exists"));
        }
        self.tombstones.remove(&cell_id);
        self.tombstone_order.retain(|retained| retained != &cell_id);
        self.active.insert(cell_id, handle);
        Ok(())
    }

    fn forget_reservation(&mut self, cell_id: &CellId) {
        self.active.remove(cell_id);
    }

    fn retain_terminal(&mut self, cell_id: CellId, terminal: CellTerminal) {
        self.active.remove(&cell_id);
        self.tombstones.insert(cell_id.clone(), terminal);
        self.tombstone_order.retain(|retained| retained != &cell_id);
        self.tombstone_order.push_back(cell_id);
        while self.tombstone_order.len() > MAX_CELL_TOMBSTONES {
            if let Some(expired) = self.tombstone_order.pop_front() {
                self.tombstones.remove(&expired);
            }
        }
    }
}

struct Inner {
    stored_values: Mutex<HashMap<String, JsonValue>>,
    // Local addition (not vendored): admission and tombstones are serialized
    // with active-cell ownership.
    registry: Mutex<CellRegistry>,
    // Local addition (not vendored): shutdown waiters avoid spin polling.
    registry_changed: Notify,
    delegate: Arc<dyn CodeModeSessionDelegate>,
    // Local addition (not vendored): every cell token descends from this
    // session root; delegated-call tokens descend from their cell token.
    session_cancellation: CancellationToken,
    next_cell_id: AtomicU64,
}

pub struct CodeModeService {
    inner: Arc<Inner>,
}

/// Local addition (not vendored): one registry lookup distinguishes active,
/// retained terminal, and unknown cells without a remove/insert race.
enum CellLookup {
    Active(CellHandle),
    Terminal(RuntimeResponse),
    Missing,
}

impl CodeModeService {
    pub fn new() -> Self {
        Self::with_delegate(Arc::new(NoopCodeModeSessionDelegate))
    }

    pub fn with_delegate(delegate: Arc<dyn CodeModeSessionDelegate>) -> Self {
        Self {
            inner: Arc::new(Inner {
                stored_values: Mutex::new(HashMap::new()),
                registry: Mutex::new(CellRegistry::new()),
                registry_changed: Notify::new(),
                delegate,
                session_cancellation: CancellationToken::new(),
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
        // Local addition (not vendored): cell and delegated-call cancellation
        // descend from the session root, while admission is reserved before V8
        // startup so shutdown can close admission without waiting on startup.
        let cancellation_token = self.inner.session_cancellation.child_token();
        let runtime_tx_slot = Arc::new(OnceLock::new());
        let handle = CellHandle {
            control_tx,
            runtime_tx: Arc::clone(&runtime_tx_slot),
        };
        self.inner
            .registry
            .lock()
            .await
            .reserve(cell_id.clone(), handle)?;

        let runtime = spawn_runtime(stored_values, request, event_tx, pending_mode);
        let (runtime_tx, runtime_control_tx, runtime_terminate_handle) = match runtime {
            Ok(runtime) => runtime,
            Err(err) => {
                self.inner
                    .registry
                    .lock()
                    .await
                    .forget_reservation(&cell_id);
                self.inner.registry_changed.notify_waiters();
                return Err(err);
            }
        };
        let _ = runtime_tx_slot.set(runtime_tx.clone());

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

    // Local addition (not vendored): normal waits consume preserved yield
    // boundaries before observing the stable terminal response.
    async fn lookup_wait(&self, cell_id: &CellId) -> CellLookup {
        let mut registry = self.inner.registry.lock().await;
        if let Some(handle) = registry.active.get(cell_id) {
            return CellLookup::Active(handle.clone());
        }
        if let Some(terminal) = registry.tombstones.get_mut(cell_id) {
            return CellLookup::Terminal(terminal.next_wait_response(cell_id));
        }
        CellLookup::Missing
    }

    // Local addition (not vendored): explicit terminate and pending-mode calls
    // observe the stable terminal outcome without consuming a pending yield.
    async fn lookup_terminal(&self, cell_id: &CellId) -> CellLookup {
        let mut registry = self.inner.registry.lock().await;
        if let Some(handle) = registry.active.get(cell_id) {
            return CellLookup::Active(handle.clone());
        }
        if let Some(terminal) = registry.tombstones.get_mut(cell_id) {
            return CellLookup::Terminal(terminal.claim_response(cell_id));
        }
        CellLookup::Missing
    }

    pub async fn wait(&self, request: WaitRequest) -> Result<WaitOutcome, String> {
        let WaitRequest {
            cell_id,
            yield_time_ms,
        } = request;
        let handle = match self.lookup_wait(&cell_id).await {
            CellLookup::Active(handle) => handle,
            CellLookup::Terminal(response) => return Ok(WaitOutcome::LiveCell(response)),
            CellLookup::Missing => {
                return Ok(WaitOutcome::MissingCell(missing_cell_response(cell_id)));
            }
        };
        let (response_tx, response_rx) = oneshot::channel();
        let control_message = CellControlCommand::Poll {
            yield_time_ms,
            response_tx,
        };
        if handle.control_tx.send(control_message).is_err() {
            // Local addition (not vendored): actor teardown installs the
            // tombstone before dropping its command receiver.
            return Ok(match self.lookup_wait(&cell_id).await {
                CellLookup::Terminal(response) => WaitOutcome::LiveCell(response),
                _ => WaitOutcome::MissingCell(missing_cell_response(cell_id)),
            });
        }
        match response_rx.await {
            Ok(response) => Ok(WaitOutcome::LiveCell(response)),
            Err(_) => Ok(match self.lookup_wait(&cell_id).await {
                CellLookup::Terminal(response) => WaitOutcome::LiveCell(response),
                _ => WaitOutcome::MissingCell(missing_cell_response(cell_id)),
            }),
        }
    }

    pub async fn terminate(&self, cell_id: CellId) -> Result<WaitOutcome, String> {
        let handle = match self.lookup_terminal(&cell_id).await {
            CellLookup::Active(handle) => handle,
            CellLookup::Terminal(response) => return Ok(WaitOutcome::LiveCell(response)),
            CellLookup::Missing => {
                return Ok(WaitOutcome::MissingCell(missing_cell_response(cell_id)));
            }
        };
        let (response_tx, response_rx) = oneshot::channel();
        if handle
            .control_tx
            .send(CellControlCommand::Terminate {
                cause: CellTerminalCause::ExplicitTermination,
                response_tx: Some(response_tx),
            })
            .is_err()
        {
            return Ok(match self.lookup_terminal(&cell_id).await {
                CellLookup::Terminal(response) => WaitOutcome::LiveCell(response),
                _ => WaitOutcome::MissingCell(missing_cell_response(cell_id)),
            });
        }
        match response_rx.await {
            Ok(response) => Ok(WaitOutcome::LiveCell(response)),
            Err(_) => Ok(match self.lookup_terminal(&cell_id).await {
                CellLookup::Terminal(response) => WaitOutcome::LiveCell(response),
                _ => WaitOutcome::MissingCell(missing_cell_response(cell_id)),
            }),
        }
    }

    pub async fn wait_to_pending(
        &self,
        request: WaitToPendingRequest,
    ) -> Result<WaitToPendingOutcome, String> {
        let cell_id = request.cell_id;
        let handle = match self.lookup_terminal(&cell_id).await {
            CellLookup::Active(handle) => handle,
            CellLookup::Terminal(response) => {
                return Ok(WaitToPendingOutcome::LiveCell(
                    ExecuteToPendingOutcome::Completed(response),
                ));
            }
            CellLookup::Missing => {
                return Ok(WaitToPendingOutcome::MissingCell(missing_cell_response(
                    cell_id,
                )));
            }
        };
        let (response_tx, response_rx) = oneshot::channel();
        if handle
            .control_tx
            .send(CellControlCommand::PollToPending { response_tx })
            .is_err()
        {
            return Ok(match self.lookup_terminal(&cell_id).await {
                CellLookup::Terminal(response) => {
                    WaitToPendingOutcome::LiveCell(ExecuteToPendingOutcome::Completed(response))
                }
                _ => WaitToPendingOutcome::MissingCell(missing_cell_response(cell_id)),
            });
        }
        match response_rx.await {
            Ok(response) => Ok(WaitToPendingOutcome::LiveCell(response)),
            Err(_) => Ok(match self.lookup_terminal(&cell_id).await {
                CellLookup::Terminal(response) => {
                    WaitToPendingOutcome::LiveCell(ExecuteToPendingOutcome::Completed(response))
                }
                _ => WaitToPendingOutcome::MissingCell(missing_cell_response(cell_id)),
            }),
        }
    }

    pub async fn shutdown(&self) -> Result<(), String> {
        self.shutdown_with_timeout(SESSION_SHUTDOWN_TIMEOUT).await
    }

    // Local addition (not vendored): admission closes and accepted children
    // are enumerated in one critical section, then shutdown waits under a
    // bounded deadline without retaining the registry lock.
    async fn shutdown_with_timeout(&self, shutdown_timeout: Duration) -> Result<(), String> {
        let handles = {
            let mut registry = self.inner.registry.lock().await;
            registry.accepting = false;
            registry.active.values().cloned().collect::<Vec<_>>()
        };
        self.inner.session_cancellation.cancel();
        for handle in handles {
            let _ = handle.control_tx.send(CellControlCommand::Terminate {
                cause: CellTerminalCause::SessionShutdown,
                response_tx: None,
            });
        }

        let wait_for_cells = async {
            loop {
                let changed = self.inner.registry_changed.notified();
                tokio::pin!(changed);
                // Local addition (not vendored): register before checking the
                // registry so actor teardown cannot race between check/wait.
                changed.as_mut().enable();
                if self.inner.registry.lock().await.active.is_empty() {
                    return;
                }
                changed.await;
            }
        };
        if tokio::time::timeout(shutdown_timeout, wait_for_cells)
            .await
            .is_err()
        {
            let remaining_handles = self
                .inner
                .registry
                .lock()
                .await
                .active
                .values()
                .cloned()
                .collect::<Vec<_>>();
            let remaining = remaining_handles.len();
            // Local addition (not vendored): after the actor deadline has
            // already failed, apply one last best-effort runtime interruption.
            for handle in remaining_handles {
                handle.terminate_runtime();
            }
            return Err(format!(
                "code mode session shutdown timed out with {remaining} active cell(s)"
            ));
        }
        Ok(())
    }
}

impl Default for CodeModeService {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for CodeModeService {
    fn drop(&mut self) {
        // Local addition (not vendored): Drop cannot await, but root
        // cancellation still reaches every accepted cell and delegated call.
        self.inner.session_cancellation.cancel();
        if let Ok(mut registry) = self.inner.registry.try_lock() {
            registry.accepting = false;
            for handle in registry.active.values() {
                let _ = handle.control_tx.send(CellControlCommand::Terminate {
                    cause: CellTerminalCause::SessionShutdown,
                    response_tx: None,
                });
                handle.terminate_runtime();
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

/// Local addition (not vendored): every lifecycle command is serialized by the
/// cell actor, including the cause that first wins termination.
enum CellControlCommand {
    Poll {
        yield_time_ms: u64,
        response_tx: oneshot::Sender<RuntimeResponse>,
    },
    PollToPending {
        response_tx: oneshot::Sender<ExecuteToPendingOutcome>,
    },
    Terminate {
        cause: CellTerminalCause,
        response_tx: Option<oneshot::Sender<RuntimeResponse>>,
    },
}

/// Local addition (not vendored): explicit terminate observers are distinct
/// from normal wait observers so preserved yields never satisfy terminate.
enum CellResponseSender {
    Runtime(oneshot::Sender<RuntimeResponse>),
    ExecuteToPending(oneshot::Sender<ExecuteToPendingOutcome>),
    Terminate(oneshot::Sender<RuntimeResponse>),
}

/// Local addition (not vendored): only this actor-owned state decides whether
/// normal completion or termination won.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CellActorState {
    Running,
    Terminating(CellTerminalCause),
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

// Local addition (not vendored): send one yield boundary to the oldest normal
// observer, returning its content if that observer disappeared.
fn try_send_yield(
    cell_id: &CellId,
    content_items: Vec<FunctionCallOutputContentItem>,
    observers: &mut VecDeque<CellResponseSender>,
) -> Result<(), Vec<FunctionCallOutputContentItem>> {
    let Some(position) = observers
        .iter()
        .position(|observer| matches!(observer, CellResponseSender::Runtime(_)))
    else {
        return Err(content_items);
    };
    let Some(CellResponseSender::Runtime(response_tx)) = observers.remove(position) else {
        unreachable!("normal observer position must contain a normal observer");
    };
    match response_tx.send(RuntimeResponse::Yielded {
        cell_id: cell_id.clone(),
        content_items,
    }) {
        Ok(()) => Ok(()),
        Err(RuntimeResponse::Yielded { content_items, .. }) => Err(content_items),
        Err(_) => unreachable!("yield sender only sends yielded responses"),
    }
}

// Local addition (not vendored): a yield is retained even when there is no
// observer or its request future was dropped.
fn send_or_buffer_yield(
    cell_id: &CellId,
    content_items: &mut Vec<FunctionCallOutputContentItem>,
    observers: &mut VecDeque<CellResponseSender>,
    pending_yields: &mut VecDeque<Vec<FunctionCallOutputContentItem>>,
) {
    let boundary = std::mem::take(content_items);
    if let Err(boundary) = try_send_yield(cell_id, boundary, observers) {
        pending_yields.push_back(boundary);
    }
}

// Local addition (not vendored): a late normal wait consumes exactly one
// preserved yield boundary before registering for future output.
fn send_preserved_yield(
    cell_id: &CellId,
    response_tx: oneshot::Sender<RuntimeResponse>,
    pending_yields: &mut VecDeque<Vec<FunctionCallOutputContentItem>>,
) -> Option<oneshot::Sender<RuntimeResponse>> {
    let Some(content_items) = pending_yields.pop_front() else {
        return Some(response_tx);
    };
    if let Err(RuntimeResponse::Yielded { content_items, .. }) =
        response_tx.send(RuntimeResponse::Yielded {
            cell_id: cell_id.clone(),
            content_items,
        })
    {
        pending_yields.push_front(content_items);
    }
    None
}

// Local addition (not vendored): pending-mode observers are fulfilled only by
// a paused frontier or a terminal outcome, never by the normal yield surface.
fn send_pending_frontier(
    cell_id: &CellId,
    content_items: &mut Vec<FunctionCallOutputContentItem>,
    pending_tool_call_ids: &mut Vec<String>,
    observers: &mut VecDeque<CellResponseSender>,
) {
    let Some(position) = observers
        .iter()
        .position(|observer| matches!(observer, CellResponseSender::ExecuteToPending(_)))
    else {
        return;
    };
    let Some(CellResponseSender::ExecuteToPending(response_tx)) = observers.remove(position) else {
        unreachable!("pending observer position must contain a pending observer");
    };
    let _ = response_tx.send(ExecuteToPendingOutcome::Pending {
        cell_id: cell_id.clone(),
        content_items: std::mem::take(content_items),
        pending_tool_call_ids: std::mem::take(pending_tool_call_ids),
    });
}

// Local addition (not vendored): terminal delivery never consumes the retained
// terminal record. Failed yield delivery is restored before tombstoning.
fn send_claimed_terminal(
    cell_id: &CellId,
    terminal: &mut CellTerminal,
    response_tx: oneshot::Sender<RuntimeResponse>,
) {
    let was_claimed = terminal.terminal_claimed;
    let response = terminal.claim_response(cell_id);
    if response_tx.send(response).is_err() && !was_claimed {
        terminal.terminal_claimed = false;
    }
}

// Local addition (not vendored): pending-mode terminal delivery participates
// in the same single-claim transition as direct exec/wait delivery.
fn send_claimed_pending_terminal(
    cell_id: &CellId,
    terminal: &mut CellTerminal,
    response_tx: oneshot::Sender<ExecuteToPendingOutcome>,
) {
    let was_claimed = terminal.terminal_claimed;
    let response = ExecuteToPendingOutcome::Completed(terminal.claim_response(cell_id));
    if response_tx.send(response).is_err() && !was_claimed {
        terminal.terminal_claimed = false;
    }
}

// Local addition (not vendored): one successful observer performs the terminal
// claim; queued and late observers receive stable replies from that state.
fn send_terminal_observers(
    cell_id: &CellId,
    terminal: &mut CellTerminal,
    observers: &mut VecDeque<CellResponseSender>,
) {
    while let Some(observer) = observers.pop_front() {
        match observer {
            CellResponseSender::Runtime(response_tx) => {
                if let Some(content_items) = terminal.pending_yields.pop_front() {
                    if let Err(RuntimeResponse::Yielded { content_items, .. }) =
                        response_tx.send(RuntimeResponse::Yielded {
                            cell_id: cell_id.clone(),
                            content_items,
                        })
                    {
                        terminal.pending_yields.push_front(content_items);
                    }
                } else {
                    send_claimed_terminal(cell_id, terminal, response_tx);
                }
            }
            CellResponseSender::ExecuteToPending(response_tx) => {
                send_claimed_pending_terminal(cell_id, terminal, response_tx);
            }
            CellResponseSender::Terminate(response_tx) => {
                send_claimed_terminal(cell_id, terminal, response_tx);
            }
        }
    }
}

// Local addition (not vendored): isolate, runtime, delegated-call, and paused
// runtime cancellation all begin after the actor records termination as winner.
fn begin_termination(
    cancellation_token: &CancellationToken,
    runtime_tx: &std::sync::mpsc::Sender<RuntimeCommand>,
    runtime_control_tx: &std::sync::mpsc::Sender<RuntimeControlCommand>,
    pending_mode: PendingRuntimeMode,
    runtime_terminate_handle: &v8::IsolateHandle,
) {
    cancellation_token.cancel();
    let _ = runtime_tx.send(RuntimeCommand::Terminate);
    terminate_paused_runtime(runtime_control_tx, pending_mode);
    let _ = runtime_terminate_handle.terminate_execution();
}

/// Local addition (not vendored): one actor owns lifecycle transitions,
/// observers, shared-store publication, and terminal tombstone installation.
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
    let mut pending_yields = VecDeque::new();
    let mut observers = VecDeque::from([initial_response_tx]);
    let mut state = CellActorState::Running;
    let mut control_closed = false;
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

    let mut terminal = loop {
        tokio::select! {
            maybe_event = event_rx.recv() => {
                let Some(event) = maybe_event else {
                    break match state {
                        CellActorState::Running => CellTerminal::internal_failure(
                            std::mem::take(&mut content_items),
                            std::mem::take(&mut pending_yields),
                        ),
                        CellActorState::Terminating(cause) => CellTerminal::terminated(
                            cause,
                            std::mem::take(&mut content_items),
                            std::mem::take(&mut pending_yields),
                        ),
                    };
                };
                match event {
                    RuntimeEvent::Started => {
                        if state == CellActorState::Running {
                            yield_timer = initial_yield_time_ms.map(|initial_yield_time_ms| {
                                Box::pin(tokio::time::sleep(Duration::from_millis(initial_yield_time_ms)))
                            });
                        }
                    }
                    RuntimeEvent::Pending => {
                        if state == CellActorState::Running {
                            send_pending_frontier(
                                &cell_id,
                                &mut content_items,
                                &mut pending_tool_call_ids,
                                &mut observers,
                            );
                        }
                    }
                    RuntimeEvent::ContentItem(item) => {
                        content_items.push(item);
                    }
                    RuntimeEvent::YieldRequested => {
                        if state == CellActorState::Running {
                            yield_timer = None;
                            send_or_buffer_yield(
                                &cell_id,
                                &mut content_items,
                                &mut observers,
                                &mut pending_yields,
                            );
                        }
                    }
                    RuntimeEvent::Notify { call_id, text } => {
                        if state != CellActorState::Running {
                            continue;
                        }
                        let delegate = Arc::clone(&inner.delegate);
                        let cell_id = cell_id.clone();
                        // Local addition (not vendored): delegated notification
                        // cancellation descends from the cell token.
                        let cancellation_token = cancellation_token.child_token();
                        notification_tasks.spawn(async move {
                            tokio::select! {
                                result = delegate.notify(
                                    call_id,
                                    cell_id.clone(),
                                    text,
                                    cancellation_token.clone(),
                                ) => {
                                    if let Err(err) = result {
                                        warn!(
                                            "failed to deliver code mode notification for cell {cell_id}: {err}"
                                        );
                                    }
                                }
                                _ = cancellation_token.cancelled() => {}
                            }
                        });
                    }
                    RuntimeEvent::ToolCall {
                        id,
                        name,
                        kind,
                        input,
                    } => {
                        if state != CellActorState::Running {
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
                        // Local addition (not vendored): delegated tool-call
                        // cancellation descends from the cell token.
                        let cancellation_token = cancellation_token.child_token();
                        tool_call_tasks.spawn(async move {
                            let response = tokio::select! {
                                response = delegate.invoke_tool(tool_call, cancellation_token.clone()) => response,
                                _ = cancellation_token.cancelled() => return,
                            };
                            let command = match response {
                                Ok(result) => RuntimeCommand::ToolResponse { id, result },
                                Err(error_text) => RuntimeCommand::ToolError { id, error_text },
                            };
                            let _ = runtime_tx.send(command);
                        });
                    }
                    RuntimeEvent::Result {
                        stored_value_writes,
                        error_text,
                    } => {
                        yield_timer = None;
                        if state != CellActorState::Running {
                            // Termination already won. Wait for runtime closure
                            // and never publish the runtime's staged writes.
                            continue;
                        }
                        // Completion wins at this serialized event boundary.
                        drain_notification_tasks(&mut notification_tasks).await;
                        // Local addition (not vendored): staged store writes
                        // publish as one lock-held commit only on success.
                        if error_text.is_none() {
                            inner
                                .stored_values
                                .lock()
                                .await
                                .extend(stored_value_writes);
                        }
                        break CellTerminal::completed(
                            std::mem::take(&mut content_items),
                            error_text,
                            std::mem::take(&mut pending_yields),
                        );
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
                if let Some(Err(err)) = task_result
                    && !err.is_cancelled()
                {
                    warn!("code mode nested tool call task failed: {err}");
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
            maybe_command = async {
                if control_closed {
                    std::future::pending::<Option<CellControlCommand>>().await
                } else {
                    control_rx.recv().await
                }
            } => {
                let Some(command) = maybe_command else {
                    control_closed = true;
                    if state == CellActorState::Running {
                        state = CellActorState::Terminating(CellTerminalCause::SessionShutdown);
                        yield_timer = None;
                        begin_termination(
                            &cancellation_token,
                            &runtime_tx,
                            &runtime_control_tx,
                            pending_mode,
                            &runtime_terminate_handle,
                        );
                    }
                    continue;
                };
                match command {
                    CellControlCommand::Poll {
                        yield_time_ms,
                        response_tx: next_response_tx,
                    } => {
                        let Some(next_response_tx) = send_preserved_yield(
                            &cell_id,
                            next_response_tx,
                            &mut pending_yields,
                        ) else {
                            continue;
                        };
                        observers.push_back(CellResponseSender::Runtime(next_response_tx));
                        yield_window_ms = Some(yield_time_ms);
                        yield_timer = Some(Box::pin(tokio::time::sleep(Duration::from_millis(yield_time_ms))));
                        resume_paused_runtime(&runtime_control_tx, pending_mode);
                    }
                    CellControlCommand::PollToPending {
                        response_tx: next_response_tx,
                    } => {
                        observers.push_back(CellResponseSender::ExecuteToPending(next_response_tx));
                        yield_window_ms = None;
                        yield_timer = None;
                        resume_paused_runtime(&runtime_control_tx, pending_mode);
                    }
                    CellControlCommand::Terminate {
                        cause,
                        response_tx: next_response_tx,
                    } => {
                        if let Some(next_response_tx) = next_response_tx {
                            observers.push_back(CellResponseSender::Terminate(next_response_tx));
                        }
                        if state == CellActorState::Running {
                            state = CellActorState::Terminating(cause);
                            yield_timer = None;
                            begin_termination(
                                &cancellation_token,
                                &runtime_tx,
                                &runtime_control_tx,
                                pending_mode,
                                &runtime_terminate_handle,
                            );
                        }
                    }
                }
            }
            _ = cancellation_token.cancelled(), if state == CellActorState::Running => {
                // Only session-root cancellation can arrive before the actor
                // records another terminal winner.
                state = CellActorState::Terminating(CellTerminalCause::SessionShutdown);
                yield_timer = None;
                begin_termination(
                    &cancellation_token,
                    &runtime_tx,
                    &runtime_control_tx,
                    pending_mode,
                    &runtime_terminate_handle,
                );
            }
            _ = async {
                if let Some(yield_timer) = yield_timer.as_mut() {
                    yield_timer.await;
                } else {
                    std::future::pending::<()>().await;
                }
            } => {
                yield_timer = None;
                send_or_buffer_yield(
                    &cell_id,
                    &mut content_items,
                    &mut observers,
                    &mut pending_yields,
                );
            }
        }
    };

    let _ = runtime_tx.send(RuntimeCommand::Terminate);
    cancellation_token.cancel();
    drain_notification_tasks(&mut notification_tasks).await;
    terminate_paused_runtime(&runtime_control_tx, pending_mode);
    send_terminal_observers(&cell_id, &mut terminal, &mut observers);
    // Local addition (not vendored): install the tombstone before dropping the
    // actor command receiver, so racing late callers never see a false miss.
    inner
        .registry
        .lock()
        .await
        .retain_terminal(cell_id.clone(), terminal);
    inner.registry_changed.notify_waiters();
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
            registry: Mutex::new(super::CellRegistry::new()),
            registry_changed: tokio::sync::Notify::new(),
            delegate: Arc::new(NoopCodeModeSessionDelegate),
            session_cancellation: tokio_util::sync::CancellationToken::new(),
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

    /// Local addition (not vendored): captures the delegated-call token so the
    /// session-root cancellation hierarchy can be asserted directly.
    struct CancellationCaptureDelegate {
        captured_token: Mutex<Option<tokio_util::sync::CancellationToken>>,
        token_captured: tokio::sync::Notify,
    }

    impl CancellationCaptureDelegate {
        fn new() -> Self {
            Self {
                captured_token: Mutex::new(None),
                token_captured: tokio::sync::Notify::new(),
            }
        }

        async fn wait_for_token(&self) -> tokio_util::sync::CancellationToken {
            loop {
                let captured = self.token_captured.notified();
                if let Some(token) = self.captured_token.lock().await.clone() {
                    return token;
                }
                captured.await;
            }
        }
    }

    impl super::CodeModeSessionDelegate for CancellationCaptureDelegate {
        fn invoke_tool<'a>(
            &'a self,
            _invocation: crate::runtime::CodeModeNestedToolCall,
            cancellation_token: tokio_util::sync::CancellationToken,
        ) -> super::ToolInvocationFuture<'a> {
            Box::pin(async move {
                *self.captured_token.lock().await = Some(cancellation_token.clone());
                self.token_captured.notify_waiters();
                cancellation_token.cancelled().await;
                Err("cancelled".to_string())
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

    /// Local addition (not vendored): terminal output survives cancellation of
    /// the initial request future and remains stable for later duplicate calls.
    #[tokio::test]
    async fn dropped_initial_observer_preserves_terminal_output() {
        let service = CodeModeService::new();
        let started = service
            .execute(ExecuteRequest {
                source: r#"text("retained");"#.to_string(),
                yield_time_ms: Some(60_000),
                ..execute_request("")
            })
            .await
            .unwrap();
        let completed_cell_id = started.cell_id.clone();
        drop(started);

        let first_late_wait = tokio::time::timeout(
            Duration::from_secs(1),
            service.wait(WaitRequest {
                cell_id: completed_cell_id.clone(),
                yield_time_ms: 60_000,
            }),
        )
        .await
        .unwrap()
        .unwrap();
        let late_terminate = service.terminate(completed_cell_id.clone()).await.unwrap();
        let second_late_wait = service
            .wait(WaitRequest {
                cell_id: completed_cell_id,
                yield_time_ms: 60_000,
            })
            .await
            .unwrap();

        let expected = RuntimeResponse::Result {
            cell_id: cell_id("1"),
            content_items: vec![FunctionCallOutputContentItem::InputText {
                text: "retained".to_string(),
            }],
            error_text: None,
        };
        assert_eq!(first_late_wait, WaitOutcome::LiveCell(expected));
        assert_eq!(late_terminate, first_late_wait);
        assert_eq!(second_late_wait, first_late_wait);
    }

    /// Local addition (not vendored): a dropped initial observer cannot erase
    /// the first explicit yield boundary even when execution then completes.
    #[tokio::test]
    async fn dropped_initial_observer_preserves_yield_before_terminal() {
        let service = CodeModeService::new();
        let started = service
            .execute(ExecuteRequest {
                source: r#"text("before"); yield_control(); text("after");"#.to_string(),
                yield_time_ms: Some(60_000),
                ..execute_request("")
            })
            .await
            .unwrap();
        let completed_cell_id = started.cell_id.clone();
        drop(started);

        let yielded = service
            .wait(WaitRequest {
                cell_id: completed_cell_id.clone(),
                yield_time_ms: 60_000,
            })
            .await
            .unwrap();
        let completed = service
            .wait(WaitRequest {
                cell_id: completed_cell_id.clone(),
                yield_time_ms: 60_000,
            })
            .await
            .unwrap();
        let repeated = service
            .wait(WaitRequest {
                cell_id: completed_cell_id,
                yield_time_ms: 60_000,
            })
            .await
            .unwrap();

        assert_eq!(
            yielded,
            WaitOutcome::LiveCell(RuntimeResponse::Yielded {
                cell_id: cell_id("1"),
                content_items: vec![FunctionCallOutputContentItem::InputText {
                    text: "before".to_string(),
                }],
            })
        );
        let expected_terminal = WaitOutcome::LiveCell(RuntimeResponse::Result {
            cell_id: cell_id("1"),
            content_items: vec![FunctionCallOutputContentItem::InputText {
                text: "after".to_string(),
            }],
            error_text: None,
        });
        assert_eq!(completed, expected_terminal);
        assert_eq!(repeated, completed);
    }

    /// Local addition (not vendored): cancelling a registered wait future does
    /// not cancel the cell or discard its eventual terminal output.
    #[tokio::test]
    async fn dropped_wait_observer_does_not_cancel_cell_or_output() {
        let service = Arc::new(CodeModeService::with_delegate(Arc::new(SlowToolDelegate {
            delay: Duration::from_millis(100),
        })));
        let started = service
            .execute(ExecuteRequest {
                enabled_tools: vec![slow_tool_definition()],
                source: "const value = await tools.slow({}); text(String(value));".to_string(),
                ..execute_request("")
            })
            .await
            .unwrap();
        let running_cell_id = started.cell_id.clone();
        assert!(matches!(
            started.initial_response().await.unwrap(),
            RuntimeResponse::Yielded { .. }
        ));

        let dropped_wait = tokio::spawn({
            let service = Arc::clone(&service);
            let running_cell_id = running_cell_id.clone();
            async move {
                service
                    .wait(WaitRequest {
                        cell_id: running_cell_id,
                        yield_time_ms: 1_000,
                    })
                    .await
            }
        });
        tokio::time::sleep(Duration::from_millis(10)).await;
        dropped_wait.abort();
        let _ = dropped_wait.await;

        let retained = tokio::time::timeout(
            Duration::from_secs(1),
            service.wait(WaitRequest {
                cell_id: running_cell_id,
                yield_time_ms: 1_000,
            }),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(
            retained,
            WaitOutcome::LiveCell(RuntimeResponse::Result {
                cell_id: cell_id("1"),
                content_items: vec![FunctionCallOutputContentItem::InputText {
                    text: "slow-result".to_string(),
                }],
                error_text: None,
            })
        );
    }

    /// Local addition (not vendored): staged store writes publish together only
    /// after a successful terminal result.
    #[tokio::test]
    async fn store_writes_publish_atomically_at_successful_completion() {
        let service = CodeModeService::new();
        let started = service
            .execute(ExecuteRequest {
                source: r#"
store("first", 1);
store("second", 2);
await new Promise((resolve) => setTimeout(resolve, 50));
text("done");
"#
                .to_string(),
                ..execute_request("")
            })
            .await
            .unwrap();
        let running_cell_id = started.cell_id.clone();
        assert_eq!(
            started.initial_response().await.unwrap(),
            RuntimeResponse::Yielded {
                cell_id: running_cell_id.clone(),
                content_items: Vec::new(),
            }
        );
        {
            let stored_values = service.inner.stored_values.lock().await;
            assert!(!stored_values.contains_key("first"));
            assert!(!stored_values.contains_key("second"));
        }

        let completed = service
            .wait(WaitRequest {
                cell_id: running_cell_id,
                yield_time_ms: 1_000,
            })
            .await
            .unwrap();
        assert!(matches!(
            completed,
            WaitOutcome::LiveCell(RuntimeResponse::Result {
                error_text: None,
                ..
            })
        ));
        let stored_values = service.inner.stored_values.lock().await;
        assert_eq!(stored_values.get("first"), Some(&serde_json::json!(1)));
        assert_eq!(stored_values.get("second"), Some(&serde_json::json!(2)));
    }

    /// Local addition (not vendored): runtime errors discard all staged store
    /// writes rather than publishing partial state.
    #[tokio::test]
    async fn failed_cell_does_not_publish_store_writes() {
        let service = CodeModeService::new();
        let response = execute(
            &service,
            ExecuteRequest {
                source: r#"store("failed", "hidden"); throw new Error("boom");"#.to_string(),
                yield_time_ms: Some(60_000),
                ..execute_request("")
            },
        )
        .await;

        assert!(matches!(
            response,
            RuntimeResponse::Result {
                error_text: Some(_),
                ..
            }
        ));
        assert!(
            !service
                .inner
                .stored_values
                .lock()
                .await
                .contains_key("failed")
        );
    }

    /// Local addition (not vendored): explicit termination wins before runtime
    /// completion and therefore cannot publish the runtime's staged writes.
    #[tokio::test]
    async fn terminated_cell_does_not_publish_store_writes() {
        let service = CodeModeService::new();
        let started = service
            .execute(ExecuteRequest {
                source: r#"
store("terminated", "hidden");
await new Promise(() => {});
"#
                .to_string(),
                ..execute_request("")
            })
            .await
            .unwrap();
        let running_cell_id = started.cell_id.clone();
        assert!(matches!(
            started.initial_response().await.unwrap(),
            RuntimeResponse::Yielded { .. }
        ));

        let terminated = service.terminate(running_cell_id.clone()).await.unwrap();
        let repeated_terminate = service.terminate(running_cell_id.clone()).await.unwrap();
        let late_wait = service
            .wait(WaitRequest {
                cell_id: running_cell_id,
                yield_time_ms: 1,
            })
            .await
            .unwrap();

        assert!(matches!(
            terminated,
            WaitOutcome::LiveCell(RuntimeResponse::Terminated { .. })
        ));
        assert_eq!(repeated_terminate, terminated);
        assert_eq!(late_wait, terminated);
        assert!(
            !service
                .inner
                .stored_values
                .lock()
                .await
                .contains_key("terminated")
        );
    }

    /// Local addition (not vendored): repeated real runtime races prove that
    /// completion and termination publish one stable winner with matching store
    /// visibility.
    #[tokio::test]
    async fn completion_and_termination_race_has_one_stable_winner() {
        let service = CodeModeService::new();

        for iteration in 0..24 {
            let key = format!("winner-{iteration}");
            let started = service
                .execute(ExecuteRequest {
                    source: format!(r#"store("{key}", true);"#),
                    yield_time_ms: Some(60_000),
                    ..execute_request("")
                })
                .await
                .unwrap();
            let racing_cell_id = started.cell_id.clone();
            let (initial, terminate) = tokio::join!(
                started.initial_response(),
                service.terminate(racing_cell_id.clone())
            );
            let initial = initial.unwrap();
            let terminate = terminate.unwrap();
            let WaitOutcome::LiveCell(terminate) = terminate else {
                panic!("racing cell must retain a terminal outcome");
            };
            assert_eq!(terminate, initial);

            let late = service
                .wait(WaitRequest {
                    cell_id: racing_cell_id,
                    yield_time_ms: 1,
                })
                .await
                .unwrap();
            assert_eq!(late, WaitOutcome::LiveCell(initial));

            let was_committed = service.inner.stored_values.lock().await.contains_key(&key);
            match terminate {
                RuntimeResponse::Result {
                    error_text: None, ..
                } => assert!(was_committed),
                RuntimeResponse::Terminated { .. } => assert!(!was_committed),
                other => panic!("unexpected race outcome: {other:?}"),
            }
        }
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

    /// Local addition (not vendored): session-root cancellation reaches the
    /// cell token and its delegated-call child before shutdown completes.
    #[tokio::test]
    async fn shutdown_cancels_delegated_call_child_token() {
        let delegate = Arc::new(CancellationCaptureDelegate::new());
        let service = CodeModeService::with_delegate(delegate.clone());
        let started = service
            .execute(ExecuteRequest {
                enabled_tools: vec![slow_tool_definition()],
                source: "await tools.slow({});".to_string(),
                ..execute_request("")
            })
            .await
            .unwrap();
        assert!(matches!(
            started.initial_response().await.unwrap(),
            RuntimeResponse::Yielded { .. }
        ));
        let delegated_token =
            tokio::time::timeout(Duration::from_secs(1), delegate.wait_for_token())
                .await
                .unwrap();

        service.shutdown().await.unwrap();

        assert!(delegated_token.is_cancelled());
    }

    /// Local addition (not vendored): a non-cooperating reserved child cannot
    /// make the library shutdown future wait forever.
    #[tokio::test]
    async fn shutdown_deadline_is_bounded() {
        let service = CodeModeService::new();
        let (control_tx, _control_rx) = mpsc::unbounded_channel();
        let handle = super::CellHandle {
            control_tx,
            runtime_tx: Arc::new(std::sync::OnceLock::new()),
        };
        service
            .inner
            .registry
            .lock()
            .await
            .reserve(cell_id("stuck"), handle)
            .unwrap();

        let error = service
            .shutdown_with_timeout(Duration::from_millis(20))
            .await
            .unwrap_err();

        assert_eq!(
            error,
            "code mode session shutdown timed out with 1 active cell(s)"
        );
        let mut registry = service.inner.registry.lock().await;
        assert!(!registry.accepting);
        registry.active.clear();
    }

    /// Local addition (not vendored): count-bounded tombstones evict the oldest
    /// terminal identity while retaining the newest stable responses.
    #[tokio::test]
    async fn tombstone_retention_is_bounded() {
        let mut registry = super::CellRegistry::new();
        for index in 0..=super::MAX_CELL_TOMBSTONES {
            registry.retain_terminal(
                cell_id(&index.to_string()),
                super::CellTerminal::completed(Vec::new(), None, std::collections::VecDeque::new()),
            );
        }

        assert_eq!(registry.tombstones.len(), super::MAX_CELL_TOMBSTONES);
        assert_eq!(registry.tombstone_order.len(), super::MAX_CELL_TOMBSTONES);
        assert!(!registry.tombstones.contains_key(&cell_id("0")));
        assert!(registry.tombstones.contains_key(&cell_id("1")));
        assert!(
            registry
                .tombstones
                .contains_key(&cell_id(&super::MAX_CELL_TOMBSTONES.to_string()))
        );
    }

    #[tokio::test]
    async fn start_cell_rejects_new_cell_after_shutdown_begins() {
        let service = CodeModeService::new();
        service.shutdown().await.unwrap();
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
        assert!(service.inner.registry.lock().await.active.is_empty());
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

        assert_eq!(
            termination,
            WaitOutcome::LiveCell(RuntimeResponse::Terminated {
                cell_id: cell_id("1"),
                content_items: Vec::new(),
            })
        );
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
            .registry
            .lock()
            .await
            .active
            .get(&cell_id("1"))
            .unwrap()
            .runtime_tx()
            .unwrap();
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

        assert_eq!(
            termination,
            WaitOutcome::LiveCell(RuntimeResponse::Terminated {
                cell_id: cell_id("1"),
                content_items: Vec::new(),
            })
        );
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
            .registry
            .lock()
            .await
            .active
            .get(&cell_id("1"))
            .unwrap()
            .runtime_tx()
            .unwrap();
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
            .registry
            .lock()
            .await
            .active
            .get(&cell_id("1"))
            .unwrap()
            .runtime_tx()
            .unwrap();
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
                cause: super::CellTerminalCause::ExplicitTermination,
                response_tx: Some(terminate_response_tx),
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
