//! §5b spike acceptance tests. Each maps to one of the seven criteria.
//!
//! All tests use multi-thread tokio runtimes (the capability executor runs on
//! the outer runtime, the V8 isolate on its own dedicated thread). The whole
//! point of several of these is that **the test process survives** a condition
//! that would otherwise abort the daemon.

use std::sync::Arc;
use std::time::Duration;

use bro_script::{
    EchoCapability, ScriptCapability, ScriptRuntime, DEFAULT_HEAP_LIMIT_BYTES, DENO_CORE_VERSION,
};

fn echo_cap() -> Arc<dyn ScriptCapability> {
    Arc::new(EchoCapability)
}

// Criterion #1 (build proof, basic execution): deno_core builds and a trivial
// script round-trips a value.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn build_proof_basic_execution() {
    assert_eq!(DENO_CORE_VERSION, "0.403.0");
    let rt = ScriptRuntime::new(echo_cap(), DEFAULT_HEAP_LIMIT_BYTES)
        .await
        .unwrap();
    let out = rt.execute("return 1 + 1;").await.unwrap();
    assert_eq!(out, "2");
}

// Criterion #3: denied ambient globals.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn denied_globals() {
    let rt = ScriptRuntime::new(echo_cap(), DEFAULT_HEAP_LIMIT_BYTES)
        .await
        .unwrap();
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

// Criterion #4: async capability bridge. JS `await cap.echo(x)` hops to async
// Rust on the outer runtime and back.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn async_capability_bridge() {
    let rt = ScriptRuntime::new(echo_cap(), DEFAULT_HEAP_LIMIT_BYTES)
        .await
        .unwrap();
    let out = rt
        .execute(r#"const r = await cap.echo("hi"); return r;"#)
        .await
        .unwrap();
    // Serialized as a JSON string.
    assert_eq!(out, "\"echo:hi\"");
}

// Criterion #5: a panic inside an op handler is caught at the boundary and
// surfaced as a JS error — the process is NOT aborted by unwinding across V8.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn panic_boundary_surfaced_as_js_error() {
    let rt = ScriptRuntime::new(echo_cap(), DEFAULT_HEAP_LIMIT_BYTES)
        .await
        .unwrap();
    let out = rt
        .execute(
            "try { Deno.core.ops.op_panic_guarded(); return 'NO_THROW'; } \
             catch (e) { return 'caught'; }",
        )
        .await
        .unwrap();
    assert_eq!(
        out, "\"caught\"",
        "panic must surface as a catchable JS error"
    );
    // And the isolate is still usable afterwards — the process survived.
    let again = rt.execute("return 7 * 6;").await.unwrap();
    assert_eq!(again, "42");
}

// Criterion #1 (the dangerous one): heap-bound OOM containment. An unbounded
// allocator is terminated by the near-heap-limit callback; THE TEST SURVIVES.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn heap_oom_containment_process_survives() {
    // Small heap so it trips fast (kept above V8's practical floor).
    let rt = ScriptRuntime::new(echo_cap(), 24 * 1024 * 1024)
        .await
        .unwrap();
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
    // The decisive assertion is implicit: we are still running. Prove the
    // process is healthy by doing more work on a fresh runtime.
    let fresh = ScriptRuntime::new(echo_cap(), DEFAULT_HEAP_LIMIT_BYTES)
        .await
        .unwrap();
    assert_eq!(fresh.execute("return 'alive';").await.unwrap(), "\"alive\"");
}

// Criterion #2: runaway-loop kill via cross-thread terminate_execution, plus the
// runtime-reusable-after-terminate finding.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn runaway_loop_killed_and_runtime_reusable() {
    let rt = ScriptRuntime::new(echo_cap(), DEFAULT_HEAP_LIMIT_BYTES)
        .await
        .unwrap();
    let handle = rt.isolate_handle();

    // Watchdog: terminate the infinite loop from another thread after a delay.
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

    // FINDING: after we clear the terminate state (cancel_terminate_execution,
    // done by the V8 thread after each job), the SAME runtime is reusable.
    let reuse = rt.execute("return 'reusable';").await.unwrap();
    assert_eq!(reuse, "\"reusable\"");
}
