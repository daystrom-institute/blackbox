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

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, RwLock};

use async_trait::async_trait;
use bro_capabilities::{
    AtomCapability, AtomInvocation, CapabilityResult, CorpusCapability, CorpusLookup, KvCapability,
    KvEntry, KvEntryInfo, KvGet, KvOrigin, KvSummary, RefactorCapability, RefactorRequest,
    ToolCallOutput, ToolCapability, ToolInvocation,
};
use bro_core::{AtomRef, BroError};
use bro_tools::{Tool, ToolCx, ToolResult};
use serde_json::{json, Value};

const DEFAULT_KV_GET_MAX_BYTES: usize = 256 * 1024;
const KV_SUMMARY_LINES: usize = 2;
const KV_SUMMARY_LINE_BYTES: usize = 160;

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

// ---------------------------------------------------------------------------
// Session KV
// ---------------------------------------------------------------------------

/// Side-backed session KV store. The contract is home-agnostic in
/// `bro-capabilities`; this implementation rides the harness `side` spine for
/// exec/resume and daemon restart persistence.
#[derive(Debug, Default)]
pub struct KvStore {
    entries: std::sync::Mutex<BTreeMap<String, KvEntry>>,
}

impl KvStore {
    /// Restore from `side["narf_kv"]`. Tolerant: absent/garbage -> empty so old
    /// session files resume cleanly.
    pub fn from_side(v: &Value) -> Self {
        let entries: BTreeMap<String, KvEntry> = v
            .get("entries")
            .and_then(|r| serde_json::from_value(r.clone()).ok())
            .unwrap_or_default();
        Self {
            entries: std::sync::Mutex::new(entries),
        }
    }

    /// Serialize back into the `side` cell.
    pub fn to_side(&self) -> Value {
        let entries = self.entries.lock().map(|e| e.clone()).unwrap_or_default();
        json!({ "entries": entries })
    }

    fn make_entry(
        name: String,
        value_json: Value,
        tags: Option<Value>,
    ) -> Result<KvEntry, BroError> {
        let bytes = serde_json::to_vec(&value_json).map_err(|e| {
            BroError::new(
                "kv_serialize_failed",
                format!("failed to serialize KV value: {e}"),
            )
        })?;
        let rendered = value_json
            .as_str()
            .map(str::to_string)
            .unwrap_or_else(|| serde_json::to_string_pretty(&value_json).unwrap_or_default());
        let info = KvEntryInfo {
            name,
            origin: KvOrigin::Agent,
            tags,
            content_type: "application/json".to_string(),
            size: bytes.len(),
            summary: summarize_value(&rendered),
        };
        Ok(KvEntry { info, value_json })
    }

    fn get_entry(&self, name: &str) -> CapabilityResult<KvEntry> {
        self.entries
            .lock()
            .map_err(|_| BroError::new("kv_poisoned", "KV store lock poisoned"))?
            .get(name)
            .cloned()
            .ok_or_else(|| BroError::new("kv_missing", format!("KV entry not found: {name}")))
    }
}

fn byte_prefix(s: &str, max: usize) -> (&str, bool) {
    if s.len() <= max {
        return (s, false);
    }
    let mut end = max;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    (&s[..end], true)
}

fn summarize_value(rendered: &str) -> KvSummary {
    let raw_lines: Vec<&str> = if rendered.is_empty() {
        vec![""]
    } else {
        rendered.lines().collect()
    };
    let lines = raw_lines.len();
    let mut truncated = lines > KV_SUMMARY_LINES * 2;

    let mut bound = |line: &&str| {
        let (bounded, was_truncated) = byte_prefix(line, KV_SUMMARY_LINE_BYTES);
        truncated |= was_truncated;
        bounded.to_string()
    };

    let head = raw_lines
        .iter()
        .take(KV_SUMMARY_LINES)
        .map(&mut bound)
        .collect();
    let tail_start = lines.saturating_sub(KV_SUMMARY_LINES);
    let tail = raw_lines.iter().skip(tail_start).map(&mut bound).collect();

    KvSummary {
        lines,
        head,
        tail,
        truncated,
    }
}

#[async_trait]
impl KvCapability for KvStore {
    async fn set(
        &self,
        name: String,
        value_json: Value,
        tags: Option<Value>,
    ) -> CapabilityResult<KvEntryInfo> {
        if name.trim().is_empty() {
            return Err(BroError::new("kv_bad_name", "KV entry name is required"));
        }
        let entry = Self::make_entry(name.clone(), value_json, tags)?;
        let info = entry.info.clone();
        self.entries
            .lock()
            .map_err(|_| BroError::new("kv_poisoned", "KV store lock poisoned"))?
            .insert(name, entry);
        Ok(info)
    }

    async fn get(&self, name: String, max_bytes: Option<usize>) -> CapabilityResult<KvGet> {
        let entry = self.get_entry(&name)?;
        let limit = max_bytes
            .filter(|n| *n > 0)
            .unwrap_or(DEFAULT_KV_GET_MAX_BYTES);
        if entry.info.size > limit {
            return Err(BroError::new(
                "kv_value_too_large",
                format!(
                    "KV entry '{}' is {} bytes, over max_bytes {}",
                    entry.info.name, entry.info.size, limit
                ),
            ));
        }
        Ok(KvGet {
            name: entry.info.name,
            value_json: entry.value_json,
            size: entry.info.size,
        })
    }

    async fn peek(&self, name: String) -> CapabilityResult<KvEntryInfo> {
        Ok(self.get_entry(&name)?.info)
    }

    async fn list(&self) -> CapabilityResult<Vec<KvEntryInfo>> {
        let entries = self
            .entries
            .lock()
            .map_err(|_| BroError::new("kv_poisoned", "KV store lock poisoned"))?;
        Ok(entries.values().map(|e| e.info.clone()).collect())
    }

    async fn delete(&self, name: String) -> CapabilityResult<bool> {
        Ok(self
            .entries
            .lock()
            .map_err(|_| BroError::new("kv_poisoned", "KV store lock poisoned"))?
            .remove(&name)
            .is_some())
    }
}

/// The generic host built-in tool seam (`narf-tool-placement.md` §5): a NARF
/// cell's `fs.*`/`shell.*`/`search.*`/`git.*`/`web.*` in-box bindings dispatch
/// here, and this runs the matching pre-beta bro-tools built-in by name against
/// the per-session [`ToolCx`] — the same `Tool::call` path the flat model-facing
/// surface uses.
///
/// §4.5 invariant: the callable set is gated by the **same** `ToolFilter` as the
/// flat surface (an unfiltered in-box surface would be a deny-bypass). The
/// caller constructs `HostTools` from the already-filtered built-in set, so a
/// denied capability is absent here and fails closed.
pub struct HostTools {
    tools: HashMap<String, Arc<dyn Tool>>,
    cx: ToolCx,
}

impl HostTools {
    /// Build the host-tool seam from a pre-filtered built-in set + the session
    /// context. `filtered_builtins` MUST already have had the session's
    /// `ToolFilter` applied by the caller (§4.5); capability/control tools
    /// (`narf_exec`, `atom_invoke`, `report`, …) are intentionally NOT included —
    /// they have their own in-box bindings or are out-box-only.
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
/// `host_tools` is the per-session generic built-in seam (§5); when present it is
/// injected into the NARF runtime so a cell can call `fs.*`/`shell.*`/… in-box.
pub fn capability_tools(
    host_tools: Option<Arc<dyn ToolCapability>>,
    kv: Option<Arc<dyn KvCapability>>,
) -> Vec<Arc<dyn Tool>> {
    let mut tools: Vec<Arc<dyn Tool>> = Vec::new();
    if let Some(c) = corpus() {
        tools.push(Arc::new(CorpusSearchTool(c)));
    }
    let atom_cap = atoms();
    let refactor_cap = refactor();
    if let Some(a) = atom_cap.clone() {
        tools.push(Arc::new(AtomInvokeTool(a)));
    }
    if let Some(r) = refactor_cap.clone() {
        tools.push(Arc::new(RefactorPlanTool(r.clone())));
        tools.push(Arc::new(RefactorPlanGetTool(r)));
    }
    if let Some(kv) = kv.clone() {
        tools.push(Arc::new(NarfKvListTool(kv.clone())));
        tools.push(Arc::new(NarfKvPeekTool(kv.clone())));
        tools.push(Arc::new(NarfKvGetTool(kv)));
    }
    if let (Some(atoms), Some(refactor), Some(kv)) = (atom_cap, refactor_cap, kv) {
        // One shared per-session runtime behind the four model-facing NARF
        // controls, so helpers + prepared scripts persist across exec/prepare/
        // run/define (box-edge invariant, narf-capability-library.md §0.1).
        let session = Arc::new(NarfSession::new(atoms, refactor, host_tools, kv));
        tools.push(Arc::new(NarfExecTool {
            session: session.clone(),
        }));
        tools.push(Arc::new(NarfPrepareTool {
            session: session.clone(),
        }));
        tools.push(Arc::new(NarfRunTool {
            session: session.clone(),
        }));
        tools.push(Arc::new(NarfDefineTool { session }));
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

struct NarfKvListTool(Arc<dyn KvCapability>);

#[async_trait]
impl Tool for NarfKvListTool {
    fn name(&self) -> &str {
        "narf_kv_list"
    }

    fn description(&self) -> &str {
        "List NARF session KV entries by name with summaries only. This is the model-facing enumeration surface; in-box cells cannot list keys."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {}
        })
    }

    async fn call(&self, _input: Value, _cx: &ToolCx) -> ToolResult {
        match self.0.list().await {
            Ok(entries) => ToolResult::Json(json!({ "entries": entries })),
            Err(e) => ToolResult::Error(format!("narf_kv_list failed: {}: {}", e.code, e.message)),
        }
    }
}

struct NarfKvPeekTool(Arc<dyn KvCapability>);

#[async_trait]
impl Tool for NarfKvPeekTool {
    fn name(&self) -> &str {
        "narf_kv_peek"
    }

    fn description(&self) -> &str {
        "Inspect one NARF session KV entry by exact name. Returns metadata and summary, never the value."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "name": { "type": "string", "description": "Exact KV entry name." }
            },
            "required": ["name"]
        })
    }

    async fn call(&self, input: Value, _cx: &ToolCx) -> ToolResult {
        let name = match input.get("name").and_then(Value::as_str) {
            Some(n) if !n.trim().is_empty() => n.to_string(),
            _ => return ToolResult::Error("narf_kv_peek: `name` is required".into()),
        };
        match self.0.peek(name).await {
            Ok(entry) => ToolResult::Json(json!(entry)),
            Err(e) => ToolResult::Error(format!("narf_kv_peek failed: {}: {}", e.code, e.message)),
        }
    }
}

struct NarfKvGetTool(Arc<dyn KvCapability>);

#[async_trait]
impl Tool for NarfKvGetTool {
    fn name(&self) -> &str {
        "narf_kv_get"
    }

    fn description(&self) -> &str {
        "Get one NARF session KV value by exact name, bounded by max_bytes (default 256 KiB). Use list/peek first to choose keys."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "name": { "type": "string", "description": "Exact KV entry name." },
                "max_bytes": {
                    "type": "integer",
                    "description": "Maximum serialized JSON bytes to return (default 262144).",
                    "minimum": 1
                }
            },
            "required": ["name"]
        })
    }

    async fn call(&self, input: Value, _cx: &ToolCx) -> ToolResult {
        let name = match input.get("name").and_then(Value::as_str) {
            Some(n) if !n.trim().is_empty() => n.to_string(),
            _ => return ToolResult::Error("narf_kv_get: `name` is required".into()),
        };
        let max_bytes = input
            .get("max_bytes")
            .and_then(Value::as_u64)
            .map(|n| n as usize);
        match self.0.get(name, max_bytes).await {
            Ok(value) => ToolResult::Json(json!(value)),
            Err(e) => ToolResult::Error(format!("narf_kv_get failed: {}: {}", e.code, e.message)),
        }
    }
}

/// The per-session NARF runtime, shared by the four model-facing control tools
/// (`narf_exec`/`narf_prepare`/`narf_run`/`narf_define`). One lazily-built
/// `ScriptRuntime` per session means session helpers (`narf_define`) and prepared
/// scripts (`narf_prepare` → `narf_run`) persist across those tools' calls. The
/// box-edge invariant (`narf-capability-library.md` §0.1): authoring/launch
/// controls are model-facing tools, never in-box `narf.*` bindings.
struct NarfSession {
    atoms: Arc<dyn AtomCapability>,
    refactor: Arc<dyn RefactorCapability>,
    tools: Option<Arc<dyn ToolCapability>>,
    kv: Arc<dyn KvCapability>,
    runtime: tokio::sync::Mutex<Option<bro_script::ScriptRuntime>>,
}

impl NarfSession {
    fn new(
        atoms: Arc<dyn AtomCapability>,
        refactor: Arc<dyn RefactorCapability>,
        tools: Option<Arc<dyn ToolCapability>>,
        kv: Arc<dyn KvCapability>,
    ) -> Self {
        Self {
            atoms,
            refactor,
            tools,
            kv,
            runtime: tokio::sync::Mutex::new(None),
        }
    }

    /// Lock the session and lazily build the runtime. The returned guard holds
    /// the lock across the caller's runtime call, serializing the session's NARF
    /// tools onto its single V8 isolate.
    async fn ensure(
        &self,
    ) -> Result<tokio::sync::MutexGuard<'_, Option<bro_script::ScriptRuntime>>, String> {
        let mut guard = self.runtime.lock().await;
        if guard.is_none() {
            let caps = bro_script::Capabilities {
                atoms: self.atoms.clone(),
                refactor: self.refactor.clone(),
                tools: self.tools.clone(),
                kv: self.kv.clone(),
            };
            match bro_script::ScriptRuntime::new(caps, bro_script::SupervisionPolicy::default())
                .await
            {
                Ok(rt) => *guard = Some(rt),
                Err(e) => return Err(format!("narf runtime init failed: {e:#}")),
            }
        }
        Ok(guard)
    }
}

/// Serialize a cell/script string result: JSON when it parses, else text.
fn narf_result(output: String) -> ToolResult {
    match serde_json::from_str::<Value>(&output) {
        Ok(value) => ToolResult::Json(value),
        Err(_) => ToolResult::Text(output),
    }
}

/// `narf_exec`: run a NARF JavaScript composition cell against the session runtime.
struct NarfExecTool {
    session: Arc<NarfSession>,
}

#[async_trait]
impl Tool for NarfExecTool {
    fn name(&self) -> &str {
        "narf_exec"
    }

    fn description(&self) -> &str {
        "runs a NARF JS composition cell in-process; the cell composes capability/tool values and returns a value."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "source": {
                    "type": "string",
                    "description": "NARF JavaScript composition cell body."
                }
            },
            "required": ["source"]
        })
    }

    async fn call(&self, input: Value, _cx: &ToolCx) -> ToolResult {
        let source = match input.get("source").and_then(Value::as_str) {
            Some(s) if !s.trim().is_empty() => s.to_string(),
            _ => return ToolResult::Error("narf_exec: `source` is required".into()),
        };
        let guard = match self.session.ensure().await {
            Ok(g) => g,
            Err(e) => return ToolResult::Error(e),
        };
        match guard.as_ref().expect("runtime").execute(source).await {
            Ok(output) => narf_result(output),
            Err(e) => ToolResult::Error(format!("narf_exec failed: {e:#}")),
        }
    }
}

/// `narf_prepare`: render + syntax-validate a script (optionally importing session
/// helpers) and return BOTH a prepared-script handle AND the rendered source, so
/// the model reviews exactly what `narf_run` will execute (the §0.1 review step).
/// This is the model-facing replacement for the mislayered in-box `narf.prepare`.
struct NarfPrepareTool {
    session: Arc<NarfSession>,
}

#[async_trait]
impl Tool for NarfPrepareTool {
    fn name(&self) -> &str {
        "narf_prepare"
    }

    fn description(&self) -> &str {
        "Render + validate a NARF script (optionally importing session helpers) WITHOUT running it. Optionally validates and echoes a declared typed-cell contract. Returns {ref, status, diagnostics, source, contract} — review the rendered source, then narf_run the handle."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "source": { "type": "string", "description": "Script body." },
                "imports": {
                    "description": "Session helper names to inject (array), or {alias: name} map.",
                    "type": ["array", "object"]
                },
                "contract": {
                    "description": "Optional typed-cell contract. JSON Schema fields are validated, and entry must name a declared JS function/variable in source.",
                    "type": "object",
                    "properties": {
                        "entry": { "type": "string" },
                        "input": {
                            "description": "JSON Schema for the cell input.",
                            "type": ["object", "boolean"]
                        },
                        "output": {
                            "description": "JSON Schema for the cell output.",
                            "type": ["object", "boolean"]
                        },
                        "effects": {
                            "type": "array",
                            "items": { "type": "string" }
                        },
                        "may_invoke": {
                            "type": "array",
                            "items": { "type": "string" }
                        },
                        "dispatch_budget": {
                            "type": "object",
                            "additionalProperties": true
                        }
                    },
                    "required": ["entry"],
                    "additionalProperties": false
                }
            },
            "required": ["source"]
        })
    }

    async fn call(&self, input: Value, _cx: &ToolCx) -> ToolResult {
        if input.get("source").and_then(Value::as_str).is_none() {
            return ToolResult::Error("narf_prepare: `source` is required".into());
        }
        let guard = match self.session.ensure().await {
            Ok(g) => g,
            Err(e) => return ToolResult::Error(e),
        };
        match guard.as_ref().expect("runtime").prepare(input).await {
            Ok(resp) => match serde_json::to_value(&resp) {
                Ok(v) => ToolResult::Json(v),
                Err(e) => ToolResult::Error(format!("narf_prepare serialize failed: {e}")),
            },
            Err(e) => ToolResult::Error(format!("narf_prepare failed: {e:#}")),
        }
    }
}

/// `narf_run`: execute a prepared script by handle and return its result.
/// Model-facing replacement for the mislayered in-box `narf.run`.
struct NarfRunTool {
    session: Arc<NarfSession>,
}

#[async_trait]
impl Tool for NarfRunTool {
    fn name(&self) -> &str {
        "narf_run"
    }

    fn description(&self) -> &str {
        "Run a prepared NARF script by its handle (from narf_prepare); returns the script's result value."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "ref": { "type": "string", "description": "Prepared script handle." }
            },
            "required": ["ref"]
        })
    }

    async fn call(&self, input: Value, _cx: &ToolCx) -> ToolResult {
        let handle = match input.get("ref").and_then(Value::as_str) {
            Some(h) if !h.trim().is_empty() => h.to_string(),
            _ => return ToolResult::Error("narf_run: `ref` is required".into()),
        };
        let guard = match self.session.ensure().await {
            Ok(g) => g,
            Err(e) => return ToolResult::Error(e),
        };
        match guard.as_ref().expect("runtime").run(handle).await {
            Ok(output) => narf_result(output),
            Err(e) => ToolResult::Error(format!("narf_run failed: {e:#}")),
        }
    }
}

/// `narf_define`: register a reusable session helper that later cells recall in-box
/// via `narf.session.import(name)`. Authoring is a control → model-facing.
struct NarfDefineTool {
    session: Arc<NarfSession>,
}

#[async_trait]
impl Tool for NarfDefineTool {
    fn name(&self) -> &str {
        "narf_define"
    }

    fn description(&self) -> &str {
        "Register a reusable NARF session helper {name, source, exports}. Later cells recall it in-box with narf.session.import(name) — keeping the helper source out of context."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "name": { "type": "string", "description": "Helper name (JS identifier)." },
                "source": { "type": "string", "description": "Helper module source (uses `export`)." },
                "exports": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Exported binding names to expose on import."
                }
            },
            "required": ["name", "source", "exports"]
        })
    }

    async fn call(&self, input: Value, _cx: &ToolCx) -> ToolResult {
        let guard = match self.session.ensure().await {
            Ok(g) => g,
            Err(e) => return ToolResult::Error(e),
        };
        match guard.as_ref().expect("runtime").define(input).await {
            Ok(()) => ToolResult::Json(json!({ "ok": true })),
            Err(e) => ToolResult::Error(format!("narf_define failed: {e:#}")),
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

    fn narf_session_with(
        atoms: Arc<dyn AtomCapability>,
        tools: Option<Arc<dyn ToolCapability>>,
    ) -> Arc<NarfSession> {
        Arc::new(NarfSession::new(
            atoms,
            Arc::new(StubRefactor::default()),
            tools,
            Arc::new(KvStore::default()),
        ))
    }

    fn narf_tool(atoms: Arc<dyn AtomCapability>) -> NarfExecTool {
        NarfExecTool {
            session: narf_session_with(atoms, None),
        }
    }

    #[tokio::test]
    async fn narf_exec_runs_trivial_cell() {
        let tool = narf_tool(Arc::new(StubAtoms));

        let result = tool
            .call(json!({ "source": "return 1 + 1;" }), &test_cx())
            .await;

        match result {
            ToolResult::Json(v) => assert_eq!(v, json!(2)),
            other => panic!("expected Json result, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn narf_exec_runs_atom_binding_and_returns_value() {
        let tool = narf_tool(Arc::new(StubAtoms));

        let result = tool
            .call(
                json!({
                    "source": "const r = await atoms.invoke('atom:x@v1', {}); return r;"
                }),
                &test_cx(),
            )
            .await;

        match result {
            ToolResult::Json(v) => {
                assert_eq!(v["atom"], "atom:x@v1");
                assert_eq!(v["echo"], json!({}));
            }
            other => panic!("expected Json result, got {other:?}"),
        }
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

    #[tokio::test]
    async fn narf_exec_reads_file_through_host_tool_seam() {
        // End-to-end: a NARF cell calls fs.read (in-box) → op_tool_invoke →
        // HostTools → real FileRead built-in → value returned to the cell.
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        std::fs::write(root.join("hello.txt"), "in-box bytes\n").unwrap();

        let mut cx = test_cx();
        cx.root = root.clone();
        let host: Arc<dyn ToolCapability> =
            Arc::new(HostTools::new(bro_tools::builtin_tools(), cx));

        let tool = NarfExecTool {
            session: narf_session_with(Arc::new(StubAtoms), Some(host)),
        };

        let result = tool
            .call(
                json!({
                    "source": "const env = await fs.read('hello.txt'); \
                               return env;"
                }),
                &test_cx(),
            )
            .await;

        match result {
            ToolResult::Json(v) => {
                let body = v.as_str().expect("fs.read returns a string");
                assert!(body.contains("in-box bytes"), "got: {body}");
            }
            other => panic!("expected Json result, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn narf_exec_mcp_proxy_uses_host_map_and_fails_closed() {
        struct EchoMcpTool;
        #[async_trait]
        impl Tool for EchoMcpTool {
            fn name(&self) -> &str {
                "mcp__blackbox__placed"
            }
            fn description(&self) -> &str {
                "placed test MCP tool"
            }
            fn input_schema(&self) -> Value {
                json!({"type": "object", "properties": {}})
            }
            async fn call(&self, input: Value, _cx: &ToolCx) -> ToolResult {
                ToolResult::Json(json!({
                    "called": self.name(),
                    "input": input,
                }))
            }
        }

        let host: Arc<dyn ToolCapability> =
            Arc::new(HostTools::new(vec![Arc::new(EchoMcpTool)], test_cx()));
        let tool = NarfExecTool {
            session: narf_session_with(Arc::new(StubAtoms), Some(host)),
        };

        let result = tool
            .call(
                json!({
                    "source": "const ok = await mcp.blackbox.placed({ x: 1 }); \
                               let denied = false; \
                               try { await mcp.blackbox.unplaced({ x: 2 }); } \
                               catch (e) { denied = String(e).includes('tool_unavailable'); } \
                               return { called: ok.called, x: ok.input.x, denied };"
                }),
                &test_cx(),
            )
            .await;

        match result {
            ToolResult::Json(v) => {
                assert_eq!(v["called"], "mcp__blackbox__placed");
                assert_eq!(v["x"], 1);
                assert_eq!(v["denied"], true);
            }
            other => panic!("expected Json result, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn kv_store_side_round_trip_preserves_entries() {
        let kv = KvStore::default();
        kv.set(
            "records".to_string(),
            json!([{ "id": 1 }]),
            Some(json!({ "source": "test" })),
        )
        .await
        .unwrap();

        let blob = kv.to_side();
        let restored = KvStore::from_side(&blob);
        let entry = restored.peek("records".to_string()).await.unwrap();
        assert_eq!(entry.name, "records");
        assert_eq!(entry.origin, KvOrigin::Agent);
        assert_eq!(entry.tags.unwrap()["source"], "test");
        assert!(entry.size > 0);

        let value = restored.get("records".to_string(), None).await.unwrap();
        assert_eq!(value.value_json, json!([{ "id": 1 }]));
    }

    #[tokio::test]
    async fn narf_exec_kv_roundtrip_accumulates_and_guards_list() {
        let session = narf_session_with(Arc::new(StubAtoms), None);
        let exec = NarfExecTool { session };

        let first = exec
            .call(
                json!({
                    "source": "await narf.kv.set('records', []); return await narf.kv.peek('records');"
                }),
                &test_cx(),
            )
            .await;
        match first {
            ToolResult::Json(v) => {
                assert_eq!(v["name"], "records");
                assert_eq!(v["summary"]["lines"], 1);
            }
            other => panic!("expected Json result, got {other:?}"),
        }

        let second = exec
            .call(
                json!({
                    "source": "const records = await narf.kv.get('records'); \
                               records.push({ id: 7 }); \
                               await narf.kv.set('records', records); \
                               return { records: await narf.kv.get('records'), \
                                        hasList: typeof narf.kv.list, \
                                        hasKeys: typeof narf.kv.keys };"
                }),
                &test_cx(),
            )
            .await;
        match second {
            ToolResult::Json(v) => {
                assert_eq!(v["records"], json!([{ "id": 7 }]));
                assert_eq!(v["hasList"], "undefined");
                assert_eq!(v["hasKeys"], "undefined");
            }
            other => panic!("expected Json result, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn narf_exec_kv_delete_removes_entry() {
        let tool = narf_tool(Arc::new(StubAtoms));
        let result = tool
            .call(
                json!({
                    "source": "await narf.kv.set('tmp', 'value'); \
                               const deleted = await narf.kv.delete('tmp'); \
                               let missing = false; \
                               try { await narf.kv.peek('tmp'); } catch (_) { missing = true; } \
                               return { deleted, missing };"
                }),
                &test_cx(),
            )
            .await;
        match result {
            ToolResult::Json(v) => {
                assert_eq!(v["deleted"], true);
                assert_eq!(v["missing"], true);
            }
            other => panic!("expected Json result, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn narf_kv_outbox_tools_list_peek_and_get_bounded() {
        let kv = Arc::new(KvStore::default());
        kv.set(
            "memo".to_string(),
            json!("alpha\nbeta\ngamma\ndelta\nepsilon"),
            None,
        )
        .await
        .unwrap();

        let list = NarfKvListTool(kv.clone()).call(json!({}), &test_cx()).await;
        match list {
            ToolResult::Json(v) => {
                let entries = v["entries"].as_array().expect("entries array");
                assert_eq!(entries.len(), 1);
                assert_eq!(entries[0]["name"], "memo");
                assert_eq!(entries[0]["summary"]["head"][0], "alpha");
                assert_eq!(entries[0]["summary"]["truncated"], true);
            }
            other => panic!("expected Json list, got {other:?}"),
        }

        let peek = NarfKvPeekTool(kv.clone())
            .call(json!({ "name": "memo" }), &test_cx())
            .await;
        match peek {
            ToolResult::Json(v) => {
                assert_eq!(v["name"], "memo");
                assert!(v.get("value_json").is_none(), "peek must not return value");
            }
            other => panic!("expected Json peek, got {other:?}"),
        }

        let too_small = NarfKvGetTool(kv.clone())
            .call(json!({ "name": "memo", "max_bytes": 4 }), &test_cx())
            .await;
        assert!(matches!(too_small, ToolResult::Error(_)));

        let get = NarfKvGetTool(kv)
            .call(json!({ "name": "memo", "max_bytes": 1024 }), &test_cx())
            .await;
        match get {
            ToolResult::Json(v) => {
                assert_eq!(v["name"], "memo");
                assert_eq!(v["value_json"], "alpha\nbeta\ngamma\ndelta\nepsilon");
            }
            other => panic!("expected Json get, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn narf_define_then_exec_import_shares_session_runtime() {
        // The corrected authoring flow (mislayer fix): narf_define is a
        // model-facing control; the cell recalls the helper in-box via
        // session.import. Both tools share one session runtime, so the helper
        // defined out-of-cell is visible to the later cell.
        let session = narf_session_with(Arc::new(StubAtoms), None);
        let define = NarfDefineTool {
            session: session.clone(),
        };
        let exec = NarfExecTool { session };

        let defined = define
            .call(
                json!({
                    "name": "math",
                    "source": "export function add(a, b) { return a + b; }",
                    "exports": ["add"],
                }),
                &test_cx(),
            )
            .await;
        match defined {
            ToolResult::Json(v) => assert_eq!(v["ok"], true),
            other => panic!("expected Json ok, got {other:?}"),
        }

        let reuse = exec
            .call(
                json!({
                    "source": "const math = narf.session.import('math'); return math.add(2, 3);"
                }),
                &test_cx(),
            )
            .await;
        match reuse {
            ToolResult::Json(v) => assert_eq!(v, json!(5)),
            other => panic!("expected Json result, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn narf_prepare_returns_source_then_run_executes() {
        // 2-step authoring: narf_prepare returns the rendered source for review +
        // a handle; narf_run executes it. Shared session runtime.
        let session = narf_session_with(Arc::new(StubAtoms), None);
        let prepare = NarfPrepareTool {
            session: session.clone(),
        };
        let run = NarfRunTool { session };

        let prepared = prepare
            .call(json!({ "source": "return 6 * 7;" }), &test_cx())
            .await;
        let handle = match prepared {
            ToolResult::Json(v) => {
                assert_eq!(v["status"], "ready");
                // prepare returns the rendered source to the model's context.
                assert!(v["source"].as_str().unwrap().contains("6 * 7"));
                v["ref"].as_str().unwrap().to_string()
            }
            other => panic!("expected Json prepare, got {other:?}"),
        };
        assert!(handle.starts_with("narf-script:"));

        let result = run.call(json!({ "ref": handle }), &test_cx()).await;
        match result {
            ToolResult::Json(v) => assert_eq!(v, json!(42)),
            other => panic!("expected Json result, got {other:?}"),
        }
    }
}
