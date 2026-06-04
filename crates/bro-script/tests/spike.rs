//! §5b acceptance tests. Each maps to a daemon-safety criterion or a real
//! capability path.
//!
//! All tests use multi-thread tokio runtimes (the capability executor runs on
//! the outer runtime, the V8 isolate on its own dedicated thread). The whole
//! point of several of these is that **the test process survives** a condition
//! that would otherwise abort the daemon.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use bro_capabilities::{
    AtomCapability, AtomInvocation, AtomOutput, CapabilityResult, CorpusCapability, CorpusHit,
    CorpusLookup, RefactorCapability, RefactorPlanHandle, RefactorRequest,
};
use bro_core::BroError;
use bro_script::{
    Capabilities, ScriptRuntime, SupervisionPolicy, DEFAULT_HEAP_LIMIT_BYTES, DENO_CORE_VERSION,
};

// ---------------------------------------------------------------------------
// Stub capability impls — exercise the real traits without any daemon coupling.
// ---------------------------------------------------------------------------

struct StubCorpus;
#[async_trait]
impl CorpusCapability for StubCorpus {
    async fn search_corpus(&self, lookup: CorpusLookup) -> CapabilityResult<Vec<CorpusHit>> {
        // Force a real await point so the bridge genuinely suspends.
        tokio::task::yield_now().await;
        Ok(vec![CorpusHit {
            id: format!("hit-{}", lookup.limit),
            text: format!("found:{}", lookup.query),
        }])
    }
}

struct StubAtoms;
#[async_trait]
impl AtomCapability for StubAtoms {
    async fn invoke_atom(&self, invocation: AtomInvocation) -> CapabilityResult<AtomOutput> {
        tokio::task::yield_now().await;
        Ok(AtomOutput {
            output_json: serde_json::json!({
                "atom": invocation.atom.as_str(),
                "echo": invocation.input_json,
            }),
        })
    }
}

struct StubRefactor;
#[async_trait]
impl RefactorCapability for StubRefactor {
    async fn plan_refactor(
        &self,
        request: RefactorRequest,
    ) -> CapabilityResult<RefactorPlanHandle> {
        tokio::task::yield_now().await;
        Ok(RefactorPlanHandle {
            id: format!("plan-{}", request.kind),
            preview: format!("preview of {}", request.kind),
        })
    }

    async fn materialize_plan(&self, id: String) -> CapabilityResult<serde_json::Value> {
        tokio::task::yield_now().await;
        Ok(serde_json::json!({ "materialized": id }))
    }
}

// Panicking variants: prove a capability panic is contained on the executor and
// surfaces as a catchable JS error (the outer-runtime guard complementing the
// V8-thread structural guard).
struct PanicCorpus;
#[async_trait]
impl CorpusCapability for PanicCorpus {
    async fn search_corpus(&self, _lookup: CorpusLookup) -> CapabilityResult<Vec<CorpusHit>> {
        panic!("corpus capability boom");
    }
}

struct PanicAtoms;
#[async_trait]
impl AtomCapability for PanicAtoms {
    async fn invoke_atom(&self, _invocation: AtomInvocation) -> CapabilityResult<AtomOutput> {
        panic!("atom capability boom");
    }
}

struct PanicRefactor;
#[async_trait]
impl RefactorCapability for PanicRefactor {
    async fn plan_refactor(
        &self,
        _request: RefactorRequest,
    ) -> CapabilityResult<RefactorPlanHandle> {
        panic!("refactor capability boom");
    }

    async fn materialize_plan(&self, _id: String) -> CapabilityResult<serde_json::Value> {
        panic!("refactor materialize boom");
    }
}

// An error-returning corpus: prove a normal CapabilityResult error surfaces as a
// catchable JS error too (distinct from a panic).
struct ErrCorpus;
#[async_trait]
impl CorpusCapability for ErrCorpus {
    async fn search_corpus(&self, _lookup: CorpusLookup) -> CapabilityResult<Vec<CorpusHit>> {
        Err(BroError::new("corpus_unavailable", "index offline"))
    }
}

fn caps_with(
    corpus: Arc<dyn CorpusCapability>,
    atoms: Arc<dyn AtomCapability>,
    refactor: Arc<dyn RefactorCapability>,
) -> Capabilities {
    Capabilities {
        corpus,
        atoms,
        refactor,
    }
}

fn stub_caps() -> Capabilities {
    caps_with(
        Arc::new(StubCorpus),
        Arc::new(StubAtoms),
        Arc::new(StubRefactor),
    )
}

async fn stub_runtime() -> ScriptRuntime {
    ScriptRuntime::new(stub_caps(), SupervisionPolicy::default())
        .await
        .unwrap()
}

// ---------------------------------------------------------------------------
// Build proof + denied globals (criteria #1 build, #3)
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn build_proof_basic_execution() {
    assert_eq!(DENO_CORE_VERSION, "0.403.0");
    let rt = stub_runtime().await;
    let out = rt.execute("return 1 + 1;").await.unwrap();
    assert_eq!(out, "2");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn denied_globals() {
    let rt = stub_runtime().await;
    let out = rt
        .execute(
            "return typeof WebAssembly === 'undefined' \
              && typeof Atomics === 'undefined' \
              && typeof SharedArrayBuffer === 'undefined' \
              && typeof console === 'undefined';",
        )
        .await
        .unwrap();
    assert_eq!(out, "true", "ambient globals must be denied");
}

// ---------------------------------------------------------------------------
// Real capability bridges (criterion #4) — one per trait method.
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn corpus_search_bridge() {
    let rt = stub_runtime().await;
    let out = rt
        .execute(
            r#"const r = await corpus.search({ query: "redis", limit: 2 }); return r[0].text;"#,
        )
        .await
        .unwrap();
    assert_eq!(out, "\"found:redis\"");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn atom_invoke_bridge() {
    let rt = stub_runtime().await;
    let out = rt
        .execute(
            r#"const r = await atoms.invoke("my-atom", { x: 1 });
               return r.output_json.atom + ":" + r.output_json.echo.x;"#,
        )
        .await
        .unwrap();
    assert_eq!(out, "\"my-atom:1\"");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn refactor_plan_and_materialize_bridge() {
    let rt = stub_runtime().await;
    let out = rt
        .execute(
            r#"const h = await refactor.plan({ kind: "rename", input_json: { sym: "foo" } });
               const m = await refactor.materialize(h);
               return h.preview + "|" + m.materialized;"#,
        )
        .await
        .unwrap();
    assert_eq!(out, "\"preview of rename|plan-rename\"");
}

// ---------------------------------------------------------------------------
// Structural panic guard (criterion #5) — sync and async op paths.
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn panic_boundary_sync_op_surfaced_as_js_error() {
    let rt = stub_runtime().await;
    let out = rt
        .execute(
            "try { Deno.core.ops.op_panic_guarded(); return 'NO_THROW'; } \
             catch (e) { return 'caught'; }",
        )
        .await
        .unwrap();
    assert_eq!(
        out, "\"caught\"",
        "sync op panic must surface as a catchable JS error"
    );
    // Isolate still usable afterwards — the process survived.
    assert_eq!(rt.execute("return 7 * 6;").await.unwrap(), "42");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn panic_boundary_async_op_surfaced_as_js_error() {
    // This op panics AFTER an await — the exact poll-resumption case that every
    // capability op's `guard_async` wrapper must catch on the V8 thread.
    let rt = stub_runtime().await;
    let out = rt
        .execute(
            "try { await Deno.core.ops.op_panic_guarded_async(); return 'NO_THROW'; } \
             catch (e) { return 'caught'; }",
        )
        .await
        .unwrap();
    assert_eq!(
        out, "\"caught\"",
        "async op panic (post-await) must surface as a catchable JS error"
    );
    assert_eq!(rt.execute("return 'alive';").await.unwrap(), "\"alive\"");
}

// A panic inside ANY of the three capability paths is contained and surfaced as
// a catchable JS error, and the isolate stays usable.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn capability_panic_corpus_surfaced_and_runtime_survives() {
    let rt = ScriptRuntime::new(
        caps_with(
            Arc::new(PanicCorpus),
            Arc::new(StubAtoms),
            Arc::new(StubRefactor),
        ),
        SupervisionPolicy::default(),
    )
    .await
    .unwrap();
    let out = rt
        .execute(
            r#"try { await corpus.search({ query: "x", limit: 1 }); return 'NO_THROW'; }
               catch (e) { return 'caught'; }"#,
        )
        .await
        .unwrap();
    assert_eq!(out, "\"caught\"");
    assert_eq!(rt.execute("return 6 * 7;").await.unwrap(), "42");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn capability_panic_atom_surfaced_and_runtime_survives() {
    let rt = ScriptRuntime::new(
        caps_with(
            Arc::new(StubCorpus),
            Arc::new(PanicAtoms),
            Arc::new(StubRefactor),
        ),
        SupervisionPolicy::default(),
    )
    .await
    .unwrap();
    let out = rt
        .execute(
            r#"try { await atoms.invoke("a", {}); return 'NO_THROW'; }
               catch (e) { return 'caught'; }"#,
        )
        .await
        .unwrap();
    assert_eq!(out, "\"caught\"");
    assert_eq!(rt.execute("return 6 * 7;").await.unwrap(), "42");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn capability_panic_refactor_surfaced_and_runtime_survives() {
    let rt = ScriptRuntime::new(
        caps_with(
            Arc::new(StubCorpus),
            Arc::new(StubAtoms),
            Arc::new(PanicRefactor),
        ),
        SupervisionPolicy::default(),
    )
    .await
    .unwrap();
    let out = rt
        .execute(
            r#"try { await refactor.plan({ kind: "k", input_json: {} }); return 'NO_THROW'; }
               catch (e) { return 'caught'; }"#,
        )
        .await
        .unwrap();
    assert_eq!(out, "\"caught\"");
    assert_eq!(rt.execute("return 6 * 7;").await.unwrap(), "42");
}

// A normal CapabilityResult error (not a panic) also surfaces as a catchable JS
// error carrying the BroError code/message.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn capability_error_surfaced_as_js_error() {
    let rt = ScriptRuntime::new(
        caps_with(
            Arc::new(ErrCorpus),
            Arc::new(StubAtoms),
            Arc::new(StubRefactor),
        ),
        SupervisionPolicy::default(),
    )
    .await
    .unwrap();
    let out = rt
        .execute(
            r#"try { await corpus.search({ query: "x", limit: 1 }); return 'NO_THROW'; }
               catch (e) { return String(e).includes('corpus_unavailable') ? 'coded' : 'caught'; }"#,
        )
        .await
        .unwrap();
    assert_eq!(out, "\"coded\"");
}

// ---------------------------------------------------------------------------
// Supervision: heap OOM, runaway-loop kill, execution timeout.
// ---------------------------------------------------------------------------

// Criterion #1 (the dangerous one): heap-bound OOM containment. THE TEST SURVIVES.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn heap_oom_containment_process_survives() {
    let policy = SupervisionPolicy {
        heap_limit_bytes: 24 * 1024 * 1024,
        execution_timeout: None,
    };
    let rt = ScriptRuntime::new(stub_caps(), policy).await.unwrap();
    let result = rt
        .execute("const a = []; for (;;) { a.push(new Array(10000).fill(0)); }")
        .await;
    assert!(
        result.is_err(),
        "runaway allocation must be terminated, got {result:?}"
    );
    assert!(
        rt.hit_heap_oom(),
        "near-heap-limit callback should have fired"
    );
    // Prove the process is healthy by doing more work on a fresh runtime.
    let fresh = stub_runtime().await;
    assert_eq!(fresh.execute("return 'alive';").await.unwrap(), "\"alive\"");
}

// Criterion #2: runaway-loop kill via an EXTERNAL cross-thread watchdog (timeout
// disabled so the raw IsolateHandle path is what kills it), plus the
// runtime-reusable-after-terminate finding.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn runaway_loop_killed_by_external_watchdog_and_runtime_reusable() {
    let policy = SupervisionPolicy {
        heap_limit_bytes: DEFAULT_HEAP_LIMIT_BYTES,
        execution_timeout: None,
    };
    let rt = ScriptRuntime::new(stub_caps(), policy).await.unwrap();
    let handle = rt.isolate_handle();

    let watchdog = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(500));
        handle.terminate_execution();
    });

    let result = rt.execute("while (true) {}").await;
    watchdog.join().unwrap();
    assert!(
        result.is_err(),
        "infinite loop must be terminated, got {result:?}"
    );

    // After the terminate state is cleared, the SAME runtime is reusable.
    assert_eq!(
        rt.execute("return 'reusable';").await.unwrap(),
        "\"reusable\""
    );
}

// Criterion #2 via the built-in supervisor: the execution timeout auto-kills a
// runaway script (no caller-supplied watchdog), and the runtime survives + stays
// reusable.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn execution_timeout_auto_kills_and_runtime_reusable() {
    let policy = SupervisionPolicy {
        heap_limit_bytes: DEFAULT_HEAP_LIMIT_BYTES,
        execution_timeout: Some(Duration::from_millis(300)),
    };
    let rt = ScriptRuntime::new(stub_caps(), policy).await.unwrap();

    let result = rt.execute("while (true) {}").await;
    assert!(
        result.is_err(),
        "runaway script must hit the execution timeout, got {result:?}"
    );
    let msg = format!("{:#}", result.unwrap_err());
    assert!(
        msg.contains("timed out"),
        "expected a timeout error, got: {msg}"
    );

    // The SAME runtime is reusable after an auto-kill.
    assert_eq!(rt.execute("return 'alive';").await.unwrap(), "\"alive\"");
    // And a fast capability call still completes well under the timeout.
    let out = rt
        .execute(r#"const r = await corpus.search({ query: "q", limit: 1 }); return r[0].text;"#)
        .await
        .unwrap();
    assert_eq!(out, "\"found:q\"");
}
