//! In-process capability bindings (harness-daemon-boundary.md §2/§6).
//!
//! When the daemon runs the harness in-process it *installs* concrete
//! [`bro_capabilities`] implementations backed by its in-memory stores. The
//! harness then exposes them as ordinary [`Tool`]s whose `call` is a direct
//! trait dispatch — no MCP round-trip, no wire serialization for blackbox's own
//! surfaces.
//!
//! The standalone `bro-harness` binary never installs anything, so the slot
//! stays empty and capability-backed tools are simply not registered: the
//! fail-closed behaviour the boundary doc requires (§2 — "the standalone binary
//! injects absent impls → corpus capabilities fail closed").

use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use async_trait::async_trait;
use bro_capabilities::{
    AtomCapability, AtomInvocation, CorpusCapability, CorpusLookup, RefactorCapability,
    RefactorRequest, ToolCallOutput, ToolCapability, ToolInvocation,
};
use bro_core::{AtomRef, BroError};
use bro_tools::{Tool, ToolCx, ToolResult};
use serde_json::{Value, json};

/// Process-global capability slots. The daemon is a singleton, so a single
/// installed implementation per capability is the whole story; standalone
/// leaves them `None`. Pushed by the daemon (`blackbox` → `bro-harness`), never
/// pulled — the harness keeps no dependency on the daemon.
static CORPUS: RwLock<Option<Arc<dyn CorpusCapability>>> = RwLock::new(None);
static ATOMS: RwLock<Option<Arc<dyn AtomCapability>>> = RwLock::new(None);
static REFACTOR: RwLock<Option<Arc<dyn RefactorCapability>>> = RwLock::new(None);

/// Install the daemon's in-memory corpus implementation. Called once, at daemon
/// startup, from the `blackbox` crate. Last writer wins.
pub fn install_corpus(capability: Arc<dyn CorpusCapability>) {
    *CORPUS.write().expect("corpus capability slot poisoned") = Some(capability);
}

/// Install the daemon's in-memory atom implementation. Called once, at daemon
/// startup, from the `blackbox` crate. Last writer wins.
pub fn install_atoms(capability: Arc<dyn AtomCapability>) {
    *ATOMS.write().expect("atom capability slot poisoned") = Some(capability);
}

/// Install the daemon's in-memory refactor implementation. Called once, at
/// daemon startup, from the `blackbox` crate. Last writer wins.
pub fn install_refactor(capability: Arc<dyn RefactorCapability>) {
    *REFACTOR.write().expect("refactor capability slot poisoned") = Some(capability);
}

fn corpus() -> Option<Arc<dyn CorpusCapability>> {
    CORPUS
        .read()
        .expect("corpus capability slot poisoned")
        .clone()
}

fn atoms() -> Option<Arc<dyn AtomCapability>> {
    ATOMS.read().expect("atom capability slot poisoned").clone()
}

fn refactor() -> Option<Arc<dyn RefactorCapability>> {
    REFACTOR
        .read()
        .expect("refactor capability slot poisoned")
        .clone()
}

/// The generic host built-in tool seam: a code-mode cell's `tools.*` call
/// dispatches here, and this runs the matching bro-tools built-in by name
/// against the per-session [`ToolCx`] — the same `Tool::call` path the flat
/// model-facing surface uses.
///
/// Deny-filter invariant: the callable set is gated by the **same** `ToolFilter`
/// as the flat surface (an unfiltered in-box surface would be a deny-bypass). The
/// caller constructs `HostTools` from the already-filtered built-in set, so a
/// denied capability is absent here and fails closed.
pub struct HostTools {
    tools: HashMap<String, Arc<dyn Tool>>,
    cx: ToolCx,
}

impl HostTools {
    /// Build the host-tool seam from a pre-filtered built-in set + the session
    /// context. `filtered_builtins` MUST already have had the session's
    /// `ToolFilter` applied by the caller; capability/control tools
    /// (`exec`, `wait`, `atom_invoke`, `report`, …) are intentionally NOT
    /// included — they are model-facing controls, not nested cell tools.
    pub fn new(filtered_builtins: Vec<Arc<dyn Tool>>, cx: ToolCx) -> Self {
        let tools = filtered_builtins
            .into_iter()
            .map(|t| (t.name().to_string(), t))
            .collect();
        Self { tools, cx }
    }
}

#[async_trait]
impl ToolCapability for HostTools {
    async fn call_tool(&self, invocation: ToolInvocation) -> Result<ToolCallOutput, BroError> {
        let tool = self.tools.get(&invocation.name).ok_or_else(|| {
            // Unknown OR filtered-out → fail closed (no in-box route around the
            // ToolFilter, §4.5).
            BroError::new(
                "tool_unavailable",
                format!(
                    "host tool '{}' is not available in-box (unknown or denied)",
                    invocation.name
                ),
            )
        })?;
        let (content, is_error, content_type) =
            match tool.call(invocation.input_json, &self.cx).await {
                ToolResult::Text(t) => (t, false, "text/plain"),
                ToolResult::Json(v) => (
                    serde_json::to_string(&v).unwrap_or_else(|_| v.to_string()),
                    false,
                    "application/json",
                ),
                ToolResult::Error(e) => (e, true, "text/plain"),
            };
        Ok(ToolCallOutput {
            content,
            is_error,
            content_type: content_type.to_string(),
        })
    }
}

/// Capability-backed tools to merge into the registry. Empty when nothing was
/// installed (standalone harness) → these surfaces fail closed by absence.
///
/// The authorial surface (cells) is now code-mode's `exec`/`wait`
/// (`crate::code_mode`), which supersedes the retired NARF tools. These remaining
/// tools are the direct trait-dispatch surfaces: corpus search, atom invoke,
/// refactor plan, and KV inspection.
pub fn capability_tools() -> Vec<Arc<dyn Tool>> {
    let mut tools: Vec<Arc<dyn Tool>> = Vec::new();
    if let Some(c) = corpus() {
        tools.push(Arc::new(CorpusSearchTool(c)));
    }
    if let Some(a) = atoms() {
        tools.push(Arc::new(AtomInvokeTool(a)));
    }
    if let Some(r) = refactor() {
        tools.push(Arc::new(RefactorPlanTool(r.clone())));
        tools.push(Arc::new(RefactorPlanGetTool(r)));
    }
    tools
}

/// `corpus_search`: ranked transcript/corpus lookup via a direct in-memory
/// trait call. This is the §6 "skip the wire" path — the model still speaks
/// JSON, but the result never crosses an MCP transport.
struct CorpusSearchTool(Arc<dyn CorpusCapability>);

#[async_trait]
impl Tool for CorpusSearchTool {
    fn name(&self) -> &str {
        "corpus_search"
    }

    fn description(&self) -> &str {
        "Search the indexed transcript/corpus in-process (no MCP round-trip). \
         Returns ranked hits, each with an id and an excerpt."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "query": { "type": "string", "description": "Search query." },
                "limit": {
                    "type": "integer",
                    "description": "Maximum hits to return (default 10, max 100).",
                    "minimum": 1
                }
            },
            "required": ["query"]
        })
    }

    async fn call(&self, input: Value, _cx: &ToolCx) -> ToolResult {
        let query = match input.get("query").and_then(Value::as_str) {
            Some(q) if !q.trim().is_empty() => q.to_string(),
            _ => return ToolResult::Error("corpus_search: `query` is required".into()),
        };
        let limit = input
            .get("limit")
            .and_then(Value::as_u64)
            .unwrap_or(10)
            .clamp(1, 100) as usize;
        match self.0.search_corpus(CorpusLookup { query, limit }).await {
            Ok(hits) => ToolResult::Json(json!({
                "hits": hits
                    .iter()
                    .map(|h| json!({ "id": h.id, "text": h.text }))
                    .collect::<Vec<_>>(),
            })),
            Err(e) => ToolResult::Error(format!("corpus_search failed: {}: {}", e.code, e.message)),
        }
    }
}

/// `atom_invoke`: dispatch a catalog atom via a direct in-memory trait call.
/// The trait signature is input-JSON → output-JSON and stays neutral on the
/// edit-effect (tx vs saga) question — that semantics lives behind the daemon
/// impl, not in the seam (harness-daemon-boundary.md §12).
struct AtomInvokeTool(Arc<dyn AtomCapability>);

#[async_trait]
impl Tool for AtomInvokeTool {
    fn name(&self) -> &str {
        "atom_invoke"
    }

    fn description(&self) -> &str {
        "Invoke a catalog atom in-process (no MCP round-trip). Provide the atom \
         ref and an args object; returns the atom's output JSON."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "atom": {
                    "type": "string",
                    "description": "Atom ref, e.g. \"atom:reviewer@v1\"."
                },
                "args": {
                    "type": "object",
                    "description": "Atom input arguments (schema-validated by the atom)."
                }
            },
            "required": ["atom"]
        })
    }

    async fn call(&self, input: Value, _cx: &ToolCx) -> ToolResult {
        let atom = match input.get("atom").and_then(Value::as_str) {
            Some(a) if !a.trim().is_empty() => a.to_string(),
            _ => return ToolResult::Error("atom_invoke: `atom` ref is required".into()),
        };
        let input_json = input.get("args").cloned().unwrap_or_else(|| json!({}));
        match self
            .0
            .invoke_atom(AtomInvocation {
                atom: AtomRef::new(atom),
                input_json,
            })
            .await
        {
            Ok(out) => ToolResult::Json(out.output_json),
            Err(e) => ToolResult::Error(format!("atom_invoke failed: {}: {}", e.code, e.message)),
        }
    }
}

/// `refactor_plan`: produce a dry-run plan via a direct trait call. The plan is
/// stored host-side; only the handle (id + preview) enters context (§6/§9).
struct RefactorPlanTool(Arc<dyn RefactorCapability>);

#[async_trait]
impl Tool for RefactorPlanTool {
    fn name(&self) -> &str {
        "refactor_plan"
    }

    fn description(&self) -> &str {
        "Create a dry-run structural refactor plan in-process (no MCP \
         round-trip). Returns a handle {id, preview}; the full plan stays \
         host-side — fetch it with refactor_plan_get."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "kind": {
                    "type": "string",
                    "description": "Refactor plan kind (pull sm-refactor for the catalog)."
                },
                "params": {
                    "type": "object",
                    "description": "Plan-kind parameters (source, target, item_names, ...)."
                }
            },
            "required": ["kind", "params"]
        })
    }

    async fn call(&self, input: Value, _cx: &ToolCx) -> ToolResult {
        let kind = match input.get("kind").and_then(Value::as_str) {
            Some(k) if !k.trim().is_empty() => k.to_string(),
            _ => return ToolResult::Error("refactor_plan: `kind` is required".into()),
        };
        let params = input.get("params").cloned().unwrap_or_else(|| json!({}));
        match self
            .0
            .plan_refactor(RefactorRequest {
                kind,
                input_json: params,
            })
            .await
        {
            Ok(handle) => ToolResult::Json(json!({
                "id": handle.id,
                "preview": handle.preview,
            })),
            Err(e) => ToolResult::Error(format!("refactor_plan failed: {}: {}", e.code, e.message)),
        }
    }
}

/// `refactor_plan_get`: materialize a host-side plan by its handle id.
struct RefactorPlanGetTool(Arc<dyn RefactorCapability>);

#[async_trait]
impl Tool for RefactorPlanGetTool {
    fn name(&self) -> &str {
        "refactor_plan_get"
    }

    fn description(&self) -> &str {
        "Materialize the full JSON of a refactor plan previously produced by \
         refactor_plan, by its handle id."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "id": { "type": "string", "description": "Plan handle id." }
            },
            "required": ["id"]
        })
    }

    async fn call(&self, input: Value, _cx: &ToolCx) -> ToolResult {
        let id = match input.get("id").and_then(Value::as_str) {
            Some(i) if !i.trim().is_empty() => i.to_string(),
            _ => return ToolResult::Error("refactor_plan_get: `id` is required".into()),
        };
        match self.0.materialize_plan(id).await {
            Ok(plan) => ToolResult::Json(plan),
            Err(e) => ToolResult::Error(format!(
                "refactor_plan_get failed: {}: {}",
                e.code, e.message
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bro_capabilities::{AtomOutput, CapabilityResult, CorpusHit, RefactorPlanHandle};
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Stub capability that records invocations — proves the tool dispatches to
    /// the injected trait object, not over any wire.
    struct StubCorpus {
        calls: AtomicUsize,
    }

    #[async_trait]
    impl CorpusCapability for StubCorpus {
        async fn search_corpus(&self, lookup: CorpusLookup) -> CapabilityResult<Vec<CorpusHit>> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(vec![CorpusHit {
                id: format!("hit-for-{}", lookup.query),
                text: format!("excerpt (limit {})", lookup.limit),
            }])
        }
    }

    fn test_cx() -> ToolCx {
        use std::sync::Mutex;
        // corpus_search ignores cx, so a minimal context is sufficient.
        ToolCx {
            root: std::env::temp_dir(),
            safety: Arc::new(bro_tools::SafetyPolicy::new()),
            http: reqwest::Client::new(),
            todos: Arc::new(Mutex::new(bro_tools::TodoList::default())),
            shell_sessions: Arc::new(Mutex::new(bro_tools::ShellSessions::default())),
            promises: Arc::new(Mutex::new(bro_tools::PromiseStore::default())),
            edits: Arc::new(Mutex::new(bro_tools::EditSink::default())),
            session_env: Arc::new(std::collections::BTreeMap::new()),
        }
    }

    #[tokio::test]
    async fn corpus_search_tool_dispatches_to_injected_capability() {
        let stub = Arc::new(StubCorpus {
            calls: AtomicUsize::new(0),
        });
        let tool = CorpusSearchTool(stub.clone());
        let result = tool
            .call(json!({ "query": "boundary", "limit": 3 }), &test_cx())
            .await;
        // The trait was actually invoked in-process — no MCP round-trip.
        assert_eq!(stub.calls.load(Ordering::SeqCst), 1);
        match result {
            ToolResult::Json(v) => {
                let hits = v.get("hits").and_then(Value::as_array).expect("hits array");
                assert_eq!(hits.len(), 1);
                assert_eq!(hits[0]["id"], "hit-for-boundary");
                assert_eq!(hits[0]["text"], "excerpt (limit 3)");
            }
            other => panic!("expected Json result, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn corpus_search_requires_query() {
        let tool = CorpusSearchTool(Arc::new(StubCorpus {
            calls: AtomicUsize::new(0),
        }));
        let result = tool.call(json!({ "limit": 3 }), &test_cx()).await;
        assert!(matches!(result, ToolResult::Error(_)));
    }

    /// Stub atom capability that echoes the ref + args it received.
    struct StubAtoms;

    #[async_trait]
    impl AtomCapability for StubAtoms {
        async fn invoke_atom(&self, invocation: AtomInvocation) -> CapabilityResult<AtomOutput> {
            Ok(AtomOutput {
                output_json: json!({
                    "atom": invocation.atom.as_str(),
                    "echo": invocation.input_json,
                }),
            })
        }
    }

    #[tokio::test]
    async fn atom_invoke_tool_dispatches_to_injected_capability() {
        let tool = AtomInvokeTool(Arc::new(StubAtoms));
        let result = tool
            .call(
                json!({ "atom": "atom:reviewer@v1", "args": { "x": 1 } }),
                &test_cx(),
            )
            .await;
        match result {
            ToolResult::Json(v) => {
                assert_eq!(v["atom"], "atom:reviewer@v1");
                assert_eq!(v["echo"]["x"], 1);
            }
            other => panic!("expected Json result, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn atom_invoke_requires_ref() {
        let tool = AtomInvokeTool(Arc::new(StubAtoms));
        let result = tool.call(json!({ "args": {} }), &test_cx()).await;
        assert!(matches!(result, ToolResult::Error(_)));
    }

    /// Stub refactor capability with a tiny host-side store, proving the
    /// plan→handle→materialize round-trip stays server-side.
    #[derive(Default)]
    struct StubRefactor {
        plans: std::sync::Mutex<std::collections::HashMap<String, Value>>,
    }

    #[async_trait]
    impl RefactorCapability for StubRefactor {
        async fn plan_refactor(
            &self,
            request: RefactorRequest,
        ) -> CapabilityResult<RefactorPlanHandle> {
            let id = format!("plan-{}", request.kind);
            let plan = json!({ "kind": request.kind, "params": request.input_json });
            self.plans.lock().unwrap().insert(id.clone(), plan);
            Ok(RefactorPlanHandle {
                id,
                preview: "1 edit".to_string(),
            })
        }

        async fn materialize_plan(&self, id: String) -> CapabilityResult<Value> {
            self.plans
                .lock()
                .unwrap()
                .get(&id)
                .cloned()
                .ok_or_else(|| bro_core::BroError::new("unknown_plan", id))
        }
    }

    #[tokio::test]
    async fn refactor_plan_handle_round_trips_host_side() {
        let cap = Arc::new(StubRefactor::default());
        let plan_tool = RefactorPlanTool(cap.clone());
        let get_tool = RefactorPlanGetTool(cap.clone());

        // plan_refactor returns only a handle — the full plan never enters here.
        let handle = plan_tool
            .call(
                json!({ "kind": "rust_lsp_rename", "params": { "source": "a.rs" } }),
                &test_cx(),
            )
            .await;
        let id = match handle {
            ToolResult::Json(v) => {
                assert_eq!(v["preview"], "1 edit");
                v["id"].as_str().unwrap().to_string()
            }
            other => panic!("expected Json handle, got {other:?}"),
        };

        // materialize dereferences the host-side plan by id.
        let plan = get_tool.call(json!({ "id": id }), &test_cx()).await;
        match plan {
            ToolResult::Json(v) => {
                assert_eq!(v["kind"], "rust_lsp_rename");
                assert_eq!(v["params"]["source"], "a.rs");
            }
            other => panic!("expected Json plan, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn refactor_plan_get_unknown_id_errors() {
        let cap = Arc::new(StubRefactor::default());
        let get_tool = RefactorPlanGetTool(cap);
        let result = get_tool.call(json!({ "id": "nope" }), &test_cx()).await;
        assert!(matches!(result, ToolResult::Error(_)));
    }

    #[tokio::test]
    async fn host_tools_filtered_set_fails_closed_on_denied() {
        // HostTools built from a filtered built-in set: file_read survives, but a
        // tool excluded by the filter (e.g. shell_run denied) is absent → calling
        // it in-box fails closed (no deny-bypass, §4.5).
        let filter = crate::mcp::ToolFilter::from_csv(Some("shell_run"), None);
        let allowed: Vec<Arc<dyn Tool>> = bro_tools::builtin_tools()
            .into_iter()
            .filter(|t| filter.permits(t.name()))
            .collect();
        let host = HostTools::new(allowed, test_cx());

        // file_read is permitted (no real file needed — it returns a tool error
        // for a missing path, which is is_error=true, NOT tool_unavailable).
        let read = host
            .call_tool(ToolInvocation {
                name: "file_read".to_string(),
                input_json: json!({ "file_path": "definitely-missing.xyz" }),
            })
            .await
            .expect("file_read is in the filtered set");
        assert!(read.is_error, "missing file → tool-level error");

        // shell_run was denied → absent from the in-box set → fail closed.
        let denied = host
            .call_tool(ToolInvocation {
                name: "shell_run".to_string(),
                input_json: json!({ "command": "echo nope" }),
            })
            .await;
        let err = denied.expect_err("denied tool must fail closed");
        assert_eq!(err.code, "tool_unavailable");
    }

}
