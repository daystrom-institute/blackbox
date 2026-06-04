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
    AtomCapability, AtomInvocation, AtomOutput, CapabilityResult, RefactorCapability,
    RefactorPlanHandle, RefactorRequest, ToolCallOutput, ToolCapability, ToolInvocation,
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

/// Default per-runtime cumulative egress budget (bytes). Caps the total bytes
/// that `narf.ref.text()` may pull out of host-side refs into the JS heap (i.e.
/// model context) over the lifetime of the runtime. Generous for ordinary
/// scripts; the egress-budget test passes a deliberately tiny value.
pub const DEFAULT_EGRESS_BUDGET_BYTES: usize = 256 * 1024;

/// Default per-call `narf.ref.text()` materialization cap (bytes) when the
/// caller omits `maxBytes`. A single `text()` never pulls more than this even if
/// the ref is larger and the cumulative budget has room.
pub const DEFAULT_EGRESS_TEXT_BYTES: usize = 8 * 1024;

/// Byte length of the bounded preview head stored on each ref and surfaced in the
/// capability envelope / `narf.ref.peek()`. The preview is the only ref content
/// that crosses into JS without spending egress budget; keep it small.
const PREVIEW_BYTES: usize = 512;

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
    /// Per-runtime cumulative cap on bytes egressed from host-side refs into JS
    /// via `narf.ref.text()`. Default-ON (see [`DEFAULT_EGRESS_BUDGET_BYTES`]);
    /// a `text()` that would push cumulative egress past this cap fails closed
    /// with a catchable JS error. This is the NARF "egress is explicit and
    /// bounded" governance non-negotiable (narf-draft2 §7).
    pub egress_budget_bytes: usize,
}

impl Default for SupervisionPolicy {
    fn default() -> Self {
        Self {
            heap_limit_bytes: DEFAULT_HEAP_LIMIT_BYTES,
            execution_timeout: Some(DEFAULT_EXECUTION_TIMEOUT),
            egress_budget_bytes: DEFAULT_EGRESS_BUDGET_BYTES,
        }
    }
}

// ---------------------------------------------------------------------------
// Ref substrate (§9-1, narf-draft2 §4 `Ref<T>`) + bounded egress (§7)
// ---------------------------------------------------------------------------
//
// NARF discipline: a big capability result must NOT flow straight into the V8
// heap / model context. Instead the full serialized value lives HOST-SIDE in a
// per-runtime `RefState`, keyed by an opaque `ref:<kind>/<id>` token, and only a
// compact envelope (handle + size + bounded preview) crosses into JS. The bytes
// enter context only via explicit, budget-bounded egress (`narf.ref.text`).
//
// `RefState` lives on the V8 thread inside `OpState` as `Rc<RefCell<RefState>>` —
// ops run on that thread, so no cross-thread sharing is needed and values never
// auto-enter JS.

/// A take a byte-bounded prefix of `s` that respects UTF-8 char boundaries. Never
/// splits a multi-byte codepoint (walks the boundary down). Returns all of `s`
/// when it is already within `max`.
fn byte_prefix(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut end = max;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

/// Build the bounded, truncation-marked preview head stored on a ref. Bounded to
/// [`PREVIEW_BYTES`]; this is the only ref content that crosses into JS for free
/// (envelope + `peek`), so it is deliberately small.
fn make_preview(value: &str) -> String {
    if value.len() <= PREVIEW_BYTES {
        return value.to_string();
    }
    let head = byte_prefix(value, PREVIEW_BYTES);
    format!(
        "{head}…[+{} bytes truncated; narf.ref.text(handle) to materialize]",
        value.len() - head.len()
    )
}

/// One stored ref: the FULL serialized value (held host-side, never auto-egressed)
/// plus metadata. `kind` is `cap` for capability results to start.
struct RefEntry {
    kind: String,
    content_type: String,
    value: String,
    size: usize,
    preview: String,
}

/// The compact envelope returned to JS in place of a capability op's full output.
/// The full value stays host-side under [`RefEntry`]; only this crosses into V8.
#[derive(Serialize)]
struct RefEnvelope {
    #[serde(rename = "ref")]
    handle: String,
    size: usize,
    preview: String,
}

/// Per-runtime ref store + cumulative egress accounting. Lives on the V8 thread.
/// Ids are a simple monotonic counter (deterministic; no uuid/rng needed since
/// this is host-side Rust state, not script-observable nondeterminism).
struct RefState {
    next_id: u64,
    entries: HashMap<String, RefEntry>,
    egress_used: usize,
    egress_budget_bytes: usize,
}

impl RefState {
    fn new(egress_budget_bytes: usize) -> Self {
        Self {
            next_id: 0,
            entries: HashMap::new(),
            egress_used: 0,
            egress_budget_bytes,
        }
    }

    /// Store a full serialized value host-side and return the JS-facing envelope.
    /// The value itself never crosses into JS here.
    fn put(&mut self, kind: &str, content_type: &str, value: String) -> RefEnvelope {
        let id = self.next_id;
        self.next_id += 1;
        let handle = format!("ref:{kind}/{id}");
        let size = value.len();
        let preview = make_preview(&value);
        self.entries.insert(
            handle.clone(),
            RefEntry {
                kind: kind.to_string(),
                content_type: content_type.to_string(),
                value,
                size,
                preview: preview.clone(),
            },
        );
        RefEnvelope {
            handle,
            size,
            preview,
        }
    }

    fn get_value(&self, handle: &str, expected_kind: &str) -> Result<String, String> {
        let entry = self
            .entries
            .get(handle)
            .ok_or_else(|| format!("unknown ref handle: {handle}"))?;
        if entry.kind != expected_kind {
            return Err(format!(
                "ref {handle} has kind {}, expected {expected_kind}",
                entry.kind
            ));
        }
        Ok(entry.value.clone())
    }

    /// Explicit, bounded egress: materialize up to `max_bytes` (0 → default
    /// [`DEFAULT_EGRESS_TEXT_BYTES`]) of the ref's value into JS, charging the
    /// cumulative budget. Fails closed (does NOT truncate) if the materialized
    /// slice would push cumulative egress past `egress_budget_bytes`, and on an
    /// unknown handle. Returns `Err(message)` for a catchable JS error.
    fn text(&mut self, handle: &str, max_bytes: usize) -> Result<String, String> {
        let cap = if max_bytes == 0 {
            DEFAULT_EGRESS_TEXT_BYTES
        } else {
            max_bytes
        };
        let entry = self
            .entries
            .get(handle)
            .ok_or_else(|| format!("unknown ref handle: {handle}"))?;
        // Owned copy ends the immutable borrow of `self.entries` before we mutate
        // the egress counter below.
        let out = byte_prefix(&entry.value, cap).to_string();
        let want = out.len();
        let remaining = self.egress_budget_bytes.saturating_sub(self.egress_used);
        if want > remaining {
            return Err(format!(
                "egress budget exceeded: requested {want} bytes, {remaining} of {} byte budget remaining",
                self.egress_budget_bytes
            ));
        }
        self.egress_used += want;
        Ok(out)
    }

    /// Metadata-only view of a ref (kind, size, content type, bounded preview).
    /// Never returns the full value and never charges egress budget.
    fn peek(&self, handle: &str) -> Result<serde_json::Value, String> {
        let entry = self
            .entries
            .get(handle)
            .ok_or_else(|| format!("unknown ref handle: {handle}"))?;
        Ok(serde_json::json!({
            "kind": entry.kind,
            "size": entry.size,
            "content_type": entry.content_type,
            "preview": entry.preview,
        }))
    }
}

type RefStateCell = Rc<RefCell<RefState>>;

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
    }
}

fn ready(handle: String) -> PrepareResponse {
    PrepareResponse {
        ref_handle: Some(handle),
        status: "ready".to_string(),
        diagnostics: vec![],
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
/// typed reply, serializing the output to a JSON string for JS. The whole body
/// runs under [`guard_async`] in the caller op, so the synchronous channel work
/// (and any panic in it) is contained on the V8 thread.
async fn bridge<O: Serialize>(
    tx: CapTx,
    make: impl FnOnce(oneshot::Sender<CapabilityResult<O>>) -> CapRequest,
) -> Result<String, JsErrorBox> {
    let (reply_tx, reply_rx) = oneshot::channel();
    tx.send(make(reply_tx))
        .map_err(|_| JsErrorBox::generic("capability executor is gone"))?;
    let output = reply_rx
        .await
        .map_err(|_| JsErrorBox::generic("capability executor dropped the reply"))?
        .map_err(|e| JsErrorBox::generic(format!("{}: {}", e.code, e.message)))?;
    serde_json::to_string(&output)
        .map_err(|e| JsErrorBox::generic(format!("failed to serialize capability output: {e}")))
}

/// Store a full serialized capability output host-side as a `cap` ref and return
/// the compact envelope JSON (`{ "ref", "size", "preview" }`) — the only thing
/// that crosses into JS. The full value stays in [`RefState`]; it enters context
/// only via explicit `narf.ref.text` egress.
fn store_and_envelope(refs: &RefStateCell, full: String) -> Result<String, JsErrorBox> {
    store_and_envelope_kind(refs, "cap", "application/json", full)
}

/// Like [`store_and_envelope`] but with an explicit ref kind + content type, so
/// host built-in tool output (`narf-tool-placement.md` §5) is stored under the
/// `tool` kind with the tool's own content type rather than the `cap` default.
fn store_and_envelope_kind(
    refs: &RefStateCell,
    kind: &str,
    content_type: &str,
    full: String,
) -> Result<String, JsErrorBox> {
    let envelope = refs.borrow_mut().put(kind, content_type, full);
    serde_json::to_string(&envelope)
        .map_err(|e| JsErrorBox::generic(format!("failed to serialize ref envelope: {e}")))
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

/// Pull the two per-runtime handles a capability op needs (the capability
/// executor channel + the host-side ref store) out of `OpState` in one short
/// borrow, BEFORE any `await`, so no `OpState` borrow is held across a suspend.
fn caps_handles(state: &Rc<RefCell<OpState>>) -> (CapTx, RefStateCell) {
    let s = state.borrow();
    (
        s.borrow::<CapTx>().clone(),
        s.borrow::<RefStateCell>().clone(),
    )
}

/// `await atoms.invoke(handle, input)` — JSON in, JSON out, over the async bridge.
#[op2(async(lazy), fast)]
#[string]
async fn op_atom_invoke(
    state: Rc<RefCell<OpState>>,
    #[string] input_json: String,
) -> Result<String, JsErrorBox> {
    guard_async(async move {
        let (tx, refs) = caps_handles(&state);
        let input: AtomInvocation = serde_json::from_str(&input_json)
            .map_err(|e| JsErrorBox::generic(format!("invalid atoms.invoke input: {e}")))?;
        let full = bridge(tx, move |reply| CapRequest::AtomInvoke { input, reply }).await?;
        store_and_envelope(&refs, full)
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
        let (tx, refs) = caps_handles(&state);
        let input: RefactorRequest = serde_json::from_str(&input_json)
            .map_err(|e| JsErrorBox::generic(format!("invalid refactor.plan input: {e}")))?;
        let full = bridge(tx, move |reply| CapRequest::RefactorPlan { input, reply }).await?;
        store_and_envelope(&refs, full)
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
        let (tx, refs) = caps_handles(&state);
        let full = bridge(tx, move |reply| CapRequest::RefactorMaterialize {
            id,
            reply,
        })
        .await?;
        store_and_envelope(&refs, full)
    })
    .await
}

/// `op_tool_invoke(name, input)` — the generic host built-in seam
/// (`narf-tool-placement.md` §5). Bridges to the injected [`ToolCapability`],
/// stores the tool's content host-side as a `tool` ref, and returns the bounded
/// `{ref,size,preview}` envelope. A tool that reports `is_error` surfaces as a
/// catchable JS exception (so a cell can `try/catch`); an unknown / denied /
/// uninstalled tool fails closed via the capability error.
#[op2(async(lazy), fast)]
#[string]
async fn op_tool_invoke(
    state: Rc<RefCell<OpState>>,
    #[string] input_json: String,
) -> Result<String, JsErrorBox> {
    guard_async(async move {
        let (tx, refs) = caps_handles(&state);
        let input: ToolInvocation = serde_json::from_str(&input_json)
            .map_err(|e| JsErrorBox::generic(format!("invalid host tool invocation: {e}")))?;
        let name = input.name.clone();
        let out = bridge_tool(tx, input).await?;
        if out.is_error {
            return Err(JsErrorBox::generic(format!("{name}: {}", out.content)));
        }
        store_and_envelope_kind(&refs, "tool", &out.content_type, out.content)
    })
    .await
}

/// `narf.ref.text(handle, maxBytes)` — explicit, bounded egress. Materialize up
/// to `max_bytes` (0 → [`DEFAULT_EGRESS_TEXT_BYTES`]) of the ref's host-side
/// value into JS, charging the per-runtime cumulative egress budget. This is the
/// ONLY path a ref's bytes enter context. Fails closed (catchable JS error) when
/// the slice would exceed the remaining budget, or on an unknown handle. Sync op
/// (pure host-state), still routed through the structural panic guard.
#[op2]
#[string]
fn op_ref_text(
    state: &mut OpState,
    #[string] handle: String,
    #[smi] max_bytes: u32,
) -> Result<String, JsErrorBox> {
    guard_op(|| {
        let refs = state.borrow::<RefStateCell>().clone();
        let mut rs = refs.borrow_mut();
        rs.text(&handle, max_bytes as usize)
            .map_err(JsErrorBox::generic)
    })
}

/// `narf.ref.peek(handle)` — metadata only (kind, size, content type, bounded
/// preview). Never returns the full value and never charges egress budget.
/// Unknown handle → catchable JS error.
#[op2]
#[string]
fn op_ref_peek(state: &mut OpState, #[string] handle: String) -> Result<String, JsErrorBox> {
    guard_op(|| {
        let refs = state.borrow::<RefStateCell>().clone();
        let meta = refs.borrow().peek(&handle).map_err(JsErrorBox::generic)?;
        serde_json::to_string(&meta)
            .map_err(|e| JsErrorBox::generic(format!("failed to serialize ref metadata: {e}")))
    })
}

/// `narf.session.define(name, { source, exports })` — store helper source in the
/// host-side session frame and create the default import alias `name -> name`.
#[op2(fast)]
fn op_session_define(state: &mut OpState, #[string] input_json: String) -> Result<(), JsErrorBox> {
    guard_op(|| {
        let input: SessionDefineInput = serde_json::from_str(&input_json)
            .map_err(|e| JsErrorBox::generic(format!("invalid session.define input: {e}")))?;
        if !is_js_identifier(&input.name) {
            return Err(JsErrorBox::generic(format!(
                "invalid helper name identifier: {}",
                input.name
            )));
        }
        let helper = SessionHelper {
            source: input.source,
            exports: input.exports,
        };
        // Validate export names early; syntax is validated on import/prepare.
        let _ = helper_expression(&helper).map_err(JsErrorBox::generic)?;
        let session = state.borrow::<SessionStateCell>().clone();
        let mut session = session.borrow_mut();
        session
            .import_aliases
            .insert(input.name.clone(), input.name.clone());
        session.helpers.insert(input.name, helper);
        Ok(())
    })
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

/// Render a prepared script candidate without storing it. The JS shim validates
/// syntax parse-only with `new Function(...)` and calls `op_prepare_store` only
/// when parsing succeeds.
#[op2]
#[string]
fn op_prepare_render(
    state: &mut OpState,
    #[string] input_json: String,
) -> Result<String, JsErrorBox> {
    guard_op(|| {
        let input: PrepareInput = serde_json::from_str(&input_json)
            .map_err(|e| JsErrorBox::generic(format!("invalid narf.prepare input: {e}")))?;
        let session = state.borrow::<SessionStateCell>().clone();
        let rendered = match render_prepare(&session.borrow(), input) {
            Ok(source) => serde_json::json!({
                "status": "ready",
                "diagnostics": [],
                "source": source,
            }),
            Err(e) => serde_json::json!({
                "status": "blocked",
                "diagnostics": [diagnostic("import", e)],
            }),
        };
        serde_json::to_string(&rendered)
            .map_err(|e| JsErrorBox::generic(format!("failed to serialize prepare render: {e}")))
    })
}

/// Store a parse-validated prepared artifact in the existing ref store under
/// `ref:narf-script/<id>`.
#[op2]
#[string]
fn op_prepare_store(
    state: &mut OpState,
    #[string] assembled_source: String,
) -> Result<String, JsErrorBox> {
    guard_op(|| {
        let refs = state.borrow::<RefStateCell>().clone();
        let envelope =
            refs.borrow_mut()
                .put("narf-script", "application/javascript", assembled_source);
        serde_json::to_string(&ready(envelope.handle))
            .map_err(|e| JsErrorBox::generic(format!("failed to serialize prepare response: {e}")))
    })
}

/// Return a stored prepared artifact's exact source for `narf.run(ref)`.
#[op2]
#[string]
fn op_prepared_source(state: &mut OpState, #[string] handle: String) -> Result<String, JsErrorBox> {
    guard_op(|| {
        let refs = state.borrow::<RefStateCell>().clone();
        let source = refs
            .borrow()
            .get_value(&handle, "narf-script")
            .map_err(JsErrorBox::generic)?;
        Ok(source)
    })
}

#[op2(fast)]
fn op_trace_record(state: &mut OpState, #[string] handle: String) -> Result<(), JsErrorBox> {
    guard_op(|| {
        let trace = state.borrow::<TraceStateCell>().clone();
        let mut trace = trace.borrow_mut();
        let sequence = trace.len();
        trace.push(TraceEntry {
            ref_handle: handle,
            sequence,
        });
        Ok(())
    })
}

#[op2]
#[string]
fn op_trace_entries(state: &mut OpState) -> Result<String, JsErrorBox> {
    guard_op(|| {
        let trace = state.borrow::<TraceStateCell>().clone();
        let entries = trace.borrow().clone();
        serde_json::to_string(&entries)
            .map_err(|e| JsErrorBox::generic(format!("failed to serialize trace: {e}")))
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
        op_ref_text,
        op_ref_peek,
        op_session_define,
        op_session_import,
        op_prepare_render,
        op_prepare_store,
        op_prepared_source,
        op_trace_record,
        op_trace_entries,
        op_panic_guarded,
        op_panic_guarded_async,
    ],
    options = { tx: CapTx, egress_budget_bytes: usize },
    state = |state, options| {
        state.put::<CapTx>(options.tx);
        state.put::<RefStateCell>(Rc::new(RefCell::new(RefState::new(options.egress_budget_bytes))));
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
        input: String,
        reply: oneshot::Sender<Result<PrepareResponse>>,
    },
    Run {
        handle: String,
        reply: oneshot::Sender<Result<String>>,
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
    // seam, returns a `{ ref, size, preview }` envelope (output stays host-side;
    // egress via narf.ref.text), and throws on tool error. Selection-from-result
    // is interpretive and must round-trip the model — these are for mechanical
    // complete-set composition, not enumerate-then-pick (narf-tool-placement.md §3).
    const hostTool = async (name, input) =>
        JSON.parse(await Deno.core.ops.op_tool_invoke(
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
        run:  (a) => hostTool('shell_run', typeof a === 'string' ? { command: a } : a),
        poll: (a) => hostTool('shell_poll', a),
        kill: (a) => hostTool('shell_kill', a),
        list: (a) => hostTool('shell_list', a ?? {}),
    };
    globalThis.web = {
        fetch: (a) => hostTool('web_fetch', typeof a === 'string' ? { url: a } : a),
    };
    // §9-1 Ref substrate: capability ops now return a `{ ref, size, preview }`
    // envelope (the full value stays host-side). `narf.ref` is the explicit,
    // bounded egress surface — `text()` is the only way a ref's bytes enter JS.
    const refToken = (h) => (typeof h === 'string' ? h : (h && h.ref));
    const parsePrepared = (source) => {
        // Parse-only validation of a body that will later run inside the same
        // async-IIFE shape as ScriptRuntime::execute/run_one.
        new Function(`return (async () => {\n${source}\n})`);
    };
    globalThis.narf = {
        ref: {
            text: (handle, maxBytes) =>
                Deno.core.ops.op_ref_text(
                    refToken(handle),
                    (maxBytes === undefined || maxBytes === null) ? 0 : maxBytes),
            peek: (handle) =>
                JSON.parse(Deno.core.ops.op_ref_peek(refToken(handle))),
        },
        session: {
            define: (name, helper) => {
                Deno.core.ops.op_session_define(JSON.stringify({
                    name,
                    source: helper && helper.source,
                    exports: helper && helper.exports,
                }));
            },
            import: (name) => {
                const expr = Deno.core.ops.op_session_import(name);
                return (0, eval)(expr);
            },
        },
        prepare: (args) => {
            const rendered = JSON.parse(Deno.core.ops.op_prepare_render(JSON.stringify(args ?? {})));
            if (rendered.status === 'blocked') {
                return { status: 'blocked', diagnostics: rendered.diagnostics ?? [] };
            }
            try {
                parsePrepared(rendered.source);
            } catch (e) {
                return {
                    status: 'blocked',
                    diagnostics: [{ kind: 'syntax', message: String(e) }],
                };
            }
            return JSON.parse(Deno.core.ops.op_prepare_store(rendered.source));
        },
        run: async (handle) => {
            const ref = refToken(handle);
            const source = Deno.core.ops.op_prepared_source(ref);
            Deno.core.ops.op_trace_record(ref);
            return await (0, eval)(`(async () => {\n${source}\n})()`);
        },
        trace: {
            entries: () => JSON.parse(Deno.core.ops.op_trace_entries()),
        },
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
        let egress_budget_bytes = policy.egress_budget_bytes;
        let thread = std::thread::Builder::new()
            .name("bro-script-v8".to_string())
            .spawn(move || {
                v8_thread_main(
                    cap_tx,
                    job_rx,
                    setup_tx,
                    heap_limit_bytes,
                    egress_budget_bytes,
                );
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

    pub async fn prepare(&self, source: impl Into<String>) -> Result<PrepareResponse> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.job_tx
            .send(Job::Prepare {
                input: source.into(),
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
    egress_budget_bytes: usize,
) {
    // V8's platform is process-global; `JsRuntime::new` initializes it through an
    // internal `Once`, so concurrent first-construction across threads is safe.
    let create_params = v8::CreateParams::default().heap_limits(0, heap_limit_bytes);
    let mut runtime = deno_core::JsRuntime::new(RuntimeOptions {
        create_params: Some(create_params),
        extensions: vec![bro_script_ext::init(cap_tx, egress_budget_bytes)],
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
                Job::Prepare { input, reply } => {
                    runtime.v8_isolate().cancel_terminate_execution();
                    let result = prepare_one(&mut runtime, &input);
                    runtime.v8_isolate().cancel_terminate_execution();
                    let _ = reply.send(result);
                }
                Job::Run { handle, reply } => {
                    runtime.v8_isolate().cancel_terminate_execution();
                    let result = run_prepared(&mut runtime, &handle).await;
                    runtime.v8_isolate().cancel_terminate_execution();
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

fn prepare_one(runtime: &mut deno_core::JsRuntime, source: &str) -> Result<PrepareResponse> {
    let input = PrepareInput {
        source: source.to_string(),
        imports: None,
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
        let refs = state.borrow::<RefStateCell>().clone();
        let handle = refs
            .borrow_mut()
            .put("narf-script", "application/javascript", assembled)
            .handle;
        handle
    };
    Ok(ready(handle))
}

async fn run_prepared(runtime: &mut deno_core::JsRuntime, handle: &str) -> Result<String> {
    let source = {
        let op_state = runtime.op_state();
        let state = op_state.borrow();
        let refs = state.borrow::<RefStateCell>().clone();
        let source = refs
            .borrow()
            .get_value(handle, "narf-script")
            .map_err(|e| anyhow!(e))?;
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
