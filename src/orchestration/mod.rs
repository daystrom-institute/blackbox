pub mod account_probes;
pub mod agents;
pub mod allocator;
pub mod atoms;
pub mod brofile;
pub mod executor;
pub mod fleetd_client;
pub mod http_fetch;
pub mod mcp;
pub mod providers;
pub mod resume_lease;
pub mod supervision;
pub mod tail;
pub mod team;

use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::Write as _;
use std::ops::{Deref, DerefMut, Index, RangeFrom};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};

use parking_lot::{Mutex, RwLock};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::Notify;

use crate::managed_worktrees;
use crate::transcripts::adapters::TranscriptAdapterRegistry;
use crate::transcripts::types::{
    TranscriptCursor, TranscriptLocation, TranscriptSource, TranscriptStorage,
};
use providers::dispatch_prelude::*;
use providers::{EventSink, Provider, Usage};
use supervision::SupervisionState;

const BLACKBOX_SERVICE_ENV_VARS: &[&str] = &[
    "BBOX_PORT",
    "BLACKBOX_MCP_NAME",
    "BLACKBOX_MCP_URL",
    "BLACKBOX_STATE_DIR",
    "BLACKBOX_KNOWLEDGE_PATH",
    "BLACKBOX_GAPS_PATH",
    "BLACKBOX_THREADS_PATH",
    "BLACKBOX_NOTES_PATH",
    "BLACKBOX_GLOBAL_CLAUDE_MD",
    "BLACKBOX_GLOBAL_CODEX_MD",
    "BLACKBOX_GLOBAL_GEMINI_MD",
    "BLACKBOX_BACKUP_DIR",
    "BLACKBOX_EXECUTOR",
    "BLACKBOX_FLEETD_ENDPOINT",
    "BLACKBOX_FLEETD_TOKEN_FILE",
    "BLACKBOX_FLEETD_WORKER_HOME",
    "BLACKBOX_FLEETD_WORKER_BRO_HOME",
    "BRO_HOME",
    "TRANSCRIPT_SEARCH_ROOTS",
    "TRANSCRIPT_SEARCH_CODEX_ROOT",
    "TRANSCRIPT_SEARCH_INDEX_PATH",
];

const HARNESS_SPAWN_SCRUB_ENV: &str = "BRO_HARNESS_SPAWN_SCRUB";

/// The process-wide executor every harness dispatch goes through.
///
/// Installed once at daemon startup from `daemon.executor` (default `fleetd`).
/// Left uninstalled, this falls back to [`executor::LocalExecutor`], which is
/// what unit tests and library consumers get: they have no daemon startup, and
/// must never dial (or auto-start) a real supervisor as a side effect of
/// calling a spawn helper.
fn harness_executor() -> &'static Arc<dyn executor::HarnessExecutor> {
    harness_executor_storage().get_or_init(|| Arc::new(executor::LocalExecutor))
}

/// Select the executor for this daemon process and wire up the state
/// re-adoption needs. Call once, early in daemon startup, before any dispatch.
///
/// Returns whether the selection took effect: a second call is a no-op, since
/// swapping executors under live sessions would orphan whatever the first one
/// is supervising.
#[cfg(test)]
pub fn install_harness_executor(
    kind: bbox_config::config::ExecutorKind,
    store_dir: std::path::PathBuf,
    task_store: Arc<RwLock<TaskStore>>,
    tail_tx: tokio::sync::broadcast::Sender<tail::TailEvent>,
    system_events: Option<crate::system_events::SharedEventHub>,
    workspace_binding_authority: Option<Arc<dyn WorkspaceBindingAuthority>>,
) -> bool {
    let fleetd_config = fleetd_client::FleetdConfig::in_state_dir(&store_dir);
    install_harness_executor_with_config(
        kind,
        fleetd_config,
        store_dir,
        task_store,
        tail_tx,
        system_events,
        workspace_binding_authority,
    )
}

/// Daemon-startup variant that resolves the explicit remote fleetd surface
/// before installing the process-wide executor. Invalid or partial remote
/// configuration fails startup instead of producing a daemon that looks
/// healthy until its first dispatch.
pub fn install_configured_harness_executor(
    kind: bbox_config::config::ExecutorKind,
    store_dir: std::path::PathBuf,
    fleetd_endpoint: Option<&str>,
    fleetd_token_file: Option<&std::path::Path>,
    fleetd_worker_home: Option<&std::path::Path>,
    fleetd_worker_bro_home: Option<&std::path::Path>,
    task_store: Arc<RwLock<TaskStore>>,
    tail_tx: tokio::sync::broadcast::Sender<tail::TailEvent>,
    system_events: Option<crate::system_events::SharedEventHub>,
    workspace_binding_authority: Option<Arc<dyn WorkspaceBindingAuthority>>,
) -> anyhow::Result<bool> {
    let fleetd_config = match kind {
        bbox_config::config::ExecutorKind::Local => {
            fleetd_client::FleetdConfig::in_state_dir(&store_dir)
        }
        bbox_config::config::ExecutorKind::Fleetd => fleetd_client::FleetdConfig::resolve(
            &store_dir,
            fleetd_endpoint,
            fleetd_token_file,
            fleetd_worker_home,
            fleetd_worker_bro_home,
        )?,
    };
    Ok(install_harness_executor_with_config(
        kind,
        fleetd_config,
        store_dir,
        task_store,
        tail_tx,
        system_events,
        workspace_binding_authority,
    ))
}

fn install_harness_executor_with_config(
    kind: bbox_config::config::ExecutorKind,
    fleetd_config: fleetd_client::FleetdConfig,
    store_dir: std::path::PathBuf,
    task_store: Arc<RwLock<TaskStore>>,
    tail_tx: tokio::sync::broadcast::Sender<tail::TailEvent>,
    system_events: Option<crate::system_events::SharedEventHub>,
    workspace_binding_authority: Option<Arc<dyn WorkspaceBindingAuthority>>,
) -> bool {
    let _ = readoption_env().set(ReadoptionEnv {
        store_dir: store_dir.clone(),
        task_store,
        tail_tx,
        system_events,
        workspace_binding_authority,
    });
    let executor: Arc<dyn executor::HarnessExecutor> = match kind {
        bbox_config::config::ExecutorKind::Local => {
            tracing::info!(
                "harness executor: local (workers are daemon children; a daemon restart \
                 drops live sessions)"
            );
            Arc::new(executor::LocalExecutor)
        }
        bbox_config::config::ExecutorKind::Fleetd => {
            tracing::info!(
                endpoint = ?fleetd_config.endpoint,
                "harness executor: fleetd (workers survive a daemon restart)"
            );
            Arc::new(fleetd_client::FleetdExecutor::new(fleetd_config))
        }
    };
    let installed = harness_executor_storage().set(executor).is_ok();
    if !installed {
        tracing::warn!("harness executor already installed; ignoring the second selection");
    }
    installed
}

/// The single cell behind both the reader and the installer. One static on
/// purpose: if these were separate, a dispatch that ran before install would
/// pin the local default in one cell while the installer wrote the other, and
/// the daemon would silently keep spawning its own children.
fn harness_executor_storage() -> &'static OnceLock<Arc<dyn executor::HarnessExecutor>> {
    static EXECUTOR: OnceLock<Arc<dyn executor::HarnessExecutor>> = OnceLock::new();
    &EXECUTOR
}

fn harness_worker_locality() -> Option<executor::WorkerLocality> {
    harness_executor().worker_locality().cloned()
}

fn harness_provider_binary_location() -> executor::ProviderBinaryLocation {
    harness_executor().provider_binary_location()
}

/// Daemon-side state a re-adopted session needs to be reattached to its task.
struct ReadoptionEnv {
    store_dir: std::path::PathBuf,
    task_store: Arc<RwLock<TaskStore>>,
    tail_tx: tokio::sync::broadcast::Sender<tail::TailEvent>,
    system_events: Option<crate::system_events::SharedEventHub>,
    workspace_binding_authority: Option<Arc<dyn WorkspaceBindingAuthority>>,
}

/// Daemon-owned authority for the path-free capability a managed harness uses
/// to select its exact workspace over the self-MCP transport.
pub(crate) trait WorkspaceBindingAuthority: Send + Sync {
    /// Durable scopes the worker host is allowed to prove for a managed cwd.
    /// This keeps catalog policy daemon-side while permitting remote
    /// filesystem inspection.
    fn candidate_scopes(&self) -> anyhow::Result<Vec<bro_protocol::WorkerWorkspaceScope>>;

    fn mint(
        &self,
        task_id: &str,
        session_id: &str,
        identity: &bro_protocol::WorkerWorkspaceIdentity,
    ) -> anyhow::Result<MintedWorkspaceBinding>;

    fn restore(
        &self,
        task_id: &str,
        session_id: &str,
        identity: &bro_protocol::WorkerWorkspaceIdentity,
        token: &bro_protocol::WorkspaceBindingToken,
    ) -> anyhow::Result<()>;

    fn revoke_task(&self, task_id: &str);
}

pub(crate) struct MintedWorkspaceBinding {
    pub(crate) token: bro_protocol::WorkspaceBindingToken,
    pub(crate) scope: bbox_corpus_core::identity::PublishedScope,
}

fn readoption_env() -> &'static OnceLock<ReadoptionEnv> {
    static ENV: OnceLock<ReadoptionEnv> = OnceLock::new();
    &ENV
}

/// One session fleetd is still holding, with the plumbing the executor client
/// built for it.
pub struct ReadoptedSession {
    pub session_id: String,
    pub task_id: String,
    pub workspace_id: Option<bro_core::WorkspaceId>,
    pub workspace_scope: Option<bro_protocol::WorkerWorkspaceScope>,
    pub workspace_binding_token: Option<bro_protocol::WorkspaceBindingToken>,
    pub pid: Option<u32>,
    pub state: bro_protocol::SessionState,
    pub control: tokio::sync::mpsc::UnboundedSender<Value>,
    pub killer: Arc<executor::WorkerKill>,
    pub events: tokio::sync::mpsc::UnboundedReceiver<String>,
    pub outcome: tokio::sync::oneshot::Receiver<executor::WorkerOutcome>,
}

/// Reattach a session that outlived this daemon instance.
///
/// Returns the task's durable ingest cursor, which the caller replays from.
/// `None` means "not ours": either the task store never knew this task (a TTL
/// reap, a wiped store, another daemon's session) or it is already terminal, in
/// which case there is nothing left to publish. The caller leaves those alone
/// rather than killing them.
///
/// A task the previous daemon left `Running` was flipped to `Failed`
/// (`recoverable: true`) by `TaskStore::load` unless owner-managed. Re-adoption puts it back to
/// `Running`, because the child genuinely never died: the daemon did.
pub fn readopt_harness_session(session: ReadoptedSession) -> Option<u64> {
    let ReadoptedSession {
        session_id,
        task_id,
        workspace_id,
        workspace_scope,
        workspace_binding_token,
        pid,
        state,
        control,
        killer,
        events,
        outcome,
    } = session;
    let env = readoption_env().get()?;
    let task = env.task_store.read().get(&task_id)?;
    {
        let inner = task.inner.lock();
        if inner.workflow_owned
            || workflow_owned_for_origin(inner.origin)
            || inner.provider == Provider::Workflow
        {
            tracing::warn!(%task_id, "owner-managed task is not eligible for ordinary harness re-adoption");
            return None;
        }
    }

    match (&workspace_id, &workspace_scope, &workspace_binding_token) {
        (Some(workspace_id), Some(workspace_scope), Some(token)) => {
            let Some(authority) = env.workspace_binding_authority.as_ref() else {
                tracing::warn!(
                    session_id = %session_id,
                    task_id = %task_id,
                    workspace_id = %workspace_id,
                    "refusing workspace-bound session re-adoption without binding authority"
                );
                return None;
            };
            let identity = bro_protocol::WorkerWorkspaceIdentity {
                workspace_id: workspace_id.clone(),
                scope: workspace_scope.clone(),
            };
            if let Err(error) = authority.restore(&task_id, &session_id, &identity, token) {
                tracing::warn!(
                    session_id = %session_id,
                    task_id = %task_id,
                    workspace_id = %workspace_id,
                    error = %error,
                    "refusing mismatched workspace-bound session re-adoption"
                );
                return None;
            }
        }
        (None, _, Some(_)) | (_, None, Some(_)) => {
            tracing::warn!(
                session_id = %session_id,
                task_id = %task_id,
                "refusing workspace binding token without complete workspace identity"
            );
            return None;
        }
        _ => {}
    }

    let (provider, cursor) = {
        let mut inner = task.inner.lock();
        let already_terminal = inner.status != TaskStatus::Running && !inner.recoverable;
        if already_terminal {
            return None;
        }
        if state == bro_protocol::SessionState::Running {
            inner.status = TaskStatus::Running;
            inner.completed_at = None;
            inner.recoverable = false;
            // The restart notice `TaskStore::load` appended is now wrong: the
            // session was never lost, so it must not be left in the record for
            // an agent to read as a failure.
            strip_restart_notice(&mut inner.stderr);
        }
        (inner.provider, inner.harness_ingest_seq)
    };
    *task.child_id.lock() = pid;

    harness_killers().write().insert(task_id.clone(), killer);
    harness_controls().write().insert(task_id.clone(), control);
    task.emit_roster_updated();

    tracing::info!(
        session_id = %session_id,
        task_id = %task_id,
        workspace_id = workspace_id.as_ref().map(bro_core::WorkspaceId::as_str),
        workspace_bound = workspace_binding_token.is_some(),
        from_seq = cursor,
        "reattached a surviving worker session to its task"
    );

    let ingest_join = spawn_harness_ingest_loop(
        task.clone(),
        provider,
        task_id.clone(),
        env.store_dir.clone(),
        harness_worker_locality().map(|_| {
            env.store_dir
                .join("harness-sessions")
                .join(format!("{session_id}.events.jsonl"))
        }),
        env.tail_tx.clone(),
        env.system_events.clone(),
        events,
    );
    spawn_harness_terminal_waiter(
        task,
        task_id,
        env.store_dir.clone(),
        env.task_store.clone(),
        env.tail_tx.clone(),
        env.system_events.clone(),
        outcome,
        ingest_join,
    );
    Some(cursor)
}

/// The exact notice `TaskStore::load` appends when it flips a running task to
/// failed at startup. Removed on re-adoption, since the premise (the provider
/// session is only recoverable by a manual `bro_resume`) turned out false.
const RESTART_NOTICE_PREFIX: &str = "\n[blackbox] server restarted while task was running.";

fn strip_restart_notice(stderr: &mut String) {
    if let Some(start) = stderr.find(RESTART_NOTICE_PREFIX) {
        stderr.truncate(start);
    }
}

fn harness_controls() -> &'static RwLock<HashMap<String, tokio::sync::mpsc::UnboundedSender<Value>>>
{
    static CONTROLS: OnceLock<RwLock<HashMap<String, tokio::sync::mpsc::UnboundedSender<Value>>>> =
        OnceLock::new();
    CONTROLS.get_or_init(|| RwLock::new(HashMap::new()))
}

/// Task-id -> idempotent kill switch for executor-backed harness workers.
/// Parallel to [`harness_controls`]: keyed the same way, populated at spawn,
/// removed by the terminal waiter. `cancel_task` consults this so the kill goes
/// through the worker handle instead of a raw `child_id` PID take. Non-harness
/// / one-shot tasks are absent here and fall back to `child_id`.
fn harness_killers() -> &'static RwLock<HashMap<String, Arc<executor::WorkerKill>>> {
    static KILLERS: OnceLock<RwLock<HashMap<String, Arc<executor::WorkerKill>>>> = OnceLock::new();
    KILLERS.get_or_init(|| RwLock::new(HashMap::new()))
}

fn harness_user_input(text: String) -> Value {
    serde_json::json!({
        "type": "user",
        "message": {
            "role": "user",
            "content": [{"type": "text", "text": text}],
        },
    })
}

fn harness_control_input(subtype: &str, request_id: String, fields: Value) -> Value {
    let mut raw = match fields {
        Value::Object(object) => Value::Object(object),
        _ => serde_json::json!({}),
    };
    let object = raw.as_object_mut().expect("normalized JSON object");
    object.insert(
        "type".to_string(),
        Value::String("control_request".to_string()),
    );
    object.insert("subtype".to_string(), Value::String(subtype.to_string()));
    object.insert("request_id".to_string(), Value::String(request_id));
    raw
}

// ── Task-store persist actor (control-plane starvation fix) ─────────────────
//
// `tasks.json` writes used to run as a synchronous `std::fs::write` of the WHOLE
// store directly on a tokio worker thread, while holding the store read guard
// (`task_store.read().persist(dir)`). Under fleet load that blocked async
// workers — starving unrelated control/knowledge-plane handlers (`bbox_note`,
// MCP `bro_status`) — and blocked store writers for the whole write.
//
// The actor owns a dedicated OS thread (NOT a tokio task, so it never consumes
// the async worker pool). Hot paths call `request_persist` — a non-blocking
// signal that coalesces a burst into a single snapshot+write. The snapshot is
// taken under a brief read lock ON the persist thread; the blocking file write
// happens entirely off the runtime.

/// Ack channel an explicit flush attaches so it can block until durable.
type PersistAck = Option<std::sync::mpsc::Sender<()>>;

struct TaskPersister {
    store: Arc<RwLock<TaskStore>>,
    store_dir: PathBuf,
    tx: std::sync::mpsc::Sender<PersistAck>,
}

impl TaskPersister {
    fn spawn(store: Arc<RwLock<TaskStore>>, store_dir: PathBuf) -> Self {
        let (tx, rx) = std::sync::mpsc::channel::<PersistAck>();
        let store_w = store.clone();
        let dir_w = store_dir.clone();
        let spawned = std::thread::Builder::new()
            .name("task-persist".to_string())
            .spawn(move || {
                while let Ok(first) = rx.recv() {
                    // Coalesce: drain everything already queued so a burst of N
                    // requests collapses to ONE snapshot+write, collecting any
                    // acks waiting on durability.
                    let mut acks: Vec<std::sync::mpsc::Sender<()>> = Vec::new();
                    if let Some(a) = first {
                        acks.push(a);
                    }
                    while let Ok(next) = rx.try_recv() {
                        if let Some(a) = next {
                            acks.push(a);
                        }
                    }
                    let data = store_w.read().serialize_snapshot(MAX_PERSISTED_EVENTS);
                    if let Some(data) = data {
                        TaskStore::write_snapshot_blocking(&dir_w, &data);
                    }
                    for a in acks {
                        let _ = a.send(());
                    }
                }
            });
        if spawned.is_err() {
            tracing::error!(
                "failed to spawn task-persist thread; persistence falls back to synchronous writes"
            );
        }
        Self {
            store,
            store_dir,
            tx,
        }
    }

    /// Non-blocking persist request, coalesced by the actor. If the actor thread
    /// is gone, fall back to a direct synchronous write so state is never lost.
    fn request(&self) {
        if self.tx.send(None).is_err() {
            self.write_now();
        }
    }

    /// Block until the current state is durable on disk (shutdown path).
    fn flush_blocking(&self) {
        let (ack_tx, ack_rx) = std::sync::mpsc::channel();
        if self.tx.send(Some(ack_tx)).is_ok() && ack_rx.recv().is_ok() {
            return;
        }
        self.write_now();
    }

    fn write_now(&self) {
        if let Some(data) = self.store.read().serialize_snapshot(MAX_PERSISTED_EVENTS) {
            TaskStore::write_snapshot_blocking(&self.store_dir, &data);
        }
    }
}

fn task_persister() -> &'static OnceLock<TaskPersister> {
    static P: OnceLock<TaskPersister> = OnceLock::new();
    &P
}

/// Initialise the global task-store persist actor. Idempotent; called once when
/// the production `SharedState` is built. Tests do NOT init it (so each test's
/// `request_persist` falls back to a synchronous write of its OWN per-test store
/// rather than routing to a global bound to a different store).
pub(crate) fn init_task_persister(store: Arc<RwLock<TaskStore>>, store_dir: PathBuf) {
    let _ = task_persister().set(TaskPersister::spawn(store, store_dir));
}

/// Request a coalesced, off-worker persist of the task store. Non-blocking on
/// the hot path. Before the actor is initialised (unit tests, early startup) it
/// falls back to a synchronous write of the passed store so on-disk state is
/// never silently dropped.
pub(crate) fn request_persist(store: &RwLock<TaskStore>, store_dir: &std::path::Path) {
    match task_persister().get() {
        Some(p) => p.request(),
        None => {
            if let Some(data) = store.read().serialize_snapshot(MAX_PERSISTED_EVENTS) {
                TaskStore::write_snapshot_blocking(store_dir, &data);
            }
        }
    }
}

/// Flush the task store and block until durable (shutdown). Synchronous fallback
/// when the actor was never initialised.
pub(crate) fn flush_persist_blocking(store: &RwLock<TaskStore>, store_dir: &std::path::Path) {
    match task_persister().get() {
        Some(p) => p.flush_blocking(),
        None => {
            if let Some(data) = store.read().serialize_snapshot(MAX_PERSISTED_EVENTS) {
                TaskStore::write_snapshot_blocking(store_dir, &data);
            }
        }
    }
}

/// Translate the shared session-control contract into the harness stdin NDJSON
/// protocol. Every variant maps to a genuinely handled child-process path.
pub fn apply_session_command(
    task_id: &str,
    command: bro_protocol::SessionCommand,
) -> Result<(), String> {
    use bro_protocol::SessionCommand;

    let tx = harness_controls()
        .read()
        .get(task_id)
        .cloned()
        .ok_or_else(|| format!("task {task_id} has no live harness control channel"))?;

    let input = match command {
        SessionCommand::UserTurn { text } => harness_user_input(text),
        SessionCommand::Interrupt => harness_control_input(
            "interrupt",
            uuid::Uuid::new_v4().to_string(),
            serde_json::json!({}),
        ),
        SessionCommand::SetModel { model } => harness_control_input(
            "set_model",
            uuid::Uuid::new_v4().to_string(),
            serde_json::json!({"model": model}),
        ),
        // `/compact` is an in-stream slash command, not a control_request.
        SessionCommand::Compact => harness_user_input("/compact".to_string()),
    };

    tx.send(input)
        .map_err(|_| format!("task {task_id} harness control channel is closed"))
}

pub fn steer_harness_task(task_id: &str, prompt: String) -> Result<(), String> {
    apply_session_command(
        task_id,
        bro_protocol::SessionCommand::UserTurn { text: prompt },
    )
}

pub fn interrupt_harness_task(task_id: &str, redirect: Option<String>) -> Result<(), String> {
    match redirect {
        // Interrupt-and-redirect (§8 op 3): the prompt rides the interrupt
        // control's raw so the harness dequeues it immediately on cancel. This
        // payload shape has no SessionCommand variant, so it stays inline.
        Some(prompt) => {
            let tx = harness_controls()
                .read()
                .get(task_id)
                .cloned()
                .ok_or_else(|| format!("task {task_id} has no live harness control channel"))?;
            tx.send(harness_control_input(
                "interrupt",
                uuid::Uuid::new_v4().to_string(),
                serde_json::json!({"prompt": prompt}),
            ))
            .map_err(|_| format!("task {task_id} harness control channel is closed"))
        }
        None => apply_session_command(task_id, bro_protocol::SessionCommand::Interrupt),
    }
}

// ---------------------------------------------------------------------------
// Task
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TaskStatus {
    Running,
    Completed,
    Failed,
    Cancelled,
}

impl TaskStatus {
    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            TaskStatus::Completed | TaskStatus::Failed | TaskStatus::Cancelled
        )
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BroReport {
    pub message: String,
    #[serde(default)]
    pub needs: Option<String>,
    #[serde(default)]
    pub data: Option<Value>,
    #[serde(rename = "reportedAt")]
    pub reported_at: u64,
}

impl BroReport {
    pub fn to_json(&self) -> Value {
        let mut obj = serde_json::json!({
            "message": self.message,
            "reportedAt": self.reported_at,
            "reportedAgo": format_elapsed(self.reported_at, None),
        });
        if let Some(ref needs) = self.needs {
            obj["needs"] = Value::String(needs.clone());
        }
        if let Some(ref data) = self.data {
            obj["data"] = data.clone();
        }
        obj
    }
}

/// Shared inner state of a task, updated by background readers.
pub struct TaskInner {
    pub id: String,
    pub provider: Provider,
    pub session_id: String,
    pub events: EventRing,
    pub model: Option<String>,
    pub last_assistant_message: Option<String>,
    pub usage: Option<Usage>,
    pub cost_usd: Option<f64>,
    pub num_turns: Option<u64>,
    /// Cache-inclusive input tokens of the session's most recent model turn:
    /// how much of the context window the last prompt occupied. Distinct from
    /// [`Self::usage`], which accumulates session totals and therefore measures
    /// work done rather than window occupancy.
    pub last_turn_input_tokens: Option<u64>,
    /// The model's context window, as reported by the producer that owns the
    /// model-keyed table. `None` for a model that table does not recognize.
    pub context_window: Option<u64>,
    pub stderr: String,
    pub status: TaskStatus,
    pub started_at: u64,
    pub completed_at: Option<u64>,
    pub exit_code: Option<i32>,
    pub cwd: Option<String>,
    /// The durable catalog project this task belongs to (plan section 3.3,
    /// adjudication Q-E2).
    ///
    /// Populated ONLY from authoritative project identity already available to
    /// the dispatch path, never inferred from [`Self::cwd`]. A cwd is a host
    /// path, and hashing or resolving one back into a project id is exactly the
    /// path-keyed guessing the catalog exists to retire; a task whose project
    /// is not known authoritatively stays `None` and is backfilled later.
    ///
    /// Held in the LIVE record, not only the persisted one: the runtime state
    /// is the authority that `serialize_snapshot` projects, so a persisted-only
    /// field would be silently erased by the first load-then-persist cycle.
    pub project_id: Option<String>,
    /// Concrete cockpit-managed worktree root for this task, if its cwd sits
    /// under a daemon-recognized managed worktree parent.
    pub managed_worktree: Option<String>,
    /// Caller-supplied identity for the dispatched bro. Format:
    /// `<team>::<member>` for ensemble dispatch (carries which member
    /// of which team this task belongs to), bare `<brofile>` for
    /// brofile-only dispatch (no team context — implementer / advisor
    /// nodes), or `None` for legacy direct dispatches that didn't
    /// supply context. The tail handler reads this when team-based
    /// `find_bro_ref_for_task` returns no match, so brofile-dispatched
    /// tasks (workflow implementer, single-bro advisor) still surface
    /// in `bro tail` with a name instead of being anonymous.
    pub bro_label: Option<String>,
    /// Daemon-owned display name for the roster/cockpit. Defaults from the
    /// first user prompt for fresh dispatches and is independent of bro_label,
    /// which still carries bro/team identity.
    pub name: Option<String>,
    /// Agent attribution set by bro_agent_dispatch. Format:
    /// `agent:<name>@v<version>`. Preserved even when record_task_to_bro
    /// overwrites bro_label for team routing. Surfaced in bro_status /
    /// bro_dashboard as agentLabel alongside broLabel.
    pub agent_label: Option<String>,
    /// Latest agent-authored progress report, set through `bro_report`
    /// and surfaced in `bro_status` / `bro_dashboard`.
    pub report: Option<BroReport>,
    /// True when the latest terminal result event represented an operator
    /// interrupt rather than a natural finish. This is a cause marker layered on
    /// top of `status`: finalization maps it to `Cancelled`, and status/roster
    /// JSON surface it as `interrupted: true`.
    pub interrupted: bool,
    /// True when this task's terminal state is "the daemon killed it
    /// because the process restarted, but the underlying provider
    /// session_id is still valid on disk (rollout / session jsonl
    /// persisted)." Calling agents that see `status=failed` AND
    /// `recoverable=true` should retry via `bro_resume(session_id=...)`
    /// rather than starting a fresh session — the conversation
    /// history is intact. Surfaced through `bro_status` / `bro_wait`.
    pub recoverable: bool,
    pub transcript_location: Option<TranscriptLocation>,
    pub transcript_cursor: Option<TranscriptCursor>,
    /// Monotonic per-task cursor for live tail events. This is distinct from
    /// provider transcript-file cursors because `tail_tx` carries task lifecycle
    /// events plus retained envelope events.
    pub live_cursor: u64,
    /// Highest harness event `seq` this daemon has durably ingested for the
    /// task's worker session.
    ///
    /// This is the daemon's half of the replay contract with fleetd (slice 5:
    /// "the daemon owns the replay cursor; fleetd owns the window"). It
    /// advances only AFTER `ingest_harness_event` has applied the event, so a
    /// re-adopting daemon that replays from it sees everything it had not
    /// applied and nothing twice. A harness build that emits events without a
    /// top-level `seq` never advances it, which degrades to replaying the whole
    /// retained window rather than to silent loss.
    pub harness_ingest_seq: u64,
    /// Last wall-clock ms a roster update was emitted from the stream-delta
    /// ingest path. Deltas arrive at token-chunk rate; rebuilding +
    /// broadcasting a roster summary per chunk is pure overhead, so delta
    /// ingest throttles roster emits to ~1/s (step-boundary events still
    /// emit unconditionally). In-memory only.
    pub last_delta_roster_emit_ms: u64,
    pub supervision: SupervisionState,
    /// Where this task was spawned FROM (Slice 1b of the daemon roster
    /// design). Persisted via `PersistedTask` so the origin survives a
    /// daemon restart and the fleet roster can group/tab tasks by
    /// source. See `bro_core::Origin` for the taxonomy.
    pub origin: bro_core::Origin,
    /// True when a live workflow/atom owns this task's lifecycle and operator
    /// closeout/interrupt should be confirm-gated.
    pub workflow_owned: bool,
}

const TASK_EVENT_RING_CAPACITY: usize = 512;

impl TaskInner {
    pub fn observed_event_count(&self) -> usize {
        let supervision_count = usize::try_from(self.supervision.event_count).unwrap_or(usize::MAX);
        self.events.total_count().max(supervision_count)
    }
}

pub struct EventRing {
    events: Vec<Value>,
    total_count: usize,
}

impl EventRing {
    pub fn new() -> Self {
        Self {
            events: Vec::with_capacity(TASK_EVENT_RING_CAPACITY.min(16)),
            total_count: 0,
        }
    }

    pub fn from_loaded(events: Vec<Value>) -> Self {
        let total_count = events.len();
        let events = retain_recent_events(events, TASK_EVENT_RING_CAPACITY);
        Self {
            events,
            total_count,
        }
    }

    pub fn push(&mut self, event: Value) {
        self.total_count = self.total_count.saturating_add(1);
        if self.events.len() == TASK_EVENT_RING_CAPACITY {
            self.events.remove(0);
        }
        self.events.push(event);
    }

    pub fn len(&self) -> usize {
        self.total_count
    }

    pub fn retained_len(&self) -> usize {
        self.events.len()
    }

    pub fn is_empty(&self) -> bool {
        self.total_count == 0
    }

    pub fn total_count(&self) -> usize {
        self.total_count
    }

    fn retained_offset_for_absolute(&self, absolute_start: usize) -> usize {
        let dropped_count = self.total_count.saturating_sub(self.events.len());
        absolute_start
            .saturating_sub(dropped_count)
            .min(self.events.len())
    }
}

impl Default for EventRing {
    fn default() -> Self {
        Self::new()
    }
}

impl From<Vec<Value>> for EventRing {
    fn from(events: Vec<Value>) -> Self {
        Self::from_loaded(events)
    }
}

impl FromIterator<Value> for EventRing {
    fn from_iter<T: IntoIterator<Item = Value>>(iter: T) -> Self {
        let mut ring = Self::new();
        for event in iter {
            ring.push(event);
        }
        ring
    }
}

impl Deref for EventRing {
    type Target = [Value];

    fn deref(&self) -> &Self::Target {
        &self.events
    }
}

impl DerefMut for EventRing {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.events
    }
}

impl Index<RangeFrom<usize>> for EventRing {
    type Output = [Value];

    fn index(&self, index: RangeFrom<usize>) -> &Self::Output {
        let start = self.retained_offset_for_absolute(index.start);
        &self.events[start..]
    }
}

impl Index<usize> for EventRing {
    type Output = Value;

    fn index(&self, index: usize) -> &Self::Output {
        &self.events[index]
    }
}

fn retain_recent_events(mut events: Vec<Value>, limit: usize) -> Vec<Value> {
    if events.len() > limit {
        let drop_count = events.len() - limit;
        events.drain(0..drop_count);
    }
    events
}

/// Daemon-side cache of `RosterSummaryV1` projections, keyed by `task_id`.
///
/// The `/control/roster` handler serves from this view instead of
/// re-deriving every summary on each request — without it, the fleet
/// cockpit poll would lock every task's inner mutex once per request,
/// contending with event ingest on busy tasks (wave 6a).
///
/// Ingest maintains the view from the same call sites that emit
/// `RosterDelta` events (`RosterEventSink::emit_added` /
/// `emit_updated` / `emit_removed`): the summary is built while the
/// emitter already holds (or has just released) the inner lock, then
/// inserted under the view's brief write lock. The handler read path
/// only takes a read guard and clones; it never touches a per-task
/// mutex.
///
/// Staleness: the view can lag an in-flight mutation by one round —
/// a snapshot served between an `emit_updated` and the next delta may
/// carry the prior summary. This is acceptable for a polling fleet
/// dashboard: subsequent deltas re-converge the client, and `version`
/// reads after the view write so a snapshot's `version` is never
/// older than the tasks it lists.
///
/// Field parity: every field of the projection is computed by the
/// same `roster_summary_from_task` that the broadcast delta uses, so
/// `view[&task_id] == delta.summary` field-for-field for any update.
#[derive(Default)]
pub struct RosterView {
    summaries: RwLock<HashMap<String, bro_protocol::RosterSummaryV1>>,
}

impl RosterView {
    pub fn new() -> Self {
        Self {
            summaries: RwLock::new(HashMap::new()),
        }
    }

    /// Insert or replace the summary for `task_id`. Called from the
    /// event sink with the summary it just computed.
    pub fn upsert(&self, task_id: String, summary: bro_protocol::RosterSummaryV1) {
        self.summaries.write().insert(task_id, summary);
    }

    /// Drop the summary for `task_id`. No-op if absent.
    pub fn evict(&self, task_id: &str) {
        self.summaries.write().remove(task_id);
    }

    /// Snapshot the view into a `Vec` in unspecified order. The
    /// handler sorts/clones/serializes the result; we don't pay a
    /// per-task lock for any element.
    pub fn snapshot(&self) -> Vec<bro_protocol::RosterSummaryV1> {
        self.summaries.read().values().cloned().collect()
    }

    /// Seed the view from a live task store at startup. Builds one
    /// summary per task and inserts; cold tasks restored from
    /// `tasks.json` appear in the view without waiting for a delta.
    pub fn rebuild_from_store(&self, store: &TaskStore) {
        let mut view = self.summaries.write();
        view.clear();
        for task in store.all_tasks() {
            let summary = roster_summary_from_task(&task);
            view.insert(summary.task_id.as_str().to_string(), summary);
        }
    }
}

#[derive(Clone)]
pub struct RosterEventSink {
    seq: Arc<AtomicU64>,
    tx: tokio::sync::broadcast::Sender<bro_protocol::RosterDelta>,
    view: Option<Arc<RosterView>>,
}

impl RosterEventSink {
    /// Construct a sink without a backing view. Test seam for
    /// scenarios that want the broadcast channel but no cache
    /// (e.g. constructing a sink in unit tests where a `RosterView`
    /// would be over-machinery). Production sinks go through
    /// `with_view`.
    #[allow(dead_code)]
    pub fn new(
        seq: Arc<AtomicU64>,
        tx: tokio::sync::broadcast::Sender<bro_protocol::RosterDelta>,
    ) -> Self {
        Self {
            seq,
            tx,
            view: None,
        }
    }

    /// Build a sink wired to the daemon's `RosterView`. The view is
    /// updated synchronously from `emit_added` / `emit_updated` /
    /// `emit_removed` so `/control/roster` reads see the projection
    /// before the broadcast delta goes out.
    pub fn with_view(
        seq: Arc<AtomicU64>,
        tx: tokio::sync::broadcast::Sender<bro_protocol::RosterDelta>,
        view: Arc<RosterView>,
    ) -> Self {
        Self {
            seq,
            tx,
            view: Some(view),
        }
    }

    pub fn current_version(&self) -> u64 {
        self.seq.load(Ordering::SeqCst)
    }

    fn next_seq(&self) -> u64 {
        self.seq.fetch_add(1, Ordering::SeqCst) + 1
    }

    pub fn emit_added(&self, task: &Task) {
        let summary = roster_summary_from_task(task);
        if let Some(view) = &self.view {
            view.upsert(summary.task_id.as_str().to_string(), summary.clone());
        }
        let delta = bro_protocol::RosterDelta::Added {
            seq: self.next_seq(),
            task: summary,
        };
        let _ = self.tx.send(delta);
    }

    pub fn emit_updated(&self, task: &Task) {
        let summary = roster_summary_from_task(task);
        if let Some(view) = &self.view {
            view.upsert(summary.task_id.as_str().to_string(), summary.clone());
        }
        let delta = bro_protocol::RosterDelta::Updated {
            seq: self.next_seq(),
            task: summary,
        };
        let _ = self.tx.send(delta);
    }

    pub fn emit_removed(&self, task_id: impl Into<String>) {
        let task_id = task_id.into();
        if let Some(view) = &self.view {
            view.evict(&task_id);
        }
        let delta = bro_protocol::RosterDelta::Removed {
            seq: self.next_seq(),
            task_id: bro_core::TaskId::new(task_id),
        };
        let _ = self.tx.send(delta);
    }
}

#[cfg(test)]
mod roster_view_tests {
    //! Wave 6a: RosterView is the daemon-side cache that lets
    //! `/control/roster` serve a snapshot without locking every
    //! task's inner mutex. These tests pin the field-parity
    //! contract between the view, the broadcast delta, and
    //! `roster_summary_from_task`.

    use super::*;

    fn make_task(id: &str, status: TaskStatus, provider: Provider) -> Arc<Task> {
        Arc::new(Task {
            inner: Mutex::new(TaskInner {
                id: id.to_string(),
                provider,
                session_id: format!("sess-{id}"),
                events: EventRing::new(),
                model: None,
                last_assistant_message: None,
                usage: None,
                cost_usd: Some(0.42),
                num_turns: Some(7),
                last_turn_input_tokens: None,
                context_window: None,
                stderr: String::new(),
                status,
                started_at: 1_700_000_000_000,
                completed_at: Some(1_700_000_001_000),
                exit_code: Some(0),
                cwd: Some(format!("/tmp/{id}")),
                managed_worktree: Some(format!("/wt/{id}")),
                bro_label: Some(format!("bro-{id}")),
                name: None,
                agent_label: Some(format!("agent-{id}")),
                report: None,
                interrupted: false,
                recoverable: false,
                transcript_location: None,
                transcript_cursor: None,
                live_cursor: 0,
                harness_ingest_seq: 0,
                last_delta_roster_emit_ms: 0,
                supervision: SupervisionState::default(),
                origin: bro_core::Origin::Cockpit,
                workflow_owned: workflow_owned_for_origin(bro_core::Origin::Cockpit),
                project_id: None,
            }),
            notify: Arc::new(Notify::new()),
            child_id: Mutex::new(None),
            roster_events: None,
        })
    }

    fn make_sink(view: Arc<RosterView>) -> (Arc<AtomicU64>, RosterEventSink) {
        let seq = Arc::new(AtomicU64::new(0));
        let (tx, _rx) = tokio::sync::broadcast::channel(16);
        let sink = RosterEventSink::with_view(seq.clone(), tx, view);
        (seq, sink)
    }

    #[test]
    fn view_upsert_then_snapshot_round_trips_summary() {
        let view = RosterView::new();
        let summary = bro_protocol::RosterSummaryV1 {
            task_id: bro_core::TaskId::new("t1"),
            status: bro_protocol::TaskStatus::Running,
            provider: Provider::Glm,
            cost: Some(0.10),
            turns: Some(1),
            cwd: None,
            label: None,
            name: None,
            session_id: None,
            last_message_snippet: None,
            model: None,
            report: None,
            last_event_at: Some(42),
            origin: bro_core::Origin::Unknown,
            managed_worktree: None,
            workflow_owned: false,
            started_at: Some(42),
            agent_label: None,
            report_full: None,
            interrupted: false,
            error_teaser: None,
            transcript_path: None,
            context: None,
        };
        view.upsert("t1".into(), summary.clone());
        let snap = view.snapshot();
        assert_eq!(snap.len(), 1);
        assert_eq!(snap[0], summary);
    }

    #[test]
    fn view_evict_drops_entry() {
        let view = RosterView::new();
        view.upsert(
            "t1".into(),
            bro_protocol::RosterSummaryV1 {
                task_id: bro_core::TaskId::new("t1"),
                status: bro_protocol::TaskStatus::Running,
                provider: Provider::Glm,
                cost: None,
                turns: None,
                cwd: None,
                label: None,
                name: None,
                session_id: None,
                last_message_snippet: None,
                model: None,
                report: None,
                last_event_at: None,
                origin: bro_core::Origin::Unknown,
                managed_worktree: None,
                workflow_owned: false,
                started_at: None,
                agent_label: None,
                report_full: None,
                interrupted: false,
                error_teaser: None,
                transcript_path: None,
                context: None,
            },
        );
        assert_eq!(view.snapshot().len(), 1);
        view.evict("t1");
        assert!(view.snapshot().is_empty());
    }

    #[test]
    fn sink_emit_added_inserts_summary_into_view() {
        let view = Arc::new(RosterView::new());
        let (_seq, sink) = make_sink(view.clone());
        let task = make_task("t1", TaskStatus::Running, Provider::Glm);

        sink.emit_added(&task);
        let snap = view.snapshot();
        assert_eq!(snap.len(), 1);
        let expected = roster_summary_from_task(&task);
        assert_eq!(
            snap[0], expected,
            "view entry must match the field-parity summary"
        );
        assert_eq!(snap[0].task_id.as_str(), "t1");
        assert_eq!(snap[0].provider, Provider::Glm);
        assert_eq!(snap[0].status, bro_protocol::TaskStatus::Running);
    }

    #[test]
    fn sink_emit_updated_replaces_existing_entry() {
        let view = Arc::new(RosterView::new());
        let (_seq, sink) = make_sink(view.clone());
        let task = make_task("t1", TaskStatus::Running, Provider::Glm);
        sink.emit_added(&task);
        assert_eq!(view.snapshot().len(), 1);

        // Mutate inner state and emit_updated — view entry must
        // reflect the new summary field-for-field.
        {
            let mut inner = task.inner.lock();
            inner.status = TaskStatus::Completed;
            inner.last_assistant_message = Some("done".to_string());
        }
        sink.emit_updated(&task);
        let snap = view.snapshot();
        assert_eq!(snap.len(), 1, "update should not duplicate the entry");
        let expected = roster_summary_from_task(&task);
        assert_eq!(snap[0], expected);
        assert_eq!(snap[0].status, bro_protocol::TaskStatus::Completed);
        assert_eq!(snap[0].last_message_snippet.as_deref(), Some("done"));
    }

    #[test]
    fn sink_emit_removed_drops_entry_from_view() {
        let view = Arc::new(RosterView::new());
        let (_seq, sink) = make_sink(view.clone());
        let task = make_task("t1", TaskStatus::Running, Provider::Glm);
        sink.emit_added(&task);
        assert_eq!(view.snapshot().len(), 1);

        sink.emit_removed("t1");
        assert!(view.snapshot().is_empty(), "eviction must drop the entry");
    }

    #[test]
    fn rebuild_from_store_seeds_view_with_every_task() {
        let view = RosterView::new();
        let mut store = TaskStore::new();
        store
            .insert(
                "t1".into(),
                make_task("t1", TaskStatus::Running, Provider::Glm),
            )
            .expect("insert t1");
        store
            .insert(
                "t2".into(),
                make_task("t2", TaskStatus::Completed, Provider::Deepseek),
            )
            .expect("insert t2");

        view.rebuild_from_store(&store);
        let snap = view.snapshot();
        assert_eq!(snap.len(), 2, "every task in the store must appear");
        let mut by_id: std::collections::HashMap<_, _> = snap
            .iter()
            .map(|s| (s.task_id.as_str().to_string(), s.clone()))
            .collect();
        let t1 = by_id.remove("t1").expect("t1 present");
        let t2 = by_id.remove("t2").expect("t2 present");
        assert_eq!(t1.status, bro_protocol::TaskStatus::Running);
        assert_eq!(t1.provider, Provider::Glm);
        assert_eq!(t2.status, bro_protocol::TaskStatus::Completed);
        assert_eq!(t2.provider, Provider::Deepseek);
    }

    #[test]
    fn rebuild_from_store_clears_stale_entries() {
        let view = RosterView::new();
        // Seed a stale entry that isn't in the store.
        view.upsert(
            "stale".into(),
            bro_protocol::RosterSummaryV1 {
                task_id: bro_core::TaskId::new("stale"),
                status: bro_protocol::TaskStatus::Completed,
                provider: Provider::Glm,
                cost: None,
                turns: None,
                cwd: None,
                label: None,
                name: None,
                session_id: None,
                last_message_snippet: None,
                model: None,
                report: None,
                last_event_at: None,
                origin: bro_core::Origin::Unknown,
                managed_worktree: None,
                workflow_owned: false,
                started_at: None,
                agent_label: None,
                report_full: None,
                interrupted: false,
                error_teaser: None,
                transcript_path: None,
                context: None,
            },
        );
        let mut store = TaskStore::new();
        store
            .insert(
                "t1".into(),
                make_task("t1", TaskStatus::Running, Provider::Glm),
            )
            .expect("insert t1");

        view.rebuild_from_store(&store);
        let snap = view.snapshot();
        assert_eq!(snap.len(), 1);
        assert_eq!(snap[0].task_id.as_str(), "t1");
    }
}

pub struct Task {
    pub inner: Mutex<TaskInner>,
    pub notify: Arc<Notify>,
    /// Handle to the child process for cancellation. Only set while running.
    child_id: Mutex<Option<u32>>, // PID
    roster_events: Option<RosterEventSink>,
}

impl Task {
    pub fn id(&self) -> String {
        self.inner.lock().id.clone()
    }

    fn next_live_cursor(&self) -> u64 {
        let mut inner = self.inner.lock();
        inner.live_cursor += 1;
        inner.live_cursor
    }

    fn emit_roster_added(&self) {
        if let Some(sink) = &self.roster_events {
            sink.emit_added(self);
        }
    }

    fn emit_roster_updated(&self) {
        if let Some(sink) = &self.roster_events {
            sink.emit_updated(self);
        }
    }
}

pub fn roster_summary_from_task(task: &Task) -> bro_protocol::RosterSummaryV1 {
    use bro_protocol::TaskStatus as Wire;

    let inner = task.inner.lock();
    let last_event_at = match inner.completed_at {
        Some(done) => done.max(inner.started_at),
        None => inner.started_at,
    };
    let status = match inner.status {
        TaskStatus::Running => Wire::Running,
        TaskStatus::Completed => Wire::Completed,
        TaskStatus::Failed => Wire::Failed,
        TaskStatus::Cancelled => Wire::Cancelled,
    };
    bro_protocol::RosterSummaryV1 {
        task_id: bro_core::TaskId::new(inner.id.clone()),
        status,
        provider: inner.provider,
        cost: inner.cost_usd,
        turns: inner.num_turns,
        cwd: inner.cwd.clone(),
        managed_worktree: inner.managed_worktree.clone(),
        label: inner
            .bro_label
            .clone()
            .or_else(|| inner.agent_label.clone()),
        name: inner
            .name
            .clone()
            .or_else(|| inner.bro_label.clone())
            .or_else(|| inner.agent_label.clone()),
        session_id: (!inner.session_id.is_empty())
            .then(|| bro_core::SessionId::new(inner.session_id.clone())),
        last_message_snippet: inner
            .last_assistant_message
            .as_deref()
            .map(|s| s.chars().take(200).collect::<String>()),
        model: inner.model.clone(),
        report: inner.report.as_ref().and_then(roster_report_teaser),
        last_event_at: Some(last_event_at),
        origin: inner.origin,
        workflow_owned: inner.workflow_owned,
        // Wave 7c: the dashboard consumer needs `started_at` to
        // recompute the legacy `elapsed` field (terminal:
        // `last_event_at - started_at`; live: `now - started_at`).
        // Set from the same `TaskInner.started_at` that the
        // existing `last_event_at` derivation already reads.
        started_at: Some(inner.started_at),
        // Wave 7c: dashboard needs `agentLabel` distinct from
        // `broLabel`; the legacy projection collapsed them into
        // `label`. Carry both so the dashboard's row projection
        // can stay off the per-task inner mutex.
        agent_label: inner.agent_label.clone(),
        // Wave 7c: structured report object for the dashboard's
        // `report` row. `report` (the teaser string) stays for the
        // fleet row UI; `report_full` carries the full
        // `BroReport::to_json()` shape for the dashboard.
        report_full: inner.report.as_ref().map(bro_report_to_wire),
        interrupted: inner.interrupted,
        // Error teaser for failed/cancelled tasks: the last non-empty line
        // of stderr, trimmed and capped, so the fleet cockpit zoom view can
        // show why a dispatch failed without querying bro_status.
        error_teaser: if inner.status.is_terminal() && !inner.stderr.trim().is_empty() {
            let trimmed = inner.stderr.trim();
            let last_line = trimmed.lines().next_back().unwrap_or(trimmed);
            Some(last_line.chars().take(200).collect::<String>())
        } else {
            None
        },
        // The child launch records the exact BRO_HOME-derived event-log path.
        // The file may not exist yet for a fresh dispatch; readers treat
        // absence as empty.
        transcript_path: inner
            .transcript_location
            .as_ref()
            .filter(|_| !inner.session_id.is_empty() && inner.session_id != "pending")
            .map(|location| location.path.to_string_lossy().into_owned()),
        // Same signal bro_status carries, projected onto the roster plane so
        // the dashboard can render it without re-locking per-task state.
        context: context_pressure_for_inner(&inner),
    }
}

/// Build a harness transcript location from loose parts.
///
/// Test-only since the one-shot cutover: production derives the location from
/// the spawn spec ([`harness_transcript_location_from_spec`]), which is the
/// single pinned derivation both the daemon and the executor flow from. This
/// survives because the pending-location resolution in `ingest_harness_event`
/// still needs a pending-shaped location to resolve, and constructing one by
/// hand is clearer in a test than assembling a whole synthetic spec.
#[cfg(test)]
fn harness_transcript_location(
    provider: Provider,
    store_dir: &std::path::Path,
    session_id: &str,
    cwd: Option<&str>,
) -> Option<TranscriptLocation> {
    if session_id.is_empty() {
        return None;
    }
    Some(TranscriptLocation {
        source: TranscriptSource::Harness(provider),
        storage: TranscriptStorage::JsonlFile,
        path: store_dir
            .join("harness-sessions")
            .join(format!("{session_id}.events.jsonl")),
        account: None,
        session_id: (session_id != "pending").then(|| session_id.to_string()),
        project: None,
        cwd: cwd.map(str::to_string),
        is_subagent: false,
        logical_key: None,
    })
}

/// Project an in-memory `BroReport` to the wire-shaped
/// `BroReportV1` (wave 7c). The dashboard's `report` row uses
/// `BroReport::to_json()` semantics (camelCase `reportedAt` /
/// `reportedAgo`); the wire DTO is snake_case to match the
/// rest of `RosterSummaryV1` and to round-trip cleanly through
/// serde defaults. `reportedAgo` is computed at projection time
/// from the current wall clock — the dashboard is for live
/// display, not for replay.
fn bro_report_to_wire(report: &BroReport) -> bro_protocol::BroReportV1 {
    bro_protocol::BroReportV1 {
        message: report.message.clone(),
        needs: report.needs.clone(),
        data: report.data.clone(),
        reported_at: report.reported_at,
        reported_ago: format_elapsed(report.reported_at, None),
    }
}

fn model_from_event(event: &serde_json::Value) -> Option<String> {
    event
        .get("model")
        .or_else(|| {
            event
                .get("message")
                .and_then(|message| message.get("model"))
        })
        .and_then(|model| model.as_str())
        .map(|model| model.to_string())
}

fn model_from_events_at_load(events: &[serde_json::Value]) -> Option<String> {
    events.iter().find_map(model_from_event)
}

const DEFAULT_TASK_NAME_CHARS: usize = 60;
const ROSTER_REPORT_TEASER_CHARS: usize = 80;

fn update_model_cache_from_event(inner: &mut TaskInner, event: &serde_json::Value) {
    if inner.model.is_none() {
        inner.model = model_from_event(event);
    }
}

fn compact_teaser(raw: &str, max_chars: usize) -> Option<String> {
    let compact = raw.split_whitespace().collect::<Vec<_>>().join(" ");
    if compact.is_empty() {
        None
    } else {
        Some(compact.chars().take(max_chars).collect())
    }
}

pub(crate) fn default_task_name_from_prompt(prompt: &str) -> Option<String> {
    compact_teaser(prompt, DEFAULT_TASK_NAME_CHARS)
}

fn roster_report_teaser(report: &BroReport) -> Option<String> {
    compact_teaser(&report.message, ROSTER_REPORT_TEASER_CHARS)
}

pub(crate) fn seed_task_roster_fields(
    task: &Task,
    name: Option<String>,
    model: Option<String>,
    task_store: &RwLock<TaskStore>,
    store_dir: &std::path::Path,
) {
    let mut changed = false;
    {
        let mut inner = task.inner.lock();
        if inner.name.is_none()
            && let Some(name) = name.and_then(|name| compact_teaser(&name, DEFAULT_TASK_NAME_CHARS))
        {
            inner.name = Some(name);
            changed = true;
        }
        if inner.model.is_none() && model.is_some() {
            inner.model = model;
            changed = true;
        }
    }
    if changed {
        task.emit_roster_updated();
        request_persist(task_store, store_dir);
    }
}

pub(crate) fn workflow_owned_for_origin(origin: bro_core::Origin) -> bool {
    matches!(origin, bro_core::Origin::Workflow | bro_core::Origin::Atom)
}

/// Test-only Task constructor for store/prune unit tests. Private fields
/// (`child_id`) keep callers outside this module from building Tasks, so
/// this gated helper is the seam tests use to populate a TaskStore.
#[cfg(test)]
pub(crate) fn test_task(id: &str, status: TaskStatus, provider: Provider) -> Arc<Task> {
    Arc::new(Task {
        inner: Mutex::new(TaskInner {
            id: id.into(),
            provider,
            session_id: format!("sess-{id}"),
            events: EventRing::new(),
            model: None,
            last_assistant_message: None,
            usage: None,
            cost_usd: None,
            num_turns: Some(3),
            last_turn_input_tokens: None,
            context_window: None,
            stderr: String::new(),
            status,
            started_at: now_ms(),
            completed_at: Some(now_ms()),
            exit_code: Some(0),
            cwd: None,
            managed_worktree: None,
            bro_label: None,
            name: None,
            agent_label: None,
            report: None,
            interrupted: false,
            recoverable: false,
            transcript_location: None,
            transcript_cursor: None,
            live_cursor: 0,
            harness_ingest_seq: 0,
            last_delta_roster_emit_ms: 0,
            supervision: SupervisionState::default(),
            origin: bro_core::Origin::Unknown,
            workflow_owned: workflow_owned_for_origin(bro_core::Origin::Unknown),
            project_id: None,
        }),
        notify: Arc::new(Notify::new()),
        child_id: Mutex::new(None),
        roster_events: None,
    })
}

// ---------------------------------------------------------------------------
// Task Store
// ---------------------------------------------------------------------------

pub struct TaskStore {
    tasks: HashMap<String, Arc<Task>>,
    reserved: HashSet<String>,
    // Opaque rows remain in snapshots and cannot acquire executable identities.
    quarantined_rows: Vec<Value>,
    quarantined_ids: HashSet<String>,
    persistence_blocked: bool,
}

impl TaskStore {
    /// (provider, session_id, bro_label) rows for the edge-index
    /// SESSION_USED_BROFILE projection — `EdgeStoreRefs` takes plain rows
    /// so the edge store sits below orchestration in the crate DAG.
    pub fn session_brofile_rows(&self) -> Vec<(String, String, String)> {
        self.all_tasks()
            .iter()
            .filter_map(|task| {
                let inner = task.inner.lock();
                let label = inner.bro_label.clone()?;
                Some((
                    inner.provider.as_str().to_string(),
                    inner.session_id.clone(),
                    label,
                ))
            })
            .collect()
    }

    pub fn new() -> Self {
        Self {
            tasks: HashMap::new(),
            reserved: HashSet::new(),
            quarantined_rows: Vec::new(),
            quarantined_ids: HashSet::new(),
            persistence_blocked: false,
        }
    }

    pub fn get(&self, id: &str) -> Option<Arc<Task>> {
        self.tasks.get(id).cloned()
    }

    pub fn contains(&self, id: &str) -> bool {
        self.tasks.contains_key(id)
            || self.reserved.contains(id)
            || self.quarantined_ids.contains(id)
    }

    pub fn reserve_id(&mut self, id: &str) -> Result<(), BroSpawnError> {
        if self.persistence_blocked {
            return Err(BroSpawnError::TaskStoreUnavailable);
        }
        if self.contains(id) {
            return Err(BroSpawnError::DuplicateTaskId { id: id.to_string() });
        }
        self.reserved.insert(id.to_string());
        Ok(())
    }

    #[allow(dead_code)] // test-only entry point; production paths use reserve_id + insert_reserved
    pub fn insert(&mut self, id: String, task: Arc<Task>) -> Result<(), BroSpawnError> {
        if self.persistence_blocked {
            return Err(BroSpawnError::TaskStoreUnavailable);
        }
        if self.tasks.contains_key(&id) {
            return Err(BroSpawnError::DuplicateTaskId { id });
        }
        if self.reserved.contains(&id) || self.quarantined_ids.contains(&id) {
            return Err(BroSpawnError::ReservedTaskId { id });
        }
        self.tasks.insert(id, task);
        Ok(())
    }

    fn insert_reserved(&mut self, id: String, task: Arc<Task>) -> Result<(), BroSpawnError> {
        if self.persistence_blocked {
            return Err(BroSpawnError::TaskStoreUnavailable);
        }
        if self.tasks.contains_key(&id) || self.quarantined_ids.contains(&id) {
            self.reserved.remove(&id);
            return Err(BroSpawnError::DuplicateTaskId { id });
        }
        self.reserved.remove(&id);
        self.tasks.insert(id, task);
        Ok(())
    }

    fn insert_loaded(&mut self, id: String, task: Arc<Task>) {
        self.reserved.remove(&id);
        self.tasks.entry(id).or_insert(task);
    }

    fn release_reservation(&mut self, id: &str) {
        self.reserved.remove(id);
    }

    pub fn all_tasks(&self) -> Vec<Arc<Task>> {
        self.tasks.values().cloned().collect()
    }

    /// Drop entries matching the predicate (e.g. failed, older than X).
    /// Returns the IDs that were removed for caller reporting + persist.
    pub fn retain_drop<F>(&mut self, mut keep: F) -> Vec<String>
    where
        F: FnMut(&Task) -> bool,
    {
        let mut dropped = Vec::new();
        self.tasks.retain(|id, t| {
            if keep(t) {
                true
            } else {
                dropped.push(id.clone());
                false
            }
        });
        dropped
    }
}

// ---------------------------------------------------------------------------
// Persistence
// ---------------------------------------------------------------------------

const MAX_PERSISTED_EVENTS: usize = 50;

#[derive(Serialize, Deserialize)]
struct PersistedTask {
    id: String,
    provider: Provider,
    session_id: String,
    events: Vec<Value>,
    #[serde(default)]
    model: Option<String>,
    last_assistant_message: Option<String>,
    usage: Option<Usage>,
    cost_usd: Option<f64>,
    num_turns: Option<u64>,
    /// See [`TaskInner::last_turn_input_tokens`]. Defaulted on read so tasks
    /// persisted before context telemetry existed still load.
    #[serde(default)]
    last_turn_input_tokens: Option<u64>,
    /// See [`TaskInner::context_window`].
    #[serde(default)]
    context_window: Option<u64>,
    stderr: String,
    status: TaskStatus,
    started_at: u64,
    completed_at: Option<u64>,
    exit_code: Option<i32>,
    cwd: Option<String>,
    /// See [`TaskInner::project_id`]. `skip_serializing_if` keeps an unstamped
    /// task byte-identical to what every pre-Phase-6 daemon wrote, so adding
    /// this field neither rewrites the store nor disturbs the row identity the
    /// backfill keys on.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    project_id: Option<String>,
    #[serde(default)]
    managed_worktree: Option<String>,
    #[serde(default)]
    bro_label: Option<String>,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    agent_label: Option<String>,
    #[serde(default)]
    report: Option<BroReport>,
    #[serde(default)]
    interrupted: bool,
    /// True when the previous daemon instance was running this task
    /// at restart and the underlying provider session_id is still
    /// recoverable on disk. Set during `TaskStore::load` when it
    /// flips a `Running` task to `Failed`. Lets calling agents
    /// distinguish "kill by restart, rollout intact, retry via
    /// `bro_resume(session_id=...)`" from "task genuinely failed".
    #[serde(default)]
    recoverable: bool,
    #[serde(default)]
    transcript_location: Option<TranscriptLocation>,
    #[serde(default)]
    transcript_cursor: Option<TranscriptCursor>,
    #[serde(default)]
    live_cursor: u64,
    /// Absent on pre-fleetd records; `0` means "replay everything fleetd still
    /// holds", which is the correct conservative answer for a session spawned
    /// before the cursor existed.
    #[serde(default)]
    harness_ingest_seq: u64,
    #[serde(default)]
    supervision: SupervisionState,
    /// Origin is back-compat default `Unknown` when absent on disk —
    /// pre-Slice-1b records have no `origin` field, and a daemon
    /// rolling forward should treat those as `Unknown` rather than
    /// failing to load.
    #[serde(default)]
    origin: bro_core::Origin,
    #[serde(default)]
    workflow_owned: Option<bool>,
}

/// Preserve the exact input before a repaired snapshot may replace it. The
/// content-addressed name avoids repeated backups of unchanged bad snapshots.
fn quarantine_task_snapshot(store_dir: &std::path::Path, data: &[u8]) -> std::io::Result<PathBuf> {
    use sha2::{Digest, Sha256};
    let backup = store_dir.join(format!("tasks.quarantine.{:x}.json", Sha256::digest(data)));
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    match options.open(&backup) {
        Ok(mut file) => {
            if let Err(error) = file.write_all(data).and_then(|_| file.sync_all()) {
                let _ = std::fs::remove_file(&backup);
                return Err(error);
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            if std::fs::read(&backup)? != data {
                return Err(std::io::Error::other(
                    "existing task quarantine does not match snapshot",
                ));
            }
            std::fs::File::open(&backup)?.sync_all()?;
        }
        Err(error) => return Err(error),
    }
    std::fs::File::open(store_dir)?.sync_all()?;
    Ok(backup)
}

impl TaskStore {
    /// Synchronous full persist. The runtime hot paths now route through the
    /// off-worker [`request_persist`] actor; this direct entry is retained for
    /// the persistence-contract tests and as the synchronous-semantics
    /// reference (it shares `serialize_snapshot` + `write_snapshot_blocking`
    /// with the actor, so they can never diverge).
    #[allow(dead_code)]
    pub fn persist(&self, store_dir: &std::path::Path) {
        self.persist_with_event_limit(store_dir, MAX_PERSISTED_EVENTS);
    }

    /// Full-history persist (no per-task event cap). The fleet client was the
    /// only production caller; with the §7 fleet-daemon-only cut its own mirror
    /// store owns this now (`bro-fleet-client`). Retained (allow-dead) for the
    /// persistence-contract test and any future full-history daemon path.
    #[allow(dead_code)]
    pub fn persist_all_events(&self, store_dir: &std::path::Path) {
        self.persist_with_event_limit(store_dir, usize::MAX);
    }

    fn persist_with_event_limit(&self, store_dir: &std::path::Path, event_limit: usize) {
        if let Some(data) = self.serialize_snapshot(event_limit) {
            Self::write_snapshot_blocking(store_dir, &data);
        }
    }

    /// Serialize a point-in-time persistence snapshot. CPU + per-task-lock only,
    /// **no file I/O** — safe to take under the store read lock, then drop the
    /// lock before handing the bytes to a writer. Shared by the synchronous
    /// `persist` path and the off-worker [`TaskPersister`] actor so the two can
    /// never diverge in what they write.
    pub fn serialize_snapshot(&self, event_limit: usize) -> Option<String> {
        if self.persistence_blocked {
            tracing::error!(
                "task persistence refused: repair the unreadable tasks.json or its quarantine storage, then restart; existing snapshot has not been replaced"
            );
            return None;
        }
        let mut records: Vec<PersistedTask> = self
            .tasks
            .values()
            .map(|t| {
                let inner = t.inner.lock();
                PersistedTask {
                    id: inner.id.clone(),
                    provider: inner.provider,
                    session_id: inner.session_id.clone(),
                    events: inner
                        .events
                        .iter()
                        .rev()
                        .take(event_limit)
                        .rev()
                        .cloned()
                        .collect(),
                    model: inner.model.clone(),
                    last_assistant_message: inner.last_assistant_message.clone(),
                    usage: inner.usage.clone(),
                    cost_usd: inner.cost_usd,
                    num_turns: inner.num_turns,
                    last_turn_input_tokens: inner.last_turn_input_tokens,
                    context_window: inner.context_window,
                    stderr: inner.stderr.chars().take(2000).collect(),
                    status: inner.status,
                    started_at: inner.started_at,
                    completed_at: inner.completed_at,
                    exit_code: inner.exit_code,
                    cwd: inner.cwd.clone(),
                    project_id: inner.project_id.clone(),
                    managed_worktree: inner.managed_worktree.clone(),
                    bro_label: inner.bro_label.clone(),
                    name: inner.name.clone(),
                    agent_label: inner.agent_label.clone(),
                    report: inner.report.clone(),
                    interrupted: inner.interrupted,
                    recoverable: inner.recoverable,
                    transcript_location: inner.transcript_location.clone(),
                    transcript_cursor: inner.transcript_cursor.clone(),
                    live_cursor: inner.live_cursor,
                    harness_ingest_seq: inner.harness_ingest_seq,
                    supervision: inner.supervision.clone(),
                    origin: inner.origin,
                    workflow_owned: Some(inner.workflow_owned),
                }
            })
            .collect();
        records.sort_by(|left, right| left.id.cmp(&right.id));
        #[derive(Serialize)]
        #[serde(untagged)]
        enum SnapshotRow<'a> {
            Task(&'a PersistedTask),
            Quarantined(&'a Value),
        }
        let rows: Vec<_> = records
            .iter()
            .map(SnapshotRow::Task)
            .chain(self.quarantined_rows.iter().map(SnapshotRow::Quarantined))
            .collect();
        let result = serde_json::to_string(&rows);
        match result {
            Ok(data) => Some(data),
            Err(error) => {
                tracing::error!(%error, "task snapshot serialization failed; existing snapshot retained");
                None
            }
        }
    }

    /// Atomically write a pre-serialized snapshot to `tasks.json` (tmp + rename).
    /// **Blocking** file I/O — never call on a tokio worker. It runs on the
    /// [`TaskPersister`] dedicated thread, or on a cold synchronous path
    /// (shutdown, tests) where blocking is acceptable.
    pub fn write_snapshot_blocking(store_dir: &std::path::Path, data: &str) {
        let file = store_dir.join("tasks.json");
        let tmp = store_dir.join("tasks.json.tmp");
        let _ = std::fs::create_dir_all(store_dir);
        if std::fs::write(&tmp, data).is_ok() {
            let _ = std::fs::rename(&tmp, &file);
        }
    }

    /// Recover readable rows independently. Damaged rows remain opaque and
    /// reserve their IDs; invalid snapshots or failed exact-byte quarantine
    /// disable persistence so later writes cannot erase the input evidence.
    pub fn load(store_dir: &std::path::Path, ttl_ms: u64) -> Self {
        let file = store_dir.join("tasks.json");
        let mut store = Self::new();
        let data = match std::fs::read(&file) {
            Ok(data) => data,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return store,
            Err(error) => {
                store.persistence_blocked = true;
                tracing::error!(%error, "cannot read tasks.json; task persistence disabled until repair and restart");
                return store;
            }
        };
        let rows: Vec<Value> = match serde_json::from_slice(&data) {
            Ok(rows) => rows,
            Err(error) => {
                store.persistence_blocked = true;
                tracing::error!(
                    line = error.line(),
                    column = error.column(),
                    "invalid tasks.json array; task persistence disabled until repair and restart; original bytes retained"
                );
                return store;
            }
        };
        let mut id_counts = HashMap::<String, usize>::new();
        for row in &rows {
            if let Some(id) = row.get("id").and_then(Value::as_str) {
                *id_counts.entry(id.to_owned()).or_default() += 1;
            }
        }
        let mut records = Vec::new();
        for row in rows {
            let duplicate = row
                .get("id")
                .and_then(Value::as_str)
                .is_some_and(|id| id_counts[id] > 1);
            match serde_json::from_value::<PersistedTask>(row.clone()) {
                Ok(record) if !duplicate => records.push(record),
                _ => {
                    if let Some(id) = row.get("id").and_then(Value::as_str) {
                        store.quarantined_ids.insert(id.to_owned());
                    }
                    store.quarantined_rows.push(row);
                }
            }
        }
        if !store.quarantined_rows.is_empty() {
            match quarantine_task_snapshot(store_dir, &data) {
                Ok(backup) => tracing::error!(
                    quarantined_rows = store.quarantined_rows.len(),
                    snapshot_backup = %backup.display(),
                    "unreadable or duplicate task rows quarantined; readable tasks retained; opaque rows remain in future snapshots and require operator repair"
                ),
                Err(error) => {
                    store.persistence_blocked = true;
                    tracing::error!(%error, quarantined_rows = store.quarantined_rows.len(),
                        "task snapshot quarantine failed; readable tasks retained in memory but persistence disabled until repair and restart");
                }
            }
        }
        // ── Task TTL retention ───────────────────────────────────────────
        // At daemon startup, tasks whose `started_at` is older than `ttl_ms`
        // are permanently dropped from the store. This is the ONLY automatic
        // task-reaping path; during runtime, tasks are only removed by an
        // explicit `bro_prune` or `DELETE /control/task/:id`. Retention is
        // uniform across all origins (Cockpit, AgentDispatch, Workflow, …).
        // If a task seems to vanish mid-session, the daemon has restarted and
        // the task's `started_at` fell below the `now - ttl_ms` horizon.
        //
        // Default `ttl_ms` is 86400000 (24 h), configurable via
        // `daemon.task_ttl_ms` / `BRO_TASK_TTL_MS` env var.
        let cutoff = now_ms().saturating_sub(ttl_ms);
        for mut rec in records {
            if rec.started_at < cutoff {
                continue;
            }
            // Ownership cannot be weakened by an older/inconsistent false flag.
            let workflow_owned = rec.workflow_owned.unwrap_or(false)
                || workflow_owned_for_origin(rec.origin)
                || rec.provider == Provider::Workflow;
            let model = rec.model.or_else(|| model_from_events_at_load(&rec.events));
            if rec.status == TaskStatus::Running {
                rec.status = TaskStatus::Failed;
                rec.completed_at = Some(now_ms());
                if workflow_owned {
                    rec.stderr.push_str(
                        "\n[blackbox] server restarted while an owner-managed task was running. \
                         Retained for inspection; ordinary bro resume/re-adoption is unavailable. \
                         The owning workflow or atom must handle recovery while that runtime exists.",
                    );
                } else {
                    rec.stderr.push_str(
                        "\n[blackbox] server restarted while task was running. \
                         The provider session is still on disk; retry with \
                         `bro_resume(session_id=...)` to continue the conversation \
                         rather than starting a fresh session.",
                    );
                }
                rec.recoverable = !workflow_owned;
            }
            if workflow_owned {
                rec.recoverable = false;
            }
            let events = EventRing::from_loaded(rec.events);
            let live_cursor = rec.live_cursor.max(events.len() as u64);
            let task = Arc::new(Task {
                inner: Mutex::new(TaskInner {
                    id: rec.id.clone(),
                    provider: rec.provider,
                    session_id: rec.session_id,
                    events,
                    model,
                    last_assistant_message: rec.last_assistant_message,
                    usage: rec.usage,
                    cost_usd: rec.cost_usd,
                    num_turns: rec.num_turns,
                    last_turn_input_tokens: rec.last_turn_input_tokens,
                    context_window: rec.context_window,
                    stderr: rec.stderr,
                    status: rec.status,
                    started_at: rec.started_at,
                    completed_at: rec.completed_at,
                    exit_code: rec.exit_code,
                    cwd: rec.cwd,
                    project_id: rec.project_id,
                    managed_worktree: rec.managed_worktree,
                    bro_label: rec.bro_label,
                    name: rec.name,
                    agent_label: rec.agent_label,
                    report: rec.report,
                    interrupted: rec.interrupted,
                    recoverable: rec.recoverable,
                    transcript_location: rec.transcript_location,
                    transcript_cursor: rec.transcript_cursor,
                    live_cursor,
                    harness_ingest_seq: rec.harness_ingest_seq,
                    last_delta_roster_emit_ms: 0,
                    supervision: rec.supervision,
                    origin: rec.origin,
                    workflow_owned,
                }),
                notify: Arc::new(Notify::new()),
                child_id: Mutex::new(None),
                roster_events: None,
            });
            store.insert_loaded(rec.id, task);
        }
        store
    }
}

// ---------------------------------------------------------------------------
// Spawn + lifecycle
// ---------------------------------------------------------------------------

// ── Dispatch-context layer (typed ingredients, harness-composed) ────
//
// The daemon owns CONTENT SELECTION only (dispatch-prompt-slots.md §3):
// which directives fire for a dispatch, each directive's empirically-
// calibrated cadence, the persona resolved from the brofile, the
// pre-bound scoping IDs (task, session, project, bro, thread,
// work-item), and the resolved pin block. It does NOT compose prompt
// text — `AmbientContext::dispatch_context` serializes the typed
// payload and the harness routes each ingredient to its per-transport
// slot (`--dispatch-context`, bro-protocol `DispatchContext`). The
// operator's prompt rides `-p` VERBATIM; nothing is ever glued onto it.
// Tool vocabulary and protocol definitions stay in the start-of-session
// layer rendered from `tool_docs` into the global memory files.

// Text recursion guard retired 2026-04-17. Every dispatch-capable
// provider (Claude, Copilot, Codex, Gemini) now has a mechanical tool
// filter applied at argv construction time. Vibe has no MCP at all, so
// no bro_* tools reach it to recurse through.
//
// If defense-in-depth text guards are wanted in the future, reintroduce
// a prefix here and gate on `AmbientContext::provider`.

/// Recall directive. The managed-region CORE RULE reliably triggers
/// `bbox_knowledge` queries on cold-start but can attention-decay within long
/// sessions on weaker providers. Keep this as a standing instruction, not a
/// per-turn reminder: repeated developer-message injection made Codex/Brodex
/// over-comply during procedural live-state work (especially gap-store work)
/// where `bbox_gaps`/`bbox_gap` is already the authoritative surface.
pub const RECALL_DIRECTIVE: &str = "\
Use `bbox_knowledge` when durable knowledge, prior decisions, conventions, or \
system runbooks could materially change the answer. It is not the surface for \
procedural live-state work already using the authoritative store: scoped pins, \
side-channel notes, active threads, transcripts, gap-store checks/writes \
(`bbox_gaps`/`bbox_gap*`), or repo-owned state commits. If a recall check is \
appropriate and the first result is empty or too broad, try one sharper phrase \
before relying on live filesystem state or prior knowledge.";

/// Ambient nudge for recursive orchestrators (allow_recursion=true).
/// They're usually fan-out coordinators, and the most common silent
/// failure mode is writing a prose rubric and pasting it into N
/// identical sub-agent prompts instead of compiling a packet once and
/// dispatching the packet_id. Fires in addition to RECALL_DIRECTIVE.
pub const ORCHESTRATOR_HINT: &str = "\
Orchestrator note: if your plan involves sending the same rubric / \
ranking criteria / decision tree / access rules to multiple sub-agents, \
compile it into a packet first via `bbox_compile` and dispatch the \
`packet_id` — every sub-agent then produces bit-identical output via \
`bbox_apply`, and a 4th agent can reproduce the results deterministically \
without re-reading prose. See `sm-rule-packets` via `bbox_knowledge`.";

/// Ambient task-shape nudge for every dispatch. Addresses a failure
/// mode observed in E10/S11 where an agent bypassed packets entirely
/// on a log-triage task ("the primitive was simply absent from my
/// mental toolkit") because `bbox_compile` wasn't deferred-loaded and
/// the task's shape read as regex-ish. Naming the packet tools in
/// the ambient prefix pre-loads their schemas into the agent's option
/// space and reframes "the AST can't do regex" as a gap-log rather
/// than a bypass. Fires for every dispatch, orthogonal to
/// ORCHESTRATOR_HINT.
///
/// **Calibration bound (E12 cross-provider data):** the current
/// wording is at the "works across claude+codex+gemini" joint.
/// Self-reported force varies by provider — Claude reads it as
/// "nudge, not decider" ("I'd have compiled regardless"), Codex as
/// "could have tipped fuzzier tasks", Gemini as "MANDATORY choice,
/// not nudge". Escalating the language (e.g. imperative verbs,
/// longer justification, explicit step-by-step) risks:
///   (a) over-constraint on Claude — it becomes noise it ignores,
///       or worse, makes the hint feel adversarial in tasks where
///       packets are clearly wrong (prose/research/synthesis);
///   (b) compliance theater on Gemini — Gemini already treats this
///       as mandatory at current wording; turning the dial up could
///       make it compile packets for tasks where the AST doesn't
///       fit, defeating the gap-tool signal.
///
/// If you change this string, re-run E12 (cross-provider S11 sweep)
/// to confirm all three providers still read it as intended.
/// Don't add imperative verbs ("MUST compile", "ALWAYS use") without
/// that verification.
pub const TASK_SHAPE_HINT: &str = "\
Task-shape check: if this task involves repeatedly classifying, \
ranking, triaging, scoring, or judging entities against stated \
criteria — try `bbox_compile` first (see `sm-rule-packets` via \
`bbox_knowledge`). Packets force explicit rule ordering and buy a \
free audit via `bbox_audit`. Log a gap via `bbox_packet_gap` when \
the AST can't express what you need — whether mechanically (no \
operator fires) or conceptually (a composition works but is \
semantically blunt — e.g., keyword StringContains where you'd \
have wanted regex or synonym matching, Any{} over a long needle \
list where you wanted a generalizer). Fidelity 1.0 on training \
alone doesn't rule out the gap; if the mechanism won't generalize \
to unseen vocabulary, that's the signal the log wants.";

/// Default per-dispatch contract. Deliberately quiet: `bbox_note` is a
/// signal channel for *notable* observations, NOT a per-dispatch ritual, so
/// the baseline neither requires a `done` note nor demands a note per finding.
/// Callers that genuinely need a guaranteed structured sign-off (atoms,
/// workflows, badgey, review arcs) layer a stricter contract on top of this.
/// The only hard part retained is the task_id/scope correlation guidance —
/// when a note *is* emitted it must carry the right keys to land.
pub const DEFAULT_COMPLETION_CONTRACT: &str = "\
If something genuinely notable came up during the work, surface it with a \
`bbox_note` call so the orchestrator doesn't have to re-read \
your transcript. This is a signal channel, not a progress log — if nothing was \
notable, emit nothing. Use the kind that fits:\n\
   • `surprise` — you expected X and found Y.\n\
   • `dispute` — the brief is wrong or contradicts what you found.\n\
   • `blocked` — you could not proceed and why.\n\
   • `followup` — concrete out-of-scope work you noticed (record only; do not \
do it).\n\
   • `assumption` — an ambiguity-resolving judgment a reviewer should know about.\n\
   • `learned` — a reusable project-local fact worth keeping.\n\
A `kind=done` note with a specific one-line acceptance summary (\"verified X \
already handles Y\"; not \"task complete\") is worth emitting when a concise \
result would help the orchestrator act — but it is optional unless this \
dispatch's instructions require one.\n\
\n\
When you do emit a note, include the correlation keys so it lands:\n\
  task_id=<copy `task:` from the `bbox_scope` context block EXACTLY — not the \
project path, not prose, not \"pending\">\n\
  project=<`project` from the `bbox_scope` block, if present>\n\
  bro=<`bro` from the `bbox_scope` block, if present>\n\
  session_id=<`session` from the `bbox_scope` block, if present>";

/// Milestone-reporting directive for every dispatch. Empirically,
/// only brodex agents called `bro_report` mid-run across 12+ fleet
/// cockpit dispatches — GLM/DeepSeek/MiniMax rows stayed blank the
/// entire run, leaving the cockpit blind. The reporting instruction in
/// the rendered AGENTS.md is session-start-only and can decay at depth on
/// weaker models, but per-turn injection is too noisy on Codex/Brodex because
/// it appears after ordinary tool calls. Positioned late (after the completion
/// contract, before workspace-tools) per repo convention so it stays in
/// attention without repeating every turn.
/// Wording is deliberately terse — shorter context survives truncation
/// and the instruction is self-explanatory.
pub const MILESTONE_REPORT_HINT: &str = "\
Report at major milestones via `bro_report` with a one-line status. \
Examples: starting implementation, tests passing, blocked on X, work complete.";

/// Workspace-tools appendix injected when `AmbientContext::coerce_workspace`
/// is true. Teaches agents to prefer workspace-scoped tool surfaces over
/// raw filesystem access. References the implemented workspace tool surface
/// (`work_smart_read`, `work_bash`, `work_git_*`) and the safe fallback
/// (`bbox_note(kind=learned)`) when those tools are not available in a
/// narrowed tool catalog.
pub const WORKSPACE_TOOLS_APPENDIX: &str = "\
[workspace-tools mode]\n\
You are in workspace-tools mode. Prefer workspace-scoped tool surfaces over \
raw filesystem access:\n\
  - Treat injected project docs/tool guidance as already-present context, not \
as a separate read-instructions ceremony. Open source files only when the next \
step needs exact lines, the injected copy appears stale, or the file was not \
injected.\n\
  - Start with the agentic grounding sequence: accept injected context and scope; \
run sandbox grounding; use blackbox retrieval/evidence bundling when claims \
depend on prior decisions, design docs, threads, code graph facts, or \
conversation history; then edit/validate in the grounded cwd.\n\
  - For the sandbox boundary, call `sandbox_grounding` when available. It \
returns the launch manifest for the harness root. Worktree creation is \
host-owned: fleet dispatch and workflow ops create isolated worktrees \
mechanically before launching editable sessions.\n\
  - For blackbox evidence, use the opening sequence instead of memory when \
provenance matters: `bbox_describe_schema` once per session, \
`bbox_hybrid_search`, `bbox_inspect_entity`, conditional `bbox_find_paths`, then \
`bbox_bundle_evidence` before making provenance-sensitive claims. The detailed \
question-shape runbook is `sm-agentic-opening-sequence`; pull it only when the \
injected tool guidance is insufficient. Use `property_mode=\"summary\"` when \
bundling broad tool/knowledge refs or other long entities. For fresh \
probe/retro evidence, if \
hybrid search returns only generic seeds, no results, or a degraded \
BM25-only/vector-warming notice, pivot to `bbox_notes`/`bbox_gaps` with exact \
task, project, bro, or short substrings before broadening to git/filesystem \
evidence. If `bbox_describe_schema` reports `project_file` / `project_file_v2` \
population `0`, do not investigate or patch indexing as part of a sandbox \
probe. State the corpus gap, dedupe/file a `sandbox-observability` gap if one \
does not already exist, and use `work_smart_read` or scoped file reads for exact \
code locations while still bundling any non-code bbox evidence that resolved \
cleanly.\n\
  - For authorial work, pick the matching primitive: `exec` to run a JS/TS cell \
that composes tool calls in-process — the whole tool surface is available as the \
typed `tools.*` namespace, `text(...)` emits output to your context, and \
`store(...)`/`load(...)` persist values across cells in the session; `wait` to \
resume a still-running cell by `cell_id`; \
the `code.*`/`java.*`/`edits.*`/`analysis.*`/`lsp.*` cell bindings for \
structural refactor and code-navigation work; \
`bro_exec` then `bro_wait`/`bro_status`/`bro_resume` for ad-hoc child agents. \
A cell's nested `tools.X(...)` call dispatches the same filtered tool the flat \
surface exposes (the deny policy is honored in-box). If a child bro completes \
with an empty/suspicious result, or a long-running cell stays `tool_running` \
after a wait timeout, call \
`bro_status(tail=N)` before resuming, cancelling, or filing a gap.\n\
  - Prefer `work_smart_read` over `Read` for file inspection.\n\
  - Prefer `work_bash` over `Bash` for shell commands.\n\
  - Prefer `work_git_status` / `work_git_diff` / `work_git_log` over \
bare `Bash(\"git …\")` invocations.\n\
Treat the harness launch root as authoritative for generic file, shell, and git \
tools. Do not create worktrees from inside the agent session; use fleet/workflow \
mechanical worktree creation before dispatch.\n\
When a workspace tool is not available in the current session, fall back to \
the standard tool and emit `bbox_note(kind=learned, body=\"work_* unavailable, \
used <standard_tool> as fallback\")` so the orchestrator can track coverage.\n\
Do NOT implement `work_*` handlers yourself; they are provided by the host.\n\
Do NOT add new workspace tool names under the `bbox_*` namespace.";

/// The workload-retrospective probe prompt, injected as a fake user turn
/// when a bro's own session is resumed by `bro_prune(retro=true)` or
/// `bro_retro`. It invites — but never compels — a `bbox_gap` substrate-gap
/// report about friction with the *blackbox substrate itself*.
///
/// The wording is the policy: there is no rule engine deciding what's
/// worth filing, so the prompt alone has to calibrate the bro's judgment
/// without skewing it. Two failure modes it steers between:
///   - **Compulsion** (a quota or "silence = incomplete" framing) →
///     manufactured friction, inbox noise.
///   - **Discouragement** ("only if it's a big deal") → real gaps go
///     unreported.
/// Levers: both file/don't-file outcomes are explicitly blameless; the
/// bar is concreteness/utility, not count; all evaluative stakes are
/// stripped; the anti-pattern is named out loud. A hard scope boundary
/// keeps it to surfaces blackbox can actually change (its MCP tools,
/// guidance, orchestration) and explicitly waves off the target repo,
/// language toolchain, and external services — those would be noise.
///
/// Field names and the `gap_kind` enum are pinned to `gaps.rs` (`GapKind`);
/// an unknown gap_kind or malformed dedupe_key would fail `bbox_gap`.
/// Deliberately NOT routed through `apply_ambient` — its recall /
/// task-shape nudges miscue a reflection turn (see `workload_retro_prompt`).
pub const WORKLOAD_RETRO_PROMPT: &str = "\
Quick retrospective — the task itself is done, nothing more is needed on it.\n\
\n\
While it's still fresh, give a short sandbox-focused assessment of the run. \
Think only about the blackbox tooling you worked through — the bbox_/bro_/work_ \
MCP tools, the sandbox/worktree boundary, the guidance and memories blackbox \
gave you, and the workflow or dispatch path it ran you through.\n\
\n\
First answer these sandbox questions in prose, even if the answers are boring:\n\
  • Did you know which cwd/base repo/managed worktree you were operating in, \
and which roots were writable?\n\
  • Could you tell which project docs, provider/account/session env, and MCP \
surface shaped the sandbox? Did `sandbox_status` or an equivalent manifest make \
that easy?\n\
  • When the task depended on prior decisions, design docs, threads, code graph \
facts, or history, was the blackbox opening sequence and `bbox_bundle_evidence` \
path clear enough to ground claims? If you skipped it, was that because the task \
did not need provenance or because the path was awkward?\n\
  • Could an outside observer reconstruct the important file reads/writes, \
shell commands, denials, cwd changes, env overrides, and tool calls from the \
available output?\n\
  • Did the sandbox encourage native idioms such as `note()`/`hybrid_search()`/\
`smart_read()`/`work_bash()`, or did you have to translate through awkward \
outside-daemon forms?\n\
\n\
Then, only if something concrete stands out, consider filing gaps for:\n\
  • a bbox_/bro_/work_ tool you reached for that didn't exist, or one that \
existed but fought you — missing parameter, awkward output, wrong shape;\n\
  • sandbox grounding that was missing or hard to trust — cwd/base repo/managed \
worktree, writable roots, durable project scope, provider/session env, MCP \
surface, denied paths, file writes, or shell commands weren't visible enough;\n\
  • a sandbox-native idiom that would have been clearer than the outside-daemon \
form you had to use, e.g. `note()`/`hybrid_search()`/`smart_read()`/`work_bash()` \
instead of a fully-qualified MCP/tool spelling;\n\
  • an evidence-bundling or blackbox-opening-sequence step that was unclear, \
too easy to skip, or awkward to complete before making a provenance-sensitive \
claim;\n\
  • something blackbox told you — a system memory, a rendered convention, a \
runbook — that was missing, stale, or actively misleading;\n\
  • a blackbox workflow or dispatch step that was clumsier than it should be.\n\
\n\
Scope matters: this channel is only for things blackbox itself can change — its \
own tools, guidance, and orchestration. It is NOT for the project you were \
working on, its compiler or language toolchain, or external services. If what \
fought you was rustc, a flaky API, or a gap in the target repo's own docs — \
that's real, but it's out of scope here, so skip it.\n\
\n\
If something concrete and in-scope stands out — something a future agent or the \
operator would genuinely be glad blackbox knew — file one gap per distinct gap \
with bbox_gap:\n\
  • title: one-line summary  (required)\n\
  • gap_kind: one of mcp_surface, tooling, workflow, agent, docs_runbook, \
refactor_primitive, ontology, eval_coverage  (required) — mcp_surface for a \
missing/awkward bbox_/bro_/work_ tool or sandbox-native alias, tooling for a \
missing sandbox observation primitive, docs_runbook for missing/stale guidance \
or memory, workflow for dispatch/orchestration friction;\n\
  • domain: the blackbox subsystem it touches, e.g. orchestration, knowledge, \
transcripts, refactor, harness/sandbox  (required)\n\
  • wanted_capability: what you wished existed, concretely  (required)\n\
  • dedupe_key as \"<gap_kind>/<domain>/<slug>\" so duplicates from other runs \
collapse, e.g. \"mcp_surface/transcripts/regex-search\"  (required)\n\
  • optional but helpful: missing_primitive; impact (low|medium|high|critical); \
fallback_used; notes.\n\
Run bbox_gaps first to dedupe — an open gap with the same dedupe_key collapses \
automatically. Keep each gap specific enough to act on.\n\
\n\
If nothing in-scope stands out, that's a completely normal way for a run to \
end — say so and file nothing. No quota, no expectation; a quiet run is a good \
run. File only when you'd genuinely want someone to see it, and please don't \
manufacture friction just to have something to say.\n\
\n\
Return this shape:\n\
Sandbox assessment: clear / mixed / unclear — one sentence.\n\
Observation surface: what was visible enough, and the biggest missing surface \
if any.\n\
Friction: none, nitpick, wishlist, or actionable gap filed.\n\
Gaps filed: list dedupe keys, or `none`.";

/// Build the workload-retro probe prompt with a minimal `[scope]` block so
/// any gap note the bro files carries the session/project correlation keys
/// and lands in `bbox_inbox` attributed correctly. Deliberately bypasses
/// `apply_ambient`: the recall directive and task-shape (packet) nudge it
/// injects would miscue a reflection turn, and the retro prompt already
/// names the exact `bbox_note` call it wants.
pub fn workload_retro_prompt(session_id: &str, project: Option<&str>) -> String {
    let mut scope = format!("[scope] session:{session_id}");
    if let Some(p) = project {
        scope.push_str(" · project:");
        scope.push_str(p);
    }
    format!("{scope}\n\n{WORKLOAD_RETRO_PROMPT}")
}

/// Pre-bound context the daemon has at dispatch time but the executor
/// would otherwise have to infer by reaching back through the prompt.
/// Emitting these into the prefix lets notes, thread links, and work-
/// item attribution land correctly on the first attempt.
#[derive(Debug, Clone, Default)]
pub struct AmbientContext {
    /// Daemon-generated dispatch task ID. Stable pre-spawn and across
    /// all providers, regardless of when each provider emits its own
    /// session ID. Used as the primary correlation key for notes:
    /// agents copy the `task:` scope value into `bbox_note.task_id`.
    pub task_id: Option<String>,
    pub session_id: Option<String>,
    pub project_dir: Option<String>,
    pub bro_name: Option<String>,
    pub thread_id: Option<String>,
    pub work_item_id: Option<String>,
    /// Scoped active-arc guidance injected from bbox_pin. Persisted on disk,
    /// hot only for matching ambient scopes, never rendered into repo memory.
    pub pin_block: Option<String>,
    /// Per-dispatch expectation, e.g. "call bbox_note(kind='done', body='…') before returning".
    pub completion_contract: Option<String>,
    pub allow_recursion: bool,
    /// Target provider. When set and the provider supports dispatch-time
    /// tool filtering (Claude/Copilot), the text recursion guard is
    /// omitted in favor of the mechanical filter applied at the CLI arg
    /// layer. Unset or unsupported provider → text guard as fallback.
    #[allow(dead_code)]
    // reserved hook for defense-in-depth text guards; see comment above apply_ambient
    pub provider: Option<providers::Provider>,
    /// Inject workspace-tools appendix. When true, `apply_ambient` appends
    /// the WORKSPACE_TOOLS_APPENDIX after the completion contract, teaching
    /// the agent to prefer work_smart_read / work_bash / work_git_* over
    /// raw filesystem access. Sourced from brofile `coerce_workspace` or
    /// per-dispatch ExecParams/ResumeParams override. Default off.
    pub coerce_workspace: bool,
}

/// Retrieval-read tools whose `project` param is a pure result *filter*
/// (gap-ae22a6b2 item 2, operator-approved). Eliding it means "unscoped
/// search"; defaulting it to the dispatch cwd scopes results to the project
/// the agent is working in. The model keeps an explicit unscoped escape
/// hatch: `resolve_project_filter` resolves an empty/whitespace `project`
/// to None. Knowledge/note/learn `project` params must NEVER appear here —
/// absence there means *global write scope*
/// (design/bro-harness/tool-arg-defaulting.md §3.1).
const RETRIEVAL_PROJECT_DEFAULT_TOOLS: &[&str] =
    &["bbox_hybrid_search", "bbox_discover_seed_entities"];

/// Gap-store tools whose `project` param is write-TARGETING, not write scope
/// (gap-b94129ba, operator-approved): the adapter resolves it through
/// `resolve_gap_project`, so a worktree dispatch cwd redirects the repo-owned
/// gap file into the session's own checkout while the gap's durable project
/// never changes. Defaulting it from the dispatch cwd is therefore safe in a
/// way the knowledge/note/learn `project` params (§3.1: absence = global
/// write scope) are not: on `bbox_gap`, `scope="global"` wins over any
/// supplied/defaulted `project` (the store drops both project and write
/// target), and on resolve/update a global gap ignores the param entirely.
/// `bbox_gaps` (list) is deliberately absent — its `project` is a result
/// filter where None means "all projects".
const GAP_WRITE_TARGET_DEFAULT_TOOLS: &[&str] =
    &["bbox_gap", "bbox_gap_resolve", "bbox_gap_update"];

impl AmbientContext {
    /// Pending session IDs (non-Claude providers before the CLI emits
    /// one) carry no useful linkage — omit rather than leak the literal
    /// "pending" into the prefix.
    fn session_field(&self) -> Option<&str> {
        match self.session_id.as_deref() {
            Some("pending") | Some("") | None => None,
            Some(s) => Some(s),
        }
    }

    pub fn tool_arg_defaults(&self) -> Option<BTreeMap<String, String>> {
        let mut defaults = BTreeMap::new();
        if let Some(session_id) = self.session_field() {
            defaults.insert(
                "default:mcp.bbox_note.session_id".to_string(),
                session_id.to_string(),
            );
        }
        // Coordination-id default (gap-ae22a6b2 item 2, operator-approved):
        // `bro_report.task_id` is the dispatch's own task — eliding it today
        // is a schema error, so filling the ambient id is pure recovery.
        // bbox_thread ids are deliberately NOT defaulted: the table is
        // per-(tool,param), not per-action, and `resolve_thread_id` prefers
        // `id` over `name` — a filled `id` would shadow name-based
        // continue/resolve and convert missing-id errors on resolve/promote/
        // rename into silent mutations of the ambient thread.
        if let Some(task_id) = self
            .task_id
            .as_deref()
            .map(str::trim)
            .filter(|t| !t.is_empty())
        {
            defaults.insert(
                "default:mcp.bro_report.task_id".to_string(),
                task_id.to_string(),
            );
        }

        let cwd = self
            .project_dir
            .as_deref()
            .map(str::trim)
            .filter(|p| !p.is_empty());

        // Worktree confinement pin (design/bro-harness/tool-arg-defaulting.md
        // §5): when the dispatch cwd is a daemon-managed worktree, pin every
        // tool's `cwd` and `project_dir` params to the canonical worktree
        // root. A model passing a different tree (usually the primary
        // checkout) is confused; the pin refuses with an explanatory error
        // instead of letting the call land in the wrong tree. BOTH names are
        // pinned because the pin guards by the literal param key present in
        // the tool input, and the table applies in the harness BEFORE the
        // daemon's serde alias normalization: dispatch tools advertise `cwd`
        // (canonical, gap-6366c92d) but still accept `project_dir` as a
        // deprecated alias, so a single-name pin would let the other
        // spelling sail past. Safe as globs because the project-scoped
        // coordination tools (notes/knowledge) take `project`, not
        // `cwd`/`project_dir` — see the schema-drift tripwire test. Plain
        // repo dispatches (`.git` directory) never pin.
        let worktree = cwd.and_then(worktree_pin_target);
        if let Some(worktree) = &worktree {
            for key in ["pin:*.cwd", "pin:*.project_dir"] {
                defaults.insert(key.to_string(), worktree.to_string_lossy().into_owned());
            }
        }

        // Retrieval-read + code-nav scope defaults (gap-ae22a6b2 item 2,
        // operator-approved). Read-scoped params only: eliding `project` on
        // a retrieval search merely means "unscoped", and the model can
        // still request an unscoped search explicitly — `resolve_project_filter`
        // treats an empty/whitespace `project` as None. The knowledge/note/
        // learn `project` params stay excluded PERMANENTLY (§3.1: absence
        // there means *global write scope*); see the exclusion test.
        if let Some(cwd) = cwd {
            // Raw dispatch cwd, canonicalized. Worktree paths are correct
            // here because the server side resolves worktree/descendant
            // paths to the registered base project via
            // `resolve_base_project_for_scope`.
            let canonical_cwd = std::path::Path::new(cwd)
                .canonicalize()
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_else(|_| cwd.to_string());
            for tool in RETRIEVAL_PROJECT_DEFAULT_TOOLS {
                defaults.insert(format!("default:mcp.{tool}.project"), canonical_cwd.clone());
            }
            // Gap write-targeting defaults (gap-b94129ba): same gating as the
            // retrieval defaults — only filled when the model elides the
            // param, and `scope="global"` / global gaps still win server-side.
            for tool in GAP_WRITE_TARGET_DEFAULT_TOOLS {
                defaults.insert(format!("default:mcp.{tool}.project"), canonical_cwd.clone());
            }
        }
        (!defaults.is_empty()).then_some(defaults)
    }
}

/// Merge tool argument defaults from broadest to most specific scope.
///
/// Ambient defaults describe the active task/session. Brofile defaults are
/// durable persona grants, and per-dispatch defaults are operator grants for
/// one invocation.
pub fn merge_tool_arg_defaults(
    ambient: Option<BTreeMap<String, String>>,
    brofile: Option<&BTreeMap<String, String>>,
    per_dispatch: Option<&BTreeMap<String, String>>,
) -> Option<BTreeMap<String, String>> {
    let mut merged = ambient.unwrap_or_default();
    for defaults in [brofile, per_dispatch].into_iter().flatten() {
        merged.extend(
            defaults
                .iter()
                .map(|(key, value)| (key.clone(), value.clone())),
        );
    }
    (!merged.is_empty()).then_some(merged)
}

impl AmbientContext {
    /// Serialize this dispatch's typed ingredients — persona (brofile lens),
    /// the selected directive set with declared cadence, the scope fields,
    /// and the pin block — into the `--dispatch-context` payload
    /// (dispatch-prompt-slots.md §4/§6). The harness owns composition;
    /// nothing here is prompt text.
    ///
    /// Recursion guarding (blocking sub-bro dispatch) stays mechanical via
    /// provider tool-filter args appended to argv outside this function; no
    /// text recursion guard is emitted.
    ///
    /// Cadence declarations carry the empirical calibration the old glued
    /// preamble encoded positionally. Directives are standing by default;
    /// recurring behavioral nudges belong in the harness HookEngine/NudgeLedger
    /// so they can be triggered and throttled by actual turn state. `contract` and
    /// `milestone` declare `needs_scope`: their texts reference the
    /// `bbox_scope` correlation keys, so the harness drops them whenever no
    /// current scope exists.
    pub fn dispatch_context(&self, lens: Option<&str>) -> bro_protocol::DispatchContext {
        use bro_protocol::{DirectiveCadence, DispatchDirective, DispatchScope};

        let directive = |id: &str, cadence: DirectiveCadence, needs_scope: bool, text: &str| {
            DispatchDirective {
                id: id.to_string(),
                cadence,
                needs_scope,
                text: text.to_string(),
            }
        };
        let mut directives = vec![
            directive(
                "recall",
                DirectiveCadence::Standing,
                false,
                RECALL_DIRECTIVE,
            ),
            directive(
                "task_shape",
                DirectiveCadence::Standing,
                false,
                TASK_SHAPE_HINT,
            ),
        ];
        // allow_recursion ⇒ this agent is a fan-out orchestrator. Surface the
        // packet primitive — the most common silent miss for these agents is
        // writing a prose rubric and pasting it into N identical sub-agent
        // prompts.
        if self.allow_recursion {
            directives.push(directive(
                "orchestrator",
                DirectiveCadence::Standing,
                false,
                ORCHESTRATOR_HINT,
            ));
        }
        if let Some(contract) = self
            .completion_contract
            .as_deref()
            .map(str::trim_end)
            .filter(|c| !c.is_empty())
        {
            directives.push(directive(
                "contract",
                DirectiveCadence::Standing,
                true,
                contract,
            ));
        }
        directives.push(directive(
            "milestone",
            DirectiveCadence::Standing,
            true,
            MILESTONE_REPORT_HINT,
        ));
        if self.coerce_workspace {
            directives.push(directive(
                "workspace",
                DirectiveCadence::Standing,
                false,
                WORKSPACE_TOOLS_APPENDIX,
            ));
        }

        let scope = DispatchScope {
            task: self.task_id.clone(),
            session: self.session_field().map(str::to_string),
            project: self.project_dir.clone(),
            bro: self.bro_name.clone(),
            thread: self.thread_id.clone(),
            work_item: self.work_item_id.clone(),
        };

        bro_protocol::DispatchContext {
            v: bro_protocol::DISPATCH_CONTEXT_VERSION,
            persona: lens
                .map(str::trim)
                .filter(|l| !l.is_empty())
                .map(str::to_string),
            directives,
            scope: (!scope.is_empty()).then_some(scope),
            pins: self
                .pin_block
                .as_deref()
                .map(str::trim_end)
                .filter(|p| !p.is_empty())
                .map(str::to_string),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BroSpawnError {
    /// The startup snapshot could not be recovered safely. Refuse new work
    /// before executor admission because its task identity cannot be persisted.
    TaskStoreUnavailable,
    DuplicateTaskId {
        id: String,
    },
    #[allow(dead_code)] // only constructed by the test-only `insert` method
    ReservedTaskId {
        id: String,
    },
}

impl std::fmt::Display for BroSpawnError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::TaskStoreUnavailable => write!(
                f,
                "task admission refused: task persistence is disabled after snapshot recovery failed; repair the configured task store and restart before dispatching new work"
            ),
            Self::DuplicateTaskId { id } => write!(f, "duplicate task id: {id}"),
            Self::ReservedTaskId { id } => write!(f, "task id is already reserved: {id}"),
        }
    }
}

impl std::error::Error for BroSpawnError {}

pub struct SpawnTaskParams {
    pub provider: Provider,
    pub args: Vec<String>,
    /// Provider session id to record. Harness-backed fresh dispatch normally
    /// pre-mints this in the daemon and passes it to the harness; legacy or
    /// provider-discovered paths may still use a temporary placeholder such as
    /// `pending` until the first stream event.
    pub session_id: String,
    pub cwd: Option<String>,
    pub env_overrides: Option<HashMap<String, String>>,
    pub store_dir: std::path::PathBuf,
    pub task_store: Arc<RwLock<TaskStore>>,
    pub tail_tx: tokio::sync::broadcast::Sender<tail::TailEvent>,
    pub roster_events: Option<RosterEventSink>,
    pub bro_label: Option<String>,
    pub agent_label: Option<String>,
    /// System event hub for emitting task lifecycle events. Task events
    /// are observation-only: emit failures are logged but do not affect
    /// task dispatch.
    pub system_events: Option<crate::system_events::SharedEventHub>,
    /// Spawn-time origin classification (Slice 1b). Determines which
    /// roster tab the task lands in. Defaults to `Unknown` at the field
    /// boundary so test helpers that build `SpawnTaskParams` directly
    /// don't have to spell it out — production spawn callers MUST set
    /// the right variant (the audit log lives in the Slice 1b
    /// dispatch note).
    pub origin: bro_core::Origin,
}

/// Dispatch against a task id the caller minted itself.
///
/// Differs from [`spawn_task_with_tool_placement`] only in duplicate policy: a
/// pre-minted id that is already claimed is an ERROR here, not a silent
/// return of the existing task, because a caller that minted its own id and
/// finds it taken has a real bug rather than a retry. Everything downstream,
/// including the choice of executor seam, is identical: harness workers become
/// fleetd children on this path exactly as on every other.
pub async fn spawn_with_pre_minted_id(
    task_id: String,
    params: SpawnTaskParams,
) -> Result<Arc<Task>, BroSpawnError> {
    params.task_store.write().reserve_id(&task_id)?;
    Ok(spawn_reserved_dispatch(task_id, params, None, None).await)
}

fn failed_duplicate_task(
    task_id: String,
    provider: Provider,
    session_id: String,
    cwd: Option<String>,
    bro_label: Option<String>,
    agent_label: Option<String>,
    message: String,
    origin: bro_core::Origin,
) -> Arc<Task> {
    Arc::new(Task {
        inner: Mutex::new(TaskInner {
            id: task_id,
            provider,
            session_id,
            events: EventRing::new(),
            model: None,
            last_assistant_message: None,
            usage: None,
            cost_usd: None,
            num_turns: None,
            last_turn_input_tokens: None,
            context_window: None,
            stderr: message,
            status: TaskStatus::Failed,
            started_at: now_ms(),
            completed_at: Some(now_ms()),
            exit_code: None,
            managed_worktree: managed_worktrees::managed_worktree_for_cwd(cwd.as_deref()),
            cwd,
            bro_label,
            name: None,
            agent_label,
            report: None,
            interrupted: false,
            recoverable: false,
            transcript_location: None,
            transcript_cursor: None,
            live_cursor: 0,
            harness_ingest_seq: 0,
            last_delta_roster_emit_ms: 0,
            supervision: SupervisionState::default(),
            origin,
            workflow_owned: workflow_owned_for_origin(origin),
            project_id: None,
        }),
        notify: Arc::new(Notify::new()),
        child_id: Mutex::new(None),
        roster_events: None,
    })
}

/// Create a tracked task for daemon-internal async work. This mirrors
/// provider-backed tasks closely enough that `bro_status`, `bro_wait`,
/// dashboards, persistence, and tail subscribers can observe it.
///
/// `origin` (Slice 1b) is propagated so the daemon-internal harness
/// tasks (workflow executor / atom) carry the same origin label as
/// the spawn site that called them.
pub fn spawn_in_process_task(
    task_id: String,
    provider: Provider,
    session_id: String,
    cwd: Option<String>,
    store_dir: std::path::PathBuf,
    task_store: Arc<RwLock<TaskStore>>,
    tail_tx: tokio::sync::broadcast::Sender<tail::TailEvent>,
    roster_events: Option<RosterEventSink>,
    bro_label: Option<String>,
    agent_label: Option<String>,
    system_events: Option<crate::system_events::SharedEventHub>,
    origin: bro_core::Origin,
) -> Arc<Task> {
    if let Err(err) = task_store.write().reserve_id(&task_id) {
        if let Some(existing) = task_store.read().get(&task_id) {
            return existing;
        }
        let failed = failed_duplicate_task(
            task_id,
            provider,
            session_id,
            cwd,
            bro_label,
            agent_label,
            err.to_string(),
            origin,
        );
        failed.notify.notify_waiters();
        return failed;
    }

    let task = Arc::new(Task {
        inner: Mutex::new(TaskInner {
            id: task_id.clone(),
            provider,
            session_id: session_id.clone(),
            events: EventRing::new(),
            model: None,
            last_assistant_message: None,
            usage: None,
            cost_usd: None,
            num_turns: None,
            last_turn_input_tokens: None,
            context_window: None,
            stderr: String::new(),
            status: TaskStatus::Running,
            started_at: now_ms(),
            completed_at: None,
            exit_code: None,
            managed_worktree: managed_worktrees::managed_worktree_for_cwd(cwd.as_deref()),
            cwd,
            bro_label,
            name: None,
            agent_label,
            report: None,
            interrupted: false,
            recoverable: false,
            transcript_location: None,
            transcript_cursor: None,
            live_cursor: 0,
            harness_ingest_seq: 0,
            last_delta_roster_emit_ms: 0,
            supervision: SupervisionState::default(),
            origin,
            workflow_owned: workflow_owned_for_origin(origin),
            project_id: None,
        }),
        notify: Arc::new(Notify::new()),
        child_id: Mutex::new(None),
        roster_events: roster_events.clone(),
    });

    if let Err(err) = task_store
        .write()
        .insert_reserved(task_id.clone(), task.clone())
    {
        task_store.write().release_reservation(&task_id);
        let failed = failed_duplicate_task(
            task_id,
            provider,
            session_id,
            task.inner.lock().cwd.clone(),
            None,
            None,
            err.to_string(),
            origin,
        );
        failed.notify.notify_waiters();
        return failed;
    }
    task.emit_roster_added();
    request_persist(&task_store, &store_dir);
    let task_id_ev = task_id.clone();
    let bro_ev = task.inner.lock().bro_label.clone();
    let provider_str = provider.to_string();
    let cursor = task.next_live_cursor();
    let _ = tail_tx.send(tail::TailEvent::TaskStarted {
        cursor,
        task_id,
        provider,
        bro_name: None,
    });
    // Emit task.started system event. Observation-only: failures logged, not propagated.
    if let Some(hub) = system_events {
        tokio::spawn(async move {
            let mut correlation = serde_json::Map::new();
            correlation.insert("task_id".into(), serde_json::json!(task_id_ev));
            let draft = crate::system_events::SystemEventDraft {
                kind: crate::system_events::types::SystemEventKind::TaskStarted,
                producer: "orchestration.dispatch".to_string(),
                project: None,
                principal: None,
                subject: None,
                correlation,
                causation_id: None,
                payload: serde_json::json!({
                    "task_id": task_id_ev,
                    "provider": provider_str,
                    "bro": bro_ev,
                }),
            };
            if let Err(e) = hub.emit(draft).await {
                tracing::warn!("task.started system event emit failed: {e:#}");
            }
        });
    }
    task
}

fn append_task_event(inner: &mut TaskInner, event: Value) -> tail::TailEvent {
    update_model_cache_from_event(inner, &event);
    inner.live_cursor += 1;
    let cursor = inner.live_cursor;
    let task_id = inner.id.clone();
    inner.events.push(event.clone());
    tail::TailEvent::TaskEvent {
        cursor,
        task_id,
        event,
    }
}

pub fn push_in_process_event(
    task: &Task,
    event: Value,
    tail_tx: &tokio::sync::broadcast::Sender<tail::TailEvent>,
) {
    let tail_event = {
        let mut inner = task.inner.lock();
        append_task_event(&mut inner, event)
    };
    let _ = tail_tx.send(tail_event);
    task.emit_roster_updated();
}

pub fn finish_in_process_task(
    task: &Task,
    status: TaskStatus,
    result: Option<String>,
    stderr: Option<String>,
    task_store: &RwLock<TaskStore>,
    store_dir: &std::path::Path,
    tail_tx: &tokio::sync::broadcast::Sender<tail::TailEvent>,
    system_events: Option<crate::system_events::SharedEventHub>,
) {
    // Resolve the durable transcript handle (the harness session event log)
    // before the terminal state is persisted, so finished task records carry
    // their transcript_location without needing a later status read.
    populate_transcript_handle(task);
    let mut inner = task.inner.lock();
    if let Some(result) = result {
        inner.last_assistant_message = Some(result);
    }
    if let Some(stderr) = stderr {
        inner.stderr.push_str(&stderr);
    }
    let status = if status == TaskStatus::Completed && inner.interrupted {
        TaskStatus::Cancelled
    } else {
        status
    };
    inner.status = status;
    inner.completed_at = Some(now_ms());
    let task_id = inner.id.clone();
    let elapsed = format_elapsed(inner.started_at, inner.completed_at);
    let cost = inner.cost_usd;
    let source_session = inner.session_id.clone();
    let task_kind = inner.bro_label.clone();
    let error: String = inner.stderr.chars().take(200).collect();
    inner.live_cursor += 1;
    let cursor = inner.live_cursor;
    drop(inner);
    task.emit_roster_updated();

    match status {
        TaskStatus::Completed => {
            let _ = tail_tx.send(tail::TailEvent::TaskCompleted {
                cursor,
                task_id: task_id.clone(),
                elapsed: elapsed.clone(),
                cost,
                source_session,
                task_kind,
            });
        }
        TaskStatus::Failed => {
            let _ = tail_tx.send(tail::TailEvent::TaskFailed {
                cursor,
                task_id: task_id.clone(),
                elapsed: elapsed.clone(),
                error: error.clone(),
            });
        }
        TaskStatus::Cancelled => {
            let _ = tail_tx.send(tail::TailEvent::TaskCancelled {
                cursor,
                task_id: task_id.clone(),
                elapsed: elapsed.clone(),
            });
        }
        TaskStatus::Running => {}
    }
    // Emit terminal system event. Observation-only: failures logged, not propagated.
    if let Some(hub) = system_events {
        let task_id_ev = task_id.clone();
        let elapsed_ev = elapsed.clone();
        let (kind, payload) = match status {
            TaskStatus::Completed => (
                crate::system_events::types::SystemEventKind::TaskCompleted,
                serde_json::json!({"task_id": task_id_ev, "elapsed": elapsed_ev, "cost_usd": cost}),
            ),
            TaskStatus::Failed => (
                crate::system_events::types::SystemEventKind::TaskFailed,
                serde_json::json!({"task_id": task_id_ev, "elapsed": elapsed_ev, "error": error}),
            ),
            TaskStatus::Cancelled => (
                crate::system_events::types::SystemEventKind::TaskCancelled,
                serde_json::json!({"task_id": task_id_ev, "elapsed": elapsed_ev}),
            ),
            TaskStatus::Running => {
                // No terminal event for running state.
                request_persist(task_store, store_dir);
                task.notify.notify_waiters();
                return;
            }
        };
        let mut correlation = serde_json::Map::new();
        correlation.insert("task_id".into(), serde_json::json!(task_id_ev));
        let draft = crate::system_events::SystemEventDraft {
            kind,
            producer: "orchestration.dispatch".to_string(),
            project: None,
            principal: None,
            subject: None,
            correlation,
            causation_id: None,
            payload,
        };
        tokio::spawn(async move {
            if let Err(e) = hub.emit(draft).await {
                tracing::warn!("task terminal system event emit failed: {e:#}");
            }
        });
    }
    request_persist(task_store, store_dir);
    task.notify.notify_waiters();
}

/// Emit a `task.progress` system event for one deduplicated snippet.
/// Spawns a background task; observation-only — failures are logged but do not
/// affect streaming.
pub(crate) fn emit_task_progress_event(
    hub: &crate::system_events::SharedEventHub,
    task_id: String,
    activity: String,
) {
    let hub = hub.clone();
    tokio::spawn(async move {
        let mut correlation = serde_json::Map::new();
        correlation.insert("task_id".into(), serde_json::json!(task_id));
        let draft = crate::system_events::SystemEventDraft {
            kind: crate::system_events::types::SystemEventKind::TaskProgress,
            producer: "orchestration.dispatch".to_string(),
            project: None,
            principal: None,
            subject: None,
            correlation,
            causation_id: None,
            payload: serde_json::json!({
                "task_id": task_id,
                "activity": activity,
            }),
        };
        if let Err(e) = hub.emit(draft).await {
            tracing::warn!("task.progress system event emit failed: {e:#}");
        }
    });
}

/// Spawn a provider CLI process and return a tracked Task.
///
/// `task_id` is pre-generated by the caller so it can be threaded into
/// the ambient `[scope]` block before the subprocess launches. That lets
/// agents emit `bbox_note(task_id=...)` records correlated back to the
/// dispatch regardless of when the provider emits its own session ID.
///
/// `origin` (Slice 1b) labels the spawn site so the fleet roster can
/// tab tasks by source — see `bro_core::Origin` for the taxonomy.
#[allow(clippy::too_many_arguments)]
pub async fn spawn_task(
    task_id: String,
    provider: Provider,
    args: Vec<String>,
    session_id: String,
    cwd: Option<String>,
    env_overrides: Option<HashMap<String, String>>,
    store_dir: std::path::PathBuf,
    task_store: Arc<RwLock<TaskStore>>,
    tail_tx: tokio::sync::broadcast::Sender<tail::TailEvent>,
    roster_events: Option<RosterEventSink>,
    bro_label: Option<String>,
    agent_label: Option<String>,
    system_events: Option<crate::system_events::SharedEventHub>,
    origin: bro_core::Origin,
) -> Arc<Task> {
    spawn_task_with_tool_placement(
        task_id,
        provider,
        args,
        session_id,
        cwd,
        env_overrides,
        store_dir,
        task_store,
        tail_tx,
        roster_events,
        bro_label,
        agent_label,
        None,
        None,
        system_events,
        origin,
    )
    .await
}

/// Resolve the project's opt-in dispatch shell env (fleet.json
/// `project_dispatch`, e.g. `RUSTC_WRAPPER=sccache`) for a task cwd.
///
/// Worktree cwds map to their base repository first (fleet.json keys are
/// canonical repo paths): a linked worktree's `.git` is a file containing
/// `gitdir: <base>/.git/worktrees/<name>`. Best-effort by contract — a
/// missing/malformed fleet.json or unreadable cwd yields None, never an
/// error. This runs once per dispatch at spawn, off the hot path.
fn project_dispatch_shell_env(
    cwd: Option<&str>,
) -> Option<std::collections::BTreeMap<String, String>> {
    let cwd = std::path::Path::new(cwd?);
    let base = worktree_base_repo(cwd).unwrap_or_else(|| cwd.to_path_buf());
    let cfg = bro_fleet_client::FleetConfig::load();
    let env = cfg.project_dispatch_for(&base)?.env.clone();
    (!env.is_empty()).then_some(env)
}

/// Resolve fleet.json `mcpServers` into provider dispatch args for one
/// dispatch. Cockpit-origin only, by policy: operator MCP servers (whose
/// `$secret` refs resolve at injection) reach the agents the operator
/// live-drives from the cockpit — never automation origins
/// (workflow/atom/cron/webhook), which keep their restricted tool surfaces.
///
/// The injection rides the harness CLI argv (`--mcp-config`), so the same
/// args would drive a standalone `bro-harness` subprocess unchanged — the
/// daemon never reaches into the harness `McpConfig` directly
/// (harness-process-boundary.md §2). Best-effort like the dispatch env: a
/// missing/malformed fleet.json injects nothing.
fn fleet_mcp_dispatch_args(provider: Provider, origin: bro_core::Origin) -> Vec<String> {
    if origin != bro_core::Origin::Cockpit {
        return Vec::new();
    }
    let cfg = bro_fleet_client::FleetConfig::load();
    providers::fleet_mcp_args(provider, &cfg.mcp_servers)
}

/// Resolve the worktree-confinement pin target for a dispatch cwd: the
/// canonical worktree root when `cwd` lies inside a daemon-managed worktree,
/// `None` otherwise (plain repos and non-repo dirs never pin).
///
/// ONE mechanical choke point for every dispatch path (bro_exec/bro_resume,
/// agent dispatch, workflow executor dispatch + resume, fleet cockpit — all
/// of which funnel through `AmbientContext::tool_arg_defaults`), rather than
/// per-site emission at each worktree-creation surface. Two structural
/// signals, checked in order:
///
/// 1. cwd under a cockpit-managed parent (`bro_home/{fleet,agent}/worktrees`)
///    — the fleet/agent worktree layout, via `managed_worktrees`.
/// 2. cwd inside a *linked* git worktree (nearest `.git` marker walking up is
///    a file pointing into `<base>/.git/worktrees/<name>`). This is the
///    structural signature of every daemon-created worktree — including
///    workflow `WorktreeCreate` worktrees, which land at arbitrary
///    operator-chosen paths a root-prefix check can't cover. A `.git`
///    *directory* (primary checkout) short-circuits to `None`.
fn worktree_pin_target(cwd: &str) -> Option<std::path::PathBuf> {
    worktree_pin_target_with_roots(
        cwd,
        &crate::managed_worktrees::cockpit_managed_worktree_roots(),
    )
}

fn worktree_pin_target_with_roots(
    cwd: &str,
    managed_roots: &[std::path::PathBuf],
) -> Option<std::path::PathBuf> {
    if let Some(wt) = crate::managed_worktrees::managed_worktree_path_for_cwd(cwd, managed_roots) {
        let wt = wt.canonicalize().unwrap_or(wt);
        return Some(wt);
    }
    let cwd = std::path::Path::new(cwd.trim());
    let cwd = cwd.canonicalize().unwrap_or_else(|_| cwd.to_path_buf());
    let mut current = cwd.as_path();
    loop {
        let dot_git = current.join(".git");
        if dot_git.is_dir() {
            // Primary checkout / plain repo: deliberately no pin.
            return None;
        }
        if dot_git.is_file() {
            // Only the linked-worktree shape qualifies; a malformed .git
            // file fails closed to no pin.
            return worktree_base_repo(current).map(|_| current.to_path_buf());
        }
        current = current.parent()?;
    }
}

/// Map a linked-worktree path to its base repository (the directory whose
/// `.git` *directory* backs the worktree). Returns None for non-worktrees.
/// Thin wrapper over the shared structural parse in
/// [`crate::git::linked_worktree_base`] (also used by the write-side
/// worktree recognition in `crate::projects`).
fn worktree_base_repo(path: &std::path::Path) -> Option<std::path::PathBuf> {
    crate::git::linked_worktree_base(path)
}

#[allow(clippy::too_many_arguments)]
pub async fn spawn_task_with_tool_placement(
    task_id: String,
    provider: Provider,
    args: Vec<String>,
    session_id: String,
    cwd: Option<String>,
    env_overrides: Option<HashMap<String, String>>,
    store_dir: std::path::PathBuf,
    task_store: Arc<RwLock<TaskStore>>,
    tail_tx: tokio::sync::broadcast::Sender<tail::TailEvent>,
    roster_events: Option<RosterEventSink>,
    bro_label: Option<String>,
    agent_label: Option<String>,
    tool_placement: Option<BTreeMap<String, String>>,
    tool_defaults: Option<BTreeMap<String, String>>,
    system_events: Option<crate::system_events::SharedEventHub>,
    origin: bro_core::Origin,
) -> Arc<Task> {
    // Reservation happens HERE, ahead of the provider branch, so both entry
    // points into `spawn_reserved_dispatch` agree on when the id is claimed
    // and each can keep its own duplicate policy. This entry is idempotent:
    // a duplicate dispatch returns the task that already exists.
    if let Err(err) = task_store.write().reserve_id(&task_id) {
        if let Some(existing) = task_store.read().get(&task_id) {
            return existing;
        }
        return failed_duplicate_task(
            task_id,
            provider,
            session_id,
            cwd,
            bro_label,
            agent_label,
            err.to_string(),
            origin,
        );
    }

    spawn_reserved_dispatch(
        task_id,
        SpawnTaskParams {
            provider,
            args,
            session_id,
            cwd,
            env_overrides,
            store_dir,
            task_store,
            tail_tx,
            roster_events,
            bro_label,
            agent_label,
            system_events,
            origin,
        },
        tool_placement,
        tool_defaults,
    )
    .await
}

/// Compose and dispatch a worker for a task id that is ALREADY RESERVED.
///
/// Every dispatch path funnels through here, which is what makes the
/// pre-dispatch treatment uniform: the scratch-cwd fallback, the project
/// dispatch env, the cockpit MCP injection, and the choice of executor seam all
/// happen once, in one place, rather than being re-derived per entry point.
///
/// Precondition: `task_id` is reserved in the store. On any setup failure the
/// reservation is released before returning a failed task, so a caller that
/// retries the same id is not blocked by a stale claim.
#[allow(clippy::too_many_arguments)]
async fn spawn_reserved_dispatch(
    task_id: String,
    params: SpawnTaskParams,
    tool_placement: Option<BTreeMap<String, String>>,
    tool_defaults: Option<BTreeMap<String, String>>,
) -> Arc<Task> {
    let SpawnTaskParams {
        provider,
        args,
        session_id,
        cwd,
        env_overrides,
        store_dir,
        task_store,
        tail_tx,
        roster_events,
        bro_label,
        agent_label,
        system_events,
        origin,
    } = params;
    // A session must never inherit the daemon's process cwd ($HOME under
    // launchd): a dispatch without an explicit cwd used to confine the
    // session's file tools to the operator's home directory and write there
    // (gap-16d79781). Fail closed into a per-task scratch dir instead;
    // callers wanting a real workspace pass cwd/project_dir explicitly.
    let cwd = cwd.or_else(|| {
        let scratch = store_dir.join("scratch").join(&task_id);
        match std::fs::create_dir_all(&scratch) {
            Ok(()) => {
                tracing::warn!(
                    task_id = %task_id,
                    scratch = %scratch.display(),
                    "dispatch without cwd; confining session to a per-task scratch dir"
                );
                Some(scratch.to_string_lossy().to_string())
            }
            Err(err) => {
                tracing::error!(
                    error = %err,
                    "scratch cwd creation failed; session inherits the daemon process cwd"
                );
                None
            }
        }
    });
    // Project-scoped dispatch env (fleet.json project_dispatch): the harness
    // lane carries it as ToolCx::shell_env (shell children only — never the
    // transport/session env); CLI providers get it merged into the child
    // process env below. Resolved here so every dispatch path — bro_exec,
    // agent dispatch, workflows, cockpit — behaves identically.
    let dispatch_shell_env = project_dispatch_shell_env(cwd.as_deref());
    // Cockpit dispatches additionally carry the operator's fleet.json
    // `mcpServers`, injected as `--mcp-config` argv and merged with the
    // daemon's complete self-MCP catalog before the child is spawned.
    let mut args = args;
    args.extend(fleet_mcp_dispatch_args(provider, origin));
    if matches!(
        provider,
        Provider::Glm
            | Provider::Deepseek
            | Provider::Minimax
            | Provider::Kimi
            | Provider::Brodex
            | Provider::VibeBh
    ) {
        return spawn_harness_child_task(
            task_id,
            provider,
            args,
            session_id,
            cwd,
            env_overrides,
            dispatch_shell_env,
            store_dir,
            task_store,
            tail_tx,
            roster_events.clone(),
            bro_label,
            agent_label,
            tool_placement,
            tool_defaults,
            system_events,
            origin,
        )
        .await;
    }

    // Nothing else is dispatchable. Every provider that can back a worker is
    // harness-backed and went through the seam above; `Provider::Workflow` is
    // a pseudo-provider for daemon-internal tasks, which are created by
    // `spawn_in_process_task` and never spawn a child at all (the allocator
    // agrees: `provider_binary_missing` reports it missing unconditionally, so
    // it is never selected as a lane).
    //
    // Reaching here therefore means a brofile literally declared
    // `provider = "workflow"`. That used to try to exec a binary named
    // "workflow" and fail with a bare "No such file or directory"; failing
    // with the actual reason is strictly more useful, and it keeps the
    // invariant this slice establishes: when the executor is fleetd, no
    // harness child is ever a direct daemon child, because there is no
    // inline spawn path left to be one.
    let _ = (args, env_overrides, dispatch_shell_env, tail_tx);
    tracing::error!(
        task_id = %task_id,
        %provider,
        "dispatch requested a non-dispatchable provider; check the brofile"
    );
    failed_harness_child_setup(
        task_id,
        provider,
        session_id,
        cwd,
        store_dir,
        task_store,
        roster_events,
        bro_label,
        agent_label,
        anyhow::anyhow!(
            "`{provider}` is not a dispatchable provider: it backs daemon-internal \
             tasks only and has no worker binary. Set a real provider on the brofile."
        ),
        origin,
    )
}

#[allow(clippy::too_many_arguments)]
async fn spawn_harness_child_task(
    task_id: String,
    provider: Provider,
    args: Vec<String>,
    session_id: String,
    cwd: Option<String>,
    env_overrides: Option<HashMap<String, String>>,
    shell_env: Option<std::collections::BTreeMap<String, String>>,
    store_dir: std::path::PathBuf,
    task_store: Arc<RwLock<TaskStore>>,
    tail_tx: tokio::sync::broadcast::Sender<tail::TailEvent>,
    roster_events: Option<RosterEventSink>,
    bro_label: Option<String>,
    agent_label: Option<String>,
    tool_placement: Option<BTreeMap<String, String>>,
    tool_defaults: Option<BTreeMap<String, String>>,
    system_events: Option<crate::system_events::SharedEventHub>,
    origin: bro_core::Origin,
) -> Arc<Task> {
    let self_mcp_url = std::env::var("BLACKBOX_MCP_URL")
        .ok()
        .filter(|url| !url.is_empty())
        .map(|url| crate::dispatch_mcp::dispatch_mcp_url_for_origin(&url, origin));
    // Resolve workspace identity on the machine that owns cwd, then compose
    // the fully-resolved spawn spec. Remote fleetd therefore returns local
    // facts before the daemon mints any authority and the daemon never opens a
    // worker-local path.
    let workspace_binding_authority = readoption_env()
        .get()
        .and_then(|env| env.workspace_binding_authority.as_deref());
    let spec_result: anyhow::Result<_> = async {
        let workspace_identity = match (cwd.as_deref(), workspace_binding_authority) {
            (Some(cwd), Some(authority)) => {
                let request = bro_protocol::WorkspaceInspectionRequest {
                    cwd: cwd.to_string(),
                    candidate_scopes: authority.candidate_scopes()?,
                };
                match harness_executor().inspect_workspace(request).await? {
                    bro_protocol::WorkspaceInspectionOutcome::Unmanaged => None,
                    bro_protocol::WorkspaceInspectionOutcome::Managed { identity } => {
                        Some(identity)
                    }
                    bro_protocol::WorkspaceInspectionOutcome::Refused { code, message } => {
                        anyhow::bail!("{code}: {message}")
                    }
                }
            }
            _ => None,
        };
        prepare_harness_child_launch(
            task_id.clone(),
            session_id.clone(),
            provider,
            args,
            cwd.as_deref(),
            env_overrides,
            shell_env,
            tool_placement,
            tool_defaults,
            &store_dir,
            self_mcp_url.as_deref(),
            workspace_binding_authority,
            workspace_identity,
        )
    }
    .await;
    let spec = match spec_result {
        Ok(spec) => spec,
        Err(error) => {
            if let Some(authority) = readoption_env()
                .get()
                .and_then(|env| env.workspace_binding_authority.as_ref())
            {
                authority.revoke_task(&task_id);
            }
            return failed_harness_child_setup(
                task_id,
                provider,
                session_id,
                cwd,
                store_dir,
                task_store,
                roster_events,
                bro_label,
                agent_label,
                error,
                origin,
            );
        }
    };

    // The worker keeps its authoritative replay log under worker BRO_HOME.
    // The daemon keeps a receipt mirror under its own BRO_HOME so indexing and
    // daemon-side transcript tools remain local after the corpus moves off
    // host. Same-host execution names the same file and needs no mirror.
    let daemon_event_log_path = store_dir
        .join("harness-sessions")
        .join(format!("{}.events.jsonl", spec.session_id));
    let mirror_event_log_path = harness_worker_locality().map(|_| daemon_event_log_path.clone());
    let transcript_location = harness_transcript_location_from_spec(
        &spec,
        &session_id,
        cwd.as_deref(),
        &daemon_event_log_path,
    );

    // Hand the spec to the executor: it owns the process (login-shell bin
    // resolution, command build, spawn, stdin control writer, stdout/stderr
    // pumps, waiter). The daemon keeps the state half below.
    let handle = match harness_executor().spawn(spec).await {
        Ok(handle) => handle,
        Err(error) => {
            if let Some(authority) = readoption_env()
                .get()
                .and_then(|env| env.workspace_binding_authority.as_ref())
            {
                authority.revoke_task(&task_id);
            }
            task_store.write().release_reservation(&task_id);
            return failed_harness_child_setup(
                task_id,
                provider,
                session_id,
                cwd,
                store_dir,
                task_store,
                roster_events,
                bro_label,
                agent_label,
                error,
                origin,
            );
        }
    };
    let executor::WorkerHandle {
        control,
        events,
        pid,
        killer,
        outcome,
    } = handle;

    let task = Arc::new(Task {
        inner: Mutex::new(TaskInner {
            id: task_id.clone(),
            provider,
            session_id: session_id.clone(),
            events: EventRing::new(),
            model: None,
            last_assistant_message: None,
            usage: None,
            cost_usd: None,
            num_turns: None,
            last_turn_input_tokens: None,
            context_window: None,
            stderr: String::new(),
            status: TaskStatus::Running,
            started_at: now_ms(),
            completed_at: None,
            exit_code: None,
            cwd: cwd.clone(),
            managed_worktree: managed_worktrees::managed_worktree_for_cwd(cwd.as_deref()),
            bro_label,
            name: None,
            agent_label,
            report: None,
            interrupted: false,
            recoverable: false,
            transcript_location,
            transcript_cursor: None,
            live_cursor: 0,
            harness_ingest_seq: 0,
            last_delta_roster_emit_ms: 0,
            supervision: SupervisionState::default(),
            origin,
            workflow_owned: workflow_owned_for_origin(origin),
            project_id: None,
        }),
        notify: Arc::new(Notify::new()),
        // child_id is display-only now; cancellation goes through the handle's
        // kill switch registered in harness_killers below.
        child_id: Mutex::new(pid),
        roster_events: roster_events.clone(),
    });

    if let Err(err) = task_store
        .write()
        .insert_reserved(task_id.clone(), task.clone())
    {
        task_store.write().release_reservation(&task_id);
        killer.kill();
        let failed = failed_duplicate_task(
            task_id,
            provider,
            session_id,
            cwd,
            None,
            None,
            err.to_string(),
            origin,
        );
        failed.notify.notify_waiters();
        return failed;
    }

    task.emit_roster_added();

    // Register the kill switch and control lane (keyed by task id, like
    // harness_controls). No await runs between insert and here, so no steer can
    // race a missing registration.
    harness_killers().write().insert(task_id.clone(), killer);
    harness_controls().write().insert(task_id.clone(), control);

    // Emit tail + system started events (unchanged from the inline path).
    let cursor = task.next_live_cursor();
    let _ = tail_tx.send(tail::TailEvent::TaskStarted {
        cursor,
        task_id: task_id.clone(),
        provider,
        bro_name: None,
    });
    if let Some(ref hub) = system_events {
        let bro_ev = task.inner.lock().bro_label.clone();
        emit_task_started_event(hub, bro_ev, task_id.clone(), provider.to_string());
    }

    // Daemon ingest: consume the executor's raw stdout line stream.
    let ingest_join = spawn_harness_ingest_loop(
        task.clone(),
        provider,
        task_id.clone(),
        store_dir.clone(),
        mirror_event_log_path,
        tail_tx.clone(),
        system_events.clone(),
        events,
    );

    // Daemon waiter: await the worker outcome, then publish terminal state.
    spawn_harness_terminal_waiter(
        task.clone(),
        task_id,
        store_dir,
        task_store,
        tail_tx,
        system_events,
        outcome,
        ingest_join,
    );

    task
}

#[allow(clippy::too_many_arguments)]
fn prepare_harness_child_launch(
    task_id: String,
    session_id: String,
    provider: Provider,
    mut args: Vec<String>,
    cwd: Option<&str>,
    env_overrides: Option<HashMap<String, String>>,
    shell_env: Option<BTreeMap<String, String>>,
    tool_placement: Option<BTreeMap<String, String>>,
    tool_defaults: Option<BTreeMap<String, String>>,
    store_dir: &std::path::Path,
    self_mcp_url: Option<&str>,
    workspace_binding_authority: Option<&dyn WorkspaceBindingAuthority>,
    workspace_identity: Option<bro_protocol::WorkerWorkspaceIdentity>,
) -> anyhow::Result<bro_protocol::WorkerSpawnSpec> {
    let initial_prompt = take_cli_value_arg(&mut args, "-p")
        .or_else(|| take_cli_value_arg(&mut args, "--prompt"))
        .ok_or_else(|| anyhow::anyhow!("harness child launch requires an initial prompt"))?;

    set_cli_value_arg(&mut args, "--input-format", "stream-json".to_string());
    ensure_cli_flag(&mut args, "--replay-user-messages");
    ensure_cli_flag(&mut args, "--exit-when-idle");
    ensure_cli_flag(&mut args, "--daemon-worker");
    if let Some(cwd) = cwd {
        set_cli_value_arg(&mut args, "--cwd", cwd.to_string());
    }
    if let Some(tool_defaults) = tool_defaults {
        set_cli_value_arg(
            &mut args,
            "--additional-context",
            serde_json::to_string(&tool_defaults)?,
        );
    }
    if let Some(shell_env) = shell_env {
        set_cli_value_arg(&mut args, "--shell-env", serde_json::to_string(&shell_env)?);
    }

    // The spec's `session_id` is the SUPERVISION key: fleetd registries, the
    // daemon's per-session slot map, and the event-log filename all hang off
    // it. Several dispatch paths still pass the placeholder "pending" because
    // the provider has not emitted a real session id yet, and two concurrent
    // pending dispatches would then collide on all three. The task id is
    // already unique by construction (`reserve_id`), so it stands in until a
    // real id exists. The task's own `session_id` is untouched and still gets
    // filled from the event stream.
    let supervision_id = if session_id.is_empty() || session_id == "pending" {
        task_id.clone()
    } else {
        session_id.clone()
    };
    let workspace_binding = match (&workspace_identity, self_mcp_url) {
        (Some(identity), Some(_)) => {
            let authority = workspace_binding_authority.ok_or_else(|| {
                anyhow::anyhow!(
                    "managed workspace dispatch requires daemon workspace binding authority"
                )
            })?;
            Some(authority.mint(&task_id, &supervision_id, identity)?)
        }
        _ => None,
    };

    if let Some(config) = build_harness_mcp_config(
        &mut args,
        tool_placement,
        self_mcp_url,
        workspace_binding.is_some(),
    )? {
        set_cli_value_arg(&mut args, "--mcp-config", config);
    }
    if self_mcp_url.is_some() {
        set_cli_value_arg(
            &mut args,
            "--capability-mcp-server",
            crate::util::blackbox_mcp_name(),
        );
    }

    // Environment: provider credentials + BRO_HARNESS_PROVIDER ride the spec's
    // SecretEnv. BRO_HOME is pinned on its own field (the executor sets it), so
    // it is intentionally NOT placed in `env`; because BRO_HOME is already in
    // BLACKBOX_SERVICE_ENV_VARS the scrub-key set is byte-identical either way.
    let mut env: std::collections::BTreeMap<String, String> =
        env_overrides.unwrap_or_default().into_iter().collect();
    env.entry("BRO_HARNESS_PROVIDER".to_string())
        .or_insert_with(|| provider.as_str().to_string());
    if let Some(binding) = &workspace_binding {
        env.insert(
            bro_protocol::WORKSPACE_BINDING_ENV.to_string(),
            binding.token.expose_secret().to_string(),
        );
        env.insert(
            bro_protocol::KNOWLEDGE_SOURCE_URL_ENV.to_string(),
            self_mcp_url
                .expect("workspace binding requires a self MCP URL")
                .to_string(),
        );
        env.insert(
            bro_protocol::WORKSPACE_SCOPE_ENV.to_string(),
            serde_json::to_string(&binding.scope)?,
        );
    }
    let mut scrub_keys = BLACKBOX_SERVICE_ENV_VARS
        .iter()
        .map(|key| (*key).to_string())
        .collect::<std::collections::BTreeSet<_>>();
    scrub_keys.extend(env.keys().cloned());
    scrub_keys.insert(HARNESS_SPAWN_SCRUB_ENV.to_string());
    env.insert(
        HARNESS_SPAWN_SCRUB_ENV.to_string(),
        scrub_keys.into_iter().collect::<Vec<_>>().join(","),
    );

    // Binary override: BRO_HARNESS_BIN / provider config resolved daemon-side;
    // the final login-shell path resolution stays executor-side.
    let bin_override = Some(if let Ok(cfg) = blackbox::config::load() {
        provider.bin_with_config(&cfg.providers)
    } else {
        provider.bin()
    });

    // A same-host executor shares the daemon's BRO_HOME. An off-host fleetd
    // writes snapshots, replay logs, and spill artifacts under its own
    // explicit worker-local BRO_HOME. Never send the container-local path to
    // another machine.
    let bro_home = harness_worker_locality()
        .map(|locality| locality.bro_home)
        .unwrap_or_else(|| store_dir.to_path_buf());
    let event_log_path = bro_home
        .join("harness-sessions")
        .join(format!("{supervision_id}.events.jsonl"));

    Ok(bro_protocol::WorkerSpawnSpec {
        task_id,
        session_id: supervision_id,
        workspace_id: workspace_identity
            .as_ref()
            .map(|identity| identity.workspace_id.clone()),
        workspace_scope: workspace_identity.map(|identity| identity.scope),
        provider,
        bin_override,
        argv: args,
        cwd: cwd.map(str::to_string),
        env: bro_protocol::SecretEnv::new(env),
        env_unset: BLACKBOX_SERVICE_ENV_VARS
            .iter()
            .map(|key| (*key).to_string())
            .collect(),
        initial_messages: vec![harness_user_input(initial_prompt)],
        bro_home,
        event_log_path,
    })
}

/// Resolve the portable workspace identity only for an explicitly managed
/// checkout. An ordinary cwd is never promoted into authority, while an
/// unsafe marker on a managed checkout fails the dispatch rather than silently
/// erasing its workspace binding. The create-once identity helper repairs an
/// empty or malformed marker by minting a fresh, reuse-safe identity.
#[cfg(test)]
fn workspace_id_for_cwd(cwd: Option<&str>) -> anyhow::Result<Option<bro_core::WorkspaceId>> {
    let Some(cwd) = cwd else {
        return Ok(None);
    };
    let Some(checkout) = bbox_corpus_core::git::managed_checkout_root(std::path::Path::new(cwd))
    else {
        return Ok(None);
    };
    let marker = checkout.join(".bbox/local/checkout-id");
    let raw = bbox_corpus_core::identity::ensure_checkout_id(&checkout)?;
    bro_core::WorkspaceId::parse(raw)
        .map(Some)
        .map_err(|error| {
            anyhow::anyhow!(
                "invalid workspace identity marker {}: {error}",
                marker.display()
            )
        })
}

/// Build the child's transcript location from the pinned spec fields. Returns
/// None when the session id is empty, mirroring [`harness_transcript_location`].
/// `provider_session_id` is the id the DISPATCH asked for, which is what
/// decides whether a real provider session is known yet. The spec's own
/// `session_id` is the supervision key and may be a stand-in task id, so it
/// must not be recorded as if it were a provider session.
fn harness_transcript_location_from_spec(
    spec: &bro_protocol::WorkerSpawnSpec,
    provider_session_id: &str,
    cwd: Option<&str>,
    daemon_event_log_path: &std::path::Path,
) -> Option<TranscriptLocation> {
    if spec.session_id.is_empty() {
        return None;
    }
    Some(TranscriptLocation {
        source: TranscriptSource::Harness(spec.provider),
        storage: TranscriptStorage::JsonlFile,
        path: daemon_event_log_path.to_path_buf(),
        account: None,
        session_id: (!provider_session_id.is_empty() && provider_session_id != "pending")
            .then(|| provider_session_id.to_string()),
        project: None,
        cwd: cwd.map(str::to_string),
        is_subagent: false,
        logical_key: None,
    })
}

/// Emit the `task.started` system event. Observation-only: failures are logged,
/// not propagated. Extracted so the executor-backed harness path shares the
/// exact shape the inline dispatch path emits.
fn emit_task_started_event(
    hub: &crate::system_events::SharedEventHub,
    bro_ev: Option<String>,
    task_id_ev: String,
    provider_str: String,
) {
    let hub_clone = hub.clone();
    tokio::spawn(async move {
        let mut correlation = serde_json::Map::new();
        correlation.insert("task_id".into(), serde_json::json!(task_id_ev));
        let draft = crate::system_events::SystemEventDraft {
            kind: crate::system_events::types::SystemEventKind::TaskStarted,
            producer: "orchestration.dispatch".to_string(),
            project: None,
            principal: None,
            subject: None,
            correlation,
            causation_id: None,
            payload: serde_json::json!({
                "task_id": task_id_ev,
                "provider": provider_str,
                "bro": bro_ev,
            }),
        };
        if let Err(e) = hub_clone.emit(draft).await {
            tracing::warn!("task.started system event emit failed: {e:#}");
        }
    });
}

/// Daemon-side ingest of the executor's raw stdout line stream: parse each
/// line, record the first provider disruption, and feed harness events into the
/// task. Mirrors the inline harness branch of the former stdout reader; the
/// executor now owns the read + tee, the daemon owns parse + ingest. Returns
/// the join handle so the terminal waiter can guarantee the stream is fully
/// drained before publishing terminal state.
fn spawn_harness_ingest_loop(
    task: Arc<Task>,
    provider: Provider,
    task_id: String,
    store_dir: std::path::PathBuf,
    mirror_event_log_path: Option<std::path::PathBuf>,
    tail_tx: tokio::sync::broadcast::Sender<tail::TailEvent>,
    system_events: Option<crate::system_events::SharedEventHub>,
    mut events: tokio::sync::mpsc::UnboundedReceiver<String>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        use tokio::io::AsyncWriteExt as _;

        let mut mirror = match mirror_event_log_path {
            Some(path) => {
                let opened = async {
                    if let Some(parent) = path.parent() {
                        tokio::fs::create_dir_all(parent).await?;
                    }
                    tokio::fs::OpenOptions::new()
                        .create(true)
                        .append(true)
                        .open(&path)
                        .await
                }
                .await;
                match opened {
                    Ok(file) => Some((path, file)),
                    Err(error) => {
                        tracing::error!(
                            task_id,
                            path = %path.display(),
                            %error,
                            "cannot open daemon-local remote harness transcript mirror"
                        );
                        None
                    }
                }
            }
            None => None,
        };
        let mut disruption_recorded = false;
        while let Some(line) = events.recv().await {
            let Ok(evt) = serde_json::from_str::<Value>(&line) else {
                continue;
            };
            if let Some((path, file)) = mirror.as_mut() {
                let record = serde_json::to_vec(&serde_json::json!({
                    "ts": chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
                    "event": &evt,
                }))
                .map(|mut record| {
                    record.push(b'\n');
                    record
                });
                match record {
                    Ok(record) => {
                        if let Err(error) = file.write_all(&record).await {
                            tracing::error!(
                                task_id,
                                path = %path.display(),
                                %error,
                                "cannot append daemon-local remote harness transcript mirror"
                            );
                            mirror = None;
                        }
                    }
                    Err(error) => {
                        tracing::error!(task_id, %error, "cannot serialize remote harness transcript mirror record");
                    }
                }
            }
            if !disruption_recorded && let Some(disruption) = provider.detect_disruption(&evt) {
                disruption_recorded = true;
                let store_dir = store_dir.clone();
                let task_id = task_id.clone();
                let observed_at = now_ms();
                tokio::task::spawn_blocking(move || {
                    let account = allocator::lookup_lease_for_task(&store_dir, &task_id)
                        .and_then(|lease| lease.account);
                    account_probes::record_disruption_cooldown(
                        &store_dir,
                        provider,
                        account.as_deref(),
                        disruption,
                        observed_at,
                    );
                });
            }
            // Read the seq BEFORE ingest moves the event, advance the durable
            // cursor AFTER: the cursor's whole contract is "everything at or
            // below this has been applied", so advancing early would let a
            // replay skip an event this daemon never actually ingested.
            let seq = evt.get("seq").and_then(Value::as_u64);
            ingest_harness_event(
                &task,
                provider,
                evt,
                &tail_tx,
                &task_id,
                system_events.clone(),
            );
            if let Some(seq) = seq {
                let mut inner = task.inner.lock();
                inner.harness_ingest_seq = inner.harness_ingest_seq.max(seq);
            }
        }
        if let Some((path, mut file)) = mirror
            && let Err(error) = file.flush().await
        {
            tracing::error!(
                task_id,
                path = %path.display(),
                %error,
                "cannot flush daemon-local remote harness transcript mirror"
            );
        }
    })
}

/// Daemon-side terminal waiter for an executor-backed harness worker. Awaits
/// the worker outcome (child exit + stdout/stderr pumps drained), ensures the
/// ingest stream is fully consumed, then publishes terminal state. Mirrors the
/// inline waiter's ordering and terminal-publication logic.
#[allow(clippy::too_many_arguments)]
fn spawn_harness_terminal_waiter(
    task: Arc<Task>,
    task_id: String,
    store_dir: std::path::PathBuf,
    task_store: Arc<RwLock<TaskStore>>,
    tail_tx: tokio::sync::broadcast::Sender<tail::TailEvent>,
    system_events: Option<crate::system_events::SharedEventHub>,
    outcome: tokio::sync::oneshot::Receiver<executor::WorkerOutcome>,
    ingest_join: tokio::task::JoinHandle<()>,
) {
    tokio::spawn(async move {
        let outcome = outcome.await.unwrap_or(executor::WorkerOutcome {
            exit_code: None,
            stderr: String::new(),
        });
        // Ensure every ingested event has been applied before we mark terminal.
        let _ = ingest_join.await;

        // The control lane and kill switch are done once terminal.
        harness_controls().write().remove(&task_id);
        harness_killers().write().remove(&task_id);
        if let Some(authority) = readoption_env()
            .get()
            .and_then(|env| env.workspace_binding_authority.as_ref())
        {
            authority.revoke_task(&task_id);
        }

        let code = outcome.exit_code;
        let (terminal_status, elapsed, cost, error_snippet, source_session, task_kind, cursor) = {
            let mut inner = task.inner.lock();
            inner.exit_code = code;
            // Append the executor's collected child stderr. `ingest_harness_event`
            // may already have written a result-error message into `inner.stderr`
            // during the run (harness result with is_error=true), so this must
            // append, not overwrite, matching the inline path where both the
            // stderr reader and the result handler push onto the same buffer.
            inner.stderr.push_str(&outcome.stderr);
            // Preserve terminal states set during stream parsing (Cancelled on
            // kill, Failed on session fork detection) — don't let a clean exit
            // code flip a detected failure back to Completed.
            if inner.status != TaskStatus::Cancelled && inner.status != TaskStatus::Failed {
                inner.status = if code == Some(0) {
                    TaskStatus::Completed
                } else {
                    TaskStatus::Failed
                };
            }
            inner.completed_at = Some(now_ms());
            let elapsed = format_elapsed(inner.started_at, inner.completed_at);
            let terminal_status = inner.status;
            let cost = inner.cost_usd;
            let error_snippet: String = inner.stderr.chars().take(200).collect();
            let source_session = inner.session_id.clone();
            let task_kind = inner.bro_label.clone();
            inner.live_cursor += 1;
            let cursor = inner.live_cursor;
            (
                terminal_status,
                elapsed,
                cost,
                error_snippet,
                source_session,
                task_kind,
                cursor,
            )
        };
        task.emit_roster_updated();
        match terminal_status {
            TaskStatus::Completed => {
                let _ = tail_tx.send(tail::TailEvent::TaskCompleted {
                    cursor,
                    task_id: task_id.clone(),
                    elapsed: elapsed.clone(),
                    cost,
                    source_session,
                    task_kind,
                });
            }
            TaskStatus::Failed => {
                let _ = tail_tx.send(tail::TailEvent::TaskFailed {
                    cursor,
                    task_id: task_id.clone(),
                    elapsed: elapsed.clone(),
                    error: error_snippet.clone(),
                });
            }
            _ => {}
        }
        // Emit terminal system event. Observation-only: failures logged.
        if let Some(ref hub) = system_events {
            let mut correlation = serde_json::Map::new();
            correlation.insert("task_id".into(), serde_json::json!(task_id));
            let (kind, payload) = match terminal_status {
                TaskStatus::Completed => (
                    crate::system_events::types::SystemEventKind::TaskCompleted,
                    serde_json::json!({"task_id": task_id, "elapsed": elapsed, "cost_usd": cost}),
                ),
                TaskStatus::Failed => (
                    crate::system_events::types::SystemEventKind::TaskFailed,
                    serde_json::json!({"task_id": task_id, "elapsed": elapsed, "error": error_snippet}),
                ),
                TaskStatus::Cancelled => (
                    crate::system_events::types::SystemEventKind::TaskCancelled,
                    serde_json::json!({"task_id": task_id, "elapsed": elapsed}),
                ),
                TaskStatus::Running => unreachable!("terminal state check above"),
            };
            let draft = crate::system_events::SystemEventDraft {
                kind,
                producer: "orchestration.dispatch".to_string(),
                project: None,
                principal: None,
                subject: None,
                correlation,
                causation_id: None,
                payload,
            };
            if let Err(e) = hub.emit(draft).await {
                tracing::warn!("task terminal system event emit failed: {e:#}");
            }
        }

        // Propagate session ID to team members.
        {
            let inner = task.inner.lock();
            if inner.session_id != "pending" {
                let sid = inner.session_id.clone();
                let tid = inner.id.clone();
                drop(inner);
                team::propagate_session_id(&tid, &sid, &store_dir);
            }
        }

        request_persist(&task_store, &store_dir);
        task.notify.notify_waiters();
    });
}

fn set_cli_value_arg(args: &mut Vec<String>, flag: &str, value: String) {
    let _ = take_cli_value_arg(args, flag);
    args.extend([flag.to_string(), value]);
}

fn ensure_cli_flag(args: &mut Vec<String>, flag: &str) {
    if !args.iter().any(|arg| arg == flag) {
        args.push(flag.to_string());
    }
}

// The child stdin control writer moved to `executor::spawn_control_writer`
// (the execution half of the harness worker). The old daemon-side copy was
// removed with the executor extraction.

#[allow(clippy::too_many_arguments)]
/// Publish a failed task for a harness dispatch that never got a child.
///
/// Precondition: `task_id` is reserved (`spawn_reserved_dispatch` claims it
/// before any provider branch runs). This consumes that reservation rather
/// than taking its own, which is why it can `insert_reserved` directly.
fn failed_harness_child_setup(
    task_id: String,
    provider: Provider,
    session_id: String,
    cwd: Option<String>,
    store_dir: std::path::PathBuf,
    task_store: Arc<RwLock<TaskStore>>,
    roster_events: Option<RosterEventSink>,
    bro_label: Option<String>,
    agent_label: Option<String>,
    error: anyhow::Error,
    origin: bro_core::Origin,
) -> Arc<Task> {
    let mut task = failed_duplicate_task(
        task_id.clone(),
        provider,
        session_id,
        cwd,
        bro_label,
        agent_label,
        format!("harness child setup failed: {error:#}"),
        origin,
    );
    Arc::get_mut(&mut task)
        .expect("new failed task is unique")
        .roster_events = roster_events;
    if task_store
        .write()
        .insert_reserved(task_id, task.clone())
        .is_err()
    {
        return task;
    }
    task.emit_roster_added();
    request_persist(&task_store, &store_dir);
    task.notify.notify_waiters();
    task
}

/// Session id extraction matching `parse_claude_event`'s lookup paths —
/// used to decide fork-acceptance BEFORE the (mutating) parse runs.
fn emitted_session_id_from_event(evt: &Value) -> Option<String> {
    evt["session_id"]
        .as_str()
        .or_else(|| evt["sessionId"].as_str())
        .or_else(|| evt["message"]["session_id"].as_str())
        .or_else(|| evt["message"]["sessionId"].as_str())
        .map(|s| s.to_string())
}

/// Last `n` chars of `msg` (ellipsis-prefixed when truncated) in O(n) — this
/// runs per ingested event, so it must not scan the whole accumulated
/// message (`chars().count()` is O(message)).
fn snippet_tail(msg: &str, n: usize) -> String {
    let mut iter = msg.char_indices().rev();
    let mut start = msg.len();
    for _ in 0..n {
        match iter.next() {
            Some((i, _)) => start = i,
            None => return msg.to_string(), // fits whole
        }
    }
    if iter.next().is_some() {
        format!("\u{2026}{}", &msg[start..])
    } else {
        msg.to_string()
    }
}

fn ingest_harness_event(
    task: &Task,
    provider: Provider,
    evt: Value,
    tail_tx: &tokio::sync::broadcast::Sender<tail::TailEvent>,
    task_id: &str,
    system_events: Option<crate::system_events::SharedEventHub>,
) {
    // Stream deltas arrive at token-chunk rate while a bro streams (50+/s);
    // everything inside the lock below must be O(chunk), never O(message) —
    // per-delta O(accumulated-message) work measurably degraded runtime
    // worker poll times (thread-935b467d §4.6 measurements).
    let is_stream_delta = evt.get("type").and_then(Value::as_str) == Some("stream_event");
    let (snippet_to_emit, emit_roster, task_event_to_emit) = {
        let mut inner = task.inner.lock();
        // Decide fork-acceptance BEFORE parsing so the parse can mutate the
        // task's accumulated message in place (taken, not cloned) — a
        // rejected forked event must never touch it.
        let emitted_session_id = emitted_session_id_from_event(&evt);
        let mut accepted = true;
        let mut session_id_observed = false;
        if let Some(sid) = emitted_session_id {
            if inner.session_id == "pending" {
                inner.session_id = sid;
                session_id_observed = true;
                let observed_session_id = inner.session_id.clone();
                if let Some(location) = inner.transcript_location.as_mut() {
                    location
                        .path
                        .set_file_name(format!("{observed_session_id}.events.jsonl"));
                    location.session_id = Some(observed_session_id);
                }
            } else if inner.session_id != sid {
                reject_forked_session(&mut inner, &sid);
                accepted = false;
            }
        }
        let mut task_event_to_emit = None;
        if accepted {
            let mut sink = EventSink {
                // Zero-copy seed: take the accumulated message so delta
                // appends are amortized O(chunk). apply_sink_updates below
                // (unconditional on this path) writes it back.
                last_assistant_message: inner.last_assistant_message.take(),
                usage: inner.usage.clone(),
                cost_usd: inner.cost_usd,
                num_turns: inner.num_turns,
                session_id: if inner.session_id != "pending" {
                    Some(inner.session_id.clone())
                } else {
                    None
                },
                interrupted: false,
                // Seeded from prior state so a partial update (an event that
                // carries no pressure block) merges rather than clears, the
                // same contract the usage/cost/turns fields above follow.
                last_turn_input_tokens: inner.last_turn_input_tokens,
                context_window: inner.context_window,
            };
            provider.parse_event(&evt, &mut sink);
            apply_cwd_updates_from_event(&mut inner, &evt);
            inner
                .supervision
                .observe_event(&evt, &sink, &supervision::config(), now_ms());
            apply_sink_updates(&mut inner, sink);
            // A terminal `result` event with `is_error: true` fails the task and
            // preserves the message in stderr. A controlled harness turn may
            // still exit the child with code zero after emitting an error
            // result, so the event itself is authoritative (gap-32113fd4).
            if evt.get("type").and_then(Value::as_str) == Some("result")
                && evt.get("is_error").and_then(Value::as_bool) == Some(true)
            {
                if inner.status != TaskStatus::Cancelled {
                    inner.status = TaskStatus::Failed;
                }
                if let Some(msg) = evt.get("result").and_then(Value::as_str)
                    && !msg.trim().is_empty()
                {
                    inner.stderr.push_str(msg);
                    inner.stderr.push('\n');
                }
            }
            // Store the event LAST so it moves instead of deep-cloning.
            // Stream deltas are not stored at all: every ring consumer
            // either filters them at read time (compact_status_event) or
            // skips them structurally (no message/model field) — see the
            // wave-15 consumer inventory in thread-935b467d. Storing one
            // per text chunk made the 512-slot ring all-deltas under
            // streaming and deep-cloned every chunk.
            if !is_stream_delta {
                task_event_to_emit = Some(append_task_event(&mut inner, evt));
            }
        }
        // Roster summaries rebuild + broadcast per emit; throttle the
        // delta-rate path to ~1/s (step-boundary events always emit).
        let now = now_ms();
        let emit_roster = !is_stream_delta
            || session_id_observed
            || now.saturating_sub(inner.last_delta_roster_emit_ms) >= 1000;
        if is_stream_delta && emit_roster {
            inner.last_delta_roster_emit_ms = now;
        }
        let snippet = accepted
            .then(|| {
                inner.last_assistant_message.as_ref().map(|msg| {
                    const TAIL_CHARS: usize = 160;
                    snippet_tail(msg, TAIL_CHARS)
                })
            })
            .flatten()
            .map(|snippet| {
                let cursor = if snippet.is_empty() {
                    None
                } else {
                    inner.live_cursor += 1;
                    Some(inner.live_cursor)
                };
                (snippet, session_id_observed, cursor)
            })
            .or_else(|| session_id_observed.then(|| (String::new(), true, None)));
        (snippet, emit_roster, task_event_to_emit)
    };

    if let Some(task_event) = task_event_to_emit {
        let _ = tail_tx.send(task_event);
    }

    if emit_roster {
        task.emit_roster_updated();
    }

    if let Some((snippet, session_id_observed, cursor)) = snippet_to_emit {
        if session_id_observed {
            task.notify.notify_waiters();
        }
        if snippet.is_empty() {
            return;
        }
        let Some(cursor) = cursor else {
            return;
        };
        let _ = tail_tx.send(tail::TailEvent::TaskProgress {
            cursor,
            task_id: task_id.to_string(),
            activity: snippet.clone(),
        });
        // System events journal every emit (fs append + reaction matching);
        // a task.progress per text DELTA wrote one journal line per token
        // chunk (20,495 of 20,513 prod journal lines were task.progress).
        // Step-boundary events still emit at turn cadence.
        if !is_stream_delta && let Some(ref hub) = system_events {
            emit_task_progress_event(hub, task_id.to_string(), snippet);
        }
    }
}

fn build_harness_mcp_config(
    args: &mut Vec<String>,
    tool_placement: Option<BTreeMap<String, String>>,
    self_mcp_url: Option<&str>,
    workspace_bound: bool,
) -> anyhow::Result<Option<String>> {
    let raw_mcp_config = take_cli_value_arg(args, "--mcp-config");
    let mut config: Value = match raw_mcp_config {
        Some(raw) => serde_json::from_str(&raw)?,
        None => serde_json::json!({}),
    };
    let config_object = config
        .as_object_mut()
        .ok_or_else(|| anyhow::anyhow!("--mcp-config must be a JSON object"))?;
    let servers_empty = {
        let servers = config_object
            .entry("mcpServers")
            .or_insert_with(|| serde_json::json!({}))
            .as_object_mut()
            .ok_or_else(|| anyhow::anyhow!("--mcp-config mcpServers must be a JSON object"))?;
        add_transient_blackbox_mcp_server(servers, self_mcp_url, workspace_bound);
        servers.is_empty()
    };
    let placement = parse_dispatch_tool_placement(tool_placement)?;
    config_object.insert("tool_placement".to_string(), Value::Object(placement));
    if servers_empty
        && config_object
            .get("tool_placement")
            .and_then(Value::as_object)
            .is_none_or(serde_json::Map::is_empty)
    {
        Ok(None)
    } else {
        Ok(Some(serde_json::to_string(&config)?))
    }
}

fn add_transient_blackbox_mcp_server(
    servers: &mut serde_json::Map<String, Value>,
    self_mcp_url: Option<&str>,
    workspace_bound: bool,
) {
    let Some(url) = self_mcp_url.filter(|s| !s.is_empty()) else {
        return;
    };
    let name = crate::util::blackbox_mcp_name();
    // This name is reserved for the daemon capability channel. Replace a
    // caller-supplied collision so capability aliases cannot be redirected to
    // an unrelated server; all differently named MCP servers remain intact.
    let headers = if workspace_bound {
        serde_json::json!({
            bro_protocol::WORKSPACE_BINDING_HEADER:
                format!("$env:{}", bro_protocol::WORKSPACE_BINDING_ENV),
        })
    } else {
        serde_json::json!({})
    };
    servers.insert(
        name,
        serde_json::json!({
            "type": "http",
            "url": url,
            "headers": headers,
            "exclude_tools": [],
        }),
    );
}

fn take_cli_value_arg(args: &mut Vec<String>, flag: &str) -> Option<String> {
    let mut idx = 0;
    let mut found = None;
    while idx < args.len() {
        if args[idx] == flag {
            args.remove(idx);
            if idx < args.len() {
                found = Some(args.remove(idx));
            }
            continue;
        }
        idx += 1;
    }
    found
}

fn parse_dispatch_tool_placement(
    raw: Option<BTreeMap<String, String>>,
) -> anyhow::Result<serde_json::Map<String, Value>> {
    let mut out = serde_json::Map::new();
    let Some(raw) = raw else {
        return Ok(out);
    };
    for (name, placement) in raw {
        if !matches!(placement.as_str(), "in-box" | "out-box" | "both") {
            anyhow::bail!(
                "invalid tool_placement for {name}: {placement}; expected in-box, out-box, or both"
            );
        }
        out.insert(name, Value::String(placement));
    }
    Ok(out)
}

const HARNESS_TEE_CHANNEL_CAPACITY: usize = 256;

struct HarnessTee {
    id: String,
    suffix: String,
    tx: std::sync::mpsc::SyncSender<String>,
    warned_drop: bool,
}

impl HarnessTee {
    fn try_write_line(&mut self, line: &str) {
        match self.tx.try_send(format!("{line}\n")) {
            Ok(()) | Err(std::sync::mpsc::TrySendError::Disconnected(_)) => {}
            Err(std::sync::mpsc::TrySendError::Full(_)) => {
                if !self.warned_drop {
                    self.warned_drop = true;
                    tracing::warn!(
                        task_id = %self.id,
                        suffix = %self.suffix,
                        "harness tee buffer full; dropping diagnostic lines for this task tee"
                    );
                }
            }
        }
    }
}

/// Start a per-session writer for tee-ing harness stdout/stderr, when
/// `BLACKBOX_HARNESS_TEE_DIR` is set (`bro fleet` sets it by default so fleet
/// spurious-stop turns are captured for postmortem). Returns None when disabled.
/// The append file is opened on the writer thread; tee-ing is best-effort
/// diagnostics and must never block or fail a dispatch. `suffix` is e.g.
/// "stdout.jsonl" / "stderr.log".
fn open_harness_tee(id: &str, suffix: &str) -> Option<HarnessTee> {
    let dir = std::env::var("BLACKBOX_HARNESS_TEE_DIR")
        .ok()
        .filter(|d| !d.is_empty())?;
    let id = id.to_string();
    let suffix = suffix.to_string();
    let path = std::path::Path::new(&dir).join(format!("{id}.{suffix}"));
    let (tx, rx) = std::sync::mpsc::sync_channel::<String>(HARNESS_TEE_CHANNEL_CAPACITY);
    let thread_name = format!(
        "harness-tee-{}-{suffix}",
        id.chars().take(8).collect::<String>()
    );
    let id_for_thread = id.clone();
    let suffix_for_thread = suffix.clone();
    let spawned = std::thread::Builder::new()
        .name(thread_name)
        .spawn(move || {
            if let Some(parent) = path.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            let Ok(mut file) = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&path)
            else {
                tracing::warn!(
                    task_id = %id_for_thread,
                    suffix = %suffix_for_thread,
                    path = %path.display(),
                    "failed to open harness tee"
                );
                return;
            };
            while let Ok(line) = rx.recv() {
                if file.write_all(line.as_bytes()).is_err() {
                    break;
                }
            }
            let _ = file.flush();
        });
    if spawned.is_err() {
        tracing::warn!(task_id = %id, suffix = %suffix, "failed to spawn harness tee writer");
        return None;
    }
    Some(HarnessTee {
        id,
        suffix,
        tx,
        warned_drop: false,
    })
}

/// Wait for a task to complete. Returns immediately if already terminal.
/// Uses `enable()` on the Notify future before checking status to avoid
/// lost-wakeup races (TOCTOU between status check and await).
pub async fn wait_for_task(task: &Task) {
    loop {
        // Register interest BEFORE checking status — avoids lost wakeup if
        // the task completes between our check and our await.
        let notified = task.notify.notified();
        tokio::pin!(notified);
        // Enable the future so it will capture a notify even if we haven't
        // .await'd yet (this is the critical fix for the race).
        notified.as_mut().enable();

        {
            let inner = task.inner.lock();
            if inner.status.is_terminal() {
                return;
            }
        }
        notified.await;
    }
}

/// Wait with timeout. Returns true if completed, false if timed out.
pub async fn wait_for_task_with_timeout(task: &Task, timeout_secs: Option<f64>) -> bool {
    match timeout_secs {
        None => {
            wait_for_task(task).await;
            true
        }
        Some(secs) => {
            let duration = std::time::Duration::from_secs_f64(secs);
            match tokio::time::timeout(duration, wait_for_task(task)).await {
                Ok(()) => true,
                // Timer expiry can race the task's completion (or a missed
                // notify): a task that IS terminal must never be reported
                // as timed out, so re-check the authoritative status
                // before declaring a timeout (gap-0301dc75).
                Err(_) => task.inner.lock().status.is_terminal(),
            }
        }
    }
}

/// Wait for a provider-backed task to publish its real session id.
///
/// Wait for a late-discovered provider session id.
///
/// Modern harness-backed fresh dispatch pre-mints a concrete id before spawn.
/// This remains as a guard for legacy/provider-discovered paths: the
/// placeholder is internal state, and public authorial surfaces should either
/// return a concrete session id or diagnose the failed handshake.
pub async fn wait_for_task_session_id_with_timeout(
    task: &Task,
    timeout_secs: f64,
) -> Option<String> {
    async fn wait_for_session_id(task: &Task) -> Option<String> {
        loop {
            let notified = task.notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();

            {
                let inner = task.inner.lock();
                if !inner.session_id.is_empty() && inner.session_id != "pending" {
                    return Some(inner.session_id.clone());
                }
                if inner.status.is_terminal() {
                    return None;
                }
            }
            notified.await;
        }
    }

    tokio::time::timeout(
        std::time::Duration::from_secs_f64(timeout_secs),
        wait_for_session_id(task),
    )
    .await
    .ok()
    .flatten()
}

/// Cancel a running task.
pub fn cancel_task(
    task: &Task,
    task_store: &RwLock<TaskStore>,
    store_dir: &std::path::Path,
) -> Result<(), String> {
    let mut inner = task.inner.lock();
    if inner.status != TaskStatus::Running {
        return Err(format!(
            "Task already {}",
            serde_json::to_string(&inner.status).unwrap_or_default()
        ));
    }
    inner.status = TaskStatus::Cancelled;
    inner.completed_at = Some(now_ms());
    drop(inner);
    task.emit_roster_updated();

    // Kill the child process. Executor-backed harness workers go through the
    // handle's idempotent kill switch (registered in harness_killers); other
    // tasks (one-shot / non-harness) still carry a raw PID in child_id.
    let task_id = task.id();
    if let Some(killer) = harness_killers().read().get(&task_id).cloned() {
        killer.kill();
    } else if let Some(pid) = task.child_id.lock().take() {
        unsafe {
            libc::kill(pid as libc::pid_t, libc::SIGTERM);
        }
    }
    request_persist(task_store, store_dir);
    task.notify.notify_waiters();
    Ok(())
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

fn apply_sink_updates(inner: &mut TaskInner, sink: EventSink) {
    // Merge — never CLEAR fields that were already captured during the
    // streaming run by overwriting with None from a partial update.
    if sink.last_assistant_message.is_some() {
        inner.last_assistant_message = sink.last_assistant_message;
    }
    if sink.usage.is_some() {
        inner.usage = sink.usage;
    }
    if sink.cost_usd.is_some() {
        inner.cost_usd = sink.cost_usd;
    }
    if sink.num_turns.is_some() {
        inner.num_turns = sink.num_turns;
    }
    if sink.last_turn_input_tokens.is_some() {
        inner.last_turn_input_tokens = sink.last_turn_input_tokens;
    }
    if sink.context_window.is_some() {
        inner.context_window = sink.context_window;
    }
    if sink.interrupted {
        inner.interrupted = true;
    } else if inner.status == TaskStatus::Running {
        inner.interrupted = false;
    }
}

fn apply_cwd_updates_from_event(inner: &mut TaskInner, evt: &Value) {
    if let Some(payload) = successful_tool_result_payload(&inner.events, evt, "enter_worktree") {
        if let Some(cwd) = payload
            .get("cwd")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
        {
            inner.cwd = Some(cwd.to_string());
        }
    }
    if let Some(payload) = successful_tool_result_payload(&inner.events, evt, "exit_worktree") {
        if payload.get("removed_worktree").is_some()
            && let Some(base_repo) = payload
                .get("base_repo")
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
        {
            inner.cwd = Some(base_repo.to_string());
        }
    }
}

fn successful_tool_result_payload(events: &[Value], evt: &Value, tool_name: &str) -> Option<Value> {
    let mut tool_names = std::collections::HashMap::new();
    for event in events {
        let Some(blocks) = event
            .get("message")
            .and_then(|m| m.get("content"))
            .and_then(|c| c.as_array())
        else {
            continue;
        };
        for block in blocks {
            if block.get("type").and_then(|t| t.as_str()) == Some("tool_use")
                && let (Some(id), Some(name)) = (
                    block.get("id").and_then(|id| id.as_str()),
                    block.get("name").and_then(|name| name.as_str()),
                )
            {
                tool_names.insert(id, name);
            }
        }
    }

    let blocks = evt
        .get("message")
        .and_then(|m| m.get("content"))
        .and_then(|c| c.as_array())?;
    blocks.iter().find_map(|block| {
        if block.get("type").and_then(|t| t.as_str()) != Some("tool_result") {
            return None;
        }
        if block
            .get("is_error")
            .and_then(|v| v.as_bool())
            .unwrap_or(false)
        {
            return None;
        }
        let id = block.get("tool_use_id").and_then(|id| id.as_str())?;
        if tool_names.get(id).copied() != Some(tool_name) {
            return None;
        }
        let content = tool_result_content_text(block.get("content"));
        let payload: Value = serde_json::from_str(&content).ok()?;
        payload
            .get("ok")
            .and_then(|ok| ok.as_bool())
            .unwrap_or(false)
            .then_some(payload)
    })
}

fn tool_result_content_text(content: Option<&Value>) -> String {
    match content {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(parts)) => parts
            .iter()
            .filter_map(|part| {
                part.get("text")
                    .and_then(|t| t.as_str())
                    .or_else(|| part.as_str())
            })
            .collect::<Vec<_>>()
            .join(""),
        Some(other) => other.to_string(),
        None => String::new(),
    }
}

fn reject_forked_session(inner: &mut TaskInner, emitted_session_id: &str) {
    if inner.status != TaskStatus::Failed {
        let requested = inner.session_id.clone();
        inner.status = TaskStatus::Failed;
        inner.stderr.push_str(&format!(
            "\nsession fork detected: requested resume of {requested}, provider emitted {emitted_session_id}"
        ));
    }
}

pub fn format_elapsed(started_at: u64, completed_at: Option<u64>) -> String {
    let end = completed_at.unwrap_or_else(now_ms);
    let ms = end.saturating_sub(started_at);
    let s = ms / 1000;
    if s < 60 {
        format!("{s}s")
    } else {
        format!("{}m {}s", s / 60, s % 60)
    }
}

pub fn task_result_json(task: &Task) -> Value {
    let inner = task.inner.lock();
    task_result_json_from_inner(&inner)
}

/// Derive the context-window pressure block for a task.
///
/// `None` until some turn has reported occupancy: a task with no measurement
/// yet must report nothing rather than a zero that reads as "plenty of room".
pub(crate) fn context_pressure_for_inner(
    inner: &TaskInner,
) -> Option<bro_protocol::ContextPressure> {
    inner.last_turn_input_tokens.map(|tokens| {
        bro_protocol::ContextPressure::derive(
            tokens,
            inner.context_window,
            supervision::context_ceiling_ratio(),
        )
    })
}

fn task_result_json_from_inner(inner: &TaskInner) -> Value {
    task_view_json_from_inner(inner, true, true, false)
}

fn task_view_json_from_inner(
    inner: &TaskInner,
    deliverable: bool,
    debug: bool,
    transcript_coordinates: bool,
) -> Value {
    let mut obj = serde_json::json!({
        "taskId": inner.id,
        "provider": inner.provider,
        "sessionId": inner.session_id,
        "status": inner.status,
        "elapsed": format_elapsed(inner.started_at, inner.completed_at),
    });

    if deliverable && let Some(ref msg) = inner.last_assistant_message {
        obj["result"] = Value::String(msg.clone());
        // Workflow task results are a serialized WorkflowRunResult; lift the
        // machine-readable exit value out of the escaped envelope so callers
        // (bro_wait, awaited bro_orchestrate_run) get it first-class instead
        // of bracket-matching the result string (gap-55be3518).
        if inner.provider == Provider::Workflow
            && let Ok(parsed) = serde_json::from_str::<Value>(msg)
            && let Some(exit) = parsed.get("structured_exit")
            && !exit.is_null()
        {
            obj["structuredExit"] = exit.clone();
        }
    }
    if !deliverable && let Some(message) = inner.last_assistant_message.as_deref() {
        obj["lastAssistantSnippet"] = json!(message.chars().take(256).collect::<String>());
        obj["resultBytes"] = json!(message.len());
        obj["resultHint"] = json!(
            "Read the deliverable with bro_status(task_id=..., detail=result); follow body.next_cursor for additional pages."
        );
    }
    if inner.interrupted {
        obj["interrupted"] = Value::Bool(true);
    }
    // hasResult is truthful: true only when the task reached a terminal state
    // AND produced a final assistant message (the deliverable). Live tasks
    // may have mid-conversation assistant turns in last_assistant_message;
    // those are available via the `result` field and the separate
    // hasLastMessage flag but must not claim a deliverable exists.
    let is_terminal = inner.status.is_terminal();
    obj["hasResult"] = Value::Bool(is_terminal && inner.last_assistant_message.is_some());
    obj["hasLastMessage"] = Value::Bool(inner.last_assistant_message.is_some());
    // Interrupted cancellations still carry meaningful terminal metadata
    // (partial usage, turn count), so the gate includes them.
    if matches!(inner.status, TaskStatus::Completed | TaskStatus::Failed) || inner.interrupted {
        if debug {
            if let Some(ref u) = inner.usage {
                // `input_tokens` is fresh (cache-exclusive). Surface the cache
                // breakdown only when present so cache-free providers stay terse.
                let mut usage = serde_json::json!({
                    "input_tokens": u.input_tokens,
                    "output_tokens": u.output_tokens,
                });
                if u.cached_input_tokens > 0 || u.cache_creation_input_tokens > 0 {
                    usage["cached_input_tokens"] = Value::from(u.cached_input_tokens);
                    usage["cache_creation_input_tokens"] =
                        Value::from(u.cache_creation_input_tokens);
                    usage["total_input_tokens"] = Value::from(u.total_input_tokens());
                }
                obj["usage"] = usage;
            }
            if let Some(cost) = inner.cost_usd {
                obj["costUsd"] = Value::from(cost);
            }
            if let Some(turns) = inner.num_turns {
                obj["numTurns"] = Value::from(turns);
            }
        }
        if inner.last_assistant_message.is_none() {
            obj["resultCapture"] = serde_json::json!({
                "status": "missing",
                "message": "task reached a terminal state without a captured assistant result",
                "eventCount": observed_event_count(inner),
                "exitCode": inner.exit_code,
                "stderrPresent": !inner.stderr.trim().is_empty(),
                "transcriptLocated": inner.transcript_location.is_some(),
            });
        }
    }
    // Occupancy remains visible while running, with the same interpretation
    // as the dashboard. Cumulative usage is a separate accounting measure.
    if let Some(pressure) = context_pressure_for_inner(inner) {
        obj["context"] = pressure.observation_json();
    }
    if let Some(ref label) = inner.bro_label {
        obj["broLabel"] = Value::String(label.clone());
    }
    if let Some(ref label) = inner.agent_label {
        obj["agentLabel"] = Value::String(label.clone());
    }
    if let Some(ref report) = inner.report {
        obj["report"] = if debug {
            report.to_json()
        } else {
            task_report_summary(
                &report.message,
                report.needs.as_deref(),
                report.data.is_some(),
                report.reported_at,
            )
        };
    }
    if transcript_coordinates {
        if let Some(ref location) = inner.transcript_location {
            obj["transcriptLocation"] = serde_json::to_value(location).unwrap_or(Value::Null);
            obj["transcriptLocationOwner"] = json!("execution_worker");
        }
        if let Some(ref cursor) = inner.transcript_cursor {
            obj["transcriptCursor"] = serde_json::to_value(cursor).unwrap_or(Value::Null);
        }
    } else if inner.transcript_location.is_some() {
        obj["transcriptAvailable"] = json!(true);
    }
    let supervision_now = inner.completed_at.unwrap_or_else(now_ms);
    // Gate the liveness row out of terminal, healthy responses — on a finished
    // task it would only restate `ok: true`. Live or alerting tasks keep it
    // (idle / tool_running / alerts still carry signal).
    if let Some(supervision) = inner.supervision.snapshot_for_response_gated(
        &supervision::config(),
        supervision_now,
        inner.status.is_terminal(),
    ) {
        obj["supervision"] = supervision;
    }
    if inner.status == TaskStatus::Failed {
        if let Some(code) = inner.exit_code {
            obj["exitCode"] = Value::from(code);
        }
        if !inner.stderr.is_empty() {
            let truncated: String = inner.stderr.chars().take(2000).collect();
            obj["stderr"] = Value::String(truncated);
        }
        // Surface the recovery hint so calling agents can distinguish
        // "task killed by daemon restart, retry via bro_resume" from
        // "task genuinely failed, start fresh."
        if inner.recoverable && inner.provider != Provider::Workflow {
            obj["recoverable"] = Value::Bool(true);
            obj["recoveryHint"] = Value::String(format!(
                "retry with bro_resume(session_id=\"{}\", provider=\"{}\")",
                inner.session_id, inner.provider
            ));
        } else if inner.recoverable {
            obj["recoverable"] = Value::Bool(true);
            obj["recoveryHint"] = Value::String(
                "workflow task was interrupted by daemon restart; redispatch the workflow spec"
                    .into(),
            );
        }
    }
    obj
}

const MCP_TASK_BODY_PAGE_BYTES: usize = 4096;
const MCP_TASK_EVENTS_BYTES: usize = 8192;

pub(crate) fn task_report_summary(
    message: &str,
    needs: Option<&str>,
    has_data: bool,
    reported_at: u64,
) -> Value {
    let mut out = json!({
        "message": message.chars().take(512).collect::<String>(),
        "reportedAt": reported_at,
    });
    if let Some(needs) = needs {
        out["needs"] = json!(needs.chars().take(512).collect::<String>());
    }
    if has_data || message.chars().count() > 512 || needs.is_some_and(|s| s.chars().count() > 512) {
        out["detailsOmitted"] = json!(true);
        out["detailHint"] = json!(
            "Read the full report with bro_status(task_id=..., detail=report); follow body.next_cursor."
        );
    }
    out
}

fn task_body_revision(task_id: &str, detail: &str, body: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut hash = Sha256::new();
    hash.update(task_id.as_bytes());
    hash.update([0]);
    hash.update(detail.as_bytes());
    hash.update([0]);
    hash.update(body.as_bytes());
    format!("{:x}", hash.finalize())
}

fn task_body_page(
    task_id: &str,
    detail: &str,
    text: &str,
    cursor: Option<&str>,
    limit: usize,
) -> anyhow::Result<Value> {
    let revision = task_body_revision(task_id, detail, text);
    let offset = match cursor {
        None => 0,
        Some(cursor) => {
            let (expected, offset) = cursor.split_once(':').ok_or_else(|| {
                anyhow::anyhow!("invalid body cursor; use body.next_cursor from the preceding page")
            })?;
            if expected != revision {
                anyhow::bail!(
                    "body changed or cursor belongs to another task/detail; restart without cursor"
                );
            }
            offset
                .parse::<usize>()
                .map_err(|_| anyhow::anyhow!("invalid body cursor offset"))?
        }
    };
    if offset > text.len() || !text.is_char_boundary(offset) {
        anyhow::bail!("invalid body cursor boundary; restart without cursor");
    }
    let limit = limit.clamp(4, MCP_TASK_BODY_PAGE_BYTES);
    let mut end = offset.saturating_add(limit).min(text.len());
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    let mut out = json!({
        "text": &text[offset..end],
        "format": if matches!(detail, "report" | "structured_exit") { "json" } else { "text" },
        "offset": offset,
        "total_bytes": text.len(),
    });
    if end < text.len() {
        out["next_cursor"] = json!(format!("{revision}:{end}"));
    }
    Ok(out)
}

/// MCP routine status and explicit exact body pages. HTTP control uses its
/// own bounded projection; workflow consumers retain full internal data.
pub(crate) fn mcp_task_status_json(
    task: &Task,
    detail: &str,
    cursor: Option<&str>,
    limit: Option<usize>,
    tail: usize,
    debug: bool,
) -> anyhow::Result<Value> {
    if !matches!(detail, "summary" | "result" | "report" | "structured_exit") {
        anyhow::bail!("detail must be summary, result, report, or structured_exit");
    }
    if detail == "summary" && (cursor.is_some() || limit.is_some()) {
        anyhow::bail!("cursor and limit require detail=result, report, or structured_exit");
    }
    if detail != "summary" && tail > 0 {
        anyhow::bail!("tail is an event preview for detail=summary; fetch the body separately");
    }
    let inner = task.inner.lock();
    let mut out = task_view_json_from_inner(&inner, false, debug, debug);
    // Debug diagnostics never reintroduce an unbounded full progress report.
    if let Some(report) = inner.report.as_ref() {
        out["report"] = task_report_summary(
            &report.message,
            report.needs.as_deref(),
            report.data.is_some(),
            report.reported_at,
        );
    }
    out["eventCount"] = json!(observed_event_count(&inner));
    if detail == "result" {
        let body = inner
            .last_assistant_message
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("task has no captured assistant result"))?;
        out["detail"] = json!(detail);
        out["body"] = task_body_page(
            &inner.id,
            detail,
            body,
            cursor,
            limit.unwrap_or(MCP_TASK_BODY_PAGE_BYTES),
        )?;
    } else if detail == "report" {
        let report = inner
            .report
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("task has no progress report"))?;
        // Stable serialized source data, not report.to_json's ticking reportedAgo.
        let body = serde_json::to_string(report)?;
        out["detail"] = json!(detail);
        out["body"] = task_body_page(
            &inner.id,
            detail,
            &body,
            cursor,
            limit.unwrap_or(MCP_TASK_BODY_PAGE_BYTES),
        )?;
    } else if detail == "structured_exit" {
        let exit = inner
            .last_assistant_message
            .as_deref()
            .filter(|_| inner.provider == Provider::Workflow)
            .and_then(|message| serde_json::from_str::<Value>(message).ok())
            .and_then(|mut value| {
                value
                    .as_object_mut()
                    .and_then(|object| object.remove("structured_exit"))
            })
            .filter(|value| !value.is_null())
            .ok_or_else(|| anyhow::anyhow!("task has no workflow structured exit"))?;
        let body = serde_json::to_string(&exit)?;
        out["detail"] = json!(detail);
        out["body"] = task_body_page(
            &inner.id,
            detail,
            &body,
            cursor,
            limit.unwrap_or(MCP_TASK_BODY_PAGE_BYTES),
        )?;
    } else if tail > 0 {
        let mut recent = Vec::new();
        let mut bytes = 0;
        for event in inner
            .events
            .iter()
            .rev()
            .filter_map(compact_status_event)
            .take(tail.min(50))
        {
            let event_bytes = serde_json::to_vec(&event)?.len();
            if bytes + event_bytes > MCP_TASK_EVENTS_BYTES {
                break;
            }
            bytes += event_bytes;
            recent.push(event);
        }
        if recent.len() < tail.min(inner.events.len()) {
            out["eventTruncation"] = json!({"requested": tail, "returned": recent.len(), "retained_events": inner.events.len(), "byte_limit": MCP_TASK_EVENTS_BYTES});
        }
        recent.reverse();
        out["recentEvents"] = json!(recent);
    }
    Ok(out)
}

/// Completed waits return the deliverable, with exact continuation for large
/// bodies. Small structured exits stay inline; large ones have an explicit
/// JSON body accessor. Internal workflow consumers keep the full exit.
pub(crate) fn mcp_task_result_json(task: &Task) -> Value {
    let inner = task.inner.lock();
    mcp_task_result_json_from_inner(&inner)
}

fn mcp_task_result_json_from_inner(inner: &TaskInner) -> Value {
    let mut out = task_view_json_from_inner(inner, true, false, false);
    if let Some(body) = inner.last_assistant_message.as_deref()
        && body.len() > MCP_TASK_BODY_PAGE_BYTES
    {
        let page = task_body_page(&inner.id, "result", body, None, MCP_TASK_BODY_PAGE_BYTES)
            .expect("initial page of an existing UTF-8 body is valid");
        out["result"] = page["text"].clone();
        out["resultTruncated"] = json!(true);
        out["resultBytes"] = json!(body.len());
        out["resultCursor"] = page["next_cursor"].clone();
        out["resultHint"] = json!(
            "Continue with bro_status(task_id=..., detail=result, cursor=resultCursor), then follow body.next_cursor; concatenate text exactly."
        );
    }
    if let Some(exit) = out.get("structuredExit") {
        let bytes = serde_json::to_vec(exit).map(|body| body.len()).unwrap_or(0);
        if bytes > MCP_TASK_BODY_PAGE_BYTES {
            out.as_object_mut().unwrap().remove("structuredExit");
            out["structuredExitOmitted"] = json!(true);
            out["structuredExitBytes"] = json!(bytes);
            out["structuredExitHint"] = json!(
                "Read bro_status(task_id=..., detail=structured_exit); concatenate body.text pages using body.next_cursor, then parse JSON."
            );
        }
    }
    out
}

fn populate_transcript_handle(task: &Task) {
    let (provider, session_id, already_located) = {
        let inner = task.inner.lock();
        (
            inner.provider,
            inner.session_id.clone(),
            inner.transcript_location.is_some(),
        )
    };
    if already_located || session_id.is_empty() || session_id == "pending" {
        return;
    }
    let registry = TranscriptAdapterRegistry::from_runtime_config();
    let Ok(Some(location)) = registry.locate(provider, &session_id) else {
        return;
    };
    let mut inner = task.inner.lock();
    if inner.transcript_location.is_none() && inner.session_id == session_id {
        inner.transcript_location = Some(location);
    }
}

/// Project a task's core state into the shared `bro_protocol` wire DTO — the
/// status-plane half of the contract bottom (harness-process-boundary.md §2).
/// The control endpoint retains optional dispatch/error facets of this DTO.
/// Current Fleet clients use roster snapshots and SSE for live task state.
/// (A free fn, not `From`, because the orphan rule forbids
/// `impl From<&TaskInner> for bro_protocol::TaskSnapshot` — both are foreign.)
pub fn protocol_task_snapshot(inner: &TaskInner) -> bro_protocol::TaskSnapshot {
    protocol_task_snapshot_projection(inner, true)
}

fn protocol_task_snapshot_projection(inner: &TaskInner, full: bool) -> bro_protocol::TaskSnapshot {
    use bro_protocol::TaskStatus as Wire;
    let status = match inner.status {
        TaskStatus::Running => Wire::Running,
        TaskStatus::Completed => Wire::Completed,
        TaskStatus::Failed => Wire::Failed,
        TaskStatus::Cancelled => Wire::Cancelled,
    };
    let error = if matches!(inner.status, TaskStatus::Failed) && !inner.stderr.trim().is_empty() {
        Some(bro_core::BroError::new(
            "task_failed",
            if full {
                inner.stderr.trim().to_string()
            } else {
                tail_str_safe(inner.stderr.trim(), 512)
            },
        ))
    } else {
        None
    };
    bro_protocol::TaskSnapshot {
        task_id: bro_core::TaskId::new(inner.id.clone()),
        session_id: (!inner.session_id.is_empty())
            .then(|| bro_core::SessionId::new(inner.session_id.clone())),
        status,
        last_message: full.then(|| inner.last_assistant_message.clone()).flatten(),
        error,
        origin: inner.origin,
        managed_worktree: inner.managed_worktree.clone(),
        workflow_owned: inner.workflow_owned,
        interrupted: inner.interrupted,
    }
}

const CONTROL_BODY_WIRE_BYTES: usize = 4096;
const CONTROL_EVENTS_WIRE_BYTES: usize = 4096;

/// Exact pages budget serialized JSON as well as UTF-8 bytes. Escaped control
/// characters can occupy six wire bytes each, and the response transport may
/// mirror this body in both text and structured content.
fn control_body_page(
    task_id: &str,
    detail: &str,
    body: &str,
    cursor: Option<&str>,
    limit: Option<usize>,
) -> anyhow::Result<Value> {
    let mut limit = limit
        .unwrap_or(MCP_TASK_BODY_PAGE_BYTES)
        .clamp(4, MCP_TASK_BODY_PAGE_BYTES);
    loop {
        let page = task_body_page(task_id, detail, body, cursor, limit)?;
        if serde_json::to_vec(&page)?.len() <= CONTROL_BODY_WIRE_BYTES || limit == 4 {
            return Ok(page);
        }
        limit = (limit / 2).max(4);
    }
}

/// Return the tail of `s` that fits within `max_bytes` on a char boundary.
fn tail_str_safe(s: &str, max_bytes: usize) -> String {
    if s.len() <= max_bytes {
        return s.to_string();
    }
    let mut start = s.len() - max_bytes;
    while start < s.len() && !s.is_char_boundary(start) {
        start += 1;
    }
    s[start..].to_string()
}

/// Compatibility status used by control and attached supervision. Full result
/// bodies stay in task_result_json for workflow consumers; this projection
/// shares the MCP deliverable and report continuation contract.
pub fn task_status_json(task: &Task, tail: usize) -> Value {
    let inner = task.inner.lock();
    let mut obj = mcp_task_result_json_from_inner(&inner);
    if let Some(result) = inner.last_assistant_message.as_deref() {
        let page = control_body_page(&inner.id, "result", result, None, None)
            .expect("initial UTF-8 body page is valid");
        obj["result"] = page["text"].clone();
        if let Some(cursor) = page.get("next_cursor") {
            obj["resultTruncated"] = json!(true);
            obj["resultBytes"] = json!(result.len());
            obj["resultCursor"] = cursor.clone();
            obj["resultHint"] = json!(
                "Continue with bro_status(task_id=..., detail=result, cursor=resultCursor), then follow body.next_cursor; concatenate text exactly."
            );
        }
    }
    // The optional snapshot keeps typed control facets but never duplicates
    // the assistant body. Its error is a preview, with exact captured stderr
    // available through the control endpoint's detail=stderr pages.
    let snapshot = protocol_task_snapshot_projection(&inner, false);
    if snapshot.error.is_some()
        || snapshot.origin != bro_core::Origin::Unknown
        || snapshot.managed_worktree.is_some()
        || snapshot.workflow_owned
        || snapshot.interrupted
    {
        let mut value = serde_json::to_value(&snapshot).unwrap_or(Value::Null);
        value.as_object_mut().unwrap().remove("last_message");
        obj["snapshot"] = value;
    }
    // A single stderr tail carries the actionable failure; the full captured
    // stream stays available as a separate body rather than a second preview.
    obj.as_object_mut().unwrap().remove("stderr");
    let event_count = observed_event_count(&inner);
    if (inner.status == TaskStatus::Failed || event_count == 0) && !inner.stderr.trim().is_empty() {
        let stderr = inner.stderr.trim_end();
        obj["stderrTail"] = json!(tail_str_safe(stderr, 1024));
        if stderr.len() > 1024 {
            obj["stderrTruncated"] = json!(true);
            obj["stderrBytes"] = json!(inner.stderr.len());
        }
    }
    obj["eventCount"] = json!(event_count);
    if tail > 0 {
        let mut recent = Vec::new();
        let mut bytes = 2;
        for event in inner
            .events
            .iter()
            .rev()
            .filter_map(compact_status_event)
            .take(tail.min(50))
        {
            let event_bytes = serde_json::to_vec(&event).map(|v| v.len()).unwrap_or(0);
            if bytes + event_bytes + 1 > CONTROL_EVENTS_WIRE_BYTES {
                break;
            }
            bytes += event_bytes + 1;
            recent.push(event);
        }
        // This is a compact preview even when every selected event fits:
        // thinking/stream partials and long strings are deliberately projected.
        obj["eventPreview"] = json!({
            "requested":tail, "returned":recent.len(),
            "retained_events":inner.events.retained_len(),
            "byte_limit":CONTROL_EVENTS_WIRE_BYTES,
        });
        recent.reverse();
        obj["recentEvents"] = json!(recent);
    }
    obj
}

/// HTTP status and exact captured detail bodies. No filesystem access is
/// required by the caller. Event pages contain the retained ring only, with
/// observed/retained counts distinguishing this from a complete transcript.
pub(crate) fn control_task_status_json(
    task: &Task,
    detail: &str,
    cursor: Option<&str>,
    limit: Option<usize>,
    tail: usize,
) -> anyhow::Result<Value> {
    if detail == "summary" {
        if cursor.is_some() || limit.is_some() {
            anyhow::bail!("cursor and limit require an explicit body detail");
        }
        let mut out = task_status_json(task, tail);
        if out.get("resultCursor").is_some() {
            out["resultHint"] = json!(
                "GET /control/status/{taskId}?detail=result&cursor={resultCursor}; concatenate body.text and follow body.next_cursor."
            );
        }
        if out.get("structuredExitOmitted").is_some() {
            out["structuredExitHint"] = json!(
                "GET /control/status/{taskId}?detail=structured_exit; concatenate body.text pages, then parse JSON."
            );
        }
        if out["report"]["detailsOmitted"] == true {
            out["report"]["detailHint"] = json!(
                "GET /control/status/{taskId}?detail=report; concatenate body.text pages, then parse JSON."
            );
        }
        if out.get("stderrTruncated").is_some() {
            out["stderrHint"] = json!(
                "GET /control/status/{taskId}?detail=stderr for exact captured stderr pages."
            );
        }
        if out.get("eventPreview").is_some() {
            out["eventPreview"]["detailHint"] = json!(
                "GET /control/status/{taskId}?detail=events for exact retained events as JSON pages; this ring may not contain the full transcript."
            );
        }
        return Ok(out);
    }
    if tail > 0 {
        anyhow::bail!("tail is only valid with detail=summary");
    }
    let inner = task.inner.lock();
    let body = match detail {
        "result" => inner
            .last_assistant_message
            .clone()
            .ok_or_else(|| anyhow::anyhow!("task has no captured assistant result"))?,
        "report" => serde_json::to_string(
            inner
                .report
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("task has no progress report"))?,
        )?,
        "stderr" => inner.stderr.clone(),
        "events" => serde_json::to_string(&inner.events.iter().collect::<Vec<_>>())?,
        "structured_exit" => {
            let exit = inner
                .last_assistant_message
                .as_deref()
                .filter(|_| inner.provider == Provider::Workflow)
                .and_then(|message| serde_json::from_str::<Value>(message).ok())
                .and_then(|mut value| {
                    value
                        .as_object_mut()
                        .and_then(|object| object.remove("structured_exit"))
                })
                .filter(|value| !value.is_null())
                .ok_or_else(|| anyhow::anyhow!("task has no workflow structured exit"))?;
            serde_json::to_string(&exit)?
        }
        _ => anyhow::bail!(
            "detail must be summary, result, report, structured_exit, stderr, or events"
        ),
    };
    let mut page = control_body_page(&inner.id, detail, &body, cursor, limit)?;
    page["format"] = json!(
        if matches!(detail, "report" | "structured_exit" | "events") {
            "json"
        } else {
            "text"
        }
    );
    let mut out = json!({"taskId":inner.id, "sessionId":inner.session_id, "status":inner.status, "detail":detail, "body":page});
    if detail == "events" {
        out["retainedEvents"] = json!(inner.events.retained_len());
        out["eventCount"] = json!(observed_event_count(&inner));
    }
    Ok(out)
}

fn compact_status_event(event: &Value) -> Option<Value> {
    if event.get("type").and_then(Value::as_str) == Some("stream_event") {
        return None;
    }
    let mut event = event.clone();
    strip_thinking_blocks(&mut event);
    bound_status_strings(&mut event);
    Some(event)
}

fn strip_thinking_blocks(value: &mut Value) {
    match value {
        Value::Array(items) => {
            items.retain(|item| item.get("type").and_then(Value::as_str) != Some("thinking"));
            for item in items {
                strip_thinking_blocks(item);
            }
        }
        Value::Object(map) => {
            for value in map.values_mut() {
                strip_thinking_blocks(value);
            }
        }
        _ => {}
    }
}

fn bound_status_strings(value: &mut Value) {
    const MAX: usize = 2000;
    match value {
        Value::String(s) => {
            if s.len() > MAX {
                let mut end = MAX;
                while end > 0 && !s.is_char_boundary(end) {
                    end -= 1;
                }
                let omitted = s.len().saturating_sub(end);
                s.truncate(end);
                s.push_str(&format!("…[truncated {omitted} bytes]"));
            }
        }
        Value::Array(items) => {
            for item in items {
                bound_status_strings(item);
            }
        }
        Value::Object(map) => {
            for value in map.values_mut() {
                bound_status_strings(value);
            }
        }
        _ => {}
    }
}

pub fn timeout_snapshot_json(task: &Task) -> Value {
    let inner = task.inner.lock();
    let elapsed = format_elapsed(inner.started_at, None);
    let event_count = observed_event_count(&inner);
    let last_activity = inner.last_assistant_message.as_deref().map(|msg| {
        let clean = msg.replace('\n', " ");
        let teaser: String = clean.chars().take(80).collect();
        if teaser.len() < clean.len() {
            format!("{teaser}…")
        } else {
            clean
        }
    });

    let keep_going = if inner.status.is_terminal() {
        "no"
    } else if event_count > 0 {
        "yes"
    } else {
        "check_status"
    };

    serde_json::json!({
        "taskId": inner.id,
        "provider": inner.provider,
        "sessionId": inner.session_id,
        "status": inner.status,
        "timed_out": true,
        "elapsed": elapsed,
        "eventCount": event_count,
        "keep_going": keep_going,
        "lastAssistantSnippet": last_activity,
        "interrupted": inner.interrupted,
        "supervision": inner
            .supervision
            .snapshot_for_response(&supervision::config(), now_ms()),
    })
}

fn observed_event_count(inner: &TaskInner) -> usize {
    inner.observed_event_count()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    // Async env-mutating tests hold the std `test_env_lock` across `.await` on
    // purpose (env must stay set while the awaited code reads it); #[tokio::test]
    // is single-threaded so this can't deadlock the runtime.
    #![allow(clippy::await_holding_lock)]
    use super::*;

    fn test_tail_tx() -> tokio::sync::broadcast::Sender<tail::TailEvent> {
        let (tail_tx, _) = tokio::sync::broadcast::channel(16);
        tail_tx
    }

    #[test]
    fn managed_checkout_workspace_id_uses_existing_marker() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let status = std::process::Command::new("git")
            .args(["init", "-q"])
            .current_dir(&root)
            .status()
            .unwrap();
        assert!(status.success());
        std::fs::write(
            root.join(".git/blackbox-managed-checkout"),
            format!("{}\n", bbox_corpus_core::git::MANAGED_CHECKOUT_MARKER_V1),
        )
        .unwrap();
        let checkout_id = bbox_corpus_core::identity::ensure_checkout_id(&root).unwrap();

        let workspace_id = workspace_id_for_cwd(root.to_str()).unwrap().unwrap();
        assert_eq!(workspace_id.as_str(), checkout_id);
    }

    #[test]
    fn managed_checkout_workspace_id_repairs_bad_marker_with_fresh_identity() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let status = std::process::Command::new("git")
            .args(["init", "-q"])
            .current_dir(&root)
            .status()
            .unwrap();
        assert!(status.success());
        std::fs::write(
            root.join(".git/blackbox-managed-checkout"),
            format!("{}\n", bbox_corpus_core::git::MANAGED_CHECKOUT_MARKER_V1),
        )
        .unwrap();
        bbox_corpus_core::identity::ensure_checkout_id(&root).unwrap();
        std::fs::write(
            root.join(".bbox/local/checkout-id"),
            "0123456789abcdef0123456789abcdeF\n",
        )
        .unwrap();

        let workspace_id = workspace_id_for_cwd(root.to_str()).unwrap().unwrap();
        assert_ne!(workspace_id.as_str(), "0123456789abcdef0123456789abcdeF");
        assert_eq!(
            bbox_corpus_core::identity::read_checkout_id(&root.join(".bbox/local/checkout-id"))
                .unwrap()
                .as_deref(),
            Some(workspace_id.as_str())
        );
    }

    #[test]
    fn ordinary_checkout_does_not_gain_workspace_authority_from_cwd() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let status = std::process::Command::new("git")
            .args(["init", "-q"])
            .current_dir(&root)
            .status()
            .unwrap();
        assert!(status.success());
        bbox_corpus_core::identity::ensure_checkout_id(&root).unwrap();

        assert_eq!(workspace_id_for_cwd(root.to_str()).unwrap(), None);
    }

    struct FixedWorkspaceBindingAuthority;

    impl WorkspaceBindingAuthority for FixedWorkspaceBindingAuthority {
        fn candidate_scopes(&self) -> anyhow::Result<Vec<bro_protocol::WorkerWorkspaceScope>> {
            Ok(vec![bro_protocol::WorkerWorkspaceScope::try_new(
                "test-repo",
                ".",
            )?])
        }

        fn mint(
            &self,
            _task_id: &str,
            _session_id: &str,
            _identity: &bro_protocol::WorkerWorkspaceIdentity,
        ) -> anyhow::Result<MintedWorkspaceBinding> {
            Ok(MintedWorkspaceBinding {
                token: bro_protocol::WorkspaceBindingToken::parse("a".repeat(64)).unwrap(),
                scope: bbox_corpus_core::identity::PublishedScope::try_new("test-repo", ".")
                    .unwrap(),
            })
        }

        fn restore(
            &self,
            _task_id: &str,
            _session_id: &str,
            _identity: &bro_protocol::WorkerWorkspaceIdentity,
            _token: &bro_protocol::WorkspaceBindingToken,
        ) -> anyhow::Result<()> {
            Ok(())
        }

        fn revoke_task(&self, _task_id: &str) {}
    }

    #[test]
    fn managed_child_launch_keeps_workspace_capability_out_of_argv() {
        let mut env = crate::util::TestEnvGuard::new();
        env.set("BLACKBOX_MCP_NAME", "selfbox");
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        std::process::Command::new("git")
            .args(["init", "-q"])
            .current_dir(&root)
            .status()
            .unwrap();
        std::fs::write(
            root.join(".git/blackbox-managed-checkout"),
            format!("{}\n", bbox_corpus_core::git::MANAGED_CHECKOUT_MARKER_V1),
        )
        .unwrap();
        env.set(
            "BLACKBOX_CONFIG",
            root.join("missing.toml").to_str().unwrap(),
        );
        let workspace_identity = bro_protocol::WorkerWorkspaceIdentity {
            workspace_id: bro_core::WorkspaceId::parse(
                bbox_corpus_core::identity::ensure_checkout_id(&root).unwrap(),
            )
            .unwrap(),
            scope: bro_protocol::WorkerWorkspaceScope::try_new("test-repo", ".").unwrap(),
        };

        let spec = prepare_harness_child_launch(
            "task-bound".to_string(),
            "pending".to_string(),
            Provider::Glm,
            vec!["-p".to_string(), "work".to_string()],
            root.to_str(),
            None,
            None,
            None,
            None,
            &root,
            Some("http://127.0.0.1:7264/mcp?surface=agent-internal"),
            Some(&FixedWorkspaceBindingAuthority),
            Some(workspace_identity),
        )
        .unwrap();

        let secret = "a".repeat(64);
        assert!(spec.workspace_id.is_some());
        assert_eq!(
            spec.env.as_map().get(bro_protocol::WORKSPACE_BINDING_ENV),
            Some(&secret)
        );
        assert_eq!(
            spec.env
                .as_map()
                .get(bro_protocol::KNOWLEDGE_SOURCE_URL_ENV)
                .map(String::as_str),
            Some("http://127.0.0.1:7264/mcp?surface=agent-internal")
        );
        assert_eq!(
            serde_json::from_str::<bbox_corpus_core::identity::PublishedScope>(
                spec.env
                    .as_map()
                    .get(bro_protocol::WORKSPACE_SCOPE_ENV)
                    .unwrap()
            )
            .unwrap(),
            bbox_corpus_core::identity::PublishedScope::try_new("test-repo", ".").unwrap()
        );
        assert!(!format!("{:?}", spec.argv).contains(&secret));
        assert!(!format!("{spec:?}").contains(&secret));
        let scrub = spec.env.as_map().get(HARNESS_SPAWN_SCRUB_ENV).unwrap();
        assert!(scrub.contains(bro_protocol::WORKSPACE_BINDING_ENV));
        assert!(scrub.contains(bro_protocol::KNOWLEDGE_SOURCE_URL_ENV));
        assert!(scrub.contains(bro_protocol::WORKSPACE_SCOPE_ENV));
        let raw_config = spec
            .argv
            .windows(2)
            .find(|pair| pair[0] == "--mcp-config")
            .map(|pair| pair[1].as_str())
            .unwrap();
        let config: Value = serde_json::from_str(raw_config).unwrap();
        assert_eq!(
            config["mcpServers"]["selfbox"]["headers"][bro_protocol::WORKSPACE_BINDING_HEADER],
            format!("$env:{}", bro_protocol::WORKSPACE_BINDING_ENV)
        );
        assert!(!raw_config.contains(&secret));
    }

    #[test]
    fn prepare_harness_child_launch_composes_worker_spec() {
        // config::load() reads $BLACKBOX_CONFIG / XDG; point it at a missing
        // path under a tempdir so composition never touches real config state.
        let _env = crate::util::test_env_lock();
        let tmp = tempfile::tempdir().unwrap();
        let store = tmp.path().canonicalize().unwrap();
        let cfg_path = store.join("missing-config.toml");
        // SAFETY: guarded by test_env_lock; the test runtime is single-threaded.
        unsafe {
            std::env::set_var("BLACKBOX_CONFIG", &cfg_path);
        }

        let args = vec![
            "-p".to_string(),
            "do the thing".to_string(),
            "-m".to_string(),
            "some-model".to_string(),
        ];
        let spec = prepare_harness_child_launch(
            "task-1".to_string(),
            "sess-1".to_string(),
            Provider::Glm,
            args,
            Some("/repo/x"),
            None, // env_overrides
            None, // shell_env
            None, // tool_placement
            None, // tool_defaults
            &store,
            None, // self_mcp_url
            None, // workspace_binding_authority
            None, // workspace_identity
        )
        .expect("compose spec");

        // Hard rule: the prompt rides initial_messages, never argv.
        assert!(
            !spec.argv.iter().any(|a| a == "-p" || a == "do the thing"),
            "prompt must not appear in argv: {:?}",
            spec.argv
        );
        assert_eq!(spec.initial_messages.len(), 1);
        assert_eq!(spec.initial_messages[0]["type"], "user");
        assert_eq!(
            spec.initial_messages[0]["message"]["content"][0]["text"],
            "do the thing"
        );

        // Daemon-worker stream-json flags are present.
        assert!(spec.argv.iter().any(|a| a == "--daemon-worker"));
        assert!(spec.argv.iter().any(|a| a == "--exit-when-idle"));
        assert!(spec.argv.iter().any(|a| a == "--replay-user-messages"));
        let ifmt = spec
            .argv
            .iter()
            .position(|a| a == "--input-format")
            .expect("--input-format present");
        assert_eq!(spec.argv[ifmt + 1], "stream-json");

        // env_unset covers the full service scrub list.
        for var in BLACKBOX_SERVICE_ENV_VARS {
            assert!(
                spec.env_unset.iter().any(|k| k == var),
                "env_unset missing service var {var}"
            );
        }
        // The scrub var is composed into env and lists the service vars.
        let scrub = spec
            .env
            .as_map()
            .get(HARNESS_SPAWN_SCRUB_ENV)
            .expect("scrub var present in env");
        assert!(scrub.contains("BRO_HOME"), "scrub list: {scrub}");
        assert!(scrub.contains("BLACKBOX_MCP_URL"), "scrub list: {scrub}");
        // BRO_HOME is pinned on its own field, not duplicated into env.
        assert!(!spec.env.as_map().contains_key("BRO_HOME"));

        // BRO_HOME and event_log_path share one pinned derivation.
        assert_eq!(spec.bro_home, store);
        assert_eq!(
            spec.event_log_path,
            store.join("harness-sessions").join("sess-1.events.jsonl")
        );

        assert_eq!(spec.cwd.as_deref(), Some("/repo/x"));

        // SAFETY: guarded by test_env_lock; restore process env.
        unsafe {
            std::env::remove_var("BLACKBOX_CONFIG");
        }
    }

    #[test]
    fn apply_session_command_maps_protocol_variants_to_harness_wire() {
        use bro_protocol::SessionCommand;

        let task_id = "test-session-command-mapping";
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        harness_controls().write().insert(task_id.to_string(), tx);

        apply_session_command(task_id, SessionCommand::UserTurn { text: "hi".into() }).unwrap();
        let user = rx.try_recv().unwrap();
        assert_eq!(user["type"], "user");
        assert_eq!(user["message"]["content"][0]["text"], "hi");

        apply_session_command(task_id, SessionCommand::Interrupt).unwrap();
        let interrupt = rx.try_recv().unwrap();
        assert_eq!(interrupt["type"], "control_request");
        assert_eq!(interrupt["subtype"], "interrupt");
        assert!(interrupt["request_id"].is_string());

        apply_session_command(task_id, SessionCommand::SetModel { model: "m2".into() }).unwrap();
        let set_model = rx.try_recv().unwrap();
        assert_eq!(set_model["subtype"], "set_model");
        assert_eq!(set_model["model"], "m2");

        apply_session_command(task_id, SessionCommand::Compact).unwrap();
        let compact = rx.try_recv().unwrap();
        assert_eq!(compact["message"]["content"][0]["text"], "/compact");

        harness_controls().write().remove(task_id);
    }

    #[test]
    fn apply_session_command_errors_without_live_channel() {
        let err =
            apply_session_command("no-such-live-task", bro_protocol::SessionCommand::Interrupt)
                .unwrap_err();
        assert!(err.contains("no live harness control channel"));
    }

    #[test]
    fn harness_mcp_config_strips_cli_arg_and_applies_dispatch_placement() {
        let mut env = crate::util::TestEnvGuard::new();
        env.set("BLACKBOX_MCP_NAME", "selfbox");

        let mut args = vec![
            "--model".to_string(),
            "glm-test".to_string(),
            "--mcp-config".to_string(),
            serde_json::json!({
                "mcpServers": {
                    "external": {
                        "type": "stdio",
                        "command": "external-mcp",
                        "args": ["--once"]
                    },
                    "selfbox": {
                        "type": "stdio",
                        "command": "must-not-shadow-daemon"
                    }
                },
                "tool_placement": {
                    "mcp__external__ignored_json_source": "both"
                }
            })
            .to_string(),
            "--effort".to_string(),
            "low".to_string(),
        ];
        let config = build_harness_mcp_config(
            &mut args,
            Some(BTreeMap::from([(
                "mcp__external__placed".to_string(),
                "in-box".to_string(),
            )])),
            Some("http://127.0.0.1:7264/mcp?surface=default"),
            false,
        )
        .unwrap()
        .unwrap();
        let config: Value = serde_json::from_str(&config).unwrap();

        assert_eq!(
            args,
            vec![
                "--model".to_string(),
                "glm-test".to_string(),
                "--effort".to_string(),
                "low".to_string()
            ]
        );
        assert_eq!(config["mcpServers"].as_object().unwrap().len(), 2);
        assert!(config["mcpServers"]["external"].is_object());
        assert_eq!(
            config["mcpServers"]["selfbox"]["url"],
            "http://127.0.0.1:7264/mcp?surface=default"
        );
        assert_eq!(config["mcpServers"]["selfbox"]["type"], "http");
        assert!(config["mcpServers"]["selfbox"].get("command").is_none());
        assert_eq!(config["tool_placement"]["mcp__external__placed"], "in-box");
        assert!(
            config["tool_placement"]
                .get("mcp__external__ignored_json_source")
                .is_none()
        );
    }

    #[test]
    fn harness_mcp_config_uses_supplied_self_mcp_surface_url() {
        let mut env = crate::util::TestEnvGuard::new();
        env.set("BLACKBOX_MCP_NAME", "selfbox");
        let mut args = Vec::new();

        let config = build_harness_mcp_config(
            &mut args,
            None,
            Some("http://127.0.0.1:7264/mcp?surface=agent-internal"),
            false,
        )
        .unwrap()
        .unwrap();
        let config: Value = serde_json::from_str(&config).unwrap();

        assert_eq!(
            config["mcpServers"]["selfbox"]["url"],
            "http://127.0.0.1:7264/mcp?surface=agent-internal"
        );
    }

    #[test]
    #[allow(clippy::disallowed_methods)]
    fn child_launch_moves_prompt_to_stdin_and_preserves_session_policy() {
        let mut env = crate::util::TestEnvGuard::new();
        env.set("BLACKBOX_MCP_NAME", "selfbox");
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        // Isolate config::load() (bin resolution) from real config state.
        env.set(
            "BLACKBOX_CONFIG",
            root.join("missing-config.toml").to_str().unwrap(),
        );
        let spec = prepare_harness_child_launch(
            "task-x".to_string(),
            "sess-x".to_string(),
            Provider::Glm,
            vec![
                "-p".to_string(),
                "initial turn".to_string(),
                "--model".to_string(),
                "glm-test".to_string(),
                "--mcp-config".to_string(),
                serde_json::json!({
                    "mcpServers": {
                        "external": {"type": "stdio", "command": "external-mcp"}
                    }
                })
                .to_string(),
            ],
            root.to_str(),
            Some(HashMap::from([(
                "ANTHROPIC_AUTH_TOKEN".to_string(),
                "secret".to_string(),
            )])),
            Some(BTreeMap::from([(
                "RUSTC_WRAPPER".to_string(),
                "sccache".to_string(),
            )])),
            Some(BTreeMap::from([(
                "mcp__external__inspect".to_string(),
                "both".to_string(),
            )])),
            Some(BTreeMap::from([(
                "default:file_read.offset".to_string(),
                "10".to_string(),
            )])),
            &root,
            Some("http://127.0.0.1:7264/mcp?surface=default"),
            None,
            None,
        )
        .unwrap();

        // Prompt rides initial_messages, never argv.
        assert_eq!(spec.initial_messages.len(), 1);
        assert_eq!(
            spec.initial_messages[0]["message"]["content"][0]["text"],
            "initial turn"
        );
        assert!(!spec.argv.iter().any(|arg| arg == "initial turn"));
        for flag in [
            "--input-format",
            "--replay-user-messages",
            "--exit-when-idle",
            "--daemon-worker",
            "--cwd",
            "--mcp-config",
            "--capability-mcp-server",
            "--additional-context",
            "--shell-env",
        ] {
            assert!(spec.argv.iter().any(|arg| arg == flag), "missing {flag}");
        }
        assert_eq!(
            spec.env.as_map().get("BRO_HARNESS_PROVIDER"),
            Some(&"glm".to_string())
        );
        // BRO_HOME is pinned on its own field, not duplicated into env.
        assert!(!spec.env.as_map().contains_key("BRO_HOME"));
        assert_eq!(spec.bro_home, root);
        assert_eq!(
            spec.event_log_path,
            root.join("harness-sessions").join("sess-x.events.jsonl")
        );
        // env_unset carries the full service scrub list.
        assert!(spec.env_unset.iter().any(|k| k == "BRO_HOME"));
        assert!(spec.env_unset.iter().any(|k| k == "BLACKBOX_MCP_URL"));
        let scrub = spec
            .env
            .as_map()
            .get(HARNESS_SPAWN_SCRUB_ENV)
            .expect("child shell scrub list");
        assert!(scrub.contains("ANTHROPIC_AUTH_TOKEN"));
        assert!(scrub.contains("BRO_HOME"));

        let raw_config = spec
            .argv
            .windows(2)
            .find(|pair| pair[0] == "--mcp-config")
            .map(|pair| pair[1].as_str())
            .unwrap();
        let config: Value = serde_json::from_str(raw_config).unwrap();
        assert!(config["mcpServers"]["external"].is_object());
        assert!(config["mcpServers"]["selfbox"].is_object());
        assert_eq!(config["tool_placement"]["mcp__external__inspect"], "both");
    }

    #[cfg(unix)]
    #[tokio::test]
    #[allow(clippy::disallowed_methods)]
    async fn harness_child_error_is_terminal_without_poisoning_sibling_process() {
        use std::os::unix::fs::PermissionsExt;

        fn write_fake_harness(path: &std::path::Path, event: &str) {
            std::fs::write(
                path,
                format!("#!/bin/sh\nIFS= read -r input\nprintf '%s\\n' '{event}'\n"),
            )
            .unwrap();
            let mut permissions = std::fs::metadata(path).unwrap().permissions();
            permissions.set_mode(0o755);
            std::fs::set_permissions(path, permissions).unwrap();
        }

        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let failed_bin = root.join("failed-harness");
        let healthy_bin = root.join("healthy-harness");
        write_fake_harness(
            &failed_bin,
            r#"{"type":"result","is_error":true,"result":"child turn failed","session_id":"session-error"}"#,
        );
        write_fake_harness(
            &healthy_bin,
            r#"{"type":"result","is_error":false,"result":"sibling ok","session_id":"session-ok"}"#,
        );

        let mut env = crate::util::TestEnvGuard::new();
        env.remove("BLACKBOX_MCP_URL");
        env.set("BRO_HARNESS_BIN", &failed_bin);
        let store = Arc::new(RwLock::new(TaskStore::new()));
        let (tail_tx, _) = tokio::sync::broadcast::channel(32);
        let store_dir = root.join("store");
        let failed = spawn_task_with_tool_placement(
            "child-error".to_string(),
            Provider::Glm,
            vec![
                "-p".to_string(),
                "fail".to_string(),
                "--model".to_string(),
                "glm-test".to_string(),
            ],
            "session-error".to_string(),
            Some(root.to_string_lossy().into_owned()),
            None,
            store_dir.clone(),
            store.clone(),
            tail_tx.clone(),
            None,
            None,
            None,
            None,
            None,
            None,
            bro_core::Origin::AgentDispatch,
        )
        .await;

        env.set("BRO_HARNESS_BIN", &healthy_bin);
        let healthy = spawn_task_with_tool_placement(
            "child-healthy".to_string(),
            Provider::Glm,
            vec![
                "-p".to_string(),
                "succeed".to_string(),
                "--model".to_string(),
                "glm-test".to_string(),
            ],
            "session-ok".to_string(),
            Some(root.to_string_lossy().into_owned()),
            None,
            store_dir,
            store,
            tail_tx,
            None,
            None,
            None,
            None,
            None,
            None,
            bro_core::Origin::AgentDispatch,
        )
        .await;

        tokio::time::timeout(std::time::Duration::from_secs(5), wait_for_task(&failed))
            .await
            .expect("failed child terminates");
        tokio::time::timeout(std::time::Duration::from_secs(5), wait_for_task(&healthy))
            .await
            .expect("healthy sibling terminates");

        let failed_inner = failed.inner.lock();
        assert_eq!(failed_inner.status, TaskStatus::Failed);
        assert!(failed_inner.stderr.contains("child turn failed"));
        drop(failed_inner);
        assert_eq!(healthy.inner.lock().status, TaskStatus::Completed);
    }

    /// The invariant slice 3 establishes: EVERY dispatch path that can produce
    /// a harness worker goes through the executor seam, so when the executor
    /// is fleetd no harness child is ever a direct daemon child.
    ///
    /// `spawn_with_pre_minted_id` is the path that used to bypass it (Badgey's
    /// one-shot persona dispatch), which is why it is the one asserted here.
    /// Two observable signatures of the seam, neither of which the old inline
    /// spawn produced: the control lane is registered in `harness_controls`
    /// (the seam's `WorkerHandle.control`), and the transcript location is
    /// pinned from the spawn spec's `event_log_path` under `harness-sessions/`.
    #[tokio::test]
    async fn a_pre_minted_dispatch_goes_through_the_executor_seam() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let harness_bin = root.join("seam-harness");
        // Idle until stdin closes, so the worker is still live while we assert.
        std::fs::write(&harness_bin, "#!/bin/sh\ncat > /dev/null\n").unwrap();
        let mut permissions = std::fs::metadata(&harness_bin).unwrap().permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&harness_bin, permissions).unwrap();

        let mut env = crate::util::TestEnvGuard::new();
        env.remove("BLACKBOX_MCP_URL");
        env.set("BRO_HARNESS_BIN", &harness_bin);

        let store = Arc::new(RwLock::new(TaskStore::new()));
        let (tail_tx, _rx) = tokio::sync::broadcast::channel(32);
        let task = spawn_with_pre_minted_id(
            "seam-task".to_string(),
            SpawnTaskParams {
                provider: Provider::Glm,
                args: vec!["-p".to_string(), "hello".to_string()],
                session_id: "pending".to_string(),
                cwd: Some(root.to_string_lossy().into_owned()),
                env_overrides: None,
                store_dir: root.join("store"),
                task_store: store.clone(),
                tail_tx,
                roster_events: None,
                bro_label: None,
                agent_label: None,
                system_events: None,
                origin: bro_core::Origin::AgentDispatch,
            },
        )
        .await
        .expect("pre-minted dispatch");

        assert!(
            harness_controls().read().contains_key("seam-task"),
            "a seam dispatch registers its control lane; the old inline spawn did not"
        );
        let location = task
            .inner
            .lock()
            .transcript_location
            .clone()
            .expect("the seam pins a transcript location from the spec");
        assert!(
            location.path.to_string_lossy().contains("harness-sessions"),
            "location must come from the spec's event_log_path: {}",
            location.path.display()
        );
        // A dispatch with no provider session yet keeps the task's session_id
        // as the placeholder while the spec uses the unique task id as its
        // supervision key, so the location records no provider session.
        assert_eq!(location.session_id, None);

        harness_killers()
            .read()
            .get("seam-task")
            .expect("a seam dispatch registers a kill switch")
            .kill();
    }

    /// `Provider::Workflow` backs daemon-internal tasks and has no worker
    /// binary. With the inline spawn path gone it must fail with the actual
    /// reason rather than trying to exec a binary named "workflow".
    #[tokio::test]
    async fn a_non_dispatchable_provider_fails_with_the_reason() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let store = Arc::new(RwLock::new(TaskStore::new()));
        let (tail_tx, _rx) = tokio::sync::broadcast::channel(32);

        let task = spawn_with_pre_minted_id(
            "workflow-task".to_string(),
            SpawnTaskParams {
                provider: Provider::Workflow,
                args: Vec::new(),
                session_id: "pending".to_string(),
                cwd: Some(root.to_string_lossy().into_owned()),
                env_overrides: None,
                store_dir: root.join("store"),
                task_store: store.clone(),
                tail_tx,
                roster_events: None,
                bro_label: None,
                agent_label: None,
                system_events: None,
                origin: bro_core::Origin::AgentDispatch,
            },
        )
        .await
        .expect("the dispatch is accepted, then fails with a reason");

        let inner = task.inner.lock();
        assert_eq!(inner.status, TaskStatus::Failed);
        assert!(
            inner.stderr.contains("not a dispatchable provider"),
            "the failure must name the real cause, not a missing binary: {}",
            inner.stderr
        );
        assert!(
            !inner.stderr.contains("No such file"),
            "must not read as a missing binary: {}",
            inner.stderr
        );
    }

    /// The durable cursor advances only for events that carry a `seq`, and
    /// only after ingest. It is what a re-adopting daemon replays from, so an
    /// event without a seq must leave it alone (replay more, never less).
    #[tokio::test]
    async fn ingest_advances_the_durable_cursor_only_on_seq_carrying_events() {
        let (tail_tx, _rx) = tokio::sync::broadcast::channel(32);
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let task = spawn_in_process_task(
            "cursor-task".to_string(),
            Provider::Workflow,
            "cursor-session".to_string(),
            None,
            root.clone(),
            Arc::new(RwLock::new(TaskStore::new())),
            tail_tx.clone(),
            None,
            None,
            None,
            None,
            bro_core::Origin::Workflow,
        );

        let (events_tx, events_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
        let join = spawn_harness_ingest_loop(
            task.clone(),
            Provider::Glm,
            "cursor-task".to_string(),
            root,
            None,
            tail_tx,
            None,
            events_rx,
        );

        for line in [
            r#"{"type":"system","seq":4}"#,
            // No seq: a pre-upgrade harness build. Must not advance.
            r#"{"type":"assistant"}"#,
            r#"{"type":"assistant","seq":7}"#,
            // Out of order: the cursor is a high-water mark, never a rewind,
            // or a replay would re-deliver events already applied.
            r#"{"type":"assistant","seq":5}"#,
        ] {
            events_tx.send(line.to_string()).unwrap();
        }
        drop(events_tx);
        join.await.expect("ingest loop drains to EOF");

        assert_eq!(
            task.inner.lock().harness_ingest_seq,
            7,
            "cursor is the high-water mark of seq-carrying ingested events"
        );
    }

    #[tokio::test]
    async fn remote_ingest_mirrors_worker_events_into_daemon_corpus_state() {
        let (tail_tx, _rx) = tokio::sync::broadcast::channel(32);
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let task = spawn_in_process_task(
            "mirror-task".to_string(),
            Provider::Workflow,
            "mirror-session".to_string(),
            None,
            root.clone(),
            Arc::new(RwLock::new(TaskStore::new())),
            tail_tx.clone(),
            None,
            None,
            None,
            None,
            bro_core::Origin::Workflow,
        );
        let mirror = root.join("daemon-bro/harness-sessions/mirror.events.jsonl");
        let (events_tx, events_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
        let join = spawn_harness_ingest_loop(
            task,
            Provider::Glm,
            "mirror-task".to_string(),
            root,
            Some(mirror.clone()),
            tail_tx,
            None,
            events_rx,
        );
        events_tx
            .send(r#"{"type":"assistant","seq":9,"message":"remote"}"#.to_string())
            .unwrap();
        drop(events_tx);
        join.await.expect("mirror ingest drains");

        let record: Value =
            serde_json::from_str(std::fs::read_to_string(mirror).unwrap().trim_end()).unwrap();
        assert!(record["ts"].as_str().is_some());
        assert_eq!(record["event"]["seq"], 9);
        assert_eq!(record["event"]["message"], "remote");
    }

    /// Re-adoption is the payoff of the whole slice: a task the previous
    /// daemon gave up on gets put back to Running, its restart notice
    /// stripped, its control/kill lanes re-registered, and its own cursor
    /// handed back so the caller can replay from exactly there.
    ///
    /// Installs the process-global re-adoption env, which is fine because
    /// nextest is process-per-test (the repo's mandated runner).
    #[tokio::test]
    async fn readoption_revives_a_task_the_restart_gave_up_on() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let store = Arc::new(RwLock::new(TaskStore::new()));
        let (tail_tx, _rx) = tokio::sync::broadcast::channel(32);

        let task = spawn_in_process_task(
            "adopt-task".to_string(),
            Provider::Glm,
            "adopt-session".to_string(),
            None,
            root.clone(),
            store.clone(),
            tail_tx.clone(),
            None,
            None,
            None,
            None,
            bro_core::Origin::AgentDispatch,
        );
        store
            .write()
            .insert_reserved("adopt-task".to_string(), task.clone())
            .ok();
        {
            // Exactly the state `TaskStore::load` leaves behind for a task
            // that was running when the daemon went down.
            let mut inner = task.inner.lock();
            inner.status = TaskStatus::Failed;
            inner.recoverable = true;
            inner.completed_at = Some(now_ms());
            inner.harness_ingest_seq = 12;
            inner.stderr.push_str(
                "\n[blackbox] server restarted while task was running. \
                 The provider session is still on disk; retry with \
                 `bro_resume(session_id=...)` to continue the conversation \
                 rather than starting a fresh session.",
            );
        }

        install_harness_executor(
            bbox_config::config::ExecutorKind::Local,
            root.clone(),
            store.clone(),
            tail_tx,
            None,
            None,
        );

        let (control_tx, _control_rx) = tokio::sync::mpsc::unbounded_channel::<Value>();
        let (_events_tx, events_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
        let (_outcome_tx, outcome_rx) = tokio::sync::oneshot::channel();
        let cursor = readopt_harness_session(ReadoptedSession {
            session_id: "adopt-session".to_string(),
            task_id: "adopt-task".to_string(),
            workspace_id: None,
            workspace_scope: None,
            workspace_binding_token: None,
            pid: Some(4242),
            state: bro_protocol::SessionState::Running,
            control: control_tx,
            killer: executor::WorkerKill::via_fleetd(
                "adopt-session".to_string(),
                tokio::sync::mpsc::unbounded_channel().0,
            ),
            events: events_rx,
            outcome: outcome_rx,
        });

        assert_eq!(
            cursor,
            Some(12),
            "the caller replays from the task's own durable cursor"
        );
        let inner = task.inner.lock();
        assert_eq!(
            inner.status,
            TaskStatus::Running,
            "a live child means the task is live, whatever the restart concluded"
        );
        assert!(!inner.recoverable);
        assert_eq!(inner.completed_at, None);
        assert!(
            !inner.stderr.contains("server restarted"),
            "the restart notice is wrong once the session is back: {}",
            inner.stderr
        );
        drop(inner);
        assert_eq!(*task.child_id.lock(), Some(4242));
        assert!(
            harness_controls().read().contains_key("adopt-task"),
            "the control lane must be reachable again for bro_steer"
        );
        assert!(
            harness_killers().read().contains_key("adopt-task"),
            "cancel must reach the re-adopted child"
        );
    }

    /// A session fleetd holds that the task store never heard of is declined,
    /// which is what makes the caller leave it running instead of reaping it.
    #[tokio::test]
    async fn readoption_declines_a_session_no_task_matches() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let store = Arc::new(RwLock::new(TaskStore::new()));
        let (tail_tx, _rx) = tokio::sync::broadcast::channel(32);
        install_harness_executor(
            bbox_config::config::ExecutorKind::Local,
            root,
            store,
            tail_tx,
            None,
            None,
        );

        let (control_tx, _control_rx) = tokio::sync::mpsc::unbounded_channel::<Value>();
        let (_events_tx, events_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
        let (_outcome_tx, outcome_rx) = tokio::sync::oneshot::channel();
        let cursor = readopt_harness_session(ReadoptedSession {
            session_id: "ghost-session".to_string(),
            task_id: "ghost-task".to_string(),
            workspace_id: None,
            workspace_scope: None,
            workspace_binding_token: None,
            pid: Some(9999),
            state: bro_protocol::SessionState::Running,
            control: control_tx,
            killer: executor::WorkerKill::via_fleetd(
                "ghost-session".to_string(),
                tokio::sync::mpsc::unbounded_channel().0,
            ),
            events: events_rx,
            outcome: outcome_rx,
        });
        assert_eq!(cursor, None, "an unknown session is declined, not adopted");
    }

    #[test]
    fn fleet_mcp_dispatch_args_is_cockpit_only() {
        // The gate must short-circuit BEFORE any fleet.json read: automation
        // origins inject nothing regardless of operator config (and the test
        // stays isolated from the host's real fleet.json).
        use bro_core::Origin;
        for origin in [
            Origin::Unknown,
            Origin::AgentDispatch,
            Origin::Workflow,
            Origin::Atom,
            Origin::Cron,
            Origin::Webhook,
        ] {
            assert!(
                fleet_mcp_dispatch_args(Provider::Glm, origin).is_empty(),
                "{origin} must not receive fleet MCP servers"
            );
        }
    }

    #[test]
    fn fleet_mcp_dispatch_args_cockpit_injects_fleet_servers() {
        // Cockpit origin end-to-end: fleet.json beside the selected config
        // (BLACKBOX_CONFIG keys the lookup, keeping the test off the host's
        // real config) lands in the dispatch argv as `--mcp-config`, and
        // build_harness_mcp_config merges it with the transient blackbox
        // server, the same consumption the child spawn path runs.
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("config.toml");
        std::fs::write(&config_path, "").unwrap();
        std::fs::write(
            dir.path().join("fleet.json"),
            serde_json::json!({
                "mcpServers": {
                    "tmux": {"type": "stdio", "command": "tmux-mcp"}
                }
            })
            .to_string(),
        )
        .unwrap();
        let mut env = crate::util::TestEnvGuard::new();
        env.set("BLACKBOX_CONFIG", config_path.to_str().unwrap());
        env.set("BLACKBOX_MCP_NAME", "selfbox");

        let mut args = fleet_mcp_dispatch_args(Provider::Glm, bro_core::Origin::Cockpit);
        assert_eq!(args[0], "--mcp-config");

        let config = build_harness_mcp_config(
            &mut args,
            None,
            Some("http://127.0.0.1:7264/mcp?surface=default"),
            false,
        )
        .unwrap()
        .unwrap();
        let config: Value = serde_json::from_str(&config).unwrap();
        assert!(args.is_empty(), "--mcp-config consumed from argv");
        assert!(config["mcpServers"]["tmux"].is_object());
        assert!(config["mcpServers"]["selfbox"].is_object());
    }

    #[test]
    fn protocol_task_snapshot_maps_status_and_error() {
        // Completed task → wire Completed, last_message carried, no error.
        let t = test_task("t1", TaskStatus::Completed, Provider::Glm);
        t.inner.lock().last_assistant_message = Some("OK".to_string());
        let snap = protocol_task_snapshot(&t.inner.lock());
        assert_eq!(snap.task_id.as_str(), "t1");
        assert_eq!(
            snap.session_id.as_ref().map(|s| s.as_str()),
            Some("sess-t1")
        );
        assert_eq!(snap.status, bro_protocol::TaskStatus::Completed);
        assert_eq!(snap.last_message.as_deref(), Some("OK"));
        assert!(snap.error.is_none());

        // Failed task with stderr → error populated with a typed code.
        let f = test_task("t2", TaskStatus::Failed, Provider::Glm);
        f.inner.lock().stderr = "boom".to_string();
        let snap2 = protocol_task_snapshot(&f.inner.lock());
        assert_eq!(snap2.status, bro_protocol::TaskStatus::Failed);
        assert_eq!(
            snap2.error.as_ref().map(|e| e.code.as_str()),
            Some("task_failed")
        );

        // Cancelled maps through; running maps to wire Running.
        assert_eq!(
            protocol_task_snapshot(
                &test_task("t3", TaskStatus::Cancelled, Provider::Glm)
                    .inner
                    .lock()
            )
            .status,
            bro_protocol::TaskStatus::Cancelled
        );
        assert_eq!(
            protocol_task_snapshot(
                &test_task("t4", TaskStatus::Running, Provider::Glm)
                    .inner
                    .lock()
            )
            .status,
            bro_protocol::TaskStatus::Running
        );
    }

    #[test]
    fn populate_transcript_handle_sets_event_log_location_for_harness_task() {
        let mut env = crate::util::TestEnvGuard::new();
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let bro_home = root.join("bro-home");
        let sessions = bro_home.join("harness-sessions");
        std::fs::create_dir_all(&sessions).unwrap();
        env.set("BRO_HOME", &bro_home);

        let log_path = sessions.join("sess-t-loc.events.jsonl");
        std::fs::write(
            &log_path,
            concat!(
                r#"{"ts":"2026-06-10T01:00:00.000Z","event":{"type":"harness_milestone","milestone":"session_start","session_id":"sess-t-loc","transport":"openai-responses","model":"gpt-5.5","cwd":"/repo/x","provider":"brodex"}}"#,
                "\n",
            ),
        )
        .unwrap();

        // test_task sets session_id = "sess-<id>".
        let task = test_task("t-loc", TaskStatus::Completed, Provider::Brodex);
        assert!(task.inner.lock().transcript_location.is_none());

        populate_transcript_handle(&task);

        let inner = task.inner.lock();
        let location = inner
            .transcript_location
            .as_ref()
            .expect("harness task resolves its event-log transcript location");
        assert_eq!(
            location.source,
            crate::transcripts::types::TranscriptSource::Harness(Provider::Brodex)
        );
        assert_eq!(location.path, log_path);
        assert_eq!(location.session_id.as_deref(), Some("sess-t-loc"));
        assert_eq!(location.cwd.as_deref(), Some("/repo/x"));
    }

    #[test]
    fn test_task_status_is_terminal() {
        assert!(!TaskStatus::Running.is_terminal());
        assert!(TaskStatus::Completed.is_terminal());
        assert!(TaskStatus::Failed.is_terminal());
        assert!(TaskStatus::Cancelled.is_terminal());
    }

    #[test]
    fn spawn_in_process_task_sets_managed_worktree_metadata_from_cwd() {
        let mut env = crate::util::TestEnvGuard::new();
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let bro_home = root.join("bro-home");
        env.set("BRO_HOME", &bro_home);

        let managed = bro_home.join("fleet").join("worktrees").join("task-wt");
        let managed_cwd = managed.join("src");
        let unmanaged_cwd = root.join("outside").join("repo");
        std::fs::create_dir_all(&managed_cwd).unwrap();
        std::fs::create_dir_all(&unmanaged_cwd).unwrap();

        let store = Arc::new(RwLock::new(TaskStore::new()));
        let (tail_tx, _) = tokio::sync::broadcast::channel(8);
        let managed_task = spawn_in_process_task(
            "task-managed".to_string(),
            Provider::Glm,
            "session-managed".to_string(),
            Some(managed_cwd.to_string_lossy().into_owned()),
            root.join("store"),
            store.clone(),
            tail_tx.clone(),
            None,
            None,
            None,
            None,
            bro_core::Origin::Cockpit,
        );
        let managed_string = managed.to_string_lossy().into_owned();
        assert_eq!(
            managed_task.inner.lock().managed_worktree.as_deref(),
            Some(managed_string.as_str())
        );

        let unmanaged_task = spawn_in_process_task(
            "task-unmanaged".to_string(),
            Provider::Glm,
            "session-unmanaged".to_string(),
            Some(unmanaged_cwd.to_string_lossy().into_owned()),
            root.join("store"),
            store,
            tail_tx,
            None,
            None,
            None,
            None,
            bro_core::Origin::Cockpit,
        );
        assert!(unmanaged_task.inner.lock().managed_worktree.is_none());
    }

    #[test]
    fn workflow_owned_metadata_reflects_origin() {
        assert!(workflow_owned_for_origin(bro_core::Origin::Workflow));
        assert!(workflow_owned_for_origin(bro_core::Origin::Atom));
        assert!(!workflow_owned_for_origin(bro_core::Origin::Cockpit));
        assert!(!workflow_owned_for_origin(bro_core::Origin::AgentDispatch));
        assert!(!workflow_owned_for_origin(bro_core::Origin::Unknown));

        let store = Arc::new(RwLock::new(TaskStore::new()));
        let (tail_tx, _) = tokio::sync::broadcast::channel(8);
        let task = spawn_in_process_task(
            "task-workflow-owned".to_string(),
            Provider::Workflow,
            "session-workflow-owned".to_string(),
            None,
            tempfile::tempdir().unwrap().path().to_path_buf(),
            store,
            tail_tx,
            None,
            None,
            None,
            None,
            bro_core::Origin::Workflow,
        );
        assert!(task.inner.lock().workflow_owned);
    }

    #[test]
    fn task_metadata_survives_persist_and_load() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let mut store = TaskStore::new();
        let task = test_task("task-meta", TaskStatus::Completed, Provider::Glm);
        {
            let mut inner = task.inner.lock();
            inner.managed_worktree = Some("/tmp/managed/task-meta".to_string());
            inner.origin = bro_core::Origin::Atom;
            inner.workflow_owned = workflow_owned_for_origin(inner.origin);
        }
        store.insert("task-meta".to_string(), task).unwrap();
        store.persist(&root);

        let loaded = TaskStore::load(&root, u64::MAX);
        let task = loaded.get("task-meta").expect("task should load");
        let inner = task.inner.lock();
        assert_eq!(
            inner.managed_worktree.as_deref(),
            Some("/tmp/managed/task-meta")
        );
        assert_eq!(inner.origin, bro_core::Origin::Atom);
        assert!(inner.workflow_owned);
    }

    #[test]
    fn task_model_cache_drives_roster_and_survives_persist() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let mut store = TaskStore::new();
        let task = test_task("task-model", TaskStatus::Completed, Provider::Glm);
        let tail_tx = test_tail_tx();

        push_in_process_event(
            &task,
            serde_json::json!({"message": {"model": "cached-model"}}),
            &tail_tx,
        );
        assert_eq!(task.inner.lock().model.as_deref(), Some("cached-model"));

        {
            let mut inner = task.inner.lock();
            inner.events = (0..5_000)
                .map(|idx| serde_json::json!({"idx": idx, "model": "scanned-model"}))
                .collect();
        }
        assert_eq!(
            roster_summary_from_task(&task).model.as_deref(),
            Some("cached-model")
        );

        store.insert("task-model".to_string(), task).unwrap();
        store.persist(&root);

        let loaded = TaskStore::load(&root, u64::MAX);
        let task = loaded.get("task-model").expect("task should load");
        assert_eq!(task.inner.lock().model.as_deref(), Some("cached-model"));
        assert_eq!(
            roster_summary_from_task(&task).model.as_deref(),
            Some("cached-model")
        );
    }

    #[test]
    fn event_ring_eviction_preserves_event_count_and_recent_events() {
        let task = test_task("task-ring", TaskStatus::Running, Provider::Glm);
        let tail_tx = test_tail_tx();
        let total = TASK_EVENT_RING_CAPACITY + 3;
        for idx in 0..total {
            push_in_process_event(
                &task,
                serde_json::json!({"type": "provider_event", "idx": idx}),
                &tail_tx,
            );
        }

        {
            let inner = task.inner.lock();
            assert_eq!(inner.observed_event_count(), total);
            assert_eq!(inner.events.len(), total);
            assert_eq!(inner.events.retained_len(), TASK_EVENT_RING_CAPACITY);
            assert_eq!(inner.events[0]["idx"], 3);
        }

        let status = task_status_json(&task, 4);
        assert_eq!(status["eventCount"], total);
        let recent = status["recentEvents"].as_array().unwrap();
        let idxs: Vec<u64> = recent
            .iter()
            .map(|event| event["idx"].as_u64().unwrap())
            .collect();
        assert_eq!(
            idxs,
            vec![
                (total - 4) as u64,
                (total - 3) as u64,
                (total - 2) as u64,
                (total - 1) as u64
            ]
        );
    }

    #[test]
    fn persisted_snapshot_preserves_under_limit_event_shape() {
        let mut store = TaskStore::new();
        let task = test_task("task-persist-events", TaskStatus::Completed, Provider::Glm);
        let tail_tx = test_tail_tx();
        let expected_events: Vec<Value> = (0..3)
            .map(|idx| serde_json::json!({"type": "provider_event", "idx": idx}))
            .collect();
        for event in expected_events.clone() {
            push_in_process_event(&task, event, &tail_tx);
        }
        store
            .insert("task-persist-events".to_string(), task)
            .unwrap();

        let snapshot = store.serialize_snapshot(MAX_PERSISTED_EVENTS).unwrap();
        let records: Value = serde_json::from_str(&snapshot).unwrap();
        let record = &records.as_array().unwrap()[0];
        assert_eq!(record["events"], Value::Array(expected_events));
        assert!(record.get("event_count").is_none());
        assert!(record.get("eventCount").is_none());
    }

    #[test]
    fn harness_tee_overflow_drops_without_blocking_and_warns_once() {
        let (tx, _rx) = std::sync::mpsc::sync_channel(0);
        let mut tee = HarnessTee {
            id: "task-tee".into(),
            suffix: "stdout.jsonl".into(),
            tx,
            warned_drop: false,
        };

        tee.try_write_line("first");
        assert!(tee.warned_drop);
        tee.try_write_line("second");
        assert!(tee.warned_drop);
    }

    #[test]
    fn task_name_defaults_from_prompt_teaser_and_persists() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let task_store = RwLock::new(TaskStore::new());
        let prompt =
            "Inspect the daemon roster regression and restore the model report name columns";
        let expected = default_task_name_from_prompt(prompt).unwrap();
        assert_eq!(expected.chars().count(), DEFAULT_TASK_NAME_CHARS);

        let task = test_task("task-name", TaskStatus::Completed, Provider::Brodex);
        task_store
            .write()
            .insert("task-name".to_string(), task.clone())
            .unwrap();
        seed_task_roster_fields(
            &task,
            default_task_name_from_prompt(prompt),
            None,
            &task_store,
            &root,
        );
        assert_eq!(task.inner.lock().name.as_deref(), Some(expected.as_str()));
        assert_eq!(
            roster_summary_from_task(&task).name.as_deref(),
            Some(expected.as_str())
        );

        task_store.read().persist(&root);
        let loaded = TaskStore::load(&root, u64::MAX);
        let loaded_task = loaded.get("task-name").expect("task should load");
        assert_eq!(
            loaded_task.inner.lock().name.as_deref(),
            Some(expected.as_str())
        );
        assert_eq!(
            roster_summary_from_task(&loaded_task).name.as_deref(),
            Some(expected.as_str())
        );
    }

    #[test]
    fn dispatch_model_cache_wins_over_event_scrape() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let task_store = RwLock::new(TaskStore::new());
        let task = test_task(
            "task-dispatch-model",
            TaskStatus::Completed,
            Provider::Brodex,
        );
        task_store
            .write()
            .insert("task-dispatch-model".to_string(), task.clone())
            .unwrap();
        seed_task_roster_fields(
            &task,
            None,
            Some("dispatch-model".to_string()),
            &task_store,
            &root,
        );
        let tail_tx = test_tail_tx();

        push_in_process_event(
            &task,
            serde_json::json!({"message": {"model": "event-model"}}),
            &tail_tx,
        );
        assert_eq!(task.inner.lock().model.as_deref(), Some("dispatch-model"));
        assert_eq!(
            roster_summary_from_task(&task).model.as_deref(),
            Some("dispatch-model")
        );
    }

    #[test]
    fn roster_summary_projects_report_teaser() {
        let task = test_task("task-report", TaskStatus::Running, Provider::Brodex);
        {
            let mut inner = task.inner.lock();
            inner.report = Some(BroReport {
                message: "Working through the daemon roster regression and checking the report teaser projection stays bounded".to_string(),
                needs: None,
                data: None,
                reported_at: now_ms(),
            });
        }

        let report = roster_summary_from_task(&task)
            .report
            .expect("report teaser should be projected");
        assert_eq!(report.chars().count(), ROSTER_REPORT_TEASER_CHARS);
        assert!(report.starts_with("Working through the daemon roster regression"));
    }

    fn task_with(status: TaskStatus, stderr: &str, events: Vec<Value>) -> Task {
        Task {
            inner: Mutex::new(TaskInner {
                id: "t".into(),
                provider: Provider::Glm,
                session_id: "s".into(),
                events: EventRing::from_loaded(events),
                model: None,
                last_assistant_message: None,
                usage: None,
                cost_usd: None,
                num_turns: None,
                last_turn_input_tokens: None,
                context_window: None,
                stderr: stderr.into(),
                status,
                started_at: now_ms(),
                completed_at: Some(now_ms()),
                exit_code: Some(1),
                cwd: None,
                managed_worktree: None,
                bro_label: None,
                name: None,
                agent_label: None,
                report: None,
                interrupted: false,
                recoverable: false,
                transcript_location: None,
                transcript_cursor: None,
                live_cursor: 0,
                harness_ingest_seq: 0,
                last_delta_roster_emit_ms: 0,
                supervision: SupervisionState::default(),
                origin: bro_core::Origin::Unknown,
                workflow_owned: workflow_owned_for_origin(bro_core::Origin::Unknown),
                project_id: None,
            }),
            notify: Arc::new(Notify::new()),
            child_id: Mutex::new(None),
            roster_events: None,
        }
    }

    fn persisted_event_count_after<F>(events: Vec<Value>, persist: F) -> usize
    where
        F: FnOnce(&TaskStore, &std::path::Path),
    {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let mut store = TaskStore::new();
        store
            .insert(
                "t".into(),
                Arc::new(task_with(TaskStatus::Failed, "", events)),
            )
            .unwrap();

        persist(&store, &root);

        TaskStore::load(&root, u64::MAX)
            .get("t")
            .unwrap()
            .inner
            .lock()
            .events
            .len()
    }

    #[test]
    fn task_store_default_persist_caps_events_but_full_persist_keeps_history() {
        let events: Vec<Value> = (0..60)
            .map(|idx| serde_json::json!({"type": "assistant", "idx": idx}))
            .collect();

        assert_eq!(
            persisted_event_count_after(events.clone(), |store, root| store.persist(root)),
            MAX_PERSISTED_EVENTS
        );
        assert_eq!(
            persisted_event_count_after(events, |store, root| store.persist_all_events(root)),
            60
        );
    }

    fn persisted_task_fixture(id: &str) -> Value {
        let mut store = TaskStore::new();
        store
            .insert(
                id.into(),
                test_task(id, TaskStatus::Completed, Provider::Brodex),
            )
            .unwrap();
        let rows: Vec<Value> =
            serde_json::from_str(&store.serialize_snapshot(50).unwrap()).unwrap();
        rows.into_iter().next().unwrap()
    }

    #[test]
    fn task_store_quarantines_unknown_rows_without_losing_readable_tasks_or_bytes() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let current = persisted_task_fixture("current");
        let mut unknown_provider = persisted_task_fixture("future-provider");
        unknown_provider["provider"] = json!("future-provider-kind");
        let mut unknown_origin = persisted_task_fixture("future-origin");
        unknown_origin["origin"] = json!("future-origin-kind");
        let mut invalid_events = persisted_task_fixture("broken-events");
        invalid_events["events"] = json!("not-an-array");
        let opaque = vec![unknown_provider, unknown_origin, invalid_events];
        let mut rows = vec![current];
        rows.extend(opaque.clone());
        let original = serde_json::to_vec_pretty(&rows).unwrap();
        std::fs::write(root.join("tasks.json"), &original).unwrap();

        let mut loaded = TaskStore::load(&root, u64::MAX);
        assert_eq!(loaded.all_tasks().len(), 1);
        assert_eq!(loaded.quarantined_rows, opaque);
        assert!(!loaded.persistence_blocked);
        let backup = quarantine_task_snapshot(&root, &original).unwrap();
        assert_eq!(std::fs::read(&backup).unwrap(), original);
        for id in ["future-provider", "future-origin", "broken-events"] {
            assert!(loaded.get(id).is_none());
            assert!(loaded.reserve_id(id).is_err());
            assert!(
                loaded
                    .insert_reserved(
                        id.into(),
                        test_task(id, TaskStatus::Running, Provider::Brodex)
                    )
                    .is_err()
            );
        }
        loaded.get("current").unwrap().inner.lock().name = Some("updated".into());
        loaded.persist(&root);
        let reloaded = TaskStore::load(&root, u64::MAX);
        assert_eq!(
            reloaded
                .get("current")
                .unwrap()
                .inner
                .lock()
                .name
                .as_deref(),
            Some("updated")
        );
        assert_eq!(reloaded.quarantined_rows, opaque);
        assert_eq!(std::fs::read(backup).unwrap(), original);
    }

    #[test]
    fn task_store_quarantines_every_duplicate_identity_instead_of_choosing_a_target() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let first = persisted_task_fixture("ambiguous");
        let mut second = first.clone();
        second["session_id"] = json!("another-session");
        let rows = json!([first, second, persisted_task_fixture("unambiguous")]);
        std::fs::write(root.join("tasks.json"), serde_json::to_vec(&rows).unwrap()).unwrap();
        let mut loaded = TaskStore::load(&root, u64::MAX);
        assert!(loaded.get("ambiguous").is_none());
        assert!(loaded.reserve_id("ambiguous").is_err());
        assert!(loaded.get("unambiguous").is_some());
        assert_eq!(loaded.quarantined_rows.len(), 2);
        loaded.persist(&root);
        let reloaded = TaskStore::load(&root, u64::MAX);
        assert!(reloaded.get("ambiguous").is_none());
        assert_eq!(reloaded.quarantined_rows.len(), 2);
    }

    #[test]
    fn task_store_unreadable_snapshot_blocks_every_snapshot_path_without_overwriting() {
        for original in [b"[{\"id\":\"partial".as_slice(), b"{}", &[0xff, 0xfe]] {
            let tmp = tempfile::tempdir().unwrap();
            let root = tmp.path().canonicalize().unwrap();
            std::fs::write(root.join("tasks.json"), original).unwrap();
            let mut loaded = TaskStore::load(&root, u64::MAX);
            assert!(loaded.persistence_blocked);
            assert_eq!(
                loaded.reserve_id("new-task"),
                Err(BroSpawnError::TaskStoreUnavailable)
            );
            assert!(matches!(
                loaded.insert(
                    "new-task".into(),
                    test_task("new-task", TaskStatus::Completed, Provider::Brodex)
                ),
                Err(BroSpawnError::TaskStoreUnavailable)
            ));
            assert!(matches!(
                loaded.insert_reserved(
                    "new-task".into(),
                    test_task("new-task", TaskStatus::Completed, Provider::Brodex)
                ),
                Err(BroSpawnError::TaskStoreUnavailable)
            ));
            assert!(loaded.serialize_snapshot(50).is_none());
            loaded.persist(&root);
            loaded.persist_all_events(&root);
            assert_eq!(std::fs::read(root.join("tasks.json")).unwrap(), original);
        }
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        std::fs::create_dir(root.join("tasks.json")).unwrap();
        assert!(TaskStore::load(&root, u64::MAX).persistence_blocked);
        assert!(!TaskStore::load(&root.join("missing-store"), u64::MAX).persistence_blocked);
    }

    #[tokio::test]
    async fn task_store_unrecoverable_snapshot_refuses_dispatch_before_executor_admission() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let original = b"incomplete snapshot";
        std::fs::write(root.join("tasks.json"), original).unwrap();
        let task_store = Arc::new(RwLock::new(TaskStore::load(&root, u64::MAX)));
        let (tail_tx, _) = tokio::sync::broadcast::channel(8);
        let result = spawn_with_pre_minted_id(
            "refused-task".into(),
            SpawnTaskParams {
                provider: Provider::Brodex,
                args: vec![],
                session_id: "refused-session".into(),
                cwd: None,
                env_overrides: None,
                store_dir: root.clone(),
                task_store: task_store.clone(),
                tail_tx,
                roster_events: None,
                bro_label: None,
                agent_label: None,
                system_events: None,
                origin: bro_core::Origin::Cockpit,
            },
        )
        .await;
        assert!(matches!(result, Err(BroSpawnError::TaskStoreUnavailable)));
        assert!(!task_store.read().contains("refused-task"));
        assert!(task_store.read().all_tasks().is_empty());
        assert_eq!(std::fs::read(root.join("tasks.json")).unwrap(), original);
        assert_eq!(std::fs::read_dir(&root).unwrap().count(), 1);
    }

    #[test]
    fn task_store_quarantine_failure_retains_valid_rows_but_blocks_overwrite() {
        use sha2::{Digest, Sha256};
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let original =
            serde_json::to_vec(&json!([persisted_task_fixture("valid"), {"id": "invalid"}]))
                .unwrap();
        std::fs::write(root.join("tasks.json"), &original).unwrap();
        let backup = root.join(format!(
            "tasks.quarantine.{:x}.json",
            Sha256::digest(&original)
        ));
        // An existing, incorrect backup must never be trusted or overwritten.
        std::fs::write(&backup, b"unrelated bytes").unwrap();
        let loaded = TaskStore::load(&root, u64::MAX);
        assert!(loaded.get("valid").is_some());
        assert!(loaded.persistence_blocked);
        loaded.persist(&root);
        assert_eq!(std::fs::read(root.join("tasks.json")).unwrap(), original);
        assert_eq!(std::fs::read(backup).unwrap(), b"unrelated bytes");
    }

    #[test]
    fn task_store_mixed_legacy_tasks_remain_inspectable_and_owner_guarded() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let mut rows = Vec::new();
        for (id, provider, origin, owned) in [
            ("current", "brodex", "cockpit", false),
            ("legacy-workflow-provider", "workflow", "unknown", false),
            ("legacy-workflow-origin", "glm", "workflow", false),
            ("legacy-atom", "glm", "atom", false),
            ("legacy-owned", "brodex", "unknown", true),
        ] {
            let mut row = persisted_task_fixture(id);
            row["provider"] = json!(provider);
            row["origin"] = json!(origin);
            row["workflow_owned"] = json!(owned);
            row["status"] = json!("running");
            row["recoverable"] = json!(true);
            row["managed_worktree"] = json!("/synthetic/retained-worktree");
            rows.push(row);
        }
        let mut missing_owner_flag = persisted_task_fixture("legacy-missing-owner");
        missing_owner_flag["origin"] = json!("atom");
        missing_owner_flag["recoverable"] = json!(true);
        missing_owner_flag["status"] = json!("failed");
        missing_owner_flag
            .as_object_mut()
            .unwrap()
            .remove("workflow_owned");
        rows.push(missing_owner_flag);
        std::fs::write(root.join("tasks.json"), serde_json::to_vec(&rows).unwrap()).unwrap();
        let loaded = TaskStore::load(&root, u64::MAX);
        assert_eq!(loaded.all_tasks().len(), rows.len());
        assert!(loaded.quarantined_rows.is_empty());
        for row in &rows {
            let id = row["id"].as_str().unwrap();
            let task = loaded.get(id).unwrap();
            let inner = task.inner.lock();
            assert_eq!(inner.status, TaskStatus::Failed);
            if id == "current" {
                assert!(!inner.workflow_owned);
                assert!(inner.recoverable);
                assert!(inner.stderr.contains("bro_resume"));
            } else {
                assert!(inner.workflow_owned, "ownership lost for {id}");
                assert!(
                    !inner.recoverable,
                    "owner-managed task became resumable: {id}"
                );
                assert!(!inner.stderr.contains("bro_resume"));
                assert_eq!(serde_json::to_value(inner.origin).unwrap(), row["origin"]);
                assert_eq!(
                    serde_json::to_value(inner.provider).unwrap(),
                    row["provider"]
                );
            }
            assert_eq!(
                inner.managed_worktree.as_deref(),
                row["managed_worktree"].as_str()
            );
        }
        loaded.persist(&root);
        let again = TaskStore::load(&root, u64::MAX);
        assert!(
            again
                .get("legacy-workflow-provider")
                .unwrap()
                .inner
                .lock()
                .workflow_owned
        );
        assert!(
            !again
                .get("legacy-missing-owner")
                .unwrap()
                .inner
                .lock()
                .recoverable
        );
    }

    // Slice 1b — the spawn-time `origin` (bro_core::Origin) must
    // SURVIVE the on-disk round-trip so a daemon restart doesn't
    // reset every task's origin to `Unknown` and lose the
    // Fleet-vs-Dispatched distinction. This is the restart-survival
    // guard called out in the Slice 1b spec.
    //
    // Two-part check: (a) PersistedTask serializes origin (as the
    // lowercase variant name, matching the wire DTO), and (b)
    // `TaskStore::load` decodes the field back to the right enum
    // variant for each task. A pre-Slice-1b record missing the
    // `origin` field entirely must decode to `Unknown` for
    // back-compat.
    #[test]
    fn origin_round_trips_through_persist_and_load() {
        let tmp = tempfile::tempdir().unwrap();
        let store_dir = tmp.path().to_path_buf();

        // Build a store with one task per representative origin so
        // a cross-task regression (e.g. always-defaulting-to-Unknown)
        // would be visible from a single test.
        let mut store = TaskStore::new();
        let variants = [
            ("origin-cockpit", bro_core::Origin::Cockpit),
            ("origin-workflow", bro_core::Origin::Workflow),
            ("origin-atom", bro_core::Origin::Atom),
            ("origin-agent", bro_core::Origin::AgentDispatch),
            ("origin-unknown", bro_core::Origin::Unknown),
            ("origin-cron", bro_core::Origin::Cron),
            ("origin-webhook", bro_core::Origin::Webhook),
        ];
        for (id, origin) in variants {
            let task = test_task(id, TaskStatus::Completed, Provider::Glm);
            task.inner.lock().origin = origin;
            store
                .insert(id.to_string(), task)
                .expect("insert origin-tagged task");
        }
        // Persist via the public entry that production uses
        // (serialize_snapshot + write_snapshot_blocking).
        store.persist(&store_dir);

        // (a) Wire-shape guard: the on-disk JSON carries the
        // lowercase variant name, so a peer's tooling or a future
        // migration can read the field without knowing the Rust
        // enum. The RosterSummaryV1 wire-form check is asserted
        // separately in bro-protocol; this one pins the on-disk
        // schema.
        let raw = std::fs::read_to_string(store_dir.join("tasks.json")).unwrap();
        for needle in [
            "\"origin\":\"cockpit\"",
            "\"origin\":\"workflow\"",
            "\"origin\":\"atom\"",
            "\"origin\":\"agentdispatch\"",
            "\"origin\":\"unknown\"",
            "\"origin\":\"cron\"",
            "\"origin\":\"webhook\"",
        ] {
            assert!(
                raw.contains(needle),
                "tasks.json must carry lowercase origin field for {needle}; raw={raw}"
            );
        }

        // (b) Reload and assert each task decodes back to the right
        // variant — restart-survival guard.
        // Use a generous ttl_ms so the just-completed tasks aren't
        // pruned by the running-task-only filter in `TaskStore::load`.
        let reloaded = TaskStore::load(&store_dir, u64::MAX);
        for (id, expected) in variants {
            let task = reloaded
                .get(id)
                .unwrap_or_else(|| panic!("reloaded store must still carry {id}"));
            assert_eq!(
                task.inner.lock().origin,
                expected,
                "origin for {id} did not survive persist+load"
            );
        }
    }

    // Slice 1b back-compat: pre-Slice-1b on-disk records lack the
    // `origin` field entirely (older daemons). `PersistedTask` has
    // `#[serde(default)]` on the `origin` field, so a missing
    // field must decode to `Origin::Unknown` (the enum's
    // `#[default]` variant) rather than failing the load or
    // losing the record. A regression that hard-codes `Unknown` to
    // something else (e.g. AgentDispatch) would silently
    // re-classify every old task; the assert here is the trip-wire.
    #[test]
    fn origin_missing_on_disk_decodes_to_unknown() {
        let tmp = tempfile::tempdir().unwrap();
        let store_dir = tmp.path().to_path_buf();

        // Hand-craft a pre-Slice-1b record: every field the
        // `#[serde(default)]` mark on `PersistedTask` allows
        // missing, AND no `origin` field.
        let pre_slice_1b = serde_json::json!([{
            "id": "legacy-task",
            "provider": "glm",
            "session_id": "sess-legacy",
            "events": [],
            "last_assistant_message": null,
            "usage": null,
            "cost_usd": null,
            "num_turns": 1,
            "stderr": "",
            "status": "completed",
            "started_at": 1_700_000_000_000u64,
            "completed_at": 1_700_000_000_001u64,
            "exit_code": 0
            // NOTE: no `origin` field — pre-Slice-1b shape.
        }]);
        std::fs::write(
            store_dir.join("tasks.json"),
            serde_json::to_string(&pre_slice_1b).unwrap(),
        )
        .unwrap();

        let reloaded = TaskStore::load(&store_dir, u64::MAX);
        let task = reloaded
            .get("legacy-task")
            .expect("legacy record must load successfully");
        assert_eq!(
            task.inner.lock().origin,
            bro_core::Origin::Unknown,
            "pre-Slice-1b record (no origin field) must decode to Origin::Unknown"
        );
    }

    #[test]
    fn status_surfaces_stderr_tail_on_silent_failure() {
        // The bug this guards: a harness that bails before any stdout left
        // bro_status showing exit 1 / 0 events with no reason.
        let failed = task_with(
            TaskStatus::Failed,
            "harness error: no --model, no resumed session model, …",
            vec![],
        );
        let json = task_status_json(&failed, 0);
        assert_eq!(json["eventCount"], 0);
        assert!(
            json["stderrTail"].as_str().unwrap().contains("no --model"),
            "failed task must surface the stderr reason, got {json}"
        );
    }

    #[test]
    fn status_shape_matches_failed_task_with_events_and_stderr() {
        let failed = task_with(
            TaskStatus::Failed,
            "provider failed before final answer\n",
            vec![
                serde_json::json!({"type": "assistant", "idx": 1, "message": "first"}),
                serde_json::json!({"type": "stream_event", "idx": 2}),
                serde_json::json!({"type": "assistant", "idx": 3, "message": "second"}),
            ],
        );

        let json = task_status_json(&failed, 10);

        assert_eq!(json["status"], "failed");
        assert_eq!(json["eventCount"], 3);
        assert_eq!(json["resultCapture"]["eventCount"], json["eventCount"]);
        assert_eq!(
            json["recentEvents"],
            serde_json::json!([
                {"type": "assistant", "idx": 1, "message": "first"},
                {"type": "assistant", "idx": 3, "message": "second"}
            ])
        );
        assert_eq!(json["stderrTail"], "provider failed before final answer");
        assert!(json["snapshot"]["error"].is_object());
    }

    #[test]
    fn status_emits_empty_recent_events_when_all_events_are_filtered() {
        let running = task_with(
            TaskStatus::Running,
            "",
            vec![
                serde_json::json!({"type": "stream_event", "idx": 1}),
                serde_json::json!({"type": "stream_event", "idx": 2}),
            ],
        );

        let json = task_status_json(&running, 10);

        assert!(json.get("recentEvents").is_some());
        assert_eq!(json["recentEvents"], serde_json::json!([]));
    }

    #[test]
    fn status_pages_large_result_and_keeps_status_fields() {
        let final_summary = "final summary ".repeat(2_000);
        let original = format!("{}{}", "progress narration ".repeat(2_000), final_summary);
        let completed = task_with(
            TaskStatus::Completed,
            "",
            vec![serde_json::json!({"type": "assistant", "idx": 1})],
        );
        completed.inner.lock().last_assistant_message = Some(original.clone());

        let json = task_status_json(&completed, 1);
        let result = json["result"].as_str().expect("result should be present");

        assert!(original.starts_with(result));
        assert!(json["resultCursor"].is_string());
        assert_eq!(json["resultTruncated"], true);
        assert_eq!(json["resultBytes"], original.len());
        assert_eq!(json["status"], "completed");
        assert_eq!(json["eventCount"], 1);
        assert!(json["recentEvents"].is_array());
    }

    #[test]
    fn status_leaves_small_result_shape_unchanged() {
        let completed = task_with(
            TaskStatus::Completed,
            "",
            vec![serde_json::json!({"type": "assistant", "idx": 1})],
        );
        completed.inner.lock().last_assistant_message = Some("short final answer".into());

        let json = task_status_json(&completed, 1);

        assert_eq!(json["result"], "short final answer");
        assert!(json.get("resultTruncated").is_none());
        assert!(json.get("resultBytes").is_none());
    }

    #[test]
    fn timeout_snapshot_truncates_last_activity_on_char_boundary() {
        let running = task_with(
            TaskStatus::Running,
            "",
            vec![serde_json::json!({"type": "assistant", "idx": 1})],
        );
        running.inner.lock().last_assistant_message =
            Some(format!("{}’{}", "x".repeat(79), "tail"));

        let json = timeout_snapshot_json(&running);

        assert_eq!(
            json["lastAssistantSnippet"],
            format!("{}’…", "x".repeat(79)),
            "timeout snapshot teaser must not byte-slice through UTF-8"
        );
        assert_eq!(json["timed_out"], true);
    }

    #[test]
    fn status_combined_worst_case_stays_under_mcp_cap() {
        let running = task_with(
            TaskStatus::Running,
            "",
            (0..80)
                .map(|idx| {
                    serde_json::json!({
                        "type": "assistant",
                        "idx": idx,
                        "message": "event payload ".repeat(400),
                    })
                })
                .collect(),
        );
        running.inner.lock().last_assistant_message = Some("status result ".repeat(2_000));

        let status = task_status_json(&running, 80);
        let bytes = serde_json::to_string(&status).unwrap().len();

        // Mirrors `BlackboxServer::MCP_RESPONSE_CAP_BYTES` without importing the
        // server type into orchestration tests.
        assert!(bytes < 80 * 1024, "status payload was {bytes} bytes");
    }

    #[test]
    fn status_omits_stderr_tail_on_clean_success() {
        let ok = task_with(
            TaskStatus::Completed,
            "",
            vec![serde_json::json!({"type": "system"})],
        );
        let json = task_status_json(&ok, 0);
        assert!(json.get("stderrTail").is_none());
    }

    #[test]
    fn workload_retro_prompt_names_only_valid_gap_kinds() {
        // The retro probe instructs bros to file gaps with a specific
        // gap_kind. Those tokens must stay parseable as `gaps::GapKind` —
        // a mismatch makes every retro `bbox_gap` call fail. (Lives here,
        // not in the gaps store, because this is the side that owns the
        // prompt and depends on the store crate.)
        use std::str::FromStr;

        use crate::gaps::GapKind;

        let prompt = WORKLOAD_RETRO_PROMPT;
        for kind in [
            "mcp_surface",
            "tooling",
            "workflow",
            "agent",
            "docs_runbook",
            "refactor_primitive",
            "ontology",
            "eval_coverage",
        ] {
            assert!(
                prompt.contains(kind),
                "retro prompt no longer names gap_kind {kind:?}"
            );
            assert!(
                GapKind::from_str(kind).is_ok(),
                "retro prompt names gap_kind {kind:?} that GapKind cannot parse"
            );
        }
    }

    #[test]
    fn workload_retro_prompt_emits_scope_block() {
        let with_project = workload_retro_prompt("sess-123", Some("/repo/x"));
        assert!(with_project.starts_with("[scope] session:sess-123 · project:/repo/x\n\n"));
        assert!(with_project.contains("bbox_gap"));
        // The body is the canonical prompt, unmodified.
        assert!(with_project.ends_with(WORKLOAD_RETRO_PROMPT));

        // Project is optional — omit it cleanly rather than leaking an
        // empty segment.
        let no_project = workload_retro_prompt("sess-123", None);
        assert!(no_project.starts_with("[scope] session:sess-123\n\n"));
        assert!(!no_project.contains("project:"));
    }

    #[test]
    fn workload_retro_prompt_keeps_the_no_compulsion_balance() {
        // These phrases are load-bearing: they're what stop the probe from
        // manufacturing friction to satisfy a perceived quota. If a future
        // edit drops them, the balance is gone — fail loudly.
        for phrase in [
            "completely normal way for a run to end",
            "a quiet run is a good run",
            "don't manufacture friction",
            "file nothing",
        ] {
            assert!(
                WORKLOAD_RETRO_PROMPT.contains(phrase),
                "retro prompt lost the anti-compulsion phrase: {phrase:?}"
            );
        }
    }

    #[test]
    fn task_store_rejects_duplicate_task_ids_without_overwrite() {
        let mut store = TaskStore::new();
        let first = Arc::new(Task {
            inner: Mutex::new(TaskInner {
                id: "task-known".to_string(),
                provider: Provider::Brodex,
                session_id: "session-a".to_string(),
                events: EventRing::new(),
                model: None,
                last_assistant_message: None,
                usage: None,
                cost_usd: None,
                num_turns: None,
                last_turn_input_tokens: None,
                context_window: None,
                stderr: String::new(),
                status: TaskStatus::Running,
                started_at: now_ms(),
                completed_at: None,
                exit_code: None,
                cwd: None,
                managed_worktree: None,
                bro_label: None,
                name: None,
                agent_label: None,
                report: None,
                interrupted: false,
                recoverable: false,
                transcript_location: None,
                transcript_cursor: None,
                live_cursor: 0,
                harness_ingest_seq: 0,
                last_delta_roster_emit_ms: 0,
                supervision: SupervisionState::default(),
                origin: bro_core::Origin::Unknown,
                workflow_owned: workflow_owned_for_origin(bro_core::Origin::Unknown),
                project_id: None,
            }),
            notify: Arc::new(Notify::new()),
            child_id: Mutex::new(None),
            roster_events: None,
        });
        let second = Arc::new(Task {
            inner: Mutex::new(TaskInner {
                id: "task-known".to_string(),
                provider: Provider::Brodex,
                session_id: "session-b".to_string(),
                events: EventRing::new(),
                model: None,
                last_assistant_message: None,
                usage: None,
                cost_usd: None,
                num_turns: None,
                last_turn_input_tokens: None,
                context_window: None,
                stderr: String::new(),
                status: TaskStatus::Running,
                started_at: now_ms(),
                completed_at: None,
                exit_code: None,
                cwd: None,
                managed_worktree: None,
                bro_label: None,
                name: None,
                agent_label: None,
                report: None,
                interrupted: false,
                recoverable: false,
                transcript_location: None,
                transcript_cursor: None,
                live_cursor: 0,
                harness_ingest_seq: 0,
                last_delta_roster_emit_ms: 0,
                supervision: SupervisionState::default(),
                origin: bro_core::Origin::Unknown,
                workflow_owned: workflow_owned_for_origin(bro_core::Origin::Unknown),
                project_id: None,
            }),
            notify: Arc::new(Notify::new()),
            child_id: Mutex::new(None),
            roster_events: None,
        });

        store.insert("task-known".to_string(), first).unwrap();
        assert!(matches!(
            store.insert("task-known".to_string(), second),
            Err(BroSpawnError::DuplicateTaskId { .. })
        ));
        assert_eq!(
            store.get("task-known").unwrap().inner.lock().session_id,
            "session-a"
        );
    }

    #[test]
    fn ingest_is_error_result_marks_child_task_failed_and_captures_message() {
        // A child can exit zero after a terminal `result {is_error:true}` event.
        // Ingesting it must fail the task and preserve the message
        // (gap-32113fd4), independently of the process exit code.
        let task = Arc::new(Task {
            inner: Mutex::new(TaskInner {
                id: "task-err".to_string(),
                provider: Provider::Minimax,
                session_id: "sess-err".to_string(),
                events: EventRing::new(),
                model: None,
                last_assistant_message: None,
                usage: None,
                cost_usd: None,
                num_turns: None,
                last_turn_input_tokens: None,
                context_window: None,
                stderr: String::new(),
                status: TaskStatus::Running,
                started_at: now_ms(),
                completed_at: None,
                exit_code: None,
                cwd: None,
                managed_worktree: None,
                bro_label: None,
                name: None,
                agent_label: None,
                report: None,
                interrupted: false,
                recoverable: false,
                transcript_location: None,
                transcript_cursor: None,
                live_cursor: 0,
                harness_ingest_seq: 0,
                last_delta_roster_emit_ms: 0,
                supervision: SupervisionState::default(),
                origin: bro_core::Origin::Unknown,
                workflow_owned: workflow_owned_for_origin(bro_core::Origin::Unknown),
                project_id: None,
            }),
            notify: Arc::new(Notify::new()),
            child_id: Mutex::new(None),
            roster_events: None,
        });
        let (tx, _rx) = tokio::sync::broadcast::channel(16);
        let evt = serde_json::json!({
            "type": "result",
            "subtype": "error",
            "is_error": true,
            "session_id": "sess-err",
            "result": "anthropic messages 400 Bad Request: boom",
            "num_turns": 2,
        });
        ingest_harness_event(&task, Provider::Minimax, evt, &tx, "task-err", None);
        let inner = task.inner.lock();
        assert!(matches!(inner.status, TaskStatus::Failed));
        assert!(inner.stderr.contains("400 Bad Request: boom"));
    }

    fn mk_ingest_task(id: &str, session_id: &str) -> Arc<Task> {
        Arc::new(Task {
            inner: Mutex::new(TaskInner {
                id: id.to_string(),
                provider: Provider::Minimax,
                session_id: session_id.to_string(),
                events: EventRing::new(),
                model: None,
                last_assistant_message: None,
                usage: None,
                cost_usd: None,
                num_turns: None,
                last_turn_input_tokens: None,
                context_window: None,
                stderr: String::new(),
                status: TaskStatus::Running,
                started_at: now_ms(),
                completed_at: None,
                exit_code: None,
                cwd: None,
                managed_worktree: None,
                bro_label: None,
                name: None,
                agent_label: None,
                report: None,
                interrupted: false,
                recoverable: false,
                transcript_location: None,
                transcript_cursor: None,
                live_cursor: 0,
                harness_ingest_seq: 0,
                last_delta_roster_emit_ms: 0,
                supervision: SupervisionState::default(),
                origin: bro_core::Origin::Unknown,
                workflow_owned: workflow_owned_for_origin(bro_core::Origin::Unknown),
                project_id: None,
            }),
            notify: Arc::new(Notify::new()),
            child_id: Mutex::new(None),
            roster_events: None,
        })
    }

    #[test]
    fn interrupted_result_finalizes_in_process_task_as_cancelled() {
        let task = mk_ingest_task("task-int", "sess-int");
        let (tx, mut rx) = tokio::sync::broadcast::channel(16);
        let evt = serde_json::json!({
            "type": "result",
            "subtype": "interrupted",
            "interrupted": true,
            "session_id": "sess-int",
            "result": "partial text before escape",
            "num_turns": 0,
            "usage": {
                "input_tokens": 11,
                "output_tokens": 0,
                "cache_read_input_tokens": 0,
                "cache_creation_input_tokens": 0
            }
        });

        ingest_harness_event(&task, Provider::Minimax, evt, &tx, "task-int", None);
        {
            let inner = task.inner.lock();
            assert_eq!(inner.status, TaskStatus::Running);
            assert!(inner.interrupted);
            assert_eq!(
                inner.last_assistant_message.as_deref(),
                Some("partial text before escape")
            );
            assert_eq!(inner.num_turns, Some(0));
        }

        let store = RwLock::new(TaskStore::new());
        let tmp = tempfile::tempdir().unwrap();
        finish_in_process_task(
            &task,
            TaskStatus::Completed,
            None,
            None,
            &store,
            tmp.path(),
            &tx,
            None,
        );

        let status = task_status_json(&task, 5);
        assert_eq!(status["status"], "cancelled");
        assert_eq!(status["interrupted"], true);
        assert_eq!(status["result"], "partial text before escape");
        assert!(status.get("numTurns").is_none());
        assert_eq!(task_result_json(&task)["numTurns"], 0);
        assert_eq!(status["snapshot"]["status"], "cancelled");
        assert_eq!(status["snapshot"]["interrupted"], true);

        let mut saw_cancelled = false;
        while let Ok(event) = rx.try_recv() {
            if matches!(event, tail::TailEvent::TaskCancelled { task_id, .. } if task_id == "task-int")
            {
                saw_cancelled = true;
                break;
            }
        }
        assert!(saw_cancelled);
    }

    #[test]
    fn ingest_stream_deltas_accumulate_without_ring_storage() {
        // Wave 15: text deltas mutate the accumulated message via the taken
        // (not cloned) buffer, and stream_event envelopes are NOT stored in
        // the event ring — only step-boundary events are.
        let task = mk_ingest_task("task-deltas", "sess-d");
        let (tx, _rx) = tokio::sync::broadcast::channel(16);
        let delta = |text: &str| {
            serde_json::json!({
                "type": "stream_event",
                "session_id": "sess-d",
                "event": {"type": "content_block_delta", "delta": {"type": "text_delta", "text": text}},
            })
        };
        ingest_harness_event(
            &task,
            Provider::Minimax,
            delta("hel"),
            &tx,
            "task-deltas",
            None,
        );
        ingest_harness_event(
            &task,
            Provider::Minimax,
            delta("lo"),
            &tx,
            "task-deltas",
            None,
        );
        let assistant = serde_json::json!({
            "type": "assistant",
            "session_id": "sess-d",
            "message": {"role": "assistant", "content": [{"type": "text", "text": "hello"}]},
        });
        ingest_harness_event(
            &task,
            Provider::Minimax,
            assistant,
            &tx,
            "task-deltas",
            None,
        );

        let inner = task.inner.lock();
        assert_eq!(inner.last_assistant_message.as_deref(), Some("hello"));
        let stored: Vec<&str> = inner
            .events
            .iter()
            .filter_map(|e| e["type"].as_str())
            .collect();
        assert_eq!(stored, vec!["assistant"], "only step events stored");
    }

    #[test]
    #[allow(clippy::disallowed_methods)]
    fn first_child_event_resolves_pending_transcript_location() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let task = mk_ingest_task("task-pending-location", "pending");
        task.inner.lock().transcript_location =
            harness_transcript_location(Provider::Minimax, &root, "pending", Some("/repo"));
        let (tx, _rx) = tokio::sync::broadcast::channel(16);

        ingest_harness_event(
            &task,
            Provider::Minimax,
            serde_json::json!({
                "type": "system",
                "subtype": "init",
                "session_id": "resolved-session",
            }),
            &tx,
            "task-pending-location",
            None,
        );

        let inner = task.inner.lock();
        let location = inner.transcript_location.as_ref().unwrap();
        assert_eq!(inner.session_id, "resolved-session");
        assert_eq!(
            location.path,
            root.join("harness-sessions")
                .join("resolved-session.events.jsonl")
        );
        assert_eq!(location.session_id.as_deref(), Some("resolved-session"));
    }

    #[test]
    fn ingest_forked_session_event_leaves_message_and_ring_untouched() {
        // The fork-acceptance decision now happens BEFORE parse, so a
        // rejected forked event must neither mutate the accumulated message
        // (which is taken, not cloned, on the accept path) nor be stored.
        let task = mk_ingest_task("task-fork", "sess-real");
        task.inner.lock().last_assistant_message = Some("real text".to_string());
        let (tx, _rx) = tokio::sync::broadcast::channel(16);
        let forked = serde_json::json!({
            "type": "stream_event",
            "session_id": "sess-FORK",
            "event": {"type": "content_block_delta", "delta": {"type": "text_delta", "text": "evil"}},
        });
        ingest_harness_event(&task, Provider::Minimax, forked, &tx, "task-fork", None);

        let inner = task.inner.lock();
        assert_eq!(inner.last_assistant_message.as_deref(), Some("real text"));
        assert!(inner.events.iter().count() == 0, "forked event not stored");
        assert!(matches!(inner.status, TaskStatus::Failed));
        assert!(inner.stderr.contains("session fork detected"));
    }

    #[test]
    fn snippet_tail_is_bounded_and_char_safe() {
        assert_eq!(snippet_tail("short", 160), "short");
        let long: String = "x".repeat(200);
        let tail = snippet_tail(&long, 160);
        assert!(tail.starts_with('\u{2026}'));
        assert_eq!(tail.chars().count(), 161);
        // Multibyte boundary safety.
        let uni: String = "é".repeat(200);
        let tail = snippet_tail(&uni, 160);
        assert!(tail.starts_with('\u{2026}'));
        assert_eq!(tail.chars().count(), 161);
        // Exactly n chars: no ellipsis.
        let exact: String = "y".repeat(160);
        assert_eq!(snippet_tail(&exact, 160), exact);
    }

    #[test]
    fn task_store_reservation_blocks_duplicate_insert_until_used() {
        let mut store = TaskStore::new();
        store.reserve_id("task-reserved").unwrap();
        let task = Arc::new(Task {
            inner: Mutex::new(TaskInner {
                id: "task-reserved".to_string(),
                provider: Provider::Brodex,
                session_id: "session-a".to_string(),
                events: EventRing::new(),
                model: None,
                last_assistant_message: None,
                usage: None,
                cost_usd: None,
                num_turns: None,
                last_turn_input_tokens: None,
                context_window: None,
                stderr: String::new(),
                status: TaskStatus::Running,
                started_at: now_ms(),
                completed_at: None,
                exit_code: None,
                cwd: None,
                managed_worktree: None,
                bro_label: None,
                name: None,
                agent_label: None,
                report: None,
                interrupted: false,
                recoverable: false,
                transcript_location: None,
                transcript_cursor: None,
                live_cursor: 0,
                harness_ingest_seq: 0,
                last_delta_roster_emit_ms: 0,
                supervision: SupervisionState::default(),
                origin: bro_core::Origin::Unknown,
                workflow_owned: workflow_owned_for_origin(bro_core::Origin::Unknown),
                project_id: None,
            }),
            notify: Arc::new(Notify::new()),
            child_id: Mutex::new(None),
            roster_events: None,
        });
        assert!(matches!(
            store.insert("task-reserved".to_string(), task.clone()),
            Err(BroSpawnError::ReservedTaskId { .. })
        ));
        store
            .insert_reserved("task-reserved".to_string(), task)
            .unwrap();
        assert!(store.get("task-reserved").is_some());
    }

    #[tokio::test]
    async fn spawn_with_pre_minted_id_tracks_known_id() {
        let _env = crate::util::test_env_lock();
        let prior_bin = std::env::var("CODEX_BIN").ok();
        unsafe {
            std::env::set_var("CODEX_BIN", "/bin/true");
        }
        let tmp = tempfile::tempdir().unwrap();
        let task_store = Arc::new(RwLock::new(TaskStore::new()));
        let (tail_tx, _) = tokio::sync::broadcast::channel(8);
        let task = spawn_with_pre_minted_id(
            "task-known-id".to_string(),
            SpawnTaskParams {
                provider: Provider::Brodex,
                args: Vec::new(),
                session_id: "observed-session".to_string(),
                cwd: None,
                env_overrides: None,
                store_dir: tmp.path().to_path_buf(),
                task_store: task_store.clone(),
                tail_tx,
                roster_events: None,
                bro_label: None,
                agent_label: None,
                system_events: None,
                // The legacy `spawn_with_pre_minted_id_tracks_known_id`
                // test predates Slice 1b; pin origin to a sentinel
                // value so a regression that drops the origin on
                // pre-minted paths would be visible from this test
                // too.
                origin: bro_core::Origin::AgentDispatch,
            },
        )
        .await
        .unwrap();

        assert_eq!(task.id(), "task-known-id");
        assert!(wait_for_task_with_timeout(&task, Some(2.0)).await);
        assert_eq!(
            task_store
                .read()
                .get("task-known-id")
                .unwrap()
                .inner
                .lock()
                .session_id,
            "observed-session"
        );

        match prior_bin {
            Some(value) => unsafe { std::env::set_var("CODEX_BIN", value) },
            None => unsafe { std::env::remove_var("CODEX_BIN") },
        }
    }

    #[test]
    fn test_format_elapsed() {
        assert_eq!(format_elapsed(1000, Some(6000)), "5s");
        assert_eq!(format_elapsed(1000, Some(91000)), "1m 30s");
    }

    #[test]
    fn dispatch_context_emits_typed_scope_no_text_guard() {
        let ctx = AmbientContext {
            session_id: Some("sess-abc".into()),
            project_dir: Some("/repo/x".into()),
            bro_name: Some("executor".into()),
            allow_recursion: false,
            provider: Some(providers::Provider::Glm),
            ..Default::default()
        };
        let payload = ctx.dispatch_context(None);
        let scope = payload.scope.as_ref().expect("scope present");
        assert_eq!(
            scope.fields(),
            vec![
                ("session", "sess-abc"),
                ("project", "/repo/x"),
                ("bro", "executor"),
            ]
        );
        // Text recursion guard retired: no directive carries it for any
        // provider — guarding is mechanical via dispatch tool filters.
        assert!(
            payload
                .directives
                .iter()
                .all(|d| !d.text.contains("IMPORTANT:")),
            "text recursion guard leaked into a directive"
        );
        // The payload is ingredients only — never the operator's prompt.
        let raw = serde_json::to_string(&payload).unwrap();
        assert!(!raw.contains("do stuff"));
        // And it round-trips through the harness's strict parser.
        assert_eq!(bro_protocol::DispatchContext::parse(&raw).unwrap(), payload);
    }

    #[test]
    fn worktree_base_repo_maps_linked_worktrees_only() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();

        // Linked worktree: .git is a FILE pointing into <base>/.git/worktrees/<n>.
        let base = root.join("repo");
        let wt = root.join("wt");
        std::fs::create_dir_all(base.join(".git").join("worktrees").join("wt")).unwrap();
        std::fs::create_dir_all(&wt).unwrap();
        std::fs::write(
            wt.join(".git"),
            format!("gitdir: {}\n", base.join(".git/worktrees/wt").display()),
        )
        .unwrap();
        assert_eq!(super::worktree_base_repo(&wt), Some(base.clone()));

        // Primary checkout (.git is a directory) is not a linked worktree.
        assert_eq!(super::worktree_base_repo(&base), None);

        // Malformed gitdir file: fail closed to None.
        let bad = root.join("bad");
        std::fs::create_dir_all(&bad).unwrap();
        std::fs::write(bad.join(".git"), "gitdir: /nowhere/special\n").unwrap();
        assert_eq!(super::worktree_base_repo(&bad), None);
    }

    #[test]
    fn ambient_tool_defaults_track_session_and_task_ids() {
        // Session only: exactly the bbox_note.session_id default.
        let ctx = AmbientContext {
            session_id: Some("sess-abc".into()),
            bro_name: Some("executor".into()),
            ..Default::default()
        };
        let defaults = ctx.tool_arg_defaults().expect("session default");
        assert_eq!(defaults.len(), 1);
        assert_eq!(
            defaults
                .get("default:mcp.bbox_note.session_id")
                .map(String::as_str),
            Some("sess-abc")
        );

        // Task id adds the bro_report coordination-id default.
        let ctx = AmbientContext {
            session_id: Some("sess-abc".into()),
            task_id: Some("task-abc".into()),
            ..Default::default()
        };
        let defaults = ctx.tool_arg_defaults().expect("session + task defaults");
        assert_eq!(defaults.len(), 2);
        assert_eq!(
            defaults
                .get("default:mcp.bro_report.task_id")
                .map(String::as_str),
            Some("task-abc")
        );

        // Pending session, no task, no cwd: nothing to emit.
        let pending = AmbientContext {
            session_id: Some("pending".into()),
            ..Default::default()
        };
        assert!(pending.tool_arg_defaults().is_none());

        // Blank ids are withheld, not emitted as empty defaults.
        let blank = AmbientContext {
            task_id: Some("  ".into()),
            project_dir: Some("".into()),
            ..Default::default()
        };
        assert!(blank.tool_arg_defaults().is_none());
    }

    #[test]
    fn tool_arg_defaults_merge_ambient_only() {
        let ambient =
            BTreeMap::from([("default:mcp.bbox_note.session_id".into(), "sess-1".into())]);
        assert_eq!(
            merge_tool_arg_defaults(Some(ambient.clone()), None, None),
            Some(ambient)
        );
    }

    #[test]
    fn tool_arg_defaults_merge_brofile_overlay() {
        let ambient = BTreeMap::from([
            ("default:mcp.bbox_note.session_id".into(), "sess-1".into()),
            (
                "default:rust.moveStructFields.acknowledge_repr".into(),
                "false".into(),
            ),
        ]);
        let brofile = BTreeMap::from([
            (
                "default:rust.moveStructFields.acknowledge_repr".into(),
                "true".into(),
            ),
            (
                "default:rust.migrateErrorType.acknowledge_public_api_change".into(),
                "true".into(),
            ),
        ]);
        let merged = merge_tool_arg_defaults(Some(ambient), Some(&brofile), None).unwrap();
        assert_eq!(
            merged
                .get("default:rust.moveStructFields.acknowledge_repr")
                .map(String::as_str),
            Some("true")
        );
        assert_eq!(
            merged
                .get("default:mcp.bbox_note.session_id")
                .map(String::as_str),
            Some("sess-1")
        );
        assert_eq!(
            merged
                .get("default:rust.migrateErrorType.acknowledge_public_api_change")
                .map(String::as_str),
            Some("true")
        );
    }

    #[test]
    fn tool_arg_defaults_merge_per_dispatch_overlay() {
        let brofile = BTreeMap::from([(
            "default:rust.moveStructFields.acknowledge_repr".into(),
            "true".into(),
        )]);
        let per_dispatch = BTreeMap::from([(
            "default:rust.migrateTypeUsages.acknowledge_public_api_change".into(),
            "true".into(),
        )]);
        let merged = merge_tool_arg_defaults(None, Some(&brofile), Some(&per_dispatch)).unwrap();
        assert_eq!(
            merged
                .get("default:rust.moveStructFields.acknowledge_repr")
                .map(String::as_str),
            Some("true")
        );
        assert_eq!(
            merged
                .get("default:rust.migrateTypeUsages.acknowledge_public_api_change")
                .map(String::as_str),
            Some("true")
        );
    }

    #[test]
    fn tool_arg_defaults_merge_per_dispatch_wins_conflicts() {
        let ambient = BTreeMap::from([(
            "default:rust.moveStructFields.acknowledge_repr".into(),
            "ambient".into(),
        )]);
        let brofile = BTreeMap::from([(
            "default:rust.moveStructFields.acknowledge_repr".into(),
            "brofile".into(),
        )]);
        let per_dispatch = BTreeMap::from([(
            "default:rust.moveStructFields.acknowledge_repr".into(),
            "dispatch".into(),
        )]);
        let merged =
            merge_tool_arg_defaults(Some(ambient), Some(&brofile), Some(&per_dispatch)).unwrap();
        assert_eq!(
            merged
                .get("default:rust.moveStructFields.acknowledge_repr")
                .map(String::as_str),
            Some("dispatch")
        );
    }

    #[test]
    fn ambient_tool_defaults_scope_retrieval_reads_to_plain_repo_cwd() {
        // Plain repo (.git directory): retrieval `project` filters default to
        // the canonicalized dispatch cwd; no worktree pin.
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().canonicalize().unwrap().join("repo");
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        std::fs::create_dir_all(repo.join("src")).unwrap();
        let cwd = repo.join("src");
        let cwd_str = cwd.to_string_lossy().into_owned();

        let ctx = AmbientContext {
            project_dir: Some(cwd_str.clone()),
            ..Default::default()
        };
        let defaults = ctx.tool_arg_defaults().expect("retrieval-read defaults");
        assert!(!defaults.contains_key("pin:*.project_dir"));
        assert!(!defaults.contains_key("pin:*.cwd"));
        for key in [
            "default:mcp.bbox_hybrid_search.project",
            "default:mcp.bbox_discover_seed_entities.project",
        ] {
            assert_eq!(
                defaults.get(key).map(String::as_str),
                Some(cwd_str.as_str())
            );
        }
    }

    #[test]
    fn ambient_tool_defaults_align_project_dir_defaults_with_worktree_pin() {
        // Worktree dispatch from a subdir: the `project` filter defaults to
        // the canonicalized cwd (server-side scope aliasing maps it to the
        // base project), while the worktree pin targets the canonical
        // worktree root.
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let (_base, wt) = fake_linked_worktree(&root);
        let cwd = wt.join("src");

        let ctx = AmbientContext {
            project_dir: Some(cwd.to_string_lossy().into_owned()),
            ..Default::default()
        };
        let defaults = ctx.tool_arg_defaults().expect("worktree defaults");
        let pin = defaults
            .get("pin:*.project_dir")
            .expect("worktree pin")
            .clone();
        assert_eq!(pin, wt.to_string_lossy());
        // The canonical dispatch-param name is pinned alongside the alias:
        // the pin guards by literal key, and the table applies before the
        // daemon's serde alias normalization (gap-6366c92d).
        assert_eq!(
            defaults.get("pin:*.cwd").map(String::as_str),
            Some(pin.as_str())
        );
        assert_eq!(
            defaults
                .get("default:mcp.bbox_hybrid_search.project")
                .map(String::as_str),
            Some(cwd.to_string_lossy().as_ref())
        );
    }

    #[test]
    fn ambient_tool_defaults_never_default_write_scope_params() {
        // §3.1 permanent exclusion (gap-ae22a6b2): `project` on the
        // knowledge/note/learn write tools means *global scope* when absent
        // and must never be mechanically filled. bbox_thread ids are also
        // excluded: the table is per-(tool,param), not per-action, and a
        // filled `id` would shadow name-based lookups and silently mutate
        // the ambient thread on resolve/promote/rename.
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let (_base, wt) = fake_linked_worktree(&root);
        let ctx = AmbientContext {
            session_id: Some("sess-max".into()),
            task_id: Some("task-max".into()),
            project_dir: Some(wt.to_string_lossy().into_owned()),
            bro_name: Some("executor".into()),
            thread_id: Some("thread-12345678".into()),
            work_item_id: Some("thread-87654321".into()),
            ..Default::default()
        };
        let defaults = ctx.tool_arg_defaults().expect("maximal ambient defaults");
        for key in defaults.keys() {
            for excluded in [
                "bbox_note.project",
                "bbox_knowledge",
                "bbox_learn",
                "bbox_remember",
                "bbox_decide",
                // bbox_gaps (list): `project` is a result filter, None = all.
                "bbox_gaps",
                "bbox_thread",
            ] {
                assert!(
                    !key.contains(excluded),
                    "write-scope/coordination param leaked into defaults: {key}"
                );
            }
        }
        // Every `.project` default targets a retrieval read tool or a gap
        // write-targeting tool (gap-b94129ba), nothing else.
        for key in defaults.keys().filter(|k| k.ends_with(".project")) {
            assert!(
                super::RETRIEVAL_PROJECT_DEFAULT_TOOLS
                    .iter()
                    .chain(super::GAP_WRITE_TARGET_DEFAULT_TOOLS)
                    .any(|tool| key == &format!("default:mcp.{tool}.project")),
                "unexpected .project default: {key}"
            );
        }
    }

    #[test]
    fn ambient_tool_defaults_fill_gap_write_targeting_from_cwd() {
        // gap-b94129ba: the three gap mutation tools get a `project`
        // write-targeting default from the canonicalized dispatch cwd,
        // gated on project_dir presence exactly like the retrieval defaults.
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().canonicalize().unwrap().join("repo");
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        let cwd_str = repo.to_string_lossy().into_owned();

        let ctx = AmbientContext {
            project_dir: Some(cwd_str.clone()),
            ..Default::default()
        };
        let defaults = ctx.tool_arg_defaults().expect("gap write-target defaults");
        for tool in ["bbox_gap", "bbox_gap_resolve", "bbox_gap_update"] {
            assert_eq!(
                defaults
                    .get(&format!("default:mcp.{tool}.project"))
                    .map(String::as_str),
                Some(cwd_str.as_str()),
                "missing gap write-target default for {tool}"
            );
        }
        // The list tool's `project` is a result filter — never defaulted.
        assert!(!defaults.contains_key("default:mcp.bbox_gaps.project"));

        // No project_dir → no gap defaults (same gate as retrieval reads).
        let no_cwd = AmbientContext {
            session_id: Some("sess-1".into()),
            ..Default::default()
        };
        let defaults = no_cwd.tool_arg_defaults().expect("session default only");
        assert!(
            !defaults.keys().any(|k| k.contains("bbox_gap")),
            "gap defaults must be gated on project_dir presence"
        );
    }

    /// Build a linked-worktree pair under `root`: a base repo whose
    /// `.git/worktrees/wt` backs a sibling worktree `wt` (`.git` file).
    fn fake_linked_worktree(root: &std::path::Path) -> (std::path::PathBuf, std::path::PathBuf) {
        let base = root.join("repo");
        let wt = root.join("wt");
        std::fs::create_dir_all(base.join(".git").join("worktrees").join("wt")).unwrap();
        std::fs::create_dir_all(wt.join("src")).unwrap();
        std::fs::write(
            wt.join(".git"),
            format!("gitdir: {}\n", base.join(".git/worktrees/wt").display()),
        )
        .unwrap();
        (base, wt)
    }

    #[test]
    fn ambient_tool_defaults_pin_project_dir_for_worktree_cwd() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let (_base, wt) = fake_linked_worktree(&root);

        // Dispatch cwd is a subdir of the worktree: the pin resolves to the
        // canonical worktree root, and the session default rides along.
        let ctx = AmbientContext {
            session_id: Some("sess-wt".into()),
            project_dir: Some(wt.join("src").to_string_lossy().into_owned()),
            ..Default::default()
        };
        let defaults = ctx.tool_arg_defaults().expect("session default + pin");
        assert_eq!(
            defaults
                .get("default:mcp.bbox_note.session_id")
                .map(String::as_str),
            Some("sess-wt")
        );
        for key in ["pin:*.project_dir", "pin:*.cwd"] {
            assert_eq!(
                defaults.get(key).map(String::as_str),
                Some(wt.to_string_lossy().as_ref()),
                "missing worktree pin {key}"
            );
        }

        // A pending session still carries the worktree pin (no session entry).
        let pending = AmbientContext {
            session_id: Some("pending".into()),
            project_dir: Some(wt.to_string_lossy().into_owned()),
            ..Default::default()
        };
        let defaults = pending.tool_arg_defaults().expect("pin only");
        assert!(!defaults.contains_key("default:mcp.bbox_note.session_id"));
        for key in ["pin:*.project_dir", "pin:*.cwd"] {
            assert_eq!(
                defaults.get(key).map(String::as_str),
                Some(wt.to_string_lossy().as_ref()),
                "missing worktree pin {key}"
            );
        }
    }

    #[test]
    fn ambient_tool_defaults_no_pin_for_plain_repo_cwd() {
        // Plain repo (.git directory): session default only — a primary
        // checkout dispatch must never get a project_dir pin.
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().canonicalize().unwrap().join("repo");
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        std::fs::create_dir_all(repo.join("src")).unwrap();

        let ctx = AmbientContext {
            session_id: Some("sess-plain".into()),
            project_dir: Some(repo.join("src").to_string_lossy().into_owned()),
            ..Default::default()
        };
        let defaults = ctx.tool_arg_defaults().expect("session default");
        assert!(!defaults.contains_key("pin:*.project_dir"));
        assert!(!defaults.contains_key("pin:*.cwd"));
    }

    #[test]
    fn worktree_pin_target_maps_cockpit_managed_roots() {
        // Managed-parent branch: bro_home/{fleet,agent}/worktrees/<repo>/<slug>
        // pins to the worktree dir even without the linked-worktree .git shape.
        let dir = tempfile::tempdir().unwrap();
        let root = dir
            .path()
            .canonicalize()
            .unwrap()
            .join("fleet")
            .join("worktrees");
        let wt = root.join("repo").join("task-1");
        std::fs::create_dir_all(wt.join("src")).unwrap();
        std::fs::write(wt.join(".git"), "gitdir: placeholder\n").unwrap();

        assert_eq!(
            super::worktree_pin_target_with_roots(
                wt.join("src").to_str().unwrap(),
                &[root.clone()]
            ),
            Some(wt.clone())
        );
        // The managed parent itself is not a worktree.
        assert_eq!(
            super::worktree_pin_target_with_roots(root.to_str().unwrap(), &[root.clone()]),
            None
        );
    }

    #[test]
    fn project_dir_pin_glob_is_safe_for_project_scoped_tools() {
        // `pin:*.cwd` / `pin:*.project_dir` glob every tool's `cwd` /
        // `project_dir` params. That is safe
        // (design/bro-harness/tool-arg-defaulting.md §3.1) because the
        // project-scoped coordination tools take `project`, not
        // `cwd`/`project_dir` — absence there means *global scope* and must
        // stay free. Tripwire: if these adapters ever grow a `cwd` or
        // `project_dir` param, re-check the globs before shipping.
        for (name, src) in [
            ("notes", include_str!("../tools/notes.rs")),
            ("knowledge", include_str!("../tools/knowledge.rs")),
            ("threads", include_str!("../tools/threads.rs")),
        ] {
            // Param-shaped matches only: the resolver attachment view's
            // `checkout_project_dir` field (phase-2 §9.2) is internal data,
            // not a wire param the pin glob could set.
            assert!(
                !src.contains("\"project_dir\"") && !src.contains("project_dir: Option<String>"),
                "src/tools/{name}.rs now carries a project_dir param; re-check pin:*.project_dir glob safety"
            );
            assert!(
                !src.contains("\"cwd\"") && !src.contains("cwd: Option<String>"),
                "src/tools/{name}.rs now carries a cwd param; re-check pin:*.cwd glob safety"
            );
        }
    }

    fn directive_ids(payload: &bro_protocol::DispatchContext) -> Vec<&str> {
        payload.directives.iter().map(|d| d.id.as_str()).collect()
    }

    fn directive<'a>(
        payload: &'a bro_protocol::DispatchContext,
        id: &str,
    ) -> &'a bro_protocol::DispatchDirective {
        payload
            .directives
            .iter()
            .find(|d| d.id == id)
            .unwrap_or_else(|| panic!("directive {id} missing"))
    }

    #[test]
    fn dispatch_context_skips_pending_session() {
        let ctx = AmbientContext {
            session_id: Some("pending".into()),
            project_dir: Some("/repo/x".into()),
            ..Default::default()
        };
        let scope = ctx.dispatch_context(None).scope.expect("scope");
        assert_eq!(scope.session, None, "pending session should be elided");
        assert_eq!(scope.project.as_deref(), Some("/repo/x"));
    }

    #[test]
    fn dispatch_context_allow_recursion_keeps_scope_and_recall() {
        // The payload carries scope + recall for every dispatch regardless of
        // `allow_recursion`. Recursion guarding is mechanical (tool filter)
        // not textual; fan-out orchestrators additionally get the packet
        // nudge as a standing directive.
        let ctx = AmbientContext {
            session_id: Some("sess-orch".into()),
            allow_recursion: true,
            provider: Some(providers::Provider::Glm),
            ..Default::default()
        };
        let payload = ctx.dispatch_context(None);
        assert_eq!(
            payload.scope.as_ref().unwrap().session.as_deref(),
            Some("sess-orch")
        );
        let recall = directive(&payload, "recall");
        assert!(recall.text.contains("bbox_knowledge"));
        let orch = directive(&payload, "orchestrator");
        assert!(orch.text.contains("bbox_compile"));
        assert_eq!(orch.cadence, bro_protocol::DirectiveCadence::Standing);
    }

    #[test]
    fn dispatch_context_contract_is_standing_and_needs_scope() {
        let ctx = AmbientContext {
            completion_contract: Some(
                "call bbox_note(kind=\"done\", body=\"summary\") before returning".into(),
            ),
            ..Default::default()
        };
        let payload = ctx.dispatch_context(None);
        let contract = directive(&payload, "contract");
        assert!(contract.text.contains("bbox_note"));
        assert_eq!(contract.cadence, bro_protocol::DirectiveCadence::Standing);
        assert!(
            contract.needs_scope,
            "contract references bbox_scope keys, so it must drop without scope"
        );
    }

    #[test]
    fn default_contract_references_bbox_scope_block() {
        // Wording follow-through (dispatch-prompt-slots.md §6): the contract's
        // correlation-key guidance is placement-neutral — it names the
        // `bbox_scope` context block, valid for both the contextual-user and
        // system-section renderings, never "[scope] above".
        assert!(DEFAULT_COMPLETION_CONTRACT.contains("`bbox_scope` context block"));
        assert!(!DEFAULT_COMPLETION_CONTRACT.contains("[scope]"));
    }

    #[test]
    fn dispatch_context_carries_pin_block_verbatim() {
        let ctx = AmbientContext {
            pin_block: Some(
                "- [bro:executor] Active arc: validate cuts against canonical doc".into(),
            ),
            ..Default::default()
        };
        let payload = ctx.dispatch_context(None);
        assert!(payload.pins.as_deref().unwrap().contains("Active arc"));
    }

    #[test]
    fn dispatch_context_recall_directive_is_standing_and_exempts_live_state_surfaces() {
        let payload = AmbientContext::default().dispatch_context(None);
        let recall = directive(&payload, "recall");
        assert_eq!(recall.cadence, bro_protocol::DirectiveCadence::Standing);
        assert!(!recall.needs_scope);
        assert!(recall.text.contains("bbox_knowledge"));
        assert!(recall.text.contains("procedural live-state work"));
        assert!(recall.text.contains("bbox_gaps"));
        assert!(recall.text.contains("bbox_gap*"));
        assert!(recall.text.contains("repo-owned state commits"));
        assert!(!recall.text.contains("FIRST tool call"));
    }

    #[test]
    fn dispatch_context_milestone_directive_is_standing() {
        let payload = AmbientContext::default().dispatch_context(None);
        let milestone = directive(&payload, "milestone");
        assert_eq!(milestone.cadence, bro_protocol::DirectiveCadence::Standing);
        assert!(milestone.needs_scope);
        assert!(milestone.text.contains("bro_report"));
    }

    #[test]
    fn dispatch_context_directive_order_and_conditionals() {
        // Solo executor: recall → task_shape → contract → milestone.
        let solo = AmbientContext {
            allow_recursion: false,
            completion_contract: Some(DEFAULT_COMPLETION_CONTRACT.to_string()),
            ..Default::default()
        };
        let payload = solo.dispatch_context(None);
        assert_eq!(
            directive_ids(&payload),
            vec!["recall", "task_shape", "contract", "milestone"]
        );
        let task_shape = directive(&payload, "task_shape");
        assert!(task_shape.text.contains("bbox_compile"));
        assert!(task_shape.text.contains("bbox_packet_gap"));
        assert_eq!(task_shape.cadence, bro_protocol::DirectiveCadence::Standing);

        // Orchestrator with workspace coercion: orchestrator after task_shape,
        // workspace last (the old preamble's ordering, preserved as the
        // directive vec order).
        let orch = AmbientContext {
            allow_recursion: true,
            coerce_workspace: true,
            ..Default::default()
        };
        let payload = orch.dispatch_context(None);
        assert_eq!(
            directive_ids(&payload),
            vec![
                "recall",
                "task_shape",
                "orchestrator",
                "milestone",
                "workspace"
            ]
        );
    }

    #[test]
    fn dispatch_context_orchestrator_absent_without_recursion() {
        let payload = AmbientContext::default().dispatch_context(None);
        assert!(!directive_ids(&payload).contains(&"orchestrator"));
    }

    #[test]
    fn coerce_workspace_gates_workspace_directive() {
        let payload = AmbientContext {
            coerce_workspace: false,
            ..Default::default()
        }
        .dispatch_context(None);
        assert!(!directive_ids(&payload).contains(&"workspace"));

        let payload = AmbientContext {
            coerce_workspace: true,
            ..Default::default()
        }
        .dispatch_context(None);
        let ws = directive(&payload, "workspace");
        for needle in [
            "[workspace-tools mode]",
            "work_smart_read",
            "work_bash",
            "work_git_status",
            "work_git_diff",
            "work_git_log",
            "bbox_note(kind=learned",
        ] {
            assert!(ws.text.contains(needle), "appendix must reference {needle}");
        }
        assert_eq!(ws.cadence, bro_protocol::DirectiveCadence::Standing);
    }

    #[test]
    fn dispatch_context_persona_normalizes_lens() {
        let ctx = AmbientContext::default();
        assert_eq!(
            ctx.dispatch_context(Some("You are a reviewer")).persona,
            Some("You are a reviewer".to_string())
        );
        assert_eq!(ctx.dispatch_context(None).persona, None);
        assert_eq!(ctx.dispatch_context(Some("   ")).persona, None);
    }

    #[test]
    fn milestone_directive_fires_for_every_provider() {
        // The reporting nudge is unconditional — every provider, regardless
        // of allow_recursion / coerce_workspace / contract state — and
        // needs_scope (its bro_report correlation rides the scope keys).
        // Cadence is standing: per-turn reinforcement was too noisy on
        // Codex/Brodex because it appears after ordinary tool calls.
        for p in [
            providers::Provider::Glm,
            providers::Provider::Deepseek,
            providers::Provider::Minimax,
            providers::Provider::Brodex,
            providers::Provider::VibeBh,
        ] {
            let payload = AmbientContext {
                provider: Some(p),
                ..Default::default()
            }
            .dispatch_context(None);
            let milestone = directive(&payload, "milestone");
            assert!(
                milestone.text.contains("bro_report"),
                "bro_report reference missing for provider {p:?}"
            );
            assert_eq!(milestone.cadence, bro_protocol::DirectiveCadence::Standing);
            assert!(milestone.needs_scope);
        }
    }

    #[test]
    fn test_task_result_json_completed() {
        let task = Arc::new(Task {
            inner: Mutex::new(TaskInner {
                id: "t1".into(),
                provider: Provider::Glm,
                session_id: "s1".into(),
                events: EventRing::new(),
                model: None,
                last_assistant_message: Some("Done!".into()),
                usage: Some(Usage {
                    input_tokens: 100,
                    output_tokens: 50,
                    ..Default::default()
                }),
                cost_usd: Some(0.05),
                num_turns: Some(3),
                last_turn_input_tokens: None,
                context_window: None,
                stderr: String::new(),
                status: TaskStatus::Completed,
                started_at: 1000,
                completed_at: Some(5000),
                exit_code: Some(0),
                cwd: None,
                managed_worktree: None,
                bro_label: None,
                name: None,
                agent_label: None,
                report: None,
                interrupted: false,
                recoverable: false,
                transcript_location: None,
                transcript_cursor: None,
                live_cursor: 0,
                harness_ingest_seq: 0,
                last_delta_roster_emit_ms: 0,
                supervision: SupervisionState::default(),
                origin: bro_core::Origin::Unknown,
                workflow_owned: workflow_owned_for_origin(bro_core::Origin::Unknown),
                project_id: None,
            }),
            notify: Arc::new(Notify::new()),
            child_id: Mutex::new(None),
            roster_events: None,
        });

        let json = task_result_json(&task);
        assert_eq!(json["taskId"], "t1");
        assert_eq!(json["result"], "Done!");
        assert_eq!(json["hasResult"], true);
        assert_eq!(json["hasLastMessage"], true);
        assert_eq!(json["costUsd"], 0.05);
        assert_eq!(json["usage"]["input_tokens"], 100);
        // Terminal + green: the liveness row is gated out — on a finished,
        // healthy task it would only restate `ok: true`.
        assert!(
            json.get("supervision").is_none(),
            "terminal healthy task should omit supervision: {json}"
        );
    }

    #[test]
    fn running_task_keeps_supervision() {
        // A live task still carries supervision (idle/tool_running thresholds
        // and any alerts are useful while it runs).
        let running = task_with(TaskStatus::Running, "", vec![]);
        let json = task_result_json(&running);
        assert!(
            json["supervision"].is_object(),
            "running task must keep supervision: {json}"
        );
    }

    // -----------------------------------------------------------------
    // Context-ceiling telemetry in the status projection
    // -----------------------------------------------------------------

    /// A task carrying a measured turn occupancy and, optionally, a window.
    fn task_with_context(
        status: TaskStatus,
        last_turn_input_tokens: Option<u64>,
        context_window: Option<u64>,
    ) -> Task {
        let task = task_with(status, "", vec![]);
        {
            let mut inner = task.inner.lock();
            inner.last_turn_input_tokens = last_turn_input_tokens;
            inner.context_window = context_window;
        }
        task
    }

    #[test]
    fn context_block_is_absent_without_a_measurement() {
        let task = task_with_context(TaskStatus::Running, None, Some(200_000));
        let json = task_result_json(&task);
        assert!(
            json.get("context").is_none(),
            "a task with no reported turn must report no pressure, not a zero \
             that reads as plenty of room: {json}"
        );
    }

    #[test]
    fn context_block_is_reported_while_the_task_is_still_running() {
        // The whole point of the signal: usage/costUsd/numTurns are gated to
        // terminal status, but a RUNNING task is the only one an orchestrator
        // can still rescue by rotating the session.
        let task = task_with_context(TaskStatus::Running, Some(50_000), Some(200_000));
        task.inner.lock().usage = Some(Usage {
            input_tokens: 1_000,
            output_tokens: 20,
            ..Default::default()
        });
        let json = task_result_json(&task);
        let context = &json["context"];
        assert!(
            context.is_object(),
            "running task must carry context pressure: {json}"
        );
        assert_eq!(context["last_turn_input_tokens"], 50_000);
        assert_eq!(context["context_window"], 200_000);
        assert_eq!(context["utilization"], 0.25);
        assert!(context.get("approaching_ceiling").is_none());
        assert!(
            json.get("usage").is_none(),
            "the terminal-only usage gate must stay closed for a running task, \
             which is exactly why context needs its own ungated path: {json}"
        );
    }

    #[test]
    fn context_block_omits_utilization_when_the_window_is_unknown() {
        let task = task_with_context(TaskStatus::Running, Some(180_000), None);
        let json = task_result_json(&task);
        let context = &json["context"];
        assert_eq!(context["last_turn_input_tokens"], 180_000);
        assert!(
            context["context_window"].is_null(),
            "an unknown window must surface as null, never as a guess: {json}"
        );
        assert!(
            context["utilization"].is_null(),
            "utilization must be absent without a denominator: {json}"
        );
        assert!(context.get("approaching_ceiling").is_none());
    }

    #[test]
    fn context_block_reports_occupancy_without_a_rotation_alarm() {
        let task = task_with_context(TaskStatus::Running, Some(200_000), Some(200_000));
        let json = task_result_json(&task);
        assert_eq!(json["context"]["utilization"], 1.0);
        assert!(json["context"].get("approaching_ceiling").is_none());
        assert!(json["context"].get("ceiling_ratio").is_none());
        assert_eq!(json["context"]["measurement"], "last_model_request");
        assert!(
            json["context"]["guidance"]
                .as_str()
                .unwrap()
                .contains("not a remaining work budget")
        );
    }

    #[test]
    fn context_block_survives_into_terminal_status() {
        let task = task_with_context(TaskStatus::Completed, Some(160_000), Some(200_000));
        let json = task_result_json(&task);
        assert_eq!(json["context"]["last_turn_input_tokens"], 160_000);
    }

    #[test]
    fn roster_summary_carries_the_same_context_block() {
        // bro_dashboard renders from the roster projection, not from
        // task_result_json, so the two must agree or a fleet-wide scan
        // silently loses the signal that a per-task check would show.
        let task = Arc::new(task_with_context(
            TaskStatus::Running,
            Some(160_000),
            Some(200_000),
        ));
        let summary = roster_summary_from_task(&task);
        let pressure = summary.context.expect("roster summary must carry context");
        assert_eq!(pressure.last_turn_input_tokens, 160_000);
        assert_eq!(pressure.context_window, Some(200_000));
        assert_eq!(pressure.utilization, Some(0.8));

        let status = task_result_json(&task);
        assert_eq!(
            pressure.observation_json(),
            status["context"],
            "roster and status projections must publish identical blocks"
        );
    }

    #[test]
    fn roster_summary_context_absent_without_a_measurement() {
        let task = Arc::new(task_with_context(TaskStatus::Running, None, None));
        assert!(roster_summary_from_task(&task).context.is_none());
    }

    #[test]
    fn task_status_json_omits_redundant_snapshot() {
        // Plain completed task: Unknown origin, no worktree, not workflow-owned,
        // no error. The typed snapshot would only duplicate the flat identity
        // fields, so it is omitted — those flat fields stay.
        let ok = task_with(
            TaskStatus::Completed,
            "",
            vec![serde_json::json!({"type": "system"})],
        );
        let json = task_status_json(&ok, 0);
        assert!(
            json.get("snapshot").is_none(),
            "redundant snapshot should be omitted: {json}"
        );
        assert_eq!(json["taskId"], "t");
        assert_eq!(json["status"], "completed");
    }

    #[test]
    fn task_status_json_keeps_snapshot_with_error() {
        // A failed task carries a structured error the flat fields don't expose
        // as a typed object, so the snapshot is retained to carry it.
        let failed = task_with(TaskStatus::Failed, "boom", vec![]);
        let json = task_status_json(&failed, 0);
        assert!(
            json["snapshot"].is_object(),
            "snapshot bearing an error should be kept: {json}"
        );
        assert_eq!(json["snapshot"]["status"], "failed");
    }

    #[test]
    fn test_task_result_json_failed() {
        let task = Arc::new(Task {
            inner: Mutex::new(TaskInner {
                id: "t2".into(),
                provider: Provider::Brodex,
                session_id: "s2".into(),
                events: EventRing::new(),
                model: None,
                last_assistant_message: None,
                usage: None,
                cost_usd: None,
                num_turns: None,
                last_turn_input_tokens: None,
                context_window: None,
                stderr: "something went wrong".into(),
                status: TaskStatus::Failed,
                started_at: 1000,
                completed_at: Some(2000),
                exit_code: Some(1),
                cwd: None,
                managed_worktree: None,
                bro_label: None,
                name: None,
                agent_label: None,
                report: None,
                interrupted: false,
                recoverable: false,
                transcript_location: None,
                transcript_cursor: None,
                live_cursor: 0,
                harness_ingest_seq: 0,
                last_delta_roster_emit_ms: 0,
                supervision: SupervisionState::default(),
                origin: bro_core::Origin::Unknown,
                workflow_owned: workflow_owned_for_origin(bro_core::Origin::Unknown),
                project_id: None,
            }),
            notify: Arc::new(Notify::new()),
            child_id: Mutex::new(None),
            roster_events: None,
        });

        let json = task_result_json(&task);
        assert_eq!(json["hasResult"], false);
        assert_eq!(json["resultCapture"]["status"], "missing");
        assert_eq!(json["resultCapture"]["stderrPresent"], true);
        assert_eq!(json["exitCode"], 1);
        assert!(
            json["stderr"]
                .as_str()
                .unwrap()
                .contains("something went wrong")
        );
    }

    #[test]
    fn task_result_json_completed_without_result_surfaces_capture_diagnostic() {
        let task = Arc::new(Task {
            inner: Mutex::new(TaskInner {
                id: "t-empty".into(),
                provider: Provider::Minimax,
                session_id: "s-empty".into(),
                events: EventRing::from_loaded(vec![
                    serde_json::json!({"type": "system", "subtype": "init"}),
                ]),
                model: None,
                last_assistant_message: None,
                usage: None,
                cost_usd: None,
                num_turns: None,
                last_turn_input_tokens: None,
                context_window: None,
                stderr: String::new(),
                status: TaskStatus::Completed,
                started_at: 1000,
                completed_at: Some(2000),
                exit_code: Some(0),
                cwd: None,
                managed_worktree: None,
                bro_label: None,
                name: None,
                agent_label: None,
                report: None,
                interrupted: false,
                recoverable: false,
                transcript_location: None,
                transcript_cursor: None,
                live_cursor: 0,
                harness_ingest_seq: 0,
                last_delta_roster_emit_ms: 0,
                supervision: SupervisionState::default(),
                origin: bro_core::Origin::Unknown,
                workflow_owned: workflow_owned_for_origin(bro_core::Origin::Unknown),
                project_id: None,
            }),
            notify: Arc::new(Notify::new()),
            child_id: Mutex::new(None),
            roster_events: None,
        });

        let json = task_result_json(&task);
        assert_eq!(json["status"], "completed");
        assert_eq!(json["hasResult"], false);
        assert_eq!(json["resultCapture"]["status"], "missing");
        assert_eq!(
            json["resultCapture"]["message"],
            "task reached a terminal state without a captured assistant result"
        );
        assert_eq!(json["resultCapture"]["eventCount"], 1);
        assert_eq!(json["resultCapture"]["exitCode"], 0);
    }

    #[test]
    fn fork_rejection_marks_failed_without_overwriting_result_state() {
        let mut inner = TaskInner {
            id: "t3".into(),
            provider: Provider::Deepseek,
            session_id: "requested-session".into(),
            events: EventRing::new(),
            model: None,
            last_assistant_message: Some("trusted prior result".into()),
            usage: Some(Usage {
                input_tokens: 10,
                output_tokens: 5,
                ..Default::default()
            }),
            cost_usd: Some(0.01),
            num_turns: Some(1),
            last_turn_input_tokens: None,
            context_window: None,
            stderr: String::new(),
            status: TaskStatus::Running,
            started_at: 1000,
            completed_at: None,
            exit_code: None,
            cwd: None,
            managed_worktree: None,
            bro_label: None,
            name: None,
            agent_label: None,
            report: None,
            interrupted: false,
            recoverable: false,
            transcript_location: None,
            transcript_cursor: None,
            live_cursor: 0,
            harness_ingest_seq: 0,
            last_delta_roster_emit_ms: 0,
            supervision: SupervisionState::default(),
            origin: bro_core::Origin::Unknown,
            workflow_owned: workflow_owned_for_origin(bro_core::Origin::Unknown),
            project_id: None,
        };

        reject_forked_session(&mut inner, "forked-session");

        assert_eq!(inner.status, TaskStatus::Failed);
        assert_eq!(
            inner.last_assistant_message.as_deref(),
            Some("trusted prior result")
        );
        assert_eq!(
            inner.stderr.trim(),
            "session fork detected: requested resume of requested-session, provider emitted forked-session"
        );
        assert_eq!(
            inner
                .usage
                .as_ref()
                .map(|u| (u.input_tokens, u.output_tokens)),
            Some((10, 5))
        );
        assert_eq!(inner.cost_usd, Some(0.01));
        assert_eq!(inner.num_turns, Some(1));
    }

    #[test]
    fn apply_sink_updates_replaces_task_observed_state() {
        let mut inner = TaskInner {
            id: "t4".into(),
            provider: Provider::Deepseek,
            session_id: "s1".into(),
            events: EventRing::new(),
            model: None,
            last_assistant_message: None,
            usage: None,
            cost_usd: None,
            num_turns: None,
            last_turn_input_tokens: None,
            context_window: None,
            stderr: String::new(),
            status: TaskStatus::Running,
            started_at: 1000,
            completed_at: None,
            exit_code: None,
            cwd: None,
            managed_worktree: None,
            bro_label: None,
            name: None,
            agent_label: None,
            report: None,
            interrupted: false,
            recoverable: false,
            transcript_location: None,
            transcript_cursor: None,
            live_cursor: 0,
            harness_ingest_seq: 0,
            last_delta_roster_emit_ms: 0,
            supervision: SupervisionState::default(),
            origin: bro_core::Origin::Unknown,
            workflow_owned: workflow_owned_for_origin(bro_core::Origin::Unknown),
            project_id: None,
        };

        let sink = EventSink {
            last_assistant_message: Some("fresh output".into()),
            usage: Some(Usage {
                input_tokens: 12,
                output_tokens: 8,
                ..Default::default()
            }),
            cost_usd: Some(0.02),
            num_turns: Some(2),
            session_id: Some("s1".into()),
            interrupted: false,
            ..Default::default()
        };

        apply_sink_updates(&mut inner, sink);

        assert_eq!(
            inner.last_assistant_message.as_deref(),
            Some("fresh output")
        );
        assert_eq!(
            inner
                .usage
                .as_ref()
                .map(|u| (u.input_tokens, u.output_tokens)),
            Some((12, 8))
        );
        assert_eq!(inner.cost_usd, Some(0.02));
        assert_eq!(inner.num_turns, Some(2));
    }

    #[test]
    fn successful_enter_worktree_updates_task_cwd() {
        let mut inner = TaskInner {
            id: "t5".into(),
            provider: Provider::Brodex,
            session_id: "s1".into(),
            events: EventRing::from_loaded(vec![serde_json::json!({
                "type": "assistant",
                "message": { "content": [
                    { "type": "tool_use", "id": "enter1", "name": "enter_worktree" }
                ]}
            })]),
            model: None,
            last_assistant_message: None,
            usage: None,
            cost_usd: None,
            num_turns: None,
            last_turn_input_tokens: None,
            context_window: None,
            stderr: String::new(),
            status: TaskStatus::Running,
            started_at: 1000,
            completed_at: None,
            exit_code: None,
            cwd: Some("/repo/base".into()),
            managed_worktree: None,
            bro_label: None,
            name: None,
            agent_label: None,
            report: None,
            interrupted: false,
            recoverable: false,
            transcript_location: None,
            transcript_cursor: None,
            live_cursor: 0,
            harness_ingest_seq: 0,
            last_delta_roster_emit_ms: 0,
            supervision: SupervisionState::default(),
            origin: bro_core::Origin::Unknown,
            workflow_owned: workflow_owned_for_origin(bro_core::Origin::Unknown),
            project_id: None,
        };
        let evt = serde_json::json!({
            "type": "user",
            "message": { "content": [
                {
                    "type": "tool_result",
                    "tool_use_id": "enter1",
                    "content": "{\"ok\":true,\"cwd\":\"/repo/.bro-fleet-worktrees/wt\",\"base_repo\":\"/repo/base\"}",
                    "is_error": false
                }
            ]}
        });
        inner.events.push(evt.clone());

        apply_cwd_updates_from_event(&mut inner, &evt);

        assert_eq!(inner.cwd.as_deref(), Some("/repo/.bro-fleet-worktrees/wt"));
    }

    #[test]
    fn successful_exit_worktree_with_removed_worktree_restores_base_cwd() {
        let mut inner = TaskInner {
            id: "t6".into(),
            provider: Provider::Brodex,
            session_id: "s1".into(),
            events: EventRing::from_loaded(vec![serde_json::json!({
                "type": "assistant",
                "message": { "content": [
                    { "type": "tool_use", "id": "exit1", "name": "exit_worktree" }
                ]}
            })]),
            model: None,
            last_assistant_message: None,
            usage: None,
            cost_usd: None,
            num_turns: None,
            last_turn_input_tokens: None,
            context_window: None,
            stderr: String::new(),
            status: TaskStatus::Running,
            started_at: 1000,
            completed_at: None,
            exit_code: None,
            cwd: Some("/repo/.bro-fleet-worktrees/wt".into()),
            managed_worktree: None,
            bro_label: None,
            name: None,
            agent_label: None,
            report: None,
            interrupted: false,
            recoverable: false,
            transcript_location: None,
            transcript_cursor: None,
            live_cursor: 0,
            harness_ingest_seq: 0,
            last_delta_roster_emit_ms: 0,
            supervision: SupervisionState::default(),
            origin: bro_core::Origin::Unknown,
            workflow_owned: workflow_owned_for_origin(bro_core::Origin::Unknown),
            project_id: None,
        };
        let evt = serde_json::json!({
            "type": "user",
            "message": { "content": [
                {
                    "type": "tool_result",
                    "tool_use_id": "exit1",
                    "content": "{\"ok\":true,\"disposition\":\"publish\",\"removed_worktree\":\"/repo/.bro-fleet-worktrees/wt\",\"base_repo\":\"/repo/base\"}",
                    "is_error": false
                }
            ]}
        });
        inner.events.push(evt.clone());

        apply_cwd_updates_from_event(&mut inner, &evt);

        assert_eq!(inner.cwd.as_deref(), Some("/repo/base"));
    }

    #[test]
    fn mcp_task_summary_keeps_blockers_without_replaying_deliverables_or_worker_paths() {
        let task = task_with(TaskStatus::Completed, "", vec![]);
        {
            let mut inner = task.inner.lock();
            inner.last_assistant_message = Some("completed work ".repeat(5000));
            inner.cost_usd = Some(0.5);
            inner.usage = Some(Usage {
                input_tokens: 500,
                output_tokens: 100,
                ..Default::default()
            });
            inner.report = Some(BroReport {
                message: "Awaiting review".into(),
                needs: Some("Review the migration".into()),
                data: Some(json!({"trace": "x".repeat(30000)})),
                reported_at: 123,
            });
            inner.transcript_location = harness_transcript_location(
                Provider::Glm,
                std::path::Path::new("/worker-only"),
                "s",
                None,
            );
        }
        let summary = mcp_task_status_json(&task, "summary", None, None, 0, false).unwrap();
        assert_eq!(summary["status"], "completed");
        assert_eq!(summary["hasResult"], true);
        assert_eq!(summary["report"]["needs"], "Review the migration");
        assert_eq!(summary["report"]["detailsOmitted"], true);
        assert!(summary["report"].get("data").is_none());
        for absent in [
            "result",
            "usage",
            "costUsd",
            "snapshot",
            "transcriptLocation",
            "transcriptCursor",
        ] {
            assert!(summary.get(absent).is_none(), "{absent}");
        }
        assert!(serde_json::to_vec(&summary).unwrap().len() < 4096);
        let full = task_result_json_from_inner(&task.inner.lock());
        assert!(full.get("transcriptLocation").is_none());
        assert!(full.get("transcriptCursor").is_none());
        assert_eq!(full["transcriptAvailable"], true);
        let debug = mcp_task_status_json(&task, "summary", None, None, 0, true).unwrap();
        assert!(debug["usage"].is_object());
        assert_eq!(debug["transcriptLocationOwner"], "execution_worker");
        assert!(debug["report"].get("data").is_none());
    }

    #[test]
    fn mcp_task_result_pages_reassemble_unicode_and_reject_changed_bodies() {
        let text = "🦀 café\n\"quoted\"".repeat(12);
        let task = task_with(TaskStatus::Completed, "", vec![]);
        task.inner.lock().last_assistant_message = Some(text.clone());
        let mut cursor: Option<String> = None;
        let mut result = String::new();
        loop {
            let page = mcp_task_status_json(&task, "result", cursor.as_deref(), Some(7), 0, false)
                .unwrap();
            let chunk = page["body"]["text"].as_str().unwrap();
            assert!(!chunk.is_empty());
            assert!(chunk.len() <= 7);
            result.push_str(chunk);
            cursor = page["body"]["next_cursor"].as_str().map(str::to_string);
            if cursor.is_none() {
                break;
            }
        }
        assert_eq!(result, text);
        let first = mcp_task_status_json(&task, "result", None, Some(7), 0, false).unwrap();
        let cursor = first["body"]["next_cursor"].as_str().unwrap();
        task.inner.lock().last_assistant_message = Some("changed".into());
        assert!(
            mcp_task_status_json(&task, "result", Some(cursor), Some(7), 0, false)
                .unwrap_err()
                .to_string()
                .contains("body changed")
        );
    }

    #[test]
    fn mcp_wait_continues_exactly_and_preserves_workflow_structured_exit() {
        let task = task_with(TaskStatus::Completed, "", vec![]);
        let original = "text 🦀\n".repeat(1400);
        task.inner.lock().last_assistant_message = Some(original.clone());
        let wait = mcp_task_result_json(&task);
        assert_eq!(wait["resultTruncated"], true);
        let mut assembled = wait["result"].as_str().unwrap().to_string();
        let mut cursor = wait["resultCursor"].as_str().map(str::to_string);
        while let Some(next) = cursor {
            let page = mcp_task_status_json(&task, "result", Some(&next), None, 0, false).unwrap();
            assembled.push_str(page["body"]["text"].as_str().unwrap());
            cursor = page["body"]["next_cursor"].as_str().map(str::to_string);
        }
        assert_eq!(assembled, original);
        assert_eq!(
            task_result_json_from_inner(&task.inner.lock())["result"],
            original
        );
        {
            let mut inner = task.inner.lock();
            inner.provider = Provider::Workflow;
            inner.last_assistant_message = Some(
                json!({"structured_exit": {"ready": true}, "trace": "x".repeat(10000)}).to_string(),
            );
        }
        assert_eq!(
            mcp_task_result_json(&task)["structuredExit"],
            json!({"ready": true})
        );
        assert_eq!(
            task_result_json_from_inner(&task.inner.lock())["structuredExit"],
            json!({"ready": true})
        );
    }

    #[test]
    fn mcp_large_structured_exit_has_exact_continuation_without_changing_workflow_exports() {
        let task = task_with(TaskStatus::Completed, "", vec![]);
        let exit = json!({"evidence": "語".repeat(5000), "ready": true});
        {
            let mut inner = task.inner.lock();
            inner.provider = Provider::Workflow;
            inner.last_assistant_message = Some(json!({"structured_exit": exit}).to_string());
        }
        let wait = mcp_task_result_json(&task);
        assert!(wait.get("structuredExit").is_none());
        assert_eq!(wait["structuredExitOmitted"], true);
        assert!(
            wait["structuredExitHint"]
                .as_str()
                .unwrap()
                .contains("structured_exit")
        );
        let mut body = String::new();
        let mut cursor: Option<String> = None;
        loop {
            let response =
                mcp_task_status_json(&task, "structured_exit", cursor.as_deref(), None, 0, false)
                    .unwrap();
            assert_eq!(response["body"]["format"], "json");
            body.push_str(response["body"]["text"].as_str().unwrap());
            cursor = response["body"]["next_cursor"].as_str().map(str::to_string);
            if cursor.is_none() {
                break;
            }
        }
        assert_eq!(serde_json::from_str::<Value>(&body).unwrap(), exit);
        assert_eq!(
            task_result_json_from_inner(&task.inner.lock())["structuredExit"],
            exit
        );
    }

    #[test]
    fn mcp_report_pages_keep_complete_data_and_stable_revision() {
        let task = task_with(TaskStatus::Running, "", vec![]);
        task.inner.lock().report = Some(BroReport {
            message: "Progress".into(),
            needs: Some("Review".into()),
            data: Some(json!({"payload": "é".repeat(9000)})),
            reported_at: 1,
        });
        let mut cursor: Option<String> = None;
        let mut full = String::new();
        loop {
            let response =
                mcp_task_status_json(&task, "report", cursor.as_deref(), None, 0, false).unwrap();
            assert_eq!(response["body"]["format"], "json");
            full.push_str(response["body"]["text"].as_str().unwrap());
            cursor = response["body"]["next_cursor"].as_str().map(str::to_string);
            if cursor.is_none() {
                break;
            }
        }
        let report: Value = serde_json::from_str(&full).unwrap();
        assert_eq!(report["data"]["payload"], "é".repeat(9000));
        assert_eq!(report["reportedAt"], 1);
        assert!(report.get("reportedAgo").is_none());
        assert!(mcp_task_status_json(&task, "summary", Some("bad"), None, 0, false).is_err());
        assert!(mcp_task_status_json(&task, "unknown", None, None, 0, false).is_err());
        assert!(mcp_task_status_json(&task, "report", None, None, 1, false).is_err());
    }

    #[test]
    fn control_status_bounds_mirrored_envelope_and_keeps_typed_control_facets() {
        let task = task_with(
            TaskStatus::Failed,
            &"\u{0001}".repeat(30000),
            vec![
                json!({"type":"assistant", "message":{"content":(0..5000).map(|_| json!({"type":"text", "text":"wide"})).collect::<Vec<_>>()}}),
            ],
        );
        {
            let mut inner = task.inner.lock();
            inner.last_assistant_message = Some("\u{0001}語".repeat(30000));
            inner.origin = bro_core::Origin::Cockpit;
            inner.workflow_owned = true;
            inner.interrupted = true;
            inner.report = Some(BroReport {
                message: "\u{0001}".repeat(9000),
                needs: Some("\u{0001}".repeat(9000)),
                data: Some(json!({"payload":"large".repeat(9000)})),
                reported_at: 1,
            });
        }
        let status = control_task_status_json(&task, "summary", None, None, usize::MAX).unwrap();
        assert_eq!(status["status"], "failed");
        assert!(status["snapshot"].get("last_message").is_none());
        let snapshot: bro_protocol::TaskSnapshot =
            serde_json::from_value(status["snapshot"].clone()).unwrap();
        assert_eq!(snapshot.origin, bro_core::Origin::Cockpit);
        assert!(snapshot.interrupted && snapshot.workflow_owned);
        assert_eq!(snapshot.error.unwrap().code, "task_failed");
        assert!(snapshot.last_message.is_none());
        assert_eq!(status["resultTruncated"], true);
        assert_eq!(status["stderrTruncated"], true);
        assert_eq!(status["eventPreview"]["returned"], 0);
        assert_eq!(status["eventPreview"]["retained_events"], 1);
        assert!(status.get("stderr").is_none());
        assert!(status.get("usage").is_none());
        // Exercise worst-case JSON escaping and the transport's optional mirror.
        let envelope = json!({"content":[{"type":"text", "text":serde_json::to_string_pretty(&status).unwrap()}], "structuredContent":status});
        assert!(serde_json::to_vec(&envelope).unwrap().len() < 80 * 1024);
        let response = crate::server::BlackboxServer::ok_json(&status);
        let wire = serde_json::to_value(response).unwrap();
        assert_ne!(wire["isError"], true);
        let decoded: Value =
            serde_json::from_str(wire["content"][0]["text"].as_str().unwrap()).unwrap();
        assert_eq!(decoded["taskId"], "t");
    }

    #[test]
    fn control_detail_pages_reassemble_exact_bodies_and_reject_stale_cursors() {
        let task = task_with(
            TaskStatus::Failed,
            &"failure \u{0001}語\n".repeat(1500),
            vec![],
        );
        {
            let mut inner = task.inner.lock();
            inner.last_assistant_message = Some("deliverable 🦀\n".repeat(1500));
            inner.report = Some(BroReport {
                message: "update".into(),
                needs: None,
                data: Some(json!({"text":"\u{0001}".repeat(9000)})),
                reported_at: 1,
            });
            for idx in 0..TASK_EVENT_RING_CAPACITY + 3 {
                inner
                    .events
                    .push(json!({"type":"assistant", "idx":idx, "message":"🦀"}));
            }
        }
        for detail in ["result", "report", "stderr", "events"] {
            let expected = {
                let inner = task.inner.lock();
                match detail {
                    "result" => inner.last_assistant_message.clone().unwrap(),
                    "report" => serde_json::to_string(inner.report.as_ref().unwrap()).unwrap(),
                    "stderr" => inner.stderr.clone(),
                    "events" => {
                        serde_json::to_string(&inner.events.iter().collect::<Vec<_>>()).unwrap()
                    }
                    _ => unreachable!(),
                }
            };
            let mut cursor: Option<String> = None;
            let mut full = String::new();
            loop {
                let page =
                    control_task_status_json(&task, detail, cursor.as_deref(), None, 0).unwrap();
                assert!(
                    serde_json::to_vec(&page["body"]).unwrap().len() <= CONTROL_BODY_WIRE_BYTES
                );
                if detail == "events" {
                    assert_eq!(page["retainedEvents"], TASK_EVENT_RING_CAPACITY);
                    assert_eq!(page["eventCount"], TASK_EVENT_RING_CAPACITY + 3);
                }
                full.push_str(page["body"]["text"].as_str().unwrap());
                cursor = page["body"]["next_cursor"].as_str().map(str::to_string);
                if cursor.is_none() {
                    break;
                }
            }
            assert_eq!(full, expected);
        }
        let first = control_task_status_json(&task, "stderr", None, None, 0).unwrap();
        let cursor = first["body"]["next_cursor"].as_str().unwrap();
        assert!(control_task_status_json(&task, "result", Some(cursor), None, 0).is_err());
        task.inner.lock().stderr.push_str("changed");
        assert!(control_task_status_json(&task, "stderr", Some(cursor), None, 0).is_err());
        assert!(control_task_status_json(&task, "summary", Some(cursor), None, 0).is_err());
        assert!(control_task_status_json(&task, "events", None, None, 1).is_err());
        assert!(control_task_status_json(&task, "unknown", None, None, 0).is_err());
    }

    #[test]
    fn report_truncated_when_oversized() {
        let huge_message = "x".repeat(16000);
        let task = task_with(TaskStatus::Running, "", vec![]);
        {
            let mut inner = task.inner.lock();
            inner.report = Some(BroReport {
                message: huge_message.clone(),
                needs: None,
                data: None,
                reported_at: now_ms(),
            });
        }
        let json = task_status_json(&task, 0);
        let report = &json["report"];
        assert_eq!(report["detailsOmitted"], true);
        assert!(
            report["detailHint"]
                .as_str()
                .unwrap()
                .contains("detail=report")
        );
        let msg = report["message"].as_str().unwrap();
        assert!(huge_message.starts_with(msg));
        assert!(msg.len() <= 512);
        // The whole status object must still be valid JSON and under the 80K cap.
        let status_str = serde_json::to_string(&json).unwrap();
        assert!(status_str.len() <= 80 * 1024);
    }

    #[test]
    fn report_unchanged_when_small() {
        let task = task_with(TaskStatus::Running, "", vec![]);
        {
            let mut inner = task.inner.lock();
            inner.report = Some(BroReport {
                message: "short".into(),
                needs: Some("input".into()),
                data: None,
                reported_at: now_ms(),
            });
        }
        let json = task_status_json(&task, 0);
        let report = &json["report"];
        assert_eq!(report["message"], "short");
        assert_eq!(report["needs"], "input");
        assert!(report.get("reportTruncated").is_none());
    }
}

#[cfg(test)]
mod async_tests {
    use super::*;
    use std::sync::Arc;
    use tokio::sync::Notify;

    #[tokio::test]
    async fn test_wait_for_task_already_terminal() {
        let task = Arc::new(Task {
            inner: Mutex::new(TaskInner {
                id: "t1".into(),
                provider: Provider::Glm,
                session_id: "s1".into(),
                events: EventRing::new(),
                model: None,
                last_assistant_message: None,
                usage: None,
                cost_usd: None,
                num_turns: None,
                last_turn_input_tokens: None,
                context_window: None,
                stderr: String::new(),
                status: TaskStatus::Completed,
                started_at: now_ms(),
                completed_at: Some(now_ms()),
                exit_code: Some(0),
                cwd: None,
                managed_worktree: None,
                bro_label: None,
                name: None,
                agent_label: None,
                report: None,
                interrupted: false,
                recoverable: false,
                transcript_location: None,
                transcript_cursor: None,
                live_cursor: 0,
                harness_ingest_seq: 0,
                last_delta_roster_emit_ms: 0,
                supervision: SupervisionState::default(),
                origin: bro_core::Origin::Unknown,
                workflow_owned: workflow_owned_for_origin(bro_core::Origin::Unknown),
                project_id: None,
            }),
            notify: Arc::new(Notify::new()),
            child_id: Mutex::new(None),
            roster_events: None,
        });
        // Should return immediately without blocking
        wait_for_task(&task).await;
    }

    #[tokio::test]
    async fn test_wait_for_task_session_id_observes_non_pending_id() {
        let task = Arc::new(Task {
            inner: Mutex::new(TaskInner {
                id: "t-session".into(),
                provider: Provider::Brodex,
                session_id: "pending".into(),
                events: EventRing::new(),
                model: None,
                last_assistant_message: None,
                usage: None,
                cost_usd: None,
                num_turns: None,
                last_turn_input_tokens: None,
                context_window: None,
                stderr: String::new(),
                status: TaskStatus::Running,
                started_at: now_ms(),
                completed_at: None,
                exit_code: None,
                cwd: None,
                managed_worktree: None,
                bro_label: None,
                name: None,
                agent_label: None,
                report: None,
                interrupted: false,
                recoverable: false,
                transcript_location: None,
                transcript_cursor: None,
                live_cursor: 0,
                harness_ingest_seq: 0,
                last_delta_roster_emit_ms: 0,
                supervision: SupervisionState::default(),
                origin: bro_core::Origin::Unknown,
                workflow_owned: workflow_owned_for_origin(bro_core::Origin::Unknown),
                project_id: None,
            }),
            notify: Arc::new(Notify::new()),
            child_id: Mutex::new(None),
            roster_events: None,
        });
        let task_clone = task.clone();
        tokio::spawn(async move {
            task_clone.inner.lock().session_id = "real-session".into();
            task_clone.notify.notify_waiters();
        });

        let session_id = wait_for_task_session_id_with_timeout(&task, 2.0).await;
        assert_eq!(session_id.as_deref(), Some("real-session"));
    }

    #[tokio::test]
    async fn test_wait_for_task_session_id_returns_none_on_terminal_pending() {
        let task = Arc::new(Task {
            inner: Mutex::new(TaskInner {
                id: "t-session-terminal".into(),
                provider: Provider::Brodex,
                session_id: "pending".into(),
                events: EventRing::new(),
                model: None,
                last_assistant_message: None,
                usage: None,
                cost_usd: None,
                num_turns: None,
                last_turn_input_tokens: None,
                context_window: None,
                stderr: String::new(),
                status: TaskStatus::Completed,
                started_at: now_ms(),
                completed_at: Some(now_ms()),
                exit_code: Some(0),
                cwd: None,
                managed_worktree: None,
                bro_label: None,
                name: None,
                agent_label: None,
                report: None,
                interrupted: false,
                recoverable: false,
                transcript_location: None,
                transcript_cursor: None,
                live_cursor: 0,
                harness_ingest_seq: 0,
                last_delta_roster_emit_ms: 0,
                supervision: SupervisionState::default(),
                origin: bro_core::Origin::Unknown,
                workflow_owned: workflow_owned_for_origin(bro_core::Origin::Unknown),
                project_id: None,
            }),
            notify: Arc::new(Notify::new()),
            child_id: Mutex::new(None),
            roster_events: None,
        });

        let session_id = wait_for_task_session_id_with_timeout(&task, 2.0).await;
        assert_eq!(session_id, None);
    }

    #[tokio::test]
    async fn test_wait_for_task_notify_race() {
        // Simulate the race: task completes between status check and await
        let task = Arc::new(Task {
            inner: Mutex::new(TaskInner {
                id: "t2".into(),
                provider: Provider::Glm,
                session_id: "s1".into(),
                events: EventRing::new(),
                model: None,
                last_assistant_message: None,
                usage: None,
                cost_usd: None,
                num_turns: None,
                last_turn_input_tokens: None,
                context_window: None,
                stderr: String::new(),
                status: TaskStatus::Running,
                started_at: now_ms(),
                completed_at: None,
                exit_code: None,
                cwd: None,
                managed_worktree: None,
                bro_label: None,
                name: None,
                agent_label: None,
                report: None,
                interrupted: false,
                recoverable: false,
                transcript_location: None,
                transcript_cursor: None,
                live_cursor: 0,
                harness_ingest_seq: 0,
                last_delta_roster_emit_ms: 0,
                supervision: SupervisionState::default(),
                origin: bro_core::Origin::Unknown,
                workflow_owned: workflow_owned_for_origin(bro_core::Origin::Unknown),
                project_id: None,
            }),
            notify: Arc::new(Notify::new()),
            child_id: Mutex::new(None),
            roster_events: None,
        });

        let task_clone = task.clone();
        // Complete the task after a brief delay
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            {
                let mut inner = task_clone.inner.lock();
                inner.status = TaskStatus::Completed;
                inner.completed_at = Some(now_ms());
            }
            task_clone.notify.notify_waiters();
        });

        // This should not hang even if the notify fires during the gap
        let completed = wait_for_task_with_timeout(&task, Some(5.0)).await;
        assert!(completed, "wait_for_task should have completed");
    }

    #[tokio::test]
    async fn test_wait_for_task_timeout() {
        let task = Arc::new(Task {
            inner: Mutex::new(TaskInner {
                id: "t3".into(),
                provider: Provider::Glm,
                session_id: "s1".into(),
                events: EventRing::new(),
                model: None,
                last_assistant_message: None,
                usage: None,
                cost_usd: None,
                num_turns: None,
                last_turn_input_tokens: None,
                context_window: None,
                stderr: String::new(),
                status: TaskStatus::Running,
                started_at: now_ms(),
                completed_at: None,
                exit_code: None,
                cwd: None,
                managed_worktree: None,
                bro_label: None,
                name: None,
                agent_label: None,
                report: None,
                interrupted: false,
                recoverable: false,
                transcript_location: None,
                transcript_cursor: None,
                live_cursor: 0,
                harness_ingest_seq: 0,
                last_delta_roster_emit_ms: 0,
                supervision: SupervisionState::default(),
                origin: bro_core::Origin::Unknown,
                workflow_owned: workflow_owned_for_origin(bro_core::Origin::Unknown),
                project_id: None,
            }),
            notify: Arc::new(Notify::new()),
            child_id: Mutex::new(None),
            roster_events: None,
        });

        // Should timeout after 0.1s
        let completed = wait_for_task_with_timeout(&task, Some(0.1)).await;
        assert!(!completed, "should have timed out");
    }

    #[tokio::test]
    async fn test_wait_timeout_rechecks_terminal_state() {
        // gap-0301dc75: completion racing the timeout (or a missed
        // notify) must not be reported as a timeout. The task below
        // turns terminal WITHOUT firing its notify, so the waiter can
        // only learn of completion through the terminal re-check on
        // the timeout path.
        let task = Arc::new(Task {
            inner: Mutex::new(TaskInner {
                id: "t4".into(),
                provider: Provider::Glm,
                session_id: "s1".into(),
                events: EventRing::new(),
                model: None,
                last_assistant_message: None,
                usage: None,
                cost_usd: None,
                num_turns: None,
                last_turn_input_tokens: None,
                context_window: None,
                stderr: String::new(),
                status: TaskStatus::Running,
                started_at: now_ms(),
                completed_at: None,
                exit_code: None,
                cwd: None,
                managed_worktree: None,
                bro_label: None,
                name: None,
                agent_label: None,
                report: None,
                interrupted: false,
                recoverable: false,
                transcript_location: None,
                transcript_cursor: None,
                live_cursor: 0,
                harness_ingest_seq: 0,
                last_delta_roster_emit_ms: 0,
                supervision: SupervisionState::default(),
                origin: bro_core::Origin::Unknown,
                workflow_owned: workflow_owned_for_origin(bro_core::Origin::Unknown),
                project_id: None,
            }),
            notify: Arc::new(Notify::new()),
            child_id: Mutex::new(None),
            roster_events: None,
        });

        let task_clone = task.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            let mut inner = task_clone.inner.lock();
            inner.status = TaskStatus::Completed;
            inner.completed_at = Some(now_ms());
            // Deliberately NO notify_waiters() — force the timeout path.
        });

        let completed = wait_for_task_with_timeout(&task, Some(0.2)).await;
        assert!(completed, "terminal task must not be reported as timed out");
    }
}
