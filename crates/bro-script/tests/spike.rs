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
    AtomCapability, AtomInvocation, AtomOutput, CapabilityResult, KvCapability, KvEntryInfo, KvGet,
    KvOrigin, KvSummary, RefactorCapability, RefactorPlanHandle, RefactorRequest,
};
use bro_core::BroError;
use bro_script::{
    Capabilities, CellContract, DEFAULT_HEAP_LIMIT_BYTES, SCRIPT_RUNTIME_SUBSTRATE, ScriptRuntime,
    SupervisionPolicy, V8_VERSION,
};
use std::collections::BTreeMap;
use std::sync::Mutex;

// ---------------------------------------------------------------------------
// Stub capability impls — exercise the real traits without any daemon coupling.
// ---------------------------------------------------------------------------

struct StubAtoms;
#[async_trait]
impl AtomCapability for StubAtoms {
    async fn invoke_atom(&self, invocation: AtomInvocation) -> CapabilityResult<AtomOutput> {
        tokio::task::yield_now().await;
        let query = invocation
            .input_json
            .get("query")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default();
        let limit = invocation
            .input_json
            .get("limit")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or_default();
        Ok(AtomOutput {
            output_json: serde_json::json!({
                "atom": invocation.atom.as_str(),
                "echo": invocation.input_json,
                "hit": {
                    "id": format!("hit-{limit}"),
                    "text": format!("found:{query}"),
                },
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

// Stub host built-in tool seam (§5): echoes back the tool name + input so a test
// can prove the in-box `fs.*`/`shell.*`/... bindings route through hostCall
// to the injected ToolCapability and return values directly.
// A `name` of "boom" yields an is_error result to exercise the JS-throw path.
struct StubTools;
#[async_trait]
impl bro_capabilities::ToolCapability for StubTools {
    async fn call_tool(
        &self,
        invocation: bro_capabilities::ToolInvocation,
    ) -> CapabilityResult<bro_capabilities::ToolCallOutput> {
        tokio::task::yield_now().await;
        if invocation.name == "boom" {
            return Ok(bro_capabilities::ToolCallOutput {
                content: "the tool blew up".to_string(),
                is_error: true,
                content_type: "text/plain".to_string(),
            });
        }
        // Simulate the promise builtins so the in-box narf.promise.* bindings can
        // be exercised without a live PromiseStore producer.
        let ok = |v: serde_json::Value| {
            Ok(bro_capabilities::ToolCallOutput {
                content: v.to_string(),
                is_error: false,
                content_type: "application/json".to_string(),
            })
        };
        let inp = &invocation.input_json;
        match invocation.name.as_str() {
            // shell.run promise mode → small by-value ticket
            "shell_run" if inp.get("mode").and_then(|m| m.as_str()) == Some("promise") => {
                ok(serde_json::json!({ "promise_id": "pr-7", "running": true }))
            }
            "promise_when_all" => {
                let ids: Vec<String> = inp
                    .get("promise_ids")
                    .and_then(|v| serde_json::from_value(v.clone()).ok())
                    .unwrap_or_default();
                let promises: Vec<_> = ids
                    .iter()
                    .map(|id| {
                        serde_json::json!({
                            "promise_id": id, "state": "completed",
                            "result": { "stdout": "done" }
                        })
                    })
                    .collect();
                ok(serde_json::json!({ "promises": promises }))
            }
            "promise_when_any" => {
                let id = inp
                    .get("promise_ids")
                    .and_then(|v| v.as_array())
                    .and_then(|a| a.first())
                    .cloned()
                    .unwrap_or(serde_json::Value::Null);
                ok(serde_json::json!({ "promise": { "promise_id": id, "state": "completed" } }))
            }
            "promise_status" => ok(serde_json::json!({
                "promise_id": inp.get("promise_id"), "state": "completed"
            })),
            "promise_list" => {
                ok(serde_json::json!([{ "promise_id": "pr-7", "state": "completed" }]))
            }
            "promise_cancel" => ok(serde_json::json!({
                "promise_id": inp.get("promise_id"), "state": "cancelled"
            })),
            _ => ok(serde_json::json!({
                "tool": invocation.name,
                "input": invocation.input_json,
            })),
        }
    }
}

struct SlowTools;
#[async_trait]
impl bro_capabilities::ToolCapability for SlowTools {
    async fn call_tool(
        &self,
        _invocation: bro_capabilities::ToolInvocation,
    ) -> CapabilityResult<bro_capabilities::ToolCallOutput> {
        tokio::time::sleep(Duration::from_secs(1)).await;
        Ok(bro_capabilities::ToolCallOutput {
            content: "{}".to_string(),
            is_error: false,
            content_type: "application/json".to_string(),
        })
    }
}

#[derive(Default)]
struct StubKv {
    values: Mutex<BTreeMap<String, serde_json::Value>>,
}

fn kv_info(name: String, value: &serde_json::Value) -> KvEntryInfo {
    let rendered = value
        .as_str()
        .map(str::to_string)
        .unwrap_or_else(|| value.to_string());
    let lines: Vec<String> = rendered.lines().map(str::to_string).collect();
    KvEntryInfo {
        name,
        origin: KvOrigin::Agent,
        tags: None,
        content_type: "application/json".to_string(),
        size: serde_json::to_vec(value).unwrap().len(),
        summary: KvSummary {
            lines: lines.len().max(1),
            head: lines.iter().take(2).cloned().collect(),
            tail: lines.iter().rev().take(2).cloned().collect(),
            truncated: lines.len() > 4,
        },
    }
}

#[async_trait]
impl KvCapability for StubKv {
    async fn set(
        &self,
        name: String,
        value_json: serde_json::Value,
        _tags: Option<serde_json::Value>,
    ) -> CapabilityResult<KvEntryInfo> {
        let info = kv_info(name.clone(), &value_json);
        self.values.lock().unwrap().insert(name, value_json);
        Ok(info)
    }

    async fn get(&self, name: String, max_bytes: Option<usize>) -> CapabilityResult<KvGet> {
        let value = self
            .values
            .lock()
            .unwrap()
            .get(&name)
            .cloned()
            .ok_or_else(|| BroError::new("kv_missing", name.clone()))?;
        let size = serde_json::to_vec(&value).unwrap().len();
        if max_bytes.is_some_and(|m| size > m) {
            return Err(BroError::new("kv_value_too_large", "too large"));
        }
        Ok(KvGet {
            name,
            value_json: value,
            size,
        })
    }

    async fn peek(&self, name: String) -> CapabilityResult<KvEntryInfo> {
        let value = self
            .values
            .lock()
            .unwrap()
            .get(&name)
            .cloned()
            .ok_or_else(|| BroError::new("kv_missing", name.clone()))?;
        Ok(kv_info(name, &value))
    }

    async fn list(&self) -> CapabilityResult<Vec<KvEntryInfo>> {
        let values = self.values.lock().unwrap();
        Ok(values
            .iter()
            .map(|(name, value)| kv_info(name.clone(), value))
            .collect())
    }

    async fn delete(&self, name: String) -> CapabilityResult<bool> {
        Ok(self.values.lock().unwrap().remove(&name).is_some())
    }
}

// Panicking variants: prove a capability panic is contained on the executor and
// surfaces as a catchable JS error (the outer-runtime guard complementing the
// V8-thread structural guard).
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

// An error-returning atom capability: prove a normal CapabilityResult error surfaces as a
// catchable JS error too (distinct from a panic).
struct ErrAtoms;
#[async_trait]
impl AtomCapability for ErrAtoms {
    async fn invoke_atom(&self, _invocation: AtomInvocation) -> CapabilityResult<AtomOutput> {
        Err(BroError::new("atom_unavailable", "atom offline"))
    }
}

// An atom whose output carries a large body, proving Phase A returns values
// directly into the cell. Return-value caps are a later phase.
struct BigAtoms;
#[async_trait]
impl AtomCapability for BigAtoms {
    async fn invoke_atom(&self, _invocation: AtomInvocation) -> CapabilityResult<AtomOutput> {
        tokio::task::yield_now().await;
        Ok(AtomOutput {
            output_json: serde_json::json!({
                "id": "big",
                "text": "x".repeat(5000),
            }),
        })
    }
}

fn caps_with(
    atoms: Arc<dyn AtomCapability>,
    refactor: Arc<dyn RefactorCapability>,
) -> Capabilities {
    Capabilities {
        atoms,
        refactor,
        tools: None,
        kv: Arc::new(StubKv::default()),
    }
}

fn stub_caps() -> Capabilities {
    caps_with(Arc::new(StubAtoms), Arc::new(StubRefactor))
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
    assert_eq!(SCRIPT_RUNTIME_SUBSTRATE, "raw-v8");
    assert_eq!(V8_VERSION, "149.2.0");
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn encode_yaml_block_style_and_roundtrips() {
    let rt = stub_runtime().await;
    let out = rt
        .execute(r#"return narf.encode.yaml({ a: 1, b: "x" });"#)
        .await
        .unwrap();
    let yaml: String = serde_json::from_str(&out).unwrap();
    assert!(yaml.contains("a: 1"), "{yaml}");
    assert!(yaml.contains("b: x"), "{yaml}");

    let reparsed: serde_norway::Value = serde_norway::from_str(&yaml).unwrap();
    let expected: serde_norway::Value = serde_norway::from_str("a: 1\nb: x\n").unwrap();
    assert_eq!(reparsed, expected);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn encode_frontmatter_wraps_yaml_and_body() {
    let rt = stub_runtime().await;
    let out = rt
        .execute(r##"return narf.encode.frontmatter({ title: "T", tags: ["a", "b"] }, "# Body");"##)
        .await
        .unwrap();
    let doc: String = serde_json::from_str(&out).unwrap();
    assert!(doc.starts_with("---\n"), "{doc}");
    assert!(doc.contains("title: T\n"), "{doc}");
    assert!(doc.contains("tags:\n- a\n- b\n"), "{doc}");
    assert!(doc.contains("---\n\n# Body"), "{doc}");
    assert!(!doc.contains("\n\n---\n\n# Body"), "{doc}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn encode_markdown_table_from_objects() {
    let rt = stub_runtime().await;
    let out = rt
        .execute(
            r#"return narf.encode.mdTable([
                 { name: "a", n: 1, note: "x|y" },
                 { name: "b", n: 2, note: "z" }
               ]);"#,
        )
        .await
        .unwrap();
    let table: String = serde_json::from_str(&out).unwrap();
    assert!(table.starts_with("| name | n | note |\n| --- | --- | --- |\n"));
    assert!(table.contains("| a | 1 | x\\|y |"), "{table}");
    assert!(table.contains("| b | 2 | z |"), "{table}");
}

// ---------------------------------------------------------------------------
// Real capability bridges (criterion #4) — one per trait method.
// ---------------------------------------------------------------------------

// Capability ops return values directly into the cell.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn atom_invoke_lookup_returns_value() {
    let rt = stub_runtime().await;
    let out = rt
        .execute(
            r#"const r = await atoms.invoke("lookup", { query: "redis", limit: 2 });
               return JSON.stringify({
                 atom: r.atom,
                 echoQuery: r.echo.query,
                 hitText: r.hit.text,
               });"#,
        )
        .await
        .unwrap();
    assert_eq!(
        out,
        r#""{\"atom\":\"lookup\",\"echoQuery\":\"redis\",\"hitText\":\"found:redis\"}""#
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn atom_invoke_lookup_value_can_be_used_directly() {
    let rt = stub_runtime().await;
    let out = rt
        .execute(
            r#"const r = await atoms.invoke("lookup", { query: "redis", limit: 2 });
               return r.hit.text;"#,
        )
        .await
        .unwrap();
    assert_eq!(out, "\"found:redis\"");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn atom_invoke_returns_atom_output_value() {
    let rt = stub_runtime().await;
    let out = rt
        .execute(
            r#"const r = await atoms.invoke("my-atom", { x: 1 });
               return r.atom + ":" + r.echo.x;"#,
        )
        .await
        .unwrap();
    assert_eq!(out, "\"my-atom:1\"");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn refactor_plan_and_materialize_return_values() {
    let rt = stub_runtime().await;
    let out = rt
        .execute(
            r#"const h = await refactor.plan({ kind: "rename", input_json: { sym: "foo" } });
               const mat = await refactor.materialize(h.id);
               return h.preview + "|" + mat.materialized;"#,
        )
        .await
        .unwrap();
    assert_eq!(out, "\"preview of rename|plan-rename\"");
}

// ---------------------------------------------------------------------------
// v1 authoring surface: session helpers + prepare -> run. After the mislayer
// fix these are MODEL-FACING host methods (ScriptRuntime::define/prepare/run);
// only `narf.session.import` remains an in-box binding (recall by exact name).
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn session_define_then_import_roundtrip() {
    let rt = stub_runtime().await;
    // define is a model-facing control (out-of-cell)…
    rt.define(serde_json::json!({
        "name": "math",
        "source": "export function add(a, b) { return a + b; }",
        "exports": ["add"],
    }))
    .await
    .unwrap();
    // …import is the in-box dereference inside the cell.
    let out = rt
        .execute(r#"const math = narf.session.import("math"); return math.add(2, 3);"#)
        .await
        .unwrap();
    assert_eq!(out, "5");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn prepare_rejects_invalid_js_syntax() {
    let rt = stub_runtime().await;
    let resp = rt
        .prepare(serde_json::json!({ "source": "const =" }))
        .await
        .unwrap();
    assert_eq!(resp.status, "blocked");
    assert_eq!(resp.diagnostics[0].kind, "syntax");
    assert!(resp.ref_handle.is_none());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn prepare_rejects_unknown_import_alias() {
    let rt = stub_runtime().await;
    let resp = rt
        .prepare(serde_json::json!({ "imports": ["missingHelper"], "source": "return 1;" }))
        .await
        .unwrap();
    assert_eq!(resp.status, "blocked");
    assert_eq!(resp.diagnostics[0].kind, "import");
    assert!(resp.diagnostics[0].message.contains("missingHelper"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn prepare_run_composes_session_helper_and_capability_binding() {
    let rt = stub_runtime().await;
    rt.define(serde_json::json!({
        "name": "lookup",
        "source": "export async function run(query) { return await atoms.invoke(\"lookup\", { query, limit: 3 }); }",
        "exports": ["run"],
    }))
    .await
    .unwrap();
    let resp = rt
        .prepare(serde_json::json!({
            "imports": ["lookup"],
            "source": "return await lookup.run(\"narf\");",
        }))
        .await
        .unwrap();
    assert_eq!(resp.status, "ready");
    let handle = resp.ref_handle.clone().unwrap();
    assert!(handle.starts_with("narf-script:"));
    // prepare returns the rendered, import-assembled source for model review.
    assert!(resp.source.as_ref().unwrap().contains("atoms.invoke"));

    // run executes the prepared script; the cell's atoms.invoke yields a value.
    let result = rt.run(handle).await.unwrap();
    let v: serde_json::Value = serde_json::from_str(&result).unwrap();
    assert_eq!(v["hit"]["text"], "found:narf");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn prepare_validates_and_echoes_contract() {
    let rt = stub_runtime().await;
    let resp = rt
        .prepare(serde_json::json!({
            "source": "const run = async (input) => ({ count: input.repo.length }); return await run({ repo: \"abc\" });",
            "contract": {
                "entry": "run",
                "input": {
                    "type": "object",
                    "properties": { "repo": { "type": "string" } },
                    "required": ["repo"]
                },
                "output": {
                    "type": "object",
                    "properties": { "count": { "type": "integer" } },
                    "required": ["count"]
                },
                "effects": ["shell"],
                "may_invoke": ["atom:reviewer@v1"],
                "dispatch_budget": { "max_bros": 3, "max_depth": 2 }
            }
        }))
        .await
        .unwrap();

    assert_eq!(resp.status, "ready");
    let contract = resp.contract.as_ref().expect("contract echoed");
    assert_eq!(contract.entry, "run");
    assert_eq!(contract.effects, vec!["shell"]);
    assert_eq!(contract.may_invoke, vec!["atom:reviewer@v1"]);

    let result = rt.run(resp.ref_handle.unwrap()).await.unwrap();
    let v: serde_json::Value = serde_json::from_str(&result).unwrap();
    assert_eq!(v["count"], 3);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn run_with_input_invokes_prepared_contract_entry() {
    let rt = stub_runtime().await;
    let resp = rt
        .prepare(serde_json::json!({
            "source": "function run(input) { return { count: input.repo.length }; }",
            "contract": {
                "entry": "run",
                "input": {
                    "type": "object",
                    "properties": { "repo": { "type": "string" } },
                    "required": ["repo"],
                    "additionalProperties": false
                },
                "output": {
                    "type": "object",
                    "properties": { "count": { "type": "integer" } },
                    "required": ["count"],
                    "additionalProperties": false
                }
            }
        }))
        .await
        .unwrap();

    let result = rt
        .run_with_input(
            resp.ref_handle.unwrap(),
            serde_json::json!({ "repo": "blackbox" }),
        )
        .await
        .unwrap();
    let v: serde_json::Value = serde_json::from_str(&result).unwrap();
    assert_eq!(v["count"], 8);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn prepare_blocks_contract_with_missing_entry() {
    let rt = stub_runtime().await;
    let resp = rt
        .prepare(serde_json::json!({
            "source": "return 42;",
            "contract": { "entry": "run" }
        }))
        .await
        .unwrap();

    assert_eq!(resp.status, "blocked");
    assert_eq!(resp.diagnostics[0].kind, "contract");
    assert!(resp.diagnostics[0].message.contains("run"));
    assert!(resp.ref_handle.is_none());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn prepare_blocks_invalid_contract_json_schema() {
    let rt = stub_runtime().await;
    let resp = rt
        .prepare(serde_json::json!({
            "source": "function run(input) { return input; }",
            "contract": {
                "entry": "run",
                "input": { "type": "definitely-not-a-json-schema-type" }
            }
        }))
        .await
        .unwrap();

    assert_eq!(resp.status, "blocked");
    assert_eq!(resp.diagnostics[0].kind, "contract");
    assert!(resp.diagnostics[0].message.contains("contract.input"));
    assert!(resp.ref_handle.is_none());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn run_records_trace_entry() {
    let rt = stub_runtime().await;
    let resp = rt
        .prepare(serde_json::json!({ "source": "return 42;" }))
        .await
        .unwrap();
    rt.run(resp.ref_handle.unwrap()).await.unwrap();
    assert_eq!(rt.trace_len().await.unwrap(), 1);
}

// ---------------------------------------------------------------------------
// Structural panic guard (criterion #5) — sync callback and async host-call paths.
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn panic_boundary_sync_callback_surfaced_as_js_error() {
    let rt = stub_runtime().await;
    let out = rt
        .execute(
            "try { __bb_test.panicGuarded(); return 'NO_THROW'; } \
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
async fn panic_boundary_async_host_call_surfaced_as_js_error() {
    let rt = stub_runtime().await;
    let out = rt
        .execute(
            "try { await __bb_test.panicGuardedAsync(); return 'NO_THROW'; } \
             catch (e) { return 'caught'; }",
        )
        .await
        .unwrap();
    assert_eq!(
        out, "\"caught\"",
        "async host-call panic must surface as a catchable JS error"
    );
    assert_eq!(rt.execute("return 'alive';").await.unwrap(), "\"alive\"");
}

// A panic inside exposed capability paths is contained and surfaced as
// a catchable JS error, and the isolate stays usable.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn capability_panic_atom_surfaced_and_runtime_survives() {
    let rt = ScriptRuntime::new(
        caps_with(Arc::new(PanicAtoms), Arc::new(StubRefactor)),
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
        caps_with(Arc::new(StubAtoms), Arc::new(PanicRefactor)),
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
        caps_with(Arc::new(ErrAtoms), Arc::new(StubRefactor)),
        SupervisionPolicy::default(),
    )
    .await
    .unwrap();
    let out = rt
        .execute(
            r#"try { await atoms.invoke("lookup", { query: "x", limit: 1 }); return 'NO_THROW'; }
               catch (e) { return String(e).includes('atom_unavailable') ? 'coded' : 'caught'; }"#,
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
            r#"const r = await atoms.invoke("lookup", { query: "q", limit: 1 });
               return r.hit.text;"#,
        )
        .await
        .unwrap();
    assert_eq!(out, "\"found:q\"");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn host_call_timeout_does_not_strand_runtime_thread() {
    let policy = SupervisionPolicy {
        heap_limit_bytes: DEFAULT_HEAP_LIMIT_BYTES,
        execution_timeout: Some(Duration::from_millis(50)),
        ..SupervisionPolicy::default()
    };
    let caps = Capabilities {
        atoms: Arc::new(StubAtoms),
        refactor: Arc::new(StubRefactor),
        tools: Some(Arc::new(SlowTools)),
        kv: Arc::new(StubKv::default()),
    };
    let rt = ScriptRuntime::new(caps, policy).await.unwrap();

    let result = rt.execute(r#"return await fs.read("slow.txt");"#).await;
    assert!(
        result.is_err(),
        "slow host promise must hit the execution timeout, got {result:?}"
    );
    let msg = format!("{:#}", result.unwrap_err());
    assert!(
        msg.contains("timed out"),
        "expected a timeout error, got: {msg}"
    );

    assert_eq!(
        rt.execute("return 'after-host-timeout';").await.unwrap(),
        "\"after-host-timeout\""
    );
}

// ---------------------------------------------------------------------------
// Phase A value-return behavior.
// ---------------------------------------------------------------------------

fn big_caps() -> Capabilities {
    caps_with(Arc::new(BigAtoms), Arc::new(StubRefactor))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn large_result_returns_direct_value_in_phase_a() {
    let rt = ScriptRuntime::new(big_caps(), SupervisionPolicy::default())
        .await
        .unwrap();
    let out = rt
        .execute(
            r#"const r = await atoms.invoke("big", { query: "big", limit: 1 });
               return JSON.stringify({
                 id: r.id,
                 textLen: r.text.length,
               });"#,
        )
        .await
        .unwrap();
    assert_eq!(out, r#""{\"id\":\"big\",\"textLen\":5000}""#);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn kv_in_box_set_get_peek_delete_roundtrip() {
    let rt = stub_runtime().await;
    let out = rt
        .execute(
            r#"await narf.kv.set("note", "alpha\nbeta\ngamma\ndelta");
               const value = await narf.kv.get("note");
               const meta = await narf.kv.peek("note");
               const deleted = await narf.kv.delete("note");
               let missing = false;
               try { await narf.kv.get("note"); } catch (_) { missing = true; }
               return JSON.stringify({
                   value,
                   lines: meta.summary.lines,
                   first: meta.summary.head[0],
                   deleted,
                   missing,
               });"#,
        )
        .await
        .unwrap();
    let v: serde_json::Value =
        serde_json::from_str(&serde_json::from_str::<String>(&out).unwrap()).unwrap();
    assert_eq!(v["value"], "alpha\nbeta\ngamma\ndelta");
    assert_eq!(v["lines"], 4);
    assert_eq!(v["first"], "alpha");
    assert_eq!(v["deleted"], true);
    assert_eq!(v["missing"], true);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn kv_accumulates_across_execute_calls() {
    let rt = stub_runtime().await;
    rt.execute(r#"await narf.kv.set("records", []); return true;"#)
        .await
        .unwrap();
    let out = rt
        .execute(
            r#"const records = await narf.kv.get("records");
               records.push({ id: 1 });
               await narf.kv.set("records", records);
               return await narf.kv.get("records");"#,
        )
        .await
        .unwrap();
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(v, serde_json::json!([{ "id": 1 }]));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn kv_list_and_keys_are_absent_in_box() {
    let rt = stub_runtime().await;
    let out = rt
        .execute(
            r#"return JSON.stringify({
                list: typeof narf.kv.list,
                keys: typeof narf.kv.keys,
            });"#,
        )
        .await
        .unwrap();
    assert_eq!(out, r#""{\"list\":\"undefined\",\"keys\":\"undefined\"}""#);
}

// ---------------------------------------------------------------------------
// §5 host built-in tool seam: in-box fs.*/shell.*/search.*/git.*/web.* parity.
// ---------------------------------------------------------------------------

fn tools_caps() -> Capabilities {
    Capabilities {
        atoms: Arc::new(StubAtoms),
        refactor: Arc::new(StubRefactor),
        tools: Some(Arc::new(StubTools)),
        kv: Arc::new(StubKv::default()),
    }
}

async fn tools_runtime() -> ScriptRuntime {
    ScriptRuntime::new(tools_caps(), SupervisionPolicy::default())
        .await
        .unwrap()
}

// fs.read routes through hostCall to the injected ToolCapability and
// returns the tool value directly.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fs_read_routes_through_tool_seam_and_returns_value() {
    let rt = tools_runtime().await;
    let out = rt
        .execute(
            r#"const env = await fs.read("src/foo.rs");
               return JSON.stringify({
                   tool: env.tool,
                   filePath: env.input.file_path,
               });"#,
        )
        .await
        .unwrap();
    let v: serde_json::Value =
        serde_json::from_str(&serde_json::from_str::<String>(&out).unwrap()).unwrap();
    assert_eq!(v["tool"], "file_read");
    assert_eq!(v["filePath"], "src/foo.rs");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mcp_proxy_routes_by_fully_qualified_name_without_enumeration() {
    let rt = tools_runtime().await;
    let out = rt
        .execute(
            r#"const env = await mcp.blackbox.bbox_slice_read({ id: "slice-1" });
               return JSON.stringify({
                   tool: env.tool,
                   id: env.input.id,
                   rootKeys: Object.keys(mcp),
                   serverKeys: Object.keys(mcp.blackbox),
                   hasServer: "blackbox" in mcp,
                   hasTool: "bbox_slice_read" in mcp.blackbox,
               });"#,
        )
        .await
        .unwrap();
    let v: serde_json::Value =
        serde_json::from_str(&serde_json::from_str::<String>(&out).unwrap()).unwrap();
    assert_eq!(v["tool"], "mcp__blackbox__bbox_slice_read");
    assert_eq!(v["id"], "slice-1");
    assert_eq!(v["rootKeys"], serde_json::json!([]));
    assert_eq!(v["serverKeys"], serde_json::json!([]));
    assert_eq!(v["hasServer"], false);
    assert_eq!(v["hasTool"], false);
}

// shell.run / search.content / git.show ergonomic sugar maps the single string arg
// onto the tool's primary input field.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn host_tool_sugar_maps_primary_field() {
    let rt = tools_runtime().await;
    let out = rt
        .execute(
            r#"const sh = await shell.run("ls -la");
               const gr = await search.content("TODO");
               const gs = await git.show("HEAD");
               return JSON.stringify({
                   command: sh.input.command,
                   pattern: gr.input.pattern,
                   rev: gs.input.rev,
               });"#,
        )
        .await
        .unwrap();
    let v: serde_json::Value =
        serde_json::from_str(&serde_json::from_str::<String>(&out).unwrap()).unwrap();
    assert_eq!(v["command"], "ls -la");
    assert_eq!(v["pattern"], "TODO");
    assert_eq!(v["rev"], "HEAD");
}

// A tool that reports is_error surfaces as a catchable JS exception carrying the
// tool's message — a cell can try/catch and recover.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn host_tool_error_throws_catchable() {
    let rt = tools_runtime().await;
    let out = rt
        .execute(
            r#"let threw = false, msg = "";
               try { await fs.read("x"); } catch (_) {}
               try {
                   await __bb_host_call('tool.invoke', { name: "boom", input_json: {} });
               } catch (e) { threw = true; msg = String(e); }
               return JSON.stringify({ threw, hasMsg: msg.includes('blew up') });"#,
        )
        .await
        .unwrap();
    let v: serde_json::Value =
        serde_json::from_str(&serde_json::from_str::<String>(&out).unwrap()).unwrap();
    assert_eq!(v["threw"], true);
    assert_eq!(v["hasMsg"], true);
}

// With no ToolCapability installed (standalone / non-host runtime), the in-box
// host bindings fail closed: the call throws rather than silently succeeding.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn host_tools_fail_closed_when_absent() {
    let rt = stub_runtime().await; // tools: None
    let out = rt
        .execute(
            r#"let threw = false, msg = "";
               try { await fs.read("x"); }
               catch (e) { threw = true; msg = String(e); }
               return JSON.stringify({ threw, failClosed: msg.includes('host_tools_unavailable') || msg.includes('not installed') });"#,
        )
        .await
        .unwrap();
    let v: serde_json::Value =
        serde_json::from_str(&serde_json::from_str::<String>(&out).unwrap()).unwrap();
    assert_eq!(v["threw"], true);
    assert_eq!(v["failClosed"], true);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn registered_cell_call_validates_input_and_output_contract() {
    let rt = stub_runtime().await;
    let contract = CellContract {
        entry: "run".to_string(),
        input: Some(serde_json::json!({
            "type": "object",
            "properties": { "n": { "type": "integer" } },
            "required": ["n"],
            "additionalProperties": false
        })),
        output: Some(serde_json::json!({ "type": "integer" })),
        effects: Vec::new(),
        may_invoke: Vec::new(),
        dispatch_budget: None,
    };

    let out = rt
        .call_cell(
            "function run(input) { return input.n + 1; }".to_string(),
            contract.clone(),
            serde_json::json!({ "n": 41 }),
        )
        .await
        .unwrap();
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(v, serde_json::json!(42));

    let err = rt
        .call_cell(
            "function run(input) { return input.n + 1; }".to_string(),
            contract,
            serde_json::json!({ "n": "bad" }),
        )
        .await
        .expect_err("schema mismatch must fail closed");
    assert!(
        err.to_string()
            .contains("cell input failed schema validation"),
        "{err:#}"
    );
}

// ---------------------------------------------------------------------------
// §5 in-box promise primitive: narf.promise.{all,any,wait,status,list,cancel,pipeline}.
// ---------------------------------------------------------------------------

// shell.run(mode:'promise') returns a by-value {promise_id} ticket, and
// narf.promise.all joins by returning the producer values directly.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn promise_ticket_and_join_return_values() {
    let rt = tools_runtime().await;
    let out = rt
        .execute(
            r#"const h = await shell.run({ command: 'echo hi', mode: 'promise' });
               const joined = await narf.promise.all([h]);
               return JSON.stringify({
                   ticketInline: h.promise_id === 'pr-7',
                   firstResult: joined.promises[0].result.stdout,
                   joinedId: joined.promises[0].promise_id,
               });"#,
        )
        .await
        .unwrap();
    let v: serde_json::Value =
        serde_json::from_str(&serde_json::from_str::<String>(&out).unwrap()).unwrap();
    assert_eq!(v["ticketInline"], true);
    assert_eq!(v["firstResult"], "done");
    assert_eq!(v["joinedId"], "pr-7");
}

// status/list/cancel are small control snapshots → inline (by-value), and the
// handle normalizer accepts both a ticket object and a bare id string.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn promise_control_ops_are_inline() {
    let rt = tools_runtime().await;
    let out = rt
        .execute(
            r#"const h = await shell.run({ command: 'sleep 1', mode: 'promise' });
               const st = await narf.promise.status(h);          // ticket object
               const stById = await narf.promise.status('pr-9'); // bare id
               const list = await narf.promise.list();
               const cancelled = await narf.promise.cancel(h);
               return JSON.stringify({
                   statusState: st.state,
                   statusId: st.promise_id,
                   byIdId: stById.promise_id,
                   listLen: list.length,
                   cancelState: cancelled.state,
               });"#,
        )
        .await
        .unwrap();
    let v: serde_json::Value =
        serde_json::from_str(&serde_json::from_str::<String>(&out).unwrap()).unwrap();
    assert_eq!(v["statusState"], "completed");
    assert_eq!(v["statusId"], "pr-7");
    assert_eq!(v["byIdId"], "pr-9");
    assert_eq!(v["listLen"], 1);
    assert_eq!(v["cancelState"], "cancelled");
}

// narf.promise.any returns the first settled promise value.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn promise_any_returns_first() {
    let rt = tools_runtime().await;
    let out = rt
        .execute(
            r#"const env = await narf.promise.any(['pr-1', 'pr-2']);
               return JSON.stringify({ id: env.promise.promise_id, state: env.promise.state });"#,
        )
        .await
        .unwrap();
    let v: serde_json::Value =
        serde_json::from_str(&serde_json::from_str::<String>(&out).unwrap()).unwrap();
    assert_eq!(v["id"], "pr-1");
    assert_eq!(v["state"], "completed");
}

// narf.promise.pipeline is pure-JS no-barrier staging: each item flows through
// all stages independently; sync and async stages compose.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn promise_pipeline_stages_each_item() {
    let rt = tools_runtime().await;
    let out = rt
        .execute(
            r#"return await narf.promise.pipeline(
                   [1, 2, 3],
                   (x) => x + 1,
                   async (x) => x * 10,
               );"#,
        )
        .await
        .unwrap();
    // execute() serializes the result value; the array round-trips directly.
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(v, serde_json::json!([20, 30, 40]));
}
