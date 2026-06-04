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

// A corpus whose single hit carries a large body, to exercise the ref/egress
// boundary: the full value must stay host-side, only the envelope crosses, and
// `text()` egress is budget-bounded.
struct BigCorpus;
#[async_trait]
impl CorpusCapability for BigCorpus {
    async fn search_corpus(&self, _lookup: CorpusLookup) -> CapabilityResult<Vec<CorpusHit>> {
        tokio::task::yield_now().await;
        Ok(vec![CorpusHit {
            id: "big".to_string(),
            text: "x".repeat(5000),
        }])
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

// Each capability op now returns a `{ ref, size, preview }` envelope; the full
// value stays host-side and is retrievable ONLY via `narf.ref.text`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn corpus_search_returns_ref_envelope() {
    let rt = stub_runtime().await;
    let out = rt
        .execute(
            r#"const r = await corpus.search({ query: "redis", limit: 2 });
               return JSON.stringify({
                 hasRef: typeof r.ref === 'string' && r.ref.startsWith('ref:cap/'),
                 hasSize: typeof r.size === 'number' && r.size > 0,
                 previewHasText: r.preview.includes('found:redis'),
                 noFullValue: r[0] === undefined,
               });"#,
        )
        .await
        .unwrap();
    assert_eq!(
        out,
        r#""{\"hasRef\":true,\"hasSize\":true,\"previewHasText\":true,\"noFullValue\":true}""#
    );
}

// Ref round-trip: the handle materializes the REAL value via bounded egress.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn corpus_search_ref_roundtrip_via_text() {
    let rt = stub_runtime().await;
    let out = rt
        .execute(
            r#"const r = await corpus.search({ query: "redis", limit: 2 });
               const hits = JSON.parse(narf.ref.text(r));
               return hits[0].text;"#,
        )
        .await
        .unwrap();
    assert_eq!(out, "\"found:redis\"");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn atom_invoke_returns_envelope_and_materializes() {
    let rt = stub_runtime().await;
    let out = rt
        .execute(
            r#"const r = await atoms.invoke("my-atom", { x: 1 });
               if (r.output_json !== undefined) return 'LEAKED_FULL_VALUE';
               const v = JSON.parse(narf.ref.text(r));
               return v.output_json.atom + ":" + v.output_json.echo.x;"#,
        )
        .await
        .unwrap();
    assert_eq!(out, "\"my-atom:1\"");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn refactor_plan_and_materialize_via_refs() {
    let rt = stub_runtime().await;
    // plan() and materialize() both return ref envelopes; the real plan id is
    // only reachable through explicit egress of the plan ref.
    let out = rt
        .execute(
            r#"const h = await refactor.plan({ kind: "rename", input_json: { sym: "foo" } });
               const plan = JSON.parse(narf.ref.text(h));
               const m = await refactor.materialize(plan.id);
               const mat = JSON.parse(narf.ref.text(m));
               return plan.preview + "|" + mat.materialized;"#,
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
        ..SupervisionPolicy::default()
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
        ..SupervisionPolicy::default()
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
        ..SupervisionPolicy::default()
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
        .execute(
            r#"const r = await corpus.search({ query: "q", limit: 1 });
               return JSON.parse(narf.ref.text(r))[0].text;"#,
        )
        .await
        .unwrap();
    assert_eq!(out, "\"found:q\"");
}

// ---------------------------------------------------------------------------
// §9-1: Ref substrate + bounded egress.
// ---------------------------------------------------------------------------

fn big_caps() -> Capabilities {
    caps_with(
        Arc::new(BigCorpus),
        Arc::new(StubAtoms),
        Arc::new(StubRefactor),
    )
}

// `peek` returns metadata (kind, size, content type, preview) — never the bytes,
// and never charges egress budget.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ref_peek_returns_metadata_not_bytes() {
    let rt = stub_runtime().await;
    let out = rt
        .execute(
            r#"const r = await corpus.search({ query: "redis", limit: 1 });
               const m = narf.ref.peek(r);
               return JSON.stringify({
                 kind: m.kind,
                 hasSize: typeof m.size === 'number',
                 contentType: m.content_type,
                 hasPreview: typeof m.preview === 'string',
                 noValue: m.value === undefined,
               });"#,
        )
        .await
        .unwrap();
    assert_eq!(
        out,
        r#""{\"kind\":\"cap\",\"hasSize\":true,\"contentType\":\"application/json\",\"hasPreview\":true,\"noValue\":true}""#
    );
}

// A large capability result does NOT appear in the op's direct JS return: the
// envelope is tiny, while the (host-side) full value is large.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn large_result_does_not_enter_js_via_op_return() {
    let rt = ScriptRuntime::new(big_caps(), SupervisionPolicy::default())
        .await
        .unwrap();
    let out = rt
        .execute(
            r#"const r = await corpus.search({ query: "big", limit: 1 });
               const envelopeStr = JSON.stringify(r);
               return JSON.stringify({
                 envelopeSmall: envelopeStr.length < 1500,
                 sizeLarge: r.size > 4000,
                 noFullValue: r[0] === undefined,
               });"#,
        )
        .await
        .unwrap();
    assert_eq!(
        out,
        r#""{\"envelopeSmall\":true,\"sizeLarge\":true,\"noFullValue\":true}""#
    );
}

// The cumulative egress budget is enforced: once spent, a further `text()` that
// would exceed the remainder fails closed with a catchable JS error.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn egress_budget_enforced_cumulatively() {
    let policy = SupervisionPolicy {
        egress_budget_bytes: 4096,
        ..SupervisionPolicy::default()
    };
    let rt = ScriptRuntime::new(big_caps(), policy).await.unwrap();
    let out = rt
        .execute(
            r#"const r = await corpus.search({ query: "big", limit: 1 });
               const a = narf.ref.text(r, 4000);   // 4000 <= 4096 remaining: ok
               let threw = false, msg = '';
               try { narf.ref.text(r, 4000); }      // 4000 > 96 remaining: fails closed
               catch (e) { threw = true; msg = String(e); }
               return JSON.stringify({
                 firstLen: a.length,
                 threw,
                 budgetErr: msg.includes('egress budget'),
               });"#,
        )
        .await
        .unwrap();
    assert_eq!(
        out,
        r#""{\"firstLen\":4000,\"threw\":true,\"budgetErr\":true}""#
    );
}

// `text()` honors its per-call cap and the default cap, charging only what it
// returns; a small ref under the default cap materializes whole.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ref_text_respects_per_call_cap() {
    let rt = ScriptRuntime::new(big_caps(), SupervisionPolicy::default())
        .await
        .unwrap();
    let out = rt
        .execute(
            r#"const r = await corpus.search({ query: "big", limit: 1 });
               const capped = narf.ref.text(r, 100);   // explicit small cap
               const dflt = narf.ref.text(r);          // default 8 KiB cap >= size
               return JSON.stringify({
                 cappedLen: capped.length,
                 defaultLen: dflt.length,
                 fullSize: r.size,
                 defaultIsFull: dflt.length === r.size,
               });"#,
        )
        .await
        .unwrap();
    // size = serialized [{"id":"big","text":"x"*5000}] which is > 5000 bytes and
    // < the 8 KiB default cap, so the default-cap read returns the whole value.
    let v: serde_json::Value =
        serde_json::from_str(&serde_json::from_str::<String>(&out).unwrap()).unwrap();
    assert_eq!(v["cappedLen"], 100);
    assert_eq!(v["defaultIsFull"], true);
    assert_eq!(v["defaultLen"], v["fullSize"]);
}

// Unknown / wrong ref handles fail closed with a clean catchable JS error on both
// egress entry points.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unknown_ref_handle_errors() {
    let rt = stub_runtime().await;
    let out = rt
        .execute(
            r#"let textThrew = false, peekThrew = false;
               try { narf.ref.text("ref:cap/9999", 100); }
               catch (e) { textThrew = String(e).includes('unknown ref'); }
               try { narf.ref.peek("ref:cap/9999"); }
               catch (e) { peekThrew = String(e).includes('unknown ref'); }
               return JSON.stringify({ textThrew, peekThrew });"#,
        )
        .await
        .unwrap();
    assert_eq!(out, r#""{\"textThrew\":true,\"peekThrew\":true}""#);
}
