//! bro-script — §5b runtime: V8 in-process via deno_core as the NARF
//! script-execution container.
//!
//! Originally a §5b de-risking spike, this crate is now hardened into a real
//! standalone runtime. It still proves, in ISOLATION (zero `blackbox`
//! dependency), the daemon-safety properties and the async-bridge property that
//! gate embedding V8 inside the long-lived blackbox daemon — but the single
//! local echo capability has been replaced with exact-handle atom and refactor
//! `bro-capabilities` traits, the panic guard is now structural, and supervision
//! is configurable.
//!
//!   1. heap-bound OOM containment   — a runaway allocator is terminated, the
//!      process survives (no V8 `abort()`).
//!   2. runaway-loop kill            — `while(true){}` is interrupted cross-thread
//!      via the V8 `IsolateHandle` (either an external watchdog or the built-in
//!      execution-timeout supervisor).
//!   3. denied globals               — no `WebAssembly`/`Atomics`/
//!      `SharedArrayBuffer`/`console` ambient host access.
//!   4. async capability bridge      — a JS `await atoms.invoke(x)` suspends the
//!      script, hops from the `!Send` isolate thread into async Rust on a
//!      *different* executor, runs a real `bro-capabilities` trait method, and
//!      returns the result into JS.
//!   5. panic boundary               — a panic inside ANY op body is caught
//!      (`catch_unwind`) and surfaced as a JS error, never unwound across V8
//!      C++ frames (which would be UB / daemon abort). This is structural: every
//!      op routes through one guard helper.
//!
//! ## Thread <-> async pattern
//!
//! `deno_core::JsRuntime` is `!Send` and executes JS synchronously, so it owns a
//! **dedicated OS thread**. On that thread a *current-thread* tokio runtime +
//! `LocalSet` drives the V8 event loop. The outside world talks to it over
//! channels:
//!
//! * inbound jobs (`Job`) arrive on a tokio mpsc; each job carries a `oneshot`
//!   reply sender, so async callers `await` a normal future.
//! * the only handle shared *outward* is the cross-thread `v8::IsolateHandle`
//!   (`Send + Sync`), used by the supervisor watchdog and the near-heap-limit
//!   callback to terminate execution from another thread.
//!
//! ## Capability bridge
//!
//! An op invoked from JS does **not** run the capability inline on the V8 thread.
//! It forwards a typed [`CapRequest`] over an mpsc channel to a *capability
//! executor* task running on the **outer** multi-thread tokio runtime, then
//! `await`s a `oneshot` reply. While the op future is pending, `run_event_loop`
//! keeps polling it; the cross-runtime oneshot waker resolves it when async Rust
//! finishes. This is the genuine `!Send`-isolate ↔ async-capability round trip.
//! The real exact-handle capability traits ([`AtomCapability`],
//! [`RefactorCapability`]) are injected as `Arc<dyn Trait>` exactly as the
//! daemon will later install them.

use std::cell::RefCell;
use std::collections::HashMap;
use std::future::Future;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::pin::Pin;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::task::{Context, Poll};
use std::thread::JoinHandle;
use std::time::Duration;

use anyhow::{anyhow, Result};
use deno_core::v8;
use deno_core::{op2, OpState, PollEventLoopOptions, RuntimeOptions};
use deno_error::JsErrorBox;
use serde::{Deserialize, Serialize};
use tokio::sync::{mpsc, oneshot};

pub use bro_capabilities::{
    AtomCapability, AtomInvocation, AtomOutput, CapabilityResult, KvCapability, KvEntryInfo, KvGet,
    RefactorCapability, RefactorPlanHandle, RefactorRequest, ToolCallOutput, ToolCapability,
    ToolInvocation,
};
use bro_core::BroError;

/// deno_core version this crate pins. Reported for the §5 cost accounting (this
/// dep later lands in blackboxd).
pub const DENO_CORE_VERSION: &str = "0.403.0";

/// Default per-isolate hard heap ceiling (bytes). Generous enough that ordinary
/// scripts never trip it; the OOM-containment test passes a deliberately small
/// limit instead.
pub const DEFAULT_HEAP_LIMIT_BYTES: usize = 256 * 1024 * 1024;

/// Default wall-clock ceiling for a single `execute` call. Generous enough that
/// ordinary scripts (including a few capability round trips) never trip it; the
/// timeout test passes a deliberately short value instead.
pub const DEFAULT_EXECUTION_TIMEOUT: Duration = Duration::from_secs(30);

// ---------------------------------------------------------------------------
// Capability injection
// ---------------------------------------------------------------------------

/// The exact-handle `bro-capabilities` trait objects the runtime exposes to JS.
/// Injected by the caller (the daemon, later; tests, now) so the runtime never
/// hard-codes an implementation.
#[derive(Clone)]
pub struct Capabilities {
    pub atoms: Arc<dyn AtomCapability>,
    pub refactor: Arc<dyn RefactorCapability>,
    /// The generic host built-in tool seam (§5.1). `None` for the standalone
    /// runtime / tests that exercise only atom+refactor composition — the
    /// `fs.*`/`shell.*`/`search.*`/`git.*`/`web.*` in-box bindings then fail
    /// closed (§4.5 fail-safe by absence).
    pub tools: Option<Arc<dyn ToolCapability>>,
    /// Durable session KV. Exact in-box deref only; enumeration stays
    /// model-facing in bro-harness tools.
    pub kv: Arc<dyn KvCapability>,
}

/// Configurable, default-ON supervision. The heap limit bounds isolate memory;
/// the execution timeout bounds wall-clock per `execute` and auto-terminates a
/// runaway script via the cross-thread `IsolateHandle` (no caller-supplied
/// watchdog needed). `execution_timeout = None` disables only the timer (the
/// heap guard always stays on).
#[derive(Clone, Debug)]
pub struct SupervisionPolicy {
    pub heap_limit_bytes: usize,
    pub execution_timeout: Option<Duration>,
}

impl Default for SupervisionPolicy {
    fn default() -> Self {
        Self {
            heap_limit_bytes: DEFAULT_HEAP_LIMIT_BYTES,
            execution_timeout: Some(DEFAULT_EXECUTION_TIMEOUT),
        }
    }
}

// ---------------------------------------------------------------------------
// Prepared script storage
// ---------------------------------------------------------------------------

/// Minimal per-runtime store for model-reviewed prepared scripts. This is not
/// the retired data-ref substrate; it only backs `narf_prepare` -> `narf_run`
/// identifier handles until the later KV phase provides the durable home.
#[derive(Default)]
struct PreparedScripts {
    next_id: u64,
    scripts: HashMap<String, String>,
}

impl PreparedScripts {
    fn put(&mut self, source: String) -> String {
        let id = self.next_id;
        self.next_id += 1;
        let handle = format!("narf-script:{id}");
        self.scripts.insert(handle.clone(), source);
        handle
    }

    fn get(&self, handle: &str) -> Result<String, String> {
        self.scripts
            .get(handle)
            .cloned()
            .ok_or_else(|| format!("unknown prepared script handle: {handle}"))
    }
}

type PreparedScriptsCell = Rc<RefCell<PreparedScripts>>;

// ---------------------------------------------------------------------------
// Session-local authoring state + prepared-script trace (§4 / §6)
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
pub struct SessionHelper {
    pub source: String,
    pub exports: Vec<String>,
}

/// Host-side per-session frame. Helpers are source, not V8 globals: imports
/// re-inject source into the current cell/prepared artifact.
#[derive(Clone, Debug, Default)]
pub struct SessionState {
    pub helpers: HashMap<String, SessionHelper>,
    pub import_aliases: HashMap<String, String>,
}

type SessionStateCell = Rc<RefCell<SessionState>>;
type TraceStateCell = Rc<RefCell<Vec<TraceEntry>>>;

#[derive(Clone, Debug, Serialize)]
pub struct TraceEntry {
    #[serde(rename = "ref")]
    pub ref_handle: String,
    pub sequence: usize,
}

#[derive(Clone, Debug, Serialize)]
pub struct PrepareDiagnostic {
    pub kind: String,
    pub message: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct PrepareResponse {
    #[serde(rename = "ref", skip_serializing_if = "Option::is_none")]
    pub ref_handle: Option<String>,
    pub status: String,
    pub diagnostics: Vec<PrepareDiagnostic>,
    /// The rendered, import-assembled script — returned to the MODEL's context so
    /// it sees exactly what `narf_run` will execute (the §0.1 review step). Only
    /// present on a `ready` response.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
}

#[derive(Deserialize)]
struct SessionDefineInput {
    name: String,
    source: String,
    exports: Vec<String>,
}

#[derive(Deserialize)]
struct PrepareInput {
    source: String,
    #[serde(default)]
    imports: Option<ImportSpec>,
}

#[derive(Deserialize)]
struct KvSetInput {
    name: String,
    value_json: serde_json::Value,
    #[serde(default)]
    tags: Option<serde_json::Value>,
}

#[derive(Deserialize)]
struct KvGetInput {
    name: String,
    #[serde(default)]
    max_bytes: Option<usize>,
}

#[derive(Deserialize)]
struct KvNameInput {
    name: String,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum ImportSpec {
    List(Vec<String>),
    Map(HashMap<String, String>),
}

fn diagnostic(kind: &str, message: impl Into<String>) -> PrepareDiagnostic {
    PrepareDiagnostic {
        kind: kind.to_string(),
        message: message.into(),
    }
}

fn blocked(kind: &str, message: impl Into<String>) -> PrepareResponse {
    PrepareResponse {
        ref_handle: None,
        status: "blocked".to_string(),
        diagnostics: vec![diagnostic(kind, message)],
        source: None,
    }
}

fn ready(handle: String, source: String) -> PrepareResponse {
    PrepareResponse {
        ref_handle: Some(handle),
        status: "ready".to_string(),
        diagnostics: vec![],
        source: Some(source),
    }
}

fn is_js_identifier(s: &str) -> bool {
    let mut chars = s.chars();
    match chars.next() {
        Some(c) if c == '_' || c == '$' || c.is_ascii_alphabetic() => {}
        _ => return false,
    }
    chars.all(|c| c == '_' || c == '$' || c.is_ascii_alphanumeric())
}

fn strip_export_keywords(source: &str) -> String {
    source
        .replace("export async function ", "async function ")
        .replace("export function ", "function ")
        .replace("export const ", "const ")
        .replace("export let ", "let ")
        .replace("export var ", "var ")
}

fn helper_expression(helper: &SessionHelper) -> Result<String, String> {
    if helper.exports.is_empty() {
        return Err("session helper must declare at least one export".to_string());
    }
    for export in &helper.exports {
        if !is_js_identifier(export) {
            return Err(format!("invalid helper export identifier: {export}"));
        }
    }
    let source = strip_export_keywords(&helper.source);
    Ok(format!(
        "(() => {{\n{source}\nreturn {{ {} }};\n}})()",
        helper.exports.join(", ")
    ))
}

fn resolve_imports(session: &SessionState, imports: Option<ImportSpec>) -> Result<String, String> {
    let imports = match imports {
        Some(ImportSpec::List(items)) => items
            .into_iter()
            .map(|alias| (alias.clone(), alias))
            .collect::<Vec<_>>(),
        Some(ImportSpec::Map(map)) => map.into_iter().collect::<Vec<_>>(),
        None => vec![],
    };

    let mut rendered = String::new();
    for (alias, target) in imports {
        if !is_js_identifier(&alias) {
            return Err(format!("invalid import alias identifier: {alias}"));
        }
        let helper_name = session
            .import_aliases
            .get(&target)
            .or_else(|| session.helpers.contains_key(&target).then_some(&target))
            .ok_or_else(|| format!("unknown import alias: {target}"))?;
        let helper = session
            .helpers
            .get(helper_name)
            .ok_or_else(|| format!("unknown session helper: {helper_name}"))?;
        rendered.push_str("const ");
        rendered.push_str(&alias);
        rendered.push_str(" = ");
        rendered.push_str(&helper_expression(helper)?);
        rendered.push_str(";\n");
    }
    Ok(rendered)
}

fn render_prepare(session: &SessionState, input: PrepareInput) -> Result<String, String> {
    let mut assembled = resolve_imports(session, input.imports)?;
    assembled.push_str(&input.source);
    Ok(assembled)
}

// ---------------------------------------------------------------------------
// Capability request channel (V8-thread op -> outer-runtime executor)
// ---------------------------------------------------------------------------

/// A capability call handed off from a V8-thread op to the async executor on the
/// outer runtime. Each variant carries the real `bro-capabilities` typed input
/// and a typed `oneshot` reply, generalizing the spike's single echo request.
enum CapRequest {
    AtomInvoke {
        input: AtomInvocation,
        reply: oneshot::Sender<CapabilityResult<AtomOutput>>,
    },
    RefactorPlan {
        input: RefactorRequest,
        reply: oneshot::Sender<CapabilityResult<RefactorPlanHandle>>,
    },
    RefactorMaterialize {
        id: String,
        reply: oneshot::Sender<CapabilityResult<serde_json::Value>>,
    },
    ToolInvoke {
        input: ToolInvocation,
        reply: oneshot::Sender<CapabilityResult<ToolCallOutput>>,
    },
    KvSet {
        name: String,
        value_json: serde_json::Value,
        tags: Option<serde_json::Value>,
        reply: oneshot::Sender<CapabilityResult<KvEntryInfo>>,
    },
    KvGet {
        name: String,
        max_bytes: Option<usize>,
        reply: oneshot::Sender<CapabilityResult<KvGet>>,
    },
    KvPeek {
        name: String,
        reply: oneshot::Sender<CapabilityResult<KvEntryInfo>>,
    },
    KvDelete {
        name: String,
        reply: oneshot::Sender<CapabilityResult<bool>>,
    },
}

type CapTx = mpsc::UnboundedSender<CapRequest>;

/// Run a capability future on the outer runtime with panic isolation. A
/// capability that panics must not kill the executor or abort the process: the
/// inner `tokio::spawn` converts a panic into a clean [`BroError`] that surfaces
/// as a catchable JS error. This is the outer-runtime complement to the
/// V8-thread structural guard ([`guard_async`]).
async fn run_caught<T, F>(fut: F) -> CapabilityResult<T>
where
    F: Future<Output = CapabilityResult<T>> + Send + 'static,
    T: Send + 'static,
{
    match tokio::spawn(fut).await {
        Ok(result) => result,
        Err(_join_err) => Err(BroError::new(
            "capability_panic",
            "capability panicked during execution; contained on the executor",
        )),
    }
}

/// Dispatch one capability request to the matching injected trait impl. Runs on
/// the outer multi-thread runtime; each call is panic-isolated via [`run_caught`].
async fn dispatch_cap(caps: Capabilities, req: CapRequest) {
    match req {
        CapRequest::AtomInvoke { input, reply } => {
            let cap = caps.atoms.clone();
            let _ = reply.send(run_caught(async move { cap.invoke_atom(input).await }).await);
        }
        CapRequest::RefactorPlan { input, reply } => {
            let cap = caps.refactor.clone();
            let _ = reply.send(run_caught(async move { cap.plan_refactor(input).await }).await);
        }
        CapRequest::RefactorMaterialize { id, reply } => {
            let cap = caps.refactor.clone();
            let _ = reply.send(run_caught(async move { cap.materialize_plan(id).await }).await);
        }
        CapRequest::ToolInvoke { input, reply } => match caps.tools.clone() {
            Some(cap) => {
                let _ = reply.send(run_caught(async move { cap.call_tool(input).await }).await);
            }
            None => {
                let _ = reply.send(Err(BroError::new(
                    "host_tools_unavailable",
                    "host built-in tools are not installed in this runtime (fail-closed)",
                )));
            }
        },
        CapRequest::KvSet {
            name,
            value_json,
            tags,
            reply,
        } => {
            let cap = caps.kv.clone();
            let _ =
                reply.send(run_caught(async move { cap.set(name, value_json, tags).await }).await);
        }
        CapRequest::KvGet {
            name,
            max_bytes,
            reply,
        } => {
            let cap = caps.kv.clone();
            let _ = reply.send(run_caught(async move { cap.get(name, max_bytes).await }).await);
        }
        CapRequest::KvPeek { name, reply } => {
            let cap = caps.kv.clone();
            let _ = reply.send(run_caught(async move { cap.peek(name).await }).await);
        }
        CapRequest::KvDelete { name, reply } => {
            let cap = caps.kv.clone();
            let _ = reply.send(run_caught(async move { cap.delete(name).await }).await);
        }
    }
}

// ---------------------------------------------------------------------------
// Structural panic guard (criterion #5)
// ---------------------------------------------------------------------------
//
// deno_core 0.403 has ZERO op-dispatch `catch_unwind`. Under the workspace's
// `panic = "unwind"`, an unguarded op panic unwinds across V8's C++ frames =
// UB / daemon abort. Every op routes its body through ONE of these two guards so
// no op work executes outside a `catch_unwind` boundary.

/// Guard a synchronous op body. A panic becomes a catchable JS error.
fn guard_op<T>(body: impl FnOnce() -> Result<T, JsErrorBox>) -> Result<T, JsErrorBox> {
    match catch_unwind(AssertUnwindSafe(body)) {
        Ok(result) => result,
        Err(_panic) => Err(JsErrorBox::generic(
            "op panicked; contained at the boundary (catch_unwind)",
        )),
    }
}

/// Future wrapper that guards EVERY poll of an async op body with `catch_unwind`.
/// A panic on any poll — including resumption code that runs on the V8 thread
/// after an `await` — is caught and the future resolves to a catchable JS error.
/// This is the async analogue of [`guard_op`]; all capability ops route through
/// it via [`guard_async`].
struct Guarded<F> {
    inner: F,
}

impl<F, T> Future for Guarded<F>
where
    F: Future<Output = Result<T, JsErrorBox>>,
{
    type Output = Result<T, JsErrorBox>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        // SAFETY: standard pin projection — `inner` is never moved out, and the
        // wrapper adds no `Unpin`-violating fields.
        let inner = unsafe { self.map_unchecked_mut(|s| &mut s.inner) };
        match catch_unwind(AssertUnwindSafe(|| inner.poll(cx))) {
            Ok(poll) => poll,
            Err(_panic) => Poll::Ready(Err(JsErrorBox::generic(
                "op panicked; contained at the boundary (catch_unwind)",
            ))),
        }
    }
}

/// Wrap an async op body in the structural panic guard.
fn guard_async<F, T>(inner: F) -> Guarded<F>
where
    F: Future<Output = Result<T, JsErrorBox>>,
{
    Guarded { inner }
}

// ---------------------------------------------------------------------------
// Capability ops + extension
// ---------------------------------------------------------------------------

/// Forward a typed capability request to the outer-runtime executor and await its
/// typed reply. The whole body
/// runs under [`guard_async`] in the caller op, so the synchronous channel work
/// (and any panic in it) is contained on the V8 thread.
async fn bridge<O>(
    tx: CapTx,
    make: impl FnOnce(oneshot::Sender<CapabilityResult<O>>) -> CapRequest,
) -> Result<O, JsErrorBox> {
    let (reply_tx, reply_rx) = oneshot::channel();
    tx.send(make(reply_tx))
        .map_err(|_| JsErrorBox::generic("capability executor is gone"))?;
    reply_rx
        .await
        .map_err(|_| JsErrorBox::generic("capability executor dropped the reply"))?
        .map_err(|e| JsErrorBox::generic(format!("{}: {}", e.code, e.message)))
}

fn serialize_value(value: impl Serialize) -> Result<String, JsErrorBox> {
    serde_json::to_string(&value)
        .map_err(|e| JsErrorBox::generic(format!("failed to serialize output: {e}")))
}

fn serialize_tool_output(out: ToolCallOutput) -> Result<String, JsErrorBox> {
    if out.content_type == "application/json" {
        let value: serde_json::Value = serde_json::from_str(&out.content)
            .map_err(|e| JsErrorBox::generic(format!("tool returned invalid JSON: {e}")))?;
        serialize_value(value)
    } else {
        serialize_value(out.content)
    }
}

#[op2]
#[string]
fn op_encode_yaml(#[string] value_json: String) -> Result<String, JsErrorBox> {
    guard_op(|| {
        let value: serde_json::Value = serde_json::from_str(&value_json)
            .map_err(|e| JsErrorBox::generic(format!("invalid YAML encode input: {e}")))?;
        serde_norway::to_string(&value)
            .map_err(|e| JsErrorBox::generic(format!("failed to encode YAML: {e}")))
    })
}

/// Forward a host built-in tool invocation to the outer-runtime executor and
/// await the typed reply. Unlike [`bridge`], the reply is inspected by the op
/// (an `is_error` result becomes a catchable JS exception), so this returns the
/// typed [`ToolCallOutput`] rather than a pre-serialized string.
async fn bridge_tool(tx: CapTx, input: ToolInvocation) -> Result<ToolCallOutput, JsErrorBox> {
    let (reply_tx, reply_rx) = oneshot::channel();
    tx.send(CapRequest::ToolInvoke {
        input,
        reply: reply_tx,
    })
    .map_err(|_| JsErrorBox::generic("capability executor is gone"))?;
    reply_rx
        .await
        .map_err(|_| JsErrorBox::generic("capability executor dropped the reply"))?
        .map_err(|e| JsErrorBox::generic(format!("{}: {}", e.code, e.message)))
}

/// Pull the capability executor channel out of `OpState` in one short
/// borrow, BEFORE any `await`, so no `OpState` borrow is held across a suspend.
fn cap_tx(state: &Rc<RefCell<OpState>>) -> CapTx {
    state.borrow().borrow::<CapTx>().clone()
}

/// `await atoms.invoke(handle, input)` — JSON in, JSON out, over the async bridge.
#[op2(async(lazy), fast)]
#[string]
async fn op_atom_invoke(
    state: Rc<RefCell<OpState>>,
    #[string] input_json: String,
) -> Result<String, JsErrorBox> {
    guard_async(async move {
        let tx = cap_tx(&state);
        let input: AtomInvocation = serde_json::from_str(&input_json)
            .map_err(|e| JsErrorBox::generic(format!("invalid atoms.invoke input: {e}")))?;
        let output = bridge(tx, move |reply| CapRequest::AtomInvoke { input, reply }).await?;
        serialize_value(output.output_json)
    })
    .await
}

/// `await refactor.plan(args)` — JSON in, JSON out, over the async bridge.
#[op2(async(lazy), fast)]
#[string]
async fn op_refactor_plan(
    state: Rc<RefCell<OpState>>,
    #[string] input_json: String,
) -> Result<String, JsErrorBox> {
    guard_async(async move {
        let tx = cap_tx(&state);
        let input: RefactorRequest = serde_json::from_str(&input_json)
            .map_err(|e| JsErrorBox::generic(format!("invalid refactor.plan input: {e}")))?;
        let handle = bridge(tx, move |reply| CapRequest::RefactorPlan { input, reply }).await?;
        serialize_value(handle)
    })
    .await
}

/// `await refactor.materialize(handle)` — handle id in, JSON plan out.
#[op2(async(lazy), fast)]
#[string]
async fn op_refactor_materialize(
    state: Rc<RefCell<OpState>>,
    #[string] id: String,
) -> Result<String, JsErrorBox> {
    guard_async(async move {
        let tx = cap_tx(&state);
        let plan = bridge(tx, move |reply| CapRequest::RefactorMaterialize {
            id,
            reply,
        })
        .await?;
        serialize_value(plan)
    })
    .await
}

/// `op_tool_invoke(name, input)` — the generic host built-in seam
/// (`narf-tool-placement.md` §5). Bridges to the injected [`ToolCapability`],
/// and returns the tool value directly into the cell: JSON content becomes a JS
/// value, non-JSON content becomes a JS string. A tool that reports `is_error`
/// surfaces as a catchable JS exception (so a cell can `try/catch`); an unknown / denied /
/// uninstalled tool fails closed via the capability error.
#[op2(async(lazy), fast)]
#[string]
async fn op_tool_invoke(
    state: Rc<RefCell<OpState>>,
    #[string] input_json: String,
) -> Result<String, JsErrorBox> {
    guard_async(async move {
        let tx = cap_tx(&state);
        let input: ToolInvocation = serde_json::from_str(&input_json)
            .map_err(|e| JsErrorBox::generic(format!("invalid host tool invocation: {e}")))?;
        let name = input.name.clone();
        let out = bridge_tool(tx, input).await?;
        if out.is_error {
            return Err(JsErrorBox::generic(format!("{name}: {}", out.content)));
        }
        serialize_tool_output(out)
    })
    .await
}

/// `op_tool_invoke_inline(name, input)` — value-out variant of the seam for
/// **control-shaped** results (`narf-tool-placement.md` §3.1 — the small
/// `Promise`/control lane that is by-value, not by-reference). Returns the tool's
/// content string directly (JS `JSON.parse`s it) instead of ref-wrapping it, so a
/// promise handle (`{promise_id}`), a `promise_status`/`list`/`cancel` snapshot,
/// or a `shell.run(mode:'promise')` ticket stays usable in the cell rather than
/// hiding behind an extra wrapper.
/// A tool `is_error` still throws; unknown/denied still fails closed.
#[op2(async(lazy), fast)]
#[string]
async fn op_tool_invoke_inline(
    state: Rc<RefCell<OpState>>,
    #[string] input_json: String,
) -> Result<String, JsErrorBox> {
    guard_async(async move {
        let tx = cap_tx(&state);
        let input: ToolInvocation = serde_json::from_str(&input_json)
            .map_err(|e| JsErrorBox::generic(format!("invalid host tool invocation: {e}")))?;
        let name = input.name.clone();
        let out = bridge_tool(tx, input).await?;
        if out.is_error {
            return Err(JsErrorBox::generic(format!("{name}: {}", out.content)));
        }
        Ok(out.content)
    })
    .await
}

#[op2(async(lazy), fast)]
#[string]
async fn op_kv_set(
    state: Rc<RefCell<OpState>>,
    #[string] input_json: String,
) -> Result<String, JsErrorBox> {
    guard_async(async move {
        let tx = cap_tx(&state);
        let input: KvSetInput = serde_json::from_str(&input_json)
            .map_err(|e| JsErrorBox::generic(format!("invalid narf.kv.set input: {e}")))?;
        let info = bridge(tx, move |reply| CapRequest::KvSet {
            name: input.name,
            value_json: input.value_json,
            tags: input.tags,
            reply,
        })
        .await?;
        serialize_value(info)
    })
    .await
}

#[op2(async(lazy), fast)]
#[string]
async fn op_kv_get(
    state: Rc<RefCell<OpState>>,
    #[string] input_json: String,
) -> Result<String, JsErrorBox> {
    guard_async(async move {
        let tx = cap_tx(&state);
        let input: KvGetInput = serde_json::from_str(&input_json)
            .map_err(|e| JsErrorBox::generic(format!("invalid narf.kv.get input: {e}")))?;
        let out = bridge(tx, move |reply| CapRequest::KvGet {
            name: input.name,
            max_bytes: input.max_bytes,
            reply,
        })
        .await?;
        serialize_value(out.value_json)
    })
    .await
}

#[op2(async(lazy), fast)]
#[string]
async fn op_kv_peek(
    state: Rc<RefCell<OpState>>,
    #[string] input_json: String,
) -> Result<String, JsErrorBox> {
    guard_async(async move {
        let tx = cap_tx(&state);
        let input: KvNameInput = serde_json::from_str(&input_json)
            .map_err(|e| JsErrorBox::generic(format!("invalid narf.kv.peek input: {e}")))?;
        let info = bridge(tx, move |reply| CapRequest::KvPeek {
            name: input.name,
            reply,
        })
        .await?;
        serialize_value(info)
    })
    .await
}

#[op2(async(lazy), fast)]
#[string]
async fn op_kv_delete(
    state: Rc<RefCell<OpState>>,
    #[string] input_json: String,
) -> Result<String, JsErrorBox> {
    guard_async(async move {
        let tx = cap_tx(&state);
        let input: KvNameInput = serde_json::from_str(&input_json)
            .map_err(|e| JsErrorBox::generic(format!("invalid narf.kv.delete input: {e}")))?;
        let deleted = bridge(tx, move |reply| CapRequest::KvDelete {
            name: input.name,
            reply,
        })
        .await?;
        serialize_value(deleted)
    })
    .await
}

/// Store a helper in the host-side session frame and create the default import
/// alias `name -> name`. Shared host-side logic behind the **model-facing**
/// `narf_define` tool (via `Job::Define`) — NOT an in-box op: authoring a session
/// helper is a control, so it lives outside the box (the box-edge invariant,
/// `narf-capability-library.md` §0.1). The in-box side keeps only
/// `narf.session.import` (recall by exact name). Validates the name and export
/// identifiers; helper *syntax* is validated on import/prepare.
fn define_session_helper(
    session: &SessionStateCell,
    input: SessionDefineInput,
) -> Result<(), String> {
    if !is_js_identifier(&input.name) {
        return Err(format!("invalid helper name identifier: {}", input.name));
    }
    let helper = SessionHelper {
        source: input.source,
        exports: input.exports,
    };
    helper_expression(&helper)?;
    let mut session = session.borrow_mut();
    session
        .import_aliases
        .insert(input.name.clone(), input.name.clone());
    session.helpers.insert(input.name, helper);
    Ok(())
}

/// `narf.session.import(name)` support: return a source expression that injects
/// the helper into the current cell. The expression is evaluated by the JS shim.
#[op2]
#[string]
fn op_session_import(state: &mut OpState, #[string] name: String) -> Result<String, JsErrorBox> {
    guard_op(|| {
        let session = state.borrow::<SessionStateCell>().clone();
        let session = session.borrow();
        let helper_name = session
            .import_aliases
            .get(&name)
            .or_else(|| session.helpers.contains_key(&name).then_some(&name))
            .ok_or_else(|| JsErrorBox::generic(format!("unknown import alias: {name}")))?;
        let helper = session
            .helpers
            .get(helper_name)
            .ok_or_else(|| JsErrorBox::generic(format!("unknown session helper: {helper_name}")))?;
        helper_expression(helper).map_err(JsErrorBox::generic)
    })
}

/// Boundary-proof op: a synchronous panic, caught by [`guard_op`]. Proves the
/// structural guard prevents a V8-frame unwind (criterion #5, sync path).
#[op2(fast)]
fn op_panic_guarded() -> Result<(), JsErrorBox> {
    guard_op(|| panic!("intentional panic inside op handler"))
}

/// Boundary-proof op: a panic raised AFTER an `await` point, caught by
/// [`guard_async`]. This exercises the exact guard every capability op uses —
/// the dangerous case where resumption code panics on the V8 thread mid-poll.
#[op2(async(lazy), fast)]
async fn op_panic_guarded_async() -> Result<(), JsErrorBox> {
    guard_async(async {
        tokio::task::yield_now().await;
        panic!("intentional async panic inside op handler");
    })
    .await
}

deno_core::extension!(
    bro_script_ext,
    ops = [
        op_atom_invoke,
        op_refactor_plan,
        op_refactor_materialize,
        op_tool_invoke,
        op_tool_invoke_inline,
        op_kv_set,
        op_kv_get,
        op_kv_peek,
        op_kv_delete,
        op_encode_yaml,
        op_session_import,
        op_panic_guarded,
        op_panic_guarded_async,
    ],
    options = { tx: CapTx },
    state = |state, options| {
        state.put::<CapTx>(options.tx);
        state.put::<PreparedScriptsCell>(Rc::new(RefCell::new(PreparedScripts::default())));
        state.put::<SessionStateCell>(Rc::new(RefCell::new(SessionState::default())));
        state.put::<TraceStateCell>(Rc::new(RefCell::new(Vec::new())));
    },
);

// ---------------------------------------------------------------------------
// ScriptRuntime
// ---------------------------------------------------------------------------

enum Job {
    Execute {
        body: String,
        reply: oneshot::Sender<Result<String>>,
    },
    Prepare {
        input_json: serde_json::Value,
        reply: oneshot::Sender<Result<PrepareResponse>>,
    },
    Run {
        handle: String,
        reply: oneshot::Sender<Result<String>>,
    },
    Define {
        input_json: serde_json::Value,
        reply: oneshot::Sender<Result<()>>,
    },
    TraceLen {
        reply: oneshot::Sender<usize>,
    },
    Shutdown,
}

/// Owns a deno_core `JsRuntime` on a dedicated OS thread and exposes an async,
/// channel-based API callable from tokio code. Supervision (heap + timeout) is
/// configured via [`SupervisionPolicy`] and is default-ON.
pub struct ScriptRuntime {
    job_tx: mpsc::UnboundedSender<Job>,
    isolate_handle: v8::IsolateHandle,
    heap_oom: Arc<AtomicBool>,
    execution_timeout: Option<Duration>,
    thread: Option<JoinHandle<()>>,
}

/// Bootstrap script run once per isolate: deny ambient host globals and install
/// the capability shims that front the capability ops. `delete` is per-isolate
/// (unlike process-wide V8 flags), so each isolate hardens itself. `Deno.core`
/// is kept — it is the op transport, not an ambient host capability. Each shim
/// is JSON-in/JSON-out and parses the op's JSON reply back into a JS value.
const BOOTSTRAP: &str = r#"
    delete globalThis.WebAssembly;
    delete globalThis.SharedArrayBuffer;
    delete globalThis.Atomics;
    delete globalThis.console;
    globalThis.atoms = {
        invoke: async (handle, input) =>
            JSON.parse(await Deno.core.ops.op_atom_invoke(
                JSON.stringify({ atom: handle, input_json: input ?? null }))),
    };
    globalThis.refactor = {
        plan: async (args) =>
            JSON.parse(await Deno.core.ops.op_refactor_plan(JSON.stringify(args))),
        materialize: async (handle) =>
            JSON.parse(await Deno.core.ops.op_refactor_materialize(
                typeof handle === 'string' ? handle : handle.id)),
    };
    // §5 host built-in tool parity: each binding rides the single op_tool_invoke
    // seam, returns a value directly into the cell, and throws on tool error.
    // Selection-from-result is interpretive and must round-trip the model —
    // these are for mechanical complete-set composition, not enumerate-then-pick.
    const hostTool = async (name, input) =>
        JSON.parse(await Deno.core.ops.op_tool_invoke(
            JSON.stringify({ name, input_json: input ?? {} })));
    // Value-out variant for control-shaped results (promise handles/snapshots):
    // small metadata stays usable in the cell rather than hiding behind a ref.
    const hostToolInline = async (name, input) =>
        JSON.parse(await Deno.core.ops.op_tool_invoke_inline(
            JSON.stringify({ name, input_json: input ?? {} })));
    globalThis.fs = {
        read:      (a) => hostTool('file_read',  typeof a === 'string' ? { file_path: a } : a),
        smartRead: (a) => hostTool('smart_read', typeof a === 'string' ? { file_path: a } : a),
        list:      (a) => hostTool('list_dir',   typeof a === 'string' ? { path: a } : (a ?? {})),
        write:     (a, content) => hostTool('file_write',
                       typeof a === 'string' ? { file_path: a, content } : a),
        edit:      (a) => hostTool('file_edit', a),
    };
    globalThis.search = {
        content: (a) => hostTool('content_search', typeof a === 'string' ? { pattern: a } : a),
        glob:    (a) => hostTool('glob',           typeof a === 'string' ? { pattern: a } : a),
    };
    globalThis.git = {
        status: (a) => hostTool('git_status', a ?? {}),
        log:    (a) => hostTool('git_log',    a ?? {}),
        diff:   (a) => hostTool('git_diff',   a ?? {}),
        show:   (a) => hostTool('git_show',   typeof a === 'string' ? { rev: a } : (a ?? {})),
        commit: (a, paths) => hostTool('git_commit',
                    typeof a === 'string' ? { message: a, paths: paths ?? [] } : a),
    };
    globalThis.shell = {
        run:  (a) => {
            const input = typeof a === 'string' ? { command: a } : a;
            return (input && input.mode === 'promise')
                ? hostToolInline('shell_run', input)
                : hostTool('shell_run', input);
        },
        poll: (a) => hostTool('shell_poll', a),
        kill: (a) => hostToolInline('shell_kill', a),
        list: (a) => hostToolInline('shell_list', a ?? {}),
    };
    globalThis.web = {
        fetch: (a) => hostTool('web_fetch', typeof a === 'string' ? { url: a } : a),
    };
    // §5 in-box promise primitive (narf-tool-placement.md §2/§5): join a cell's
    // OWN same-dispatch promises over the shared PromiseStore. Handles are the
    // by-value {promise_id} tickets producers return (e.g. shell.run mode:promise).
    const promiseId = (h) =>
        (typeof h === 'string' ? h : (h && (h.promise_id ?? (h.detail && h.detail.promise_id))));
    globalThis.narf = {
        kv: {
            set: async (name, value, options) =>
                JSON.parse(await Deno.core.ops.op_kv_set(
                    JSON.stringify({
                        name,
                        value_json: value ?? null,
                        tags: options && options.tags !== undefined ? options.tags : null,
                    }))),
            get: async (name, maxBytes) =>
                JSON.parse(await Deno.core.ops.op_kv_get(
                    JSON.stringify({
                        name,
                        max_bytes: (maxBytes === undefined || maxBytes === null) ? null : maxBytes,
                    }))),
            peek: async (name) =>
                JSON.parse(await Deno.core.ops.op_kv_peek(JSON.stringify({ name }))),
            delete: async (name) =>
                JSON.parse(await Deno.core.ops.op_kv_delete(JSON.stringify({ name }))),
        },
        session: {
            // §2.2 in-box EXCEPTION: recall a cached helper by exact name — a
            // dereference (keeps the helper source host-side, out of context),
            // NOT a control. `define`/`prepare`/`run` are MODEL-FACING tools
            // (narf_define/narf_prepare/narf_run): the box must not hold the
            // controls that open or author it (the box-edge invariant, §0.1).
            import: (name) => {
                const expr = Deno.core.ops.op_session_import(name);
                return (0, eval)(expr);
            },
        },
    };
    const yaml = (value) =>
        Deno.core.ops.op_encode_yaml(JSON.stringify(value ?? null));
    const yamlForFrontmatter = (value) => {
        const y = yaml(value);
        return y.endsWith('\n') ? y : y + '\n';
    };
    const escapeTableCell = (value) =>
        String(value === undefined || value === null
            ? ''
            : (typeof value === 'string' ? value : JSON.stringify(value)))
            .replace(/\|/g, '\\|')
            .replace(/\r?\n/g, '<br>');
    globalThis.narf.encode = {
        yaml,
        frontmatter: (meta, body) =>
            '---\n' + yamlForFrontmatter(meta ?? {}) + '---\n\n' + (body ?? ''),
        mdTable: (rows, columns) => {
            const data = Array.isArray(rows) ? rows : [];
            const cols = Array.isArray(columns) && columns.length
                ? columns
                : Array.from(data.reduce((set, row) => {
                    if (row && typeof row === 'object' && !Array.isArray(row)) {
                        Object.keys(row).forEach((key) => set.add(key));
                    }
                    return set;
                }, new Set()));
            const line = (cells) => '| ' + cells.join(' | ') + ' |';
            return [
                line(cols.map(escapeTableCell)),
                line(cols.map(() => '---')),
                ...data.map((row) => line(cols.map((col) =>
                    escapeTableCell(row && typeof row === 'object' ? row[col] : undefined)))),
            ].join('\n');
        },
    };
    // §5 promise join: all/any/wait return producer values; status/list/cancel
    // are small control snapshots.
    globalThis.narf.promise = {
        all:    (handles, timeoutMs) => hostTool('promise_when_all',
                    { promise_ids: (handles ?? []).map(promiseId), timeout_ms: timeoutMs }),
        any:    (handles, timeoutMs) => hostTool('promise_when_any',
                    { promise_ids: (handles ?? []).map(promiseId), timeout_ms: timeoutMs }),
        wait:   (handle, timeoutMs) => hostTool('promise_wait',
                    { promise_id: promiseId(handle), timeout_ms: timeoutMs }),
        status: (handle) => hostToolInline('promise_status', { promise_id: promiseId(handle) }),
        list:   () => hostToolInline('promise_list', {}),
        cancel: (handle) => hostToolInline('promise_cancel', { promise_id: promiseId(handle) }),
        // Pure-JS no-barrier staging: each item flows through ALL stages
        // independently (wall-clock = slowest single chain, not sum-of-stages).
        // A stage may be sync or async; each returned value passes to the next.
        pipeline: (items, ...stages) => Promise.all(
            (items ?? []).map((item) =>
                stages.reduce((acc, stage) => acc.then(stage), Promise.resolve(item)))),
    };
"#;

impl ScriptRuntime {
    /// Spawn the dedicated V8 thread and the capability executor, injecting the
    /// real capability impls and the supervision policy.
    pub async fn new(caps: Capabilities, policy: SupervisionPolicy) -> Result<Self> {
        let (cap_tx, mut cap_rx) = mpsc::unbounded_channel::<CapRequest>();

        // Capability executor: runs on the OUTER (multi-thread) tokio runtime.
        // One task per request so a slow or panicking capability cannot stall or
        // poison the executor loop.
        tokio::spawn(async move {
            while let Some(req) = cap_rx.recv().await {
                let caps = caps.clone();
                tokio::spawn(async move {
                    dispatch_cap(caps, req).await;
                });
            }
        });

        let (job_tx, job_rx) = mpsc::unbounded_channel::<Job>();
        let (setup_tx, setup_rx) =
            oneshot::channel::<Result<(v8::IsolateHandle, Arc<AtomicBool>)>>();

        let heap_limit_bytes = policy.heap_limit_bytes;
        let thread = std::thread::Builder::new()
            .name("bro-script-v8".to_string())
            .spawn(move || {
                v8_thread_main(cap_tx, job_rx, setup_tx, heap_limit_bytes);
            })?;

        let (isolate_handle, heap_oom) = setup_rx
            .await
            .map_err(|_| anyhow!("V8 thread exited before setup"))??;

        Ok(Self {
            job_tx,
            isolate_handle,
            heap_oom,
            execution_timeout: policy.execution_timeout,
            thread: Some(thread),
        })
    }

    /// Cross-thread isolate handle for external watchdog termination (criterion
    /// #2). The built-in execution-timeout supervisor uses this same handle.
    pub fn isolate_handle(&self) -> v8::IsolateHandle {
        self.isolate_handle.clone()
    }

    /// True if the near-heap-limit callback fired (OOM containment, criterion #1).
    pub fn hit_heap_oom(&self) -> bool {
        self.heap_oom.load(Ordering::SeqCst)
    }

    /// Execute a script body. The body is wrapped in an async IIFE, so it may use
    /// `await` and should `return` its result; the resolved value is serialized
    /// to a JSON string.
    ///
    /// If the policy sets an execution timeout, a script that overruns it is
    /// auto-terminated via the cross-thread `IsolateHandle` and a timeout error
    /// is returned; the runtime survives and stays reusable.
    pub async fn execute(&self, body: impl Into<String>) -> Result<String> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.job_tx
            .send(Job::Execute {
                body: body.into(),
                reply: reply_tx,
            })
            .map_err(|_| anyhow!("V8 thread is gone"))?;

        match self.execution_timeout {
            Some(timeout) => match tokio::time::timeout(timeout, reply_rx).await {
                Ok(reply) => reply.map_err(|_| anyhow!("V8 thread dropped the reply"))?,
                Err(_elapsed) => {
                    // Cross-thread kill of the runaway job. The V8 thread clears
                    // the terminate state before/after each job, so the runtime
                    // stays reusable for the next `execute`.
                    self.isolate_handle.terminate_execution();
                    Err(anyhow!("script execution timed out after {timeout:?}"))
                }
            },
            None => reply_rx
                .await
                .map_err(|_| anyhow!("V8 thread dropped the reply"))?,
        }
    }

    /// Render + syntax-validate + store a prepared script, returning a handle and
    /// the **rendered source** for the model to review (the §0.1 review step).
    /// `input_json` is `{ source, imports? }` — `imports` resolves session helpers
    /// (defined via [`ScriptRuntime::define`]) into the assembled script. Backs the
    /// model-facing `narf_prepare` tool; never an in-box binding.
    pub async fn prepare(&self, input_json: serde_json::Value) -> Result<PrepareResponse> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.job_tx
            .send(Job::Prepare {
                input_json,
                reply: reply_tx,
            })
            .map_err(|_| anyhow!("V8 thread is gone"))?;
        reply_rx
            .await
            .map_err(|_| anyhow!("V8 thread dropped the reply"))?
    }

    /// Register a session helper (`{ name, source, exports }`) in the host-side
    /// session frame so a later in-box `narf.session.import(name)` can recall it.
    /// Backs the model-facing `narf_define` tool (authoring is a control → outside
    /// the box, §0.1).
    pub async fn define(&self, input_json: serde_json::Value) -> Result<()> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.job_tx
            .send(Job::Define {
                input_json,
                reply: reply_tx,
            })
            .map_err(|_| anyhow!("V8 thread is gone"))?;
        reply_rx
            .await
            .map_err(|_| anyhow!("V8 thread dropped the reply"))?
    }

    pub async fn run(&self, handle: impl Into<String>) -> Result<String> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.job_tx
            .send(Job::Run {
                handle: handle.into(),
                reply: reply_tx,
            })
            .map_err(|_| anyhow!("V8 thread is gone"))?;

        match self.execution_timeout {
            Some(timeout) => match tokio::time::timeout(timeout, reply_rx).await {
                Ok(reply) => reply.map_err(|_| anyhow!("V8 thread dropped the reply"))?,
                Err(_elapsed) => {
                    self.isolate_handle.terminate_execution();
                    Err(anyhow!("script execution timed out after {timeout:?}"))
                }
            },
            None => reply_rx
                .await
                .map_err(|_| anyhow!("V8 thread dropped the reply"))?,
        }
    }

    pub async fn trace_len(&self) -> Result<usize> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.job_tx
            .send(Job::TraceLen { reply: reply_tx })
            .map_err(|_| anyhow!("V8 thread is gone"))?;
        reply_rx
            .await
            .map_err(|_| anyhow!("V8 thread dropped the reply"))
    }
}

impl Drop for ScriptRuntime {
    fn drop(&mut self) {
        let _ = self.job_tx.send(Job::Shutdown);
        // Nudge the isolate in case it is mid-execution.
        self.isolate_handle.terminate_execution();
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

fn v8_thread_main(
    cap_tx: CapTx,
    mut job_rx: mpsc::UnboundedReceiver<Job>,
    setup_tx: oneshot::Sender<Result<(v8::IsolateHandle, Arc<AtomicBool>)>>,
    heap_limit_bytes: usize,
) {
    // V8's platform is process-global; `JsRuntime::new` initializes it through an
    // internal `Once`, so concurrent first-construction across threads is safe.
    let create_params = v8::CreateParams::default().heap_limits(0, heap_limit_bytes);
    let mut runtime = deno_core::JsRuntime::new(RuntimeOptions {
        create_params: Some(create_params),
        extensions: vec![bro_script_ext::init(cap_tx)],
        ..Default::default()
    });

    let isolate_handle = runtime.v8_isolate().thread_safe_handle();
    let heap_oom = Arc::new(AtomicBool::new(false));

    // Near-heap-limit callback: flag + terminate, then hand V8 extra headroom so
    // it can unwind to us instead of calling abort(). Criterion #1.
    {
        let flag = heap_oom.clone();
        let handle = isolate_handle.clone();
        runtime.add_near_heap_limit_callback(move |current, _initial| {
            flag.store(true, Ordering::SeqCst);
            handle.terminate_execution();
            // Double the limit so V8 has room to propagate the termination.
            current * 2
        });
    }

    // Current-thread runtime + LocalSet to drive the (non-Send) event loop.
    let rt = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            let _ = setup_tx.send(Err(anyhow!("failed to build V8-thread runtime: {e}")));
            return;
        }
    };
    let local = tokio::task::LocalSet::new();

    let run = local.run_until(async move {
        // Deny ambient globals + install the capability shims before any user
        // script.
        if let Err(e) = runtime.execute_script("<bro-script:bootstrap>", BOOTSTRAP) {
            let _ = setup_tx.send(Err(anyhow!("bootstrap failed: {e}")));
            return;
        }
        if setup_tx.send(Ok((isolate_handle, heap_oom))).is_err() {
            return; // constructor went away
        }

        while let Some(job) = job_rx.recv().await {
            match job {
                Job::Shutdown => break,
                Job::Execute { body, reply } => {
                    // Clear any stale termination left by a prior timed-out job
                    // BEFORE running so the next job starts on a clean isolate
                    // (closes the timeout/finish race).
                    runtime.v8_isolate().cancel_terminate_execution();
                    let result = run_one(&mut runtime, &body).await;
                    // And clear again AFTER so the isolate is reusable for the
                    // next job (the runtime-reusable finding from the spike).
                    runtime.v8_isolate().cancel_terminate_execution();
                    let _ = reply.send(result);
                }
                Job::Prepare { input_json, reply } => {
                    runtime.v8_isolate().cancel_terminate_execution();
                    let result = prepare_one(&mut runtime, input_json);
                    runtime.v8_isolate().cancel_terminate_execution();
                    let _ = reply.send(result);
                }
                Job::Run { handle, reply } => {
                    runtime.v8_isolate().cancel_terminate_execution();
                    let result = run_prepared(&mut runtime, &handle).await;
                    runtime.v8_isolate().cancel_terminate_execution();
                    let _ = reply.send(result);
                }
                Job::Define { input_json, reply } => {
                    let result = define_one(&mut runtime, input_json);
                    let _ = reply.send(result);
                }
                Job::TraceLen { reply } => {
                    let trace = {
                        let op_state = runtime.op_state();
                        let state = op_state.borrow();
                        let trace = state.borrow::<TraceStateCell>().clone();
                        let len = trace.borrow().len();
                        len
                    };
                    let _ = reply.send(trace);
                }
            }
        }
    });

    rt.block_on(run);
}

async fn run_one(runtime: &mut deno_core::JsRuntime, body: &str) -> Result<String> {
    let wrapped = format!("(async () => {{ {body} }})()");
    let promise = runtime
        .execute_script("<bro-script>", wrapped)
        .map_err(|e| anyhow!("{e}"))?;

    // Drive the event loop while resolving the IIFE promise — this is what lets
    // a pending async op (the capability bridge) make progress. `resolve` returns
    // a future that does not borrow the runtime; box-pin it for the `Unpin`
    // bound `with_event_loop_promise` requires.
    let resolve = Box::pin(runtime.resolve(promise));
    let global = runtime
        .with_event_loop_promise(resolve, PollEventLoopOptions::default())
        .await
        .map_err(|e| anyhow!("{e}"))?;

    deno_core::scope!(scope, runtime);
    let local = v8::Local::new(scope, global);
    let value: serde_json::Value = deno_core::serde_v8::from_v8(scope, local)
        .map_err(|e| anyhow!("failed to deserialize result: {e}"))?;
    Ok(value.to_string())
}

fn prepare_one(
    runtime: &mut deno_core::JsRuntime,
    input_json: serde_json::Value,
) -> Result<PrepareResponse> {
    let input: PrepareInput = match serde_json::from_value(input_json) {
        Ok(i) => i,
        Err(e) => return Ok(blocked("input", format!("invalid narf_prepare input: {e}"))),
    };
    let assembled = {
        let op_state = runtime.op_state();
        let state = op_state.borrow();
        let session = state.borrow::<SessionStateCell>().borrow();
        match render_prepare(&session, input) {
            Ok(source) => source,
            Err(e) => return Ok(blocked("import", e)),
        }
    };

    if let Err(message) = validate_script_syntax(runtime, &assembled) {
        return Ok(blocked("syntax", message));
    }

    let handle = {
        let op_state = runtime.op_state();
        let state = op_state.borrow();
        let scripts = state.borrow::<PreparedScriptsCell>().clone();
        let handle = scripts.borrow_mut().put(assembled.clone());
        handle
    };
    // Return the rendered source so the model reviews exactly what narf_run runs.
    Ok(ready(handle, assembled))
}

/// Host-side `narf_define`: deserialize `{ name, source, exports }` and register
/// the session helper. Runs on the V8 thread (the session frame lives in
/// `OpState`); no script execution, so no terminate-state dance.
fn define_one(runtime: &mut deno_core::JsRuntime, input_json: serde_json::Value) -> Result<()> {
    let input: SessionDefineInput = serde_json::from_value(input_json)
        .map_err(|e| anyhow!("invalid narf_define input: {e}"))?;
    let op_state = runtime.op_state();
    let state = op_state.borrow();
    let session = state.borrow::<SessionStateCell>().clone();
    define_session_helper(&session, input).map_err(|e| anyhow!(e))
}

async fn run_prepared(runtime: &mut deno_core::JsRuntime, handle: &str) -> Result<String> {
    let source = {
        let op_state = runtime.op_state();
        let state = op_state.borrow();
        let scripts = state.borrow::<PreparedScriptsCell>().clone();
        let source = scripts.borrow().get(handle).map_err(|e| anyhow!(e))?;
        source
    };
    {
        let op_state = runtime.op_state();
        let state = op_state.borrow();
        let trace = state.borrow::<TraceStateCell>().clone();
        let mut trace = trace.borrow_mut();
        let sequence = trace.len();
        trace.push(TraceEntry {
            ref_handle: handle.to_string(),
            sequence,
        });
    }
    run_one(runtime, &source).await
}

fn validate_script_syntax(runtime: &mut deno_core::JsRuntime, body: &str) -> Result<(), String> {
    let wrapped = format!("(async () => {{\n{body}\n}})");
    deno_core::scope!(scope, runtime);
    let source = v8::String::new(scope, &wrapped)
        .ok_or_else(|| "failed to allocate V8 source string".to_string())?;
    v8::tc_scope!(let tc_scope, scope);
    match v8::Script::compile(tc_scope, source, None) {
        Some(_) => Ok(()),
        None => {
            let exception = tc_scope
                .exception()
                .ok_or_else(|| "unknown JavaScript syntax error".to_string())?;
            let message = exception
                .to_string(tc_scope)
                .map(|s| s.to_rust_string_lossy(tc_scope))
                .unwrap_or_else(|| "unknown JavaScript syntax error".to_string());
            Err(message)
        }
    }
}
