//! bro-script — §5b de-risking spike: V8 in-process via deno_core as the NARF
//! script-execution container.
//!
//! This crate proves, in ISOLATION (zero `blackbox` dependency), the four
//! daemon-safety properties and the one async-bridge property that gate
//! embedding V8 inside the long-lived blackbox daemon:
//!
//!   1. heap-bound OOM containment   — a runaway allocator is terminated, the
//!      process survives (no V8 `abort()`).
//!   2. runaway-loop kill            — `while(true){}` is interrupted cross-thread
//!      via the V8 `IsolateHandle`.
//!   3. denied globals               — no `WebAssembly`/`Atomics`/
//!      `SharedArrayBuffer`/`console` ambient host access.
//!   4. async capability bridge      — a JS `await cap.echo(x)` suspends the
//!      script, hops from the `!Send` isolate thread into async Rust on a
//!      *different* executor, and returns the result into JS.
//!   5. panic boundary               — a panic inside an op body is caught
//!      (`catch_unwind`) and surfaced as a JS error, never unwound across V8
//!      C++ frames (which would be UB / daemon abort).
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
//!   (`Send + Sync`), used by the watchdog and the near-heap-limit callback to
//!   terminate execution from another thread.
//!
//! ## Capability bridge
//!
//! An op invoked from JS does **not** run the capability inline on the V8 thread.
//! It forwards a `CapRequest` over an mpsc channel to a *capability executor*
//! task running on the **outer** multi-thread tokio runtime, then `await`s a
//! `oneshot` reply. While the op future is pending, `run_event_loop` keeps
//! polling it; the cross-runtime oneshot waker resolves it when async Rust
//! finishes. This is the genuine `!Send`-isolate ↔ async-capability round trip.

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;

use anyhow::{anyhow, Result};
use deno_core::v8;
use deno_core::{op2, OpState, PollEventLoopOptions, RuntimeOptions};
use deno_error::JsErrorBox;
use tokio::sync::{mpsc, oneshot};

/// deno_core version this spike pins. Reported by the spike for the §5 cost
/// accounting (this dep later lands in blackboxd).
pub const DENO_CORE_VERSION: &str = "0.403.0";

/// Default per-isolate hard heap ceiling (bytes). Generous enough that ordinary
/// scripts never trip it; the OOM-containment test passes a deliberately small
/// limit instead.
pub const DEFAULT_HEAP_LIMIT_BYTES: usize = 256 * 1024 * 1024;

// ---------------------------------------------------------------------------
// Capability stub — shaped like a bro-capabilities async trait, but LOCAL.
// The spike must build with zero blackbox/bro-capabilities coupling.
// ---------------------------------------------------------------------------

/// Local stand-in for a `bro-capabilities`-style async trait. Real capabilities
/// (corpus, refactor, …) would expose async methods like this; the spike only
/// needs one to prove the bridge.
#[async_trait::async_trait]
pub trait ScriptCapability: Send + Sync + 'static {
    async fn echo(&self, input: String) -> Result<String>;
}

/// Trivial echo capability used by the bridge test.
pub struct EchoCapability;

#[async_trait::async_trait]
impl ScriptCapability for EchoCapability {
    async fn echo(&self, input: String) -> Result<String> {
        // Force an actual await point so the round trip genuinely suspends.
        tokio::task::yield_now().await;
        Ok(format!("echo:{input}"))
    }
}

/// A capability call handed off from the V8-thread op to the async executor.
struct CapRequest {
    method: String,
    input: String,
    reply: oneshot::Sender<Result<String>>,
}

type CapTx = mpsc::UnboundedSender<CapRequest>;

// ---------------------------------------------------------------------------
// Ops + extension
// ---------------------------------------------------------------------------

/// `await cap.echo(x)` from JS: suspend, hop to async Rust on the outer runtime,
/// return the result into JS. This is criterion #4 (the core risk).
#[op2(async(lazy), fast)]
#[string]
async fn op_cap_echo(
    state: Rc<RefCell<OpState>>,
    #[string] input: String,
) -> Result<String, JsErrorBox> {
    // Clone the sender out under a short synchronous borrow; never hold an
    // OpState borrow across an await.
    let tx = state.borrow().borrow::<CapTx>().clone();
    let (reply_tx, reply_rx) = oneshot::channel();
    tx.send(CapRequest {
        method: "echo".to_string(),
        input,
        reply: reply_tx,
    })
    .map_err(|_| JsErrorBox::generic("capability executor is gone"))?;
    reply_rx
        .await
        .map_err(|_| JsErrorBox::generic("capability executor dropped the reply"))?
        .map_err(|e| JsErrorBox::generic(e.to_string()))
}

/// Proves the panic boundary (criterion #5): a panic raised inside the op body is
/// caught with `catch_unwind` and converted to an error that becomes a JS
/// exception — it is never allowed to unwind across V8's C++ frames.
#[op2(fast)]
fn op_panic_guarded() -> Result<(), JsErrorBox> {
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        panic!("intentional panic inside op handler");
    }));
    match outcome {
        Ok(()) => Ok(()),
        Err(_) => Err(JsErrorBox::generic(
            "op panicked; contained at the boundary (catch_unwind)",
        )),
    }
}

deno_core::extension!(
    bro_script_ext,
    ops = [op_cap_echo, op_panic_guarded],
    options = { tx: CapTx },
    state = |state, options| {
        state.put::<CapTx>(options.tx);
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
    Shutdown,
}

/// Owns a deno_core `JsRuntime` on a dedicated OS thread and exposes an async,
/// channel-based API callable from tokio code.
pub struct ScriptRuntime {
    job_tx: mpsc::UnboundedSender<Job>,
    isolate_handle: v8::IsolateHandle,
    heap_oom: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

/// Bootstrap script run once per isolate: deny ambient host globals and install
/// the `cap` shim that fronts the capability ops. `delete` is per-isolate (unlike
/// process-wide V8 flags), so each isolate hardens itself. `Deno.core` is kept —
/// it is the op transport, not an ambient host capability.
const BOOTSTRAP: &str = r#"
    delete globalThis.WebAssembly;
    delete globalThis.SharedArrayBuffer;
    delete globalThis.Atomics;
    delete globalThis.console;
    globalThis.cap = {
        echo: (x) => Deno.core.ops.op_cap_echo(x),
    };
"#;

impl ScriptRuntime {
    /// Spawn the dedicated V8 thread and the capability executor.
    pub async fn new(cap: Arc<dyn ScriptCapability>, heap_limit_bytes: usize) -> Result<Self> {
        let (cap_tx, mut cap_rx) = mpsc::unbounded_channel::<CapRequest>();

        // Capability executor: runs on the OUTER (multi-thread) tokio runtime.
        tokio::spawn(async move {
            while let Some(req) = cap_rx.recv().await {
                let cap = cap.clone();
                tokio::spawn(async move {
                    let result = match req.method.as_str() {
                        "echo" => cap.echo(req.input).await,
                        other => Err(anyhow!("unknown capability method: {other}")),
                    };
                    let _ = req.reply.send(result);
                });
            }
        });

        let (job_tx, job_rx) = mpsc::unbounded_channel::<Job>();
        let (setup_tx, setup_rx) =
            oneshot::channel::<Result<(v8::IsolateHandle, Arc<AtomicBool>)>>();

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
            thread: Some(thread),
        })
    }

    /// Cross-thread isolate handle for watchdog termination (criterion #2).
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
    pub async fn execute(&self, body: impl Into<String>) -> Result<String> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.job_tx
            .send(Job::Execute {
                body: body.into(),
                reply: reply_tx,
            })
            .map_err(|_| anyhow!("V8 thread is gone"))?;
        reply_rx
            .await
            .map_err(|_| anyhow!("V8 thread dropped the reply"))?
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
        // Deny ambient globals + install the cap shim before any user script.
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
                    let result = run_one(&mut runtime, &body).await;
                    // Clear any lingering termination state so the isolate is
                    // reusable for the next job (see runtime-reusable finding).
                    runtime.v8_isolate().cancel_terminate_execution();
                    let _ = reply.send(result);
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
