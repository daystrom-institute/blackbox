//! bro-script — raw V8 NARF script runtime.
//!
//! This crate embeds V8 directly, with a Blackbox-owned host-call ABI rather
//! than Deno ops. The runtime shape is intentionally close to Codex code-mode:
//! a live V8 activation creates JS promises for host calls, Rust capability work
//! runs outside the isolate, and the result resolves/rejects the same JS promise
//! back inside the live activation.
//!
//! Durability is deliberately not hidden in `await`. Plain JS promises are
//! live-activation promises. Cross-turn or cross-restart waiting belongs above
//! this crate as explicit daemon-owned durable handles.

use std::cell::RefCell;
use std::collections::HashMap;
use std::ffi::c_void;
use std::future::Future;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc as std_mpsc, Arc, OnceLock};
use std::thread::JoinHandle;
use std::time::Duration;

use anyhow::{anyhow, Result};
use jsonschema::{Draft, JSONSchema};
use serde::{Deserialize, Serialize};
use serde_json::Value as JsonValue;
use tokio::sync::{mpsc, oneshot};

pub use bro_capabilities::{
    AtomCapability, AtomInvocation, AtomOutput, CapabilityResult, KvCapability, KvEntryInfo, KvGet,
    RefactorCapability, RefactorPlanHandle, RefactorRequest, ToolCallOutput, ToolCapability,
    ToolInvocation,
};
use bro_core::BroError;

/// Runtime substrate reported by tests and diagnostics.
pub const SCRIPT_RUNTIME_SUBSTRATE: &str = "raw-v8";

/// Direct V8 crate version this runtime pins.
pub const V8_VERSION: &str = "149.2.0";

/// Default per-isolate hard heap ceiling (bytes).
pub const DEFAULT_HEAP_LIMIT_BYTES: usize = 256 * 1024 * 1024;

/// Default wall-clock ceiling for a single execution job.
pub const DEFAULT_EXECUTION_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Clone)]
pub struct Capabilities {
    pub atoms: Arc<dyn AtomCapability>,
    pub refactor: Arc<dyn RefactorCapability>,
    pub tools: Option<Arc<dyn ToolCapability>>,
    pub kv: Arc<dyn KvCapability>,
}

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

#[derive(Default)]
struct PreparedScripts {
    next_id: u64,
    scripts: HashMap<String, PreparedCell>,
}

impl PreparedScripts {
    fn put(&mut self, source: String, contract: Option<CellContract>) -> String {
        let id = self.next_id;
        self.next_id += 1;
        let handle = format!("narf-script:{id}");
        self.scripts
            .insert(handle.clone(), PreparedCell { source, contract });
        handle
    }

    fn get(&self, handle: &str) -> Result<PreparedCell, String> {
        self.scripts
            .get(handle)
            .cloned()
            .ok_or_else(|| format!("unknown prepared script handle: {handle}"))
    }
}

type PreparedScriptsCell = Rc<RefCell<PreparedScripts>>;
type SessionStateCell = Rc<RefCell<SessionState>>;
type TraceStateCell = Rc<RefCell<Vec<TraceEntry>>>;

#[derive(Clone, Debug)]
pub struct SessionHelper {
    pub source: String,
    pub exports: Vec<String>,
}

#[derive(Clone, Debug, Default)]
pub struct SessionState {
    pub helpers: HashMap<String, SessionHelper>,
    pub import_aliases: HashMap<String, String>,
}

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

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CellContract {
    pub entry: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub input: Option<JsonValue>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output: Option<JsonValue>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub effects: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub may_invoke: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dispatch_budget: Option<JsonValue>,
}

#[derive(Clone, Debug, Serialize)]
pub struct PrepareResponse {
    #[serde(rename = "ref", skip_serializing_if = "Option::is_none")]
    pub ref_handle: Option<String>,
    pub status: String,
    pub diagnostics: Vec<PrepareDiagnostic>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub contract: Option<CellContract>,
}

#[derive(Clone, Debug, Serialize)]
pub struct PreparedCell {
    pub source: String,
    pub contract: Option<CellContract>,
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
    #[serde(default)]
    contract: Option<CellContract>,
}

#[derive(Deserialize)]
struct KvSetInput {
    name: String,
    value_json: JsonValue,
    #[serde(default)]
    tags: Option<JsonValue>,
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
        contract: None,
    }
}

fn ready(handle: String, source: String, contract: Option<CellContract>) -> PrepareResponse {
    PrepareResponse {
        ref_handle: Some(handle),
        status: "ready".to_string(),
        diagnostics: vec![],
        source: Some(source),
        contract,
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

fn validate_cell_contract(contract: &CellContract, source: &str) -> Result<(), String> {
    if !is_js_identifier(&contract.entry) {
        return Err(format!(
            "contract.entry must be a JavaScript identifier, got `{}`",
            contract.entry
        ));
    }
    if !source_declares_entry(source, &contract.entry) {
        return Err(format!(
            "contract.entry `{}` was not found as a function/const/let/var declaration",
            contract.entry
        ));
    }
    if contract.effects.iter().any(|s| s.trim().is_empty()) {
        return Err("contract.effects entries must be non-empty strings".to_string());
    }
    if contract.may_invoke.iter().any(|s| s.trim().is_empty()) {
        return Err("contract.may_invoke entries must be non-empty strings".to_string());
    }
    if let Some(schema) = &contract.input {
        validate_json_schema("contract.input", schema)?;
    }
    if let Some(schema) = &contract.output {
        validate_json_schema("contract.output", schema)?;
    }
    if let Some(budget) = &contract.dispatch_budget {
        if !budget.is_object() {
            return Err("contract.dispatch_budget must be an object".to_string());
        }
    }
    Ok(())
}

fn validate_json_schema(label: &str, schema: &JsonValue) -> Result<(), String> {
    JSONSchema::options()
        .with_draft(Draft::Draft202012)
        .compile(schema)
        .map(|_| ())
        .map_err(|e| format!("{label} is not a valid JSON Schema: {e}"))
}

fn validate_json_value(label: &str, schema: &JsonValue, value: &JsonValue) -> Result<(), String> {
    let compiled = JSONSchema::options()
        .with_draft(Draft::Draft202012)
        .compile(schema)
        .map_err(|e| format!("{label} schema is not a valid JSON Schema: {e}"))?;
    let errors = compiled
        .validate(value)
        .err()
        .map(|errors| errors.map(|e| e.to_string()).collect::<Vec<_>>())
        .unwrap_or_default();
    if errors.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "{label} failed schema validation: {}",
            errors.join("; ")
        ))
    }
}

fn source_declares_entry(source: &str, entry: &str) -> bool {
    [
        format!("function {entry}("),
        format!("async function {entry}("),
        format!("const {entry} ="),
        format!("let {entry} ="),
        format!("var {entry} ="),
    ]
    .iter()
    .any(|needle| source.contains(needle))
}

enum CapRequest {
    HostCall {
        id: String,
        name: String,
        input_json: JsonValue,
        command_tx: std_mpsc::Sender<RuntimeCommand>,
    },
}

type CapTx = mpsc::UnboundedSender<CapRequest>;

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

async fn dispatch_cap(caps: Capabilities, req: CapRequest) {
    match req {
        CapRequest::HostCall {
            id,
            name,
            input_json,
            command_tx,
        } => {
            let response = dispatch_host_call(caps, &name, input_json).await;
            let command = match response {
                Ok(result) => RuntimeCommand::HostResponse { id, result },
                Err(error_text) => RuntimeCommand::HostError { id, error_text },
            };
            let _ = command_tx.send(command);
        }
    }
}

async fn dispatch_host_call(
    caps: Capabilities,
    name: &str,
    input_json: JsonValue,
) -> Result<JsonValue, String> {
    match name {
        "atoms.invoke" => {
            let input: AtomInvocation = serde_json::from_value(input_json)
                .map_err(|e| format!("invalid atoms.invoke input: {e}"))?;
            let cap = caps.atoms.clone();
            let output = run_caught(async move { cap.invoke_atom(input).await })
                .await
                .map_err(bro_error_text)?;
            Ok(output.output_json)
        }
        "refactor.plan" => {
            let input: RefactorRequest = serde_json::from_value(input_json)
                .map_err(|e| format!("invalid refactor.plan input: {e}"))?;
            let cap = caps.refactor.clone();
            serde_json::to_value(
                run_caught(async move { cap.plan_refactor(input).await })
                    .await
                    .map_err(bro_error_text)?,
            )
            .map_err(|e| format!("failed to serialize refactor plan handle: {e}"))
        }
        "refactor.materialize" => {
            let id = input_json
                .as_str()
                .map(str::to_string)
                .or_else(|| {
                    input_json
                        .get("id")
                        .and_then(JsonValue::as_str)
                        .map(str::to_string)
                })
                .ok_or_else(|| "invalid refactor.materialize handle".to_string())?;
            let cap = caps.refactor.clone();
            run_caught(async move { cap.materialize_plan(id).await })
                .await
                .map_err(bro_error_text)
        }
        "tool.invoke" => {
            let input: ToolInvocation = serde_json::from_value(input_json)
                .map_err(|e| format!("invalid host tool invocation: {e}"))?;
            tool_result(caps, input, false).await
        }
        "tool.invoke_inline" => {
            let input: ToolInvocation = serde_json::from_value(input_json)
                .map_err(|e| format!("invalid host tool invocation: {e}"))?;
            tool_result(caps, input, true).await
        }
        "kv.set" => {
            let input: KvSetInput = serde_json::from_value(input_json)
                .map_err(|e| format!("invalid narf.kv.set input: {e}"))?;
            let cap = caps.kv.clone();
            serde_json::to_value(
                run_caught(async move { cap.set(input.name, input.value_json, input.tags).await })
                    .await
                    .map_err(bro_error_text)?,
            )
            .map_err(|e| format!("failed to serialize kv set output: {e}"))
        }
        "kv.get" => {
            let input: KvGetInput = serde_json::from_value(input_json)
                .map_err(|e| format!("invalid narf.kv.get input: {e}"))?;
            let cap = caps.kv.clone();
            let out = run_caught(async move { cap.get(input.name, input.max_bytes).await })
                .await
                .map_err(bro_error_text)?;
            Ok(out.value_json)
        }
        "kv.peek" => {
            let input: KvNameInput = serde_json::from_value(input_json)
                .map_err(|e| format!("invalid narf.kv.peek input: {e}"))?;
            let cap = caps.kv.clone();
            serde_json::to_value(
                run_caught(async move { cap.peek(input.name).await })
                    .await
                    .map_err(bro_error_text)?,
            )
            .map_err(|e| format!("failed to serialize kv peek output: {e}"))
        }
        "kv.delete" => {
            let input: KvNameInput = serde_json::from_value(input_json)
                .map_err(|e| format!("invalid narf.kv.delete input: {e}"))?;
            let cap = caps.kv.clone();
            serde_json::to_value(
                run_caught(async move { cap.delete(input.name).await })
                    .await
                    .map_err(bro_error_text)?,
            )
            .map_err(|e| format!("failed to serialize kv delete output: {e}"))
        }
        other => Err(format!("unknown host call `{other}`")),
    }
}

async fn tool_result(
    caps: Capabilities,
    input: ToolInvocation,
    inline: bool,
) -> Result<JsonValue, String> {
    let name = input.name.clone();
    let Some(cap) = caps.tools.clone() else {
        return Err("host built-in tools are not installed in this runtime (fail-closed)".into());
    };
    let out = run_caught(async move { cap.call_tool(input).await })
        .await
        .map_err(bro_error_text)?;
    if out.is_error {
        return Err(format!("{name}: {}", out.content));
    }
    if inline || out.content_type == "application/json" {
        serde_json::from_str(&out.content).map_err(|e| format!("tool returned invalid JSON: {e}"))
    } else {
        Ok(JsonValue::String(out.content))
    }
}

fn bro_error_text(e: BroError) -> String {
    format!("{}: {}", e.code, e.message)
}

enum Job {
    Execute {
        body: String,
        reply: oneshot::Sender<Result<String>>,
    },
    Prepare {
        input_json: JsonValue,
        reply: oneshot::Sender<Result<PrepareResponse>>,
    },
    Run {
        handle: String,
        reply: oneshot::Sender<Result<String>>,
    },
    Prepared {
        handle: String,
        reply: oneshot::Sender<Result<PreparedCell>>,
    },
    CallCell {
        source: String,
        contract: CellContract,
        input_json: JsonValue,
        reply: oneshot::Sender<Result<String>>,
    },
    Define {
        input_json: JsonValue,
        reply: oneshot::Sender<Result<()>>,
    },
    TraceLen {
        reply: oneshot::Sender<usize>,
    },
    Shutdown,
}

enum RuntimeCommand {
    HostResponse { id: String, result: JsonValue },
    HostError { id: String, error_text: String },
    Terminate,
}

struct RuntimeState {
    cap_tx: CapTx,
    command_tx: std_mpsc::Sender<RuntimeCommand>,
    pending_host_calls: HashMap<String, v8::Global<v8::PromiseResolver>>,
    prepared: PreparedScriptsCell,
    session: SessionStateCell,
    trace: TraceStateCell,
    next_host_call_id: u64,
}

pub struct ScriptRuntime {
    job_tx: mpsc::UnboundedSender<Job>,
    runtime_tx: std_mpsc::Sender<RuntimeCommand>,
    isolate_handle: v8::IsolateHandle,
    heap_oom: Arc<AtomicBool>,
    execution_timeout: Option<Duration>,
    thread: Option<JoinHandle<()>>,
}

const BOOTSTRAP: &str = r#"
    delete globalThis.WebAssembly;
    delete globalThis.SharedArrayBuffer;
    delete globalThis.Atomics;
    delete globalThis.console;
    const hostCall = globalThis.__bb_host_call;
    const hostTool = (name, input) =>
        hostCall('tool.invoke', { name, input_json: input ?? {} });
    const hostToolInline = (name, input) =>
        hostCall('tool.invoke_inline', { name, input_json: input ?? {} });
    globalThis.atoms = {
        invoke: (handle, input) =>
            hostCall('atoms.invoke', { atom: handle, input_json: input ?? null }),
    };
    globalThis.refactor = {
        plan: (args) => hostCall('refactor.plan', args ?? {}),
        materialize: (handle) =>
            hostCall('refactor.materialize', typeof handle === 'string' ? handle : handle.id),
    };
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
    const mcpServerProxy = (server) => new Proxy(Object.create(null), {
        get: (_target, tool) => {
            if (typeof tool !== 'string' || tool === 'then') return undefined;
            return (args) => hostTool(`mcp__${server}__${tool}`, args ?? {});
        },
        has: () => false,
        ownKeys: () => [],
        getOwnPropertyDescriptor: () => undefined,
    });
    globalThis.mcp = new Proxy(Object.create(null), {
        get: (_target, server) => {
            if (typeof server !== 'string' || server === 'then') return undefined;
            return mcpServerProxy(server);
        },
        has: () => false,
        ownKeys: () => [],
        getOwnPropertyDescriptor: () => undefined,
    });
    const promiseId = (h) =>
        (typeof h === 'string' ? h : (h && (h.promise_id ?? (h.detail && h.detail.promise_id))));
    globalThis.narf = {
        kv: {
            set: (name, value, options) =>
                hostCall('kv.set', {
                    name,
                    value_json: value ?? null,
                    tags: options && options.tags !== undefined ? options.tags : null,
                }),
            get: (name, maxBytes) =>
                hostCall('kv.get', {
                    name,
                    max_bytes: (maxBytes === undefined || maxBytes === null) ? null : maxBytes,
                }),
            peek: (name) => hostCall('kv.peek', { name }),
            delete: (name) => hostCall('kv.delete', { name }),
        },
        session: {
            import: (name) => {
                const expr = globalThis.__bb_session_import(name);
                return (0, eval)(expr);
            },
        },
    };
    const yaml = (value) => globalThis.__bb_encode_yaml(value ?? null);
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
        pipeline: (items, ...stages) => Promise.all(
            (items ?? []).map((item) =>
                stages.reduce((acc, stage) => acc.then(stage), Promise.resolve(item)))),
    };
    globalThis.__bb_test = {
        panicGuarded: () => globalThis.__bb_panic_guarded(),
        panicGuardedAsync: () => hostCall('__bb_test.panic_async', {}),
    };
"#;

impl ScriptRuntime {
    pub async fn new(caps: Capabilities, policy: SupervisionPolicy) -> Result<Self> {
        initialize_v8().map_err(|e| anyhow!(e))?;

        let (cap_tx, mut cap_rx) = mpsc::unbounded_channel::<CapRequest>();
        tokio::spawn(async move {
            while let Some(req) = cap_rx.recv().await {
                let caps = caps.clone();
                tokio::spawn(async move {
                    dispatch_cap(caps, req).await;
                });
            }
        });

        let (job_tx, job_rx) = mpsc::unbounded_channel::<Job>();
        let (setup_tx, setup_rx) = oneshot::channel::<
            Result<(
                v8::IsolateHandle,
                Arc<AtomicBool>,
                std_mpsc::Sender<RuntimeCommand>,
            )>,
        >();
        let heap_limit_bytes = policy.heap_limit_bytes;
        let thread = std::thread::Builder::new()
            .name("bro-script-v8".to_string())
            .spawn(move || {
                v8_thread_main(cap_tx, job_rx, setup_tx, heap_limit_bytes);
            })?;

        let (isolate_handle, heap_oom, runtime_tx) = setup_rx
            .await
            .map_err(|_| anyhow!("V8 thread exited before setup"))??;

        Ok(Self {
            job_tx,
            runtime_tx,
            isolate_handle,
            heap_oom,
            execution_timeout: policy.execution_timeout,
            thread: Some(thread),
        })
    }

    pub fn isolate_handle(&self) -> v8::IsolateHandle {
        self.isolate_handle.clone()
    }

    pub fn hit_heap_oom(&self) -> bool {
        self.heap_oom.load(Ordering::SeqCst)
    }

    pub async fn execute(&self, body: impl Into<String>) -> Result<String> {
        self.execute_job(|reply| Job::Execute {
            body: body.into(),
            reply,
        })
        .await
    }

    pub async fn prepare(&self, input_json: JsonValue) -> Result<PrepareResponse> {
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

    pub async fn define(&self, input_json: JsonValue) -> Result<()> {
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
        self.execute_job(|reply| Job::Run {
            handle: handle.into(),
            reply,
        })
        .await
    }

    pub async fn prepared(&self, handle: impl Into<String>) -> Result<PreparedCell> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.job_tx
            .send(Job::Prepared {
                handle: handle.into(),
                reply: reply_tx,
            })
            .map_err(|_| anyhow!("V8 thread is gone"))?;
        reply_rx
            .await
            .map_err(|_| anyhow!("V8 thread dropped the reply"))?
    }

    pub async fn call_cell(
        &self,
        source: String,
        contract: CellContract,
        input_json: JsonValue,
    ) -> Result<String> {
        self.execute_job(|reply| Job::CallCell {
            source,
            contract,
            input_json,
            reply,
        })
        .await
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

    async fn execute_job(
        &self,
        make: impl FnOnce(oneshot::Sender<Result<String>>) -> Job,
    ) -> Result<String> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.job_tx
            .send(make(reply_tx))
            .map_err(|_| anyhow!("V8 thread is gone"))?;

        match self.execution_timeout {
            Some(timeout) => match tokio::time::timeout(timeout, reply_rx).await {
                Ok(reply) => reply.map_err(|_| anyhow!("V8 thread dropped the reply"))?,
                Err(_elapsed) => {
                    let _ = self.runtime_tx.send(RuntimeCommand::Terminate);
                    self.isolate_handle.terminate_execution();
                    Err(anyhow!("script execution timed out after {timeout:?}"))
                }
            },
            None => reply_rx
                .await
                .map_err(|_| anyhow!("V8 thread dropped the reply"))?,
        }
    }
}

impl Drop for ScriptRuntime {
    fn drop(&mut self) {
        let _ = self.job_tx.send(Job::Shutdown);
        let _ = self.runtime_tx.send(RuntimeCommand::Terminate);
        self.isolate_handle.terminate_execution();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn initialize_v8() -> Result<(), String> {
    static PLATFORM: OnceLock<Result<v8::SharedRef<v8::Platform>, String>> = OnceLock::new();
    match PLATFORM.get_or_init(|| {
        v8::icu::set_common_data_77(deno_core_icudata::ICU_DATA)
            .map_err(|code| format!("failed to initialize ICU data: {code}"))?;
        let platform = v8::new_default_platform(0, false).make_shared();
        v8::V8::initialize_platform(platform.clone());
        v8::V8::initialize();
        Ok(platform)
    }) {
        Ok(_) => Ok(()),
        Err(e) => Err(e.clone()),
    }
}

fn v8_thread_main(
    cap_tx: CapTx,
    mut job_rx: mpsc::UnboundedReceiver<Job>,
    setup_tx: oneshot::Sender<
        Result<(
            v8::IsolateHandle,
            Arc<AtomicBool>,
            std_mpsc::Sender<RuntimeCommand>,
        )>,
    >,
    heap_limit_bytes: usize,
) {
    let create_params = v8::CreateParams::default().heap_limits(0, heap_limit_bytes);
    let isolate = &mut v8::Isolate::new(create_params);
    let isolate_handle = isolate.thread_safe_handle();
    let heap_oom = Arc::new(AtomicBool::new(false));
    let heap_state = Box::leak(Box::new(HeapLimitState {
        flag: heap_oom.clone(),
        handle: isolate_handle.clone(),
    }));
    isolate.add_near_heap_limit_callback(
        near_heap_limit_callback,
        heap_state as *mut HeapLimitState as *mut c_void,
    );

    let (command_tx, command_rx) = std_mpsc::channel::<RuntimeCommand>();
    v8::scope!(let scope, isolate);
    let context = v8::Context::new(scope, Default::default());
    let scope = &mut v8::ContextScope::new(scope, context);
    scope.set_slot(RuntimeState {
        cap_tx,
        command_tx: command_tx.clone(),
        pending_host_calls: HashMap::new(),
        prepared: Rc::new(RefCell::new(PreparedScripts::default())),
        session: Rc::new(RefCell::new(SessionState::default())),
        trace: Rc::new(RefCell::new(Vec::new())),
        next_host_call_id: 1,
    });

    if let Err(e) = install_globals(scope).and_then(|_| execute_script_no_result(scope, BOOTSTRAP))
    {
        let _ = setup_tx.send(Err(anyhow!("bootstrap failed: {e}")));
        return;
    }

    if setup_tx
        .send(Ok((isolate_handle, heap_oom, command_tx.clone())))
        .is_err()
    {
        return;
    }

    while let Some(job) = job_rx.blocking_recv() {
        match job {
            Job::Shutdown => break,
            Job::Execute { body, reply } => {
                scope.cancel_terminate_execution();
                drain_runtime_commands(&command_rx);
                let result = run_one(scope, &command_rx, &body);
                scope.cancel_terminate_execution();
                let _ = reply.send(result);
            }
            Job::Prepare { input_json, reply } => {
                scope.cancel_terminate_execution();
                let result = prepare_one(scope, input_json);
                scope.cancel_terminate_execution();
                let _ = reply.send(result);
            }
            Job::Run { handle, reply } => {
                scope.cancel_terminate_execution();
                drain_runtime_commands(&command_rx);
                let result = run_prepared(scope, &command_rx, &handle);
                scope.cancel_terminate_execution();
                let _ = reply.send(result);
            }
            Job::Prepared { handle, reply } => {
                let result = get_prepared(scope, &handle);
                let _ = reply.send(result);
            }
            Job::CallCell {
                source,
                contract,
                input_json,
                reply,
            } => {
                scope.cancel_terminate_execution();
                drain_runtime_commands(&command_rx);
                let result = call_cell(scope, &command_rx, &source, &contract, input_json);
                scope.cancel_terminate_execution();
                let _ = reply.send(result);
            }
            Job::Define { input_json, reply } => {
                let result = define_one(scope, input_json);
                let _ = reply.send(result);
            }
            Job::TraceLen { reply } => {
                let len = scope
                    .get_slot::<RuntimeState>()
                    .map(|state| state.trace.borrow().len())
                    .unwrap_or_default();
                let _ = reply.send(len);
            }
        }
    }
}

macro_rules! install_fn {
    ($scope:expr, $global:expr, $name:expr, $callback:expr) => {{
        let key = v8::String::new($scope, $name)
            .ok_or_else(|| format!("failed to allocate {}", $name))?;
        let template = v8::FunctionTemplate::new($scope, $callback);
        let function = template
            .get_function($scope)
            .ok_or_else(|| format!("failed to create {}", $name))?;
        $global.set($scope, key.into(), function.into());
        Ok::<(), String>(())
    }};
}

macro_rules! exception_text {
    ($tc:expr) => {{
        $tc.exception()
            .map(|exception| value_to_error_text(&mut $tc, exception))
            .unwrap_or_else(|| "unknown JavaScript exception".to_string())
    }};
}

fn install_globals(scope: &mut v8::PinScope<'_, '_>) -> Result<(), String> {
    let global = scope.get_current_context().global(scope);
    install_fn!(scope, global, "__bb_host_call", host_call_callback)?;
    install_fn!(scope, global, "__bb_encode_yaml", encode_yaml_callback)?;
    install_fn!(
        scope,
        global,
        "__bb_session_import",
        session_import_callback
    )?;
    install_fn!(scope, global, "__bb_panic_guarded", panic_guarded_callback)?;
    Ok(())
}

struct HeapLimitState {
    flag: Arc<AtomicBool>,
    handle: v8::IsolateHandle,
}

unsafe extern "C" fn near_heap_limit_callback(
    data: *mut c_void,
    current_heap_limit: usize,
    _initial_heap_limit: usize,
) -> usize {
    let state = unsafe { &*(data as *const HeapLimitState) };
    state.flag.store(true, Ordering::SeqCst);
    state.handle.terminate_execution();
    current_heap_limit.saturating_mul(2)
}

fn host_call_callback(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments,
    mut retval: v8::ReturnValue<v8::Value>,
) {
    let result = catch_unwind(AssertUnwindSafe(|| {
        let name = args
            .get(0)
            .to_string(scope)
            .ok_or_else(|| "host call name must be a string".to_string())?
            .to_rust_string_lossy(scope);
        if name == "__bb_test.panic_async" {
            return Err("host call panicked; contained at the boundary (catch_unwind)".to_string());
        }
        let input_json = if args.length() < 2 {
            JsonValue::Null
        } else {
            v8_value_to_json(scope, args.get(1))?
        };
        let resolver = v8::PromiseResolver::new(scope)
            .ok_or_else(|| "failed to create host-call promise".to_string())?;
        let promise = resolver.get_promise(scope);
        let resolver = v8::Global::new(scope, resolver);

        let state = scope
            .get_slot_mut::<RuntimeState>()
            .ok_or_else(|| "runtime state unavailable".to_string())?;
        let id = format!("host-{}", state.next_host_call_id);
        state.next_host_call_id = state.next_host_call_id.saturating_add(1);
        state.pending_host_calls.insert(id.clone(), resolver);
        state
            .cap_tx
            .send(CapRequest::HostCall {
                id,
                name,
                input_json,
                command_tx: state.command_tx.clone(),
            })
            .map_err(|_| "capability executor is gone".to_string())?;
        Ok::<_, String>(promise)
    }));

    match result {
        Ok(Ok(promise)) => retval.set(promise.into()),
        Ok(Err(e)) => throw_error(scope, &e),
        Err(_) => throw_error(
            scope,
            "callback panicked; contained at the boundary (catch_unwind)",
        ),
    }
}

fn encode_yaml_callback(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments,
    mut retval: v8::ReturnValue<v8::Value>,
) {
    let result = catch_unwind(AssertUnwindSafe(|| {
        let value = if args.length() == 0 {
            JsonValue::Null
        } else {
            v8_value_to_json(scope, args.get(0))?
        };
        serde_norway::to_string(&value).map_err(|e| format!("failed to encode YAML: {e}"))
    }));
    match result {
        Ok(Ok(yaml)) => {
            if let Some(value) = v8::String::new(scope, &yaml) {
                retval.set(value.into());
            } else {
                throw_error(scope, "failed to allocate YAML output");
            }
        }
        Ok(Err(e)) => throw_error(scope, &e),
        Err(_) => throw_error(
            scope,
            "callback panicked; contained at the boundary (catch_unwind)",
        ),
    }
}

fn session_import_callback(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments,
    mut retval: v8::ReturnValue<v8::Value>,
) {
    let result = catch_unwind(AssertUnwindSafe(|| {
        let name = args
            .get(0)
            .to_string(scope)
            .ok_or_else(|| "session import name must be a string".to_string())?
            .to_rust_string_lossy(scope);
        let state = scope
            .get_slot::<RuntimeState>()
            .ok_or_else(|| "runtime state unavailable".to_string())?;
        let session = state.session.borrow();
        let helper_name = session
            .import_aliases
            .get(&name)
            .or_else(|| session.helpers.contains_key(&name).then_some(&name))
            .ok_or_else(|| format!("unknown import alias: {name}"))?;
        let helper = session
            .helpers
            .get(helper_name)
            .ok_or_else(|| format!("unknown session helper: {helper_name}"))?;
        helper_expression(helper)
    }));
    match result {
        Ok(Ok(expr)) => {
            if let Some(value) = v8::String::new(scope, &expr) {
                retval.set(value.into());
            } else {
                throw_error(scope, "failed to allocate session import output");
            }
        }
        Ok(Err(e)) => throw_error(scope, &e),
        Err(_) => throw_error(
            scope,
            "callback panicked; contained at the boundary (catch_unwind)",
        ),
    }
}

fn panic_guarded_callback(
    scope: &mut v8::PinScope<'_, '_>,
    _args: v8::FunctionCallbackArguments,
    _retval: v8::ReturnValue<v8::Value>,
) {
    let result = catch_unwind(AssertUnwindSafe(|| {
        panic!("intentional panic inside host callback");
    }));
    if result.is_err() {
        throw_error(
            scope,
            "host callback panicked; contained at the boundary (catch_unwind)",
        );
    }
}

fn throw_error(scope: &mut v8::PinScope<'_, '_>, message: &str) {
    let Some(message) = v8::String::new(scope, message) else {
        return;
    };
    let exception = v8::Exception::error(scope, message);
    scope.throw_exception(exception);
}

fn execute_script_no_result(scope: &mut v8::PinScope<'_, '_>, source: &str) -> Result<(), String> {
    let tc = std::pin::pin!(v8::TryCatch::new(scope));
    let mut tc = tc.init();
    let source =
        v8::String::new(&tc, source).ok_or_else(|| "failed to allocate source".to_string())?;
    let script = match v8::Script::compile(&tc, source, None) {
        Some(script) => script,
        None => return Err(exception_text!(tc)),
    };
    if script.run(&tc).is_none() {
        return Err(exception_text!(tc));
    }
    Ok(())
}

fn run_one(
    scope: &mut v8::PinScope<'_, '_>,
    command_rx: &std_mpsc::Receiver<RuntimeCommand>,
    body: &str,
) -> Result<String> {
    let wrapped = format!("(async () => {{ {body} }})()");
    run_wrapped(scope, command_rx, &wrapped)
}

fn drain_runtime_commands(command_rx: &std_mpsc::Receiver<RuntimeCommand>) {
    while command_rx.try_recv().is_ok() {}
}

fn run_wrapped(
    scope: &mut v8::PinScope<'_, '_>,
    command_rx: &std_mpsc::Receiver<RuntimeCommand>,
    wrapped: &str,
) -> Result<String> {
    enum InitialResult {
        Promise(v8::Global<v8::Promise>),
        Value(String),
    }

    let initial = {
        let tc = std::pin::pin!(v8::TryCatch::new(scope));
        let mut tc = tc.init();
        let source =
            v8::String::new(&tc, wrapped).ok_or_else(|| anyhow!("failed to allocate source"))?;
        let script = match v8::Script::compile(&tc, source, None) {
            Some(script) => script,
            None => return Err(anyhow!(exception_text!(tc))),
        };
        let result = match script.run(&tc) {
            Some(result) => result,
            None => return Err(anyhow!(exception_text!(tc))),
        };
        tc.perform_microtask_checkpoint();

        if result.is_promise() {
            let promise = v8::Local::<v8::Promise>::try_from(result)
                .map_err(|_| anyhow!("failed to read script promise"))?;
            InitialResult::Promise(v8::Global::new(&tc, promise))
        } else {
            let value = v8_value_to_json(&mut tc, result).map_err(|e| anyhow!(e))?;
            InitialResult::Value(value.to_string())
        }
    };

    match initial {
        InitialResult::Value(value) => Ok(value),
        InitialResult::Promise(promise) => {
            drive_promise(scope, command_rx, &promise)?;
            let local = v8::Local::new(scope, &promise);
            promise_to_json_string(scope, local)
        }
    }
}

fn drive_promise(
    scope: &mut v8::PinScope<'_, '_>,
    command_rx: &std_mpsc::Receiver<RuntimeCommand>,
    promise: &v8::Global<v8::Promise>,
) -> Result<()> {
    loop {
        let local = v8::Local::new(scope, promise);
        match local.state() {
            v8::PromiseState::Pending => {}
            v8::PromiseState::Fulfilled => return Ok(()),
            v8::PromiseState::Rejected => {
                let result = local.result(scope);
                return Err(anyhow!(value_to_error_text(scope, result)));
            }
        }

        match command_rx.recv() {
            Ok(RuntimeCommand::HostResponse { id, result }) => {
                resolve_host_call(scope, &id, Ok(result)).map_err(|e| anyhow!(e))?;
            }
            Ok(RuntimeCommand::HostError { id, error_text }) => {
                resolve_host_call(scope, &id, Err(error_text)).map_err(|e| anyhow!(e))?;
            }
            Ok(RuntimeCommand::Terminate) | Err(_) => {
                return Err(anyhow!("script execution terminated"));
            }
        }
        scope.perform_microtask_checkpoint();
    }
}

fn resolve_host_call(
    scope: &mut v8::PinScope<'_, '_>,
    id: &str,
    response: Result<JsonValue, String>,
) -> Result<(), String> {
    let Some(resolver) = ({
        let state = scope
            .get_slot_mut::<RuntimeState>()
            .ok_or_else(|| "runtime state unavailable".to_string())?;
        state.pending_host_calls.remove(id)
    }) else {
        // A timed-out activation may leave late host responses in the runtime
        // command queue. Ignore them so a later activation is not poisoned by an
        // already-abandoned host-call id.
        return Ok(());
    };

    let tc = std::pin::pin!(v8::TryCatch::new(scope));
    let mut tc = tc.init();
    let resolver = v8::Local::new(&tc, &resolver);
    match response {
        Ok(value) => {
            let value = json_to_v8(&mut tc, &value)
                .ok_or_else(|| "failed to serialize host response".to_string())?;
            resolver.resolve(&tc, value);
        }
        Err(error_text) => {
            let value = v8::String::new(&tc, &error_text)
                .ok_or_else(|| "failed to allocate host error".to_string())?;
            resolver.reject(&tc, value.into());
        }
    }
    if tc.has_caught() {
        return Err(exception_text!(tc));
    }
    Ok(())
}

fn promise_to_json_string(
    scope: &mut v8::PinScope<'_, '_>,
    promise: v8::Local<'_, v8::Promise>,
) -> Result<String> {
    match promise.state() {
        v8::PromiseState::Fulfilled => {
            let value = promise.result(scope);
            let value = v8_value_to_json(scope, value).map_err(|e| anyhow!(e))?;
            Ok(value.to_string())
        }
        v8::PromiseState::Rejected => {
            Err(anyhow!(value_to_error_text(scope, promise.result(scope))))
        }
        v8::PromiseState::Pending => Err(anyhow!("script promise is still pending")),
    }
}

fn call_cell(
    scope: &mut v8::PinScope<'_, '_>,
    command_rx: &std_mpsc::Receiver<RuntimeCommand>,
    source: &str,
    contract: &CellContract,
    input_json: JsonValue,
) -> Result<String> {
    validate_cell_contract(contract, source).map_err(|e| anyhow!("cell contract invalid: {e}"))?;
    if let Some(schema) = &contract.input {
        validate_json_value("cell input", schema, &input_json).map_err(|e| anyhow!(e))?;
    }
    let encoded_input =
        serde_json::to_string(&input_json).map_err(|e| anyhow!("cell input encode failed: {e}"))?;
    let wrapped = format!(
        "(async () => {{\n{source}\nreturn await {entry}({encoded_input});\n}})()",
        entry = contract.entry
    );
    let output = run_wrapped(scope, command_rx, &wrapped)?;
    let output_json: JsonValue =
        serde_json::from_str(&output).map_err(|e| anyhow!("cell output decode failed: {e}"))?;
    if let Some(schema) = &contract.output {
        validate_json_value("cell output", schema, &output_json).map_err(|e| anyhow!(e))?;
    }
    Ok(output_json.to_string())
}

fn prepare_one(scope: &mut v8::PinScope<'_, '_>, input_json: JsonValue) -> Result<PrepareResponse> {
    let input: PrepareInput = match serde_json::from_value(input_json) {
        Ok(i) => i,
        Err(e) => return Ok(blocked("input", format!("invalid narf_prepare input: {e}"))),
    };
    let contract = input.contract.clone();
    let assembled = {
        let state = scope
            .get_slot::<RuntimeState>()
            .ok_or_else(|| anyhow!("runtime state unavailable"))?;
        let session = state.session.borrow();
        match render_prepare(&session, input) {
            Ok(source) => source,
            Err(e) => return Ok(blocked("import", e)),
        }
    };

    if let Err(message) = validate_script_syntax(scope, &assembled) {
        return Ok(blocked("syntax", message));
    }
    if let Some(contract) = &contract {
        if let Err(message) = validate_cell_contract(contract, &assembled) {
            return Ok(blocked("contract", message));
        }
    }

    let handle = {
        let state = scope
            .get_slot::<RuntimeState>()
            .ok_or_else(|| anyhow!("runtime state unavailable"))?;
        state
            .prepared
            .borrow_mut()
            .put(assembled.clone(), contract.clone())
    };
    Ok(ready(handle, assembled, contract))
}

fn define_one(scope: &mut v8::PinScope<'_, '_>, input_json: JsonValue) -> Result<()> {
    let input: SessionDefineInput = serde_json::from_value(input_json)
        .map_err(|e| anyhow!("invalid narf_define input: {e}"))?;
    let state = scope
        .get_slot::<RuntimeState>()
        .ok_or_else(|| anyhow!("runtime state unavailable"))?;
    define_session_helper(&state.session, input).map_err(|e| anyhow!(e))
}

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

fn run_prepared(
    scope: &mut v8::PinScope<'_, '_>,
    command_rx: &std_mpsc::Receiver<RuntimeCommand>,
    handle: &str,
) -> Result<String> {
    let script = get_prepared(scope, handle)?;
    {
        let state = scope
            .get_slot::<RuntimeState>()
            .ok_or_else(|| anyhow!("runtime state unavailable"))?;
        let mut trace = state.trace.borrow_mut();
        let sequence = trace.len();
        trace.push(TraceEntry {
            ref_handle: handle.to_string(),
            sequence,
        });
    }
    run_one(scope, command_rx, &script.source)
}

fn get_prepared(scope: &mut v8::PinScope<'_, '_>, handle: &str) -> Result<PreparedCell> {
    let state = scope
        .get_slot::<RuntimeState>()
        .ok_or_else(|| anyhow!("runtime state unavailable"))?;
    state.prepared.borrow().get(handle).map_err(|e| anyhow!(e))
}

fn validate_script_syntax(scope: &mut v8::PinScope<'_, '_>, body: &str) -> Result<(), String> {
    let wrapped = format!("(async () => {{\n{body}\n}})");
    let tc = std::pin::pin!(v8::TryCatch::new(scope));
    let mut tc = tc.init();
    let source = v8::String::new(&tc, &wrapped)
        .ok_or_else(|| "failed to allocate V8 source string".to_string())?;
    match v8::Script::compile(&tc, source, None) {
        Some(_) => Ok(()),
        None => Err(exception_text!(tc)),
    }
}

fn json_to_v8<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    value: &JsonValue,
) -> Option<v8::Local<'s, v8::Value>> {
    match value {
        JsonValue::Null => Some(v8::null(scope).into()),
        JsonValue::Bool(value) => Some(v8::Boolean::new(scope, *value).into()),
        JsonValue::Number(value) => value
            .as_f64()
            .and_then(|n| v8::Number::new(scope, n).try_into().ok()),
        JsonValue::String(value) => v8::String::new(scope, value).map(Into::into),
        JsonValue::Array(values) => {
            let array = v8::Array::new(scope, values.len() as i32);
            for (idx, value) in values.iter().enumerate() {
                let value = json_to_v8(scope, value)?;
                array.set_index(scope, idx as u32, value);
            }
            Some(array.into())
        }
        JsonValue::Object(map) => {
            let object = v8::Object::new(scope);
            for (key, value) in map {
                let key = v8::String::new(scope, key)?;
                let value = json_to_v8(scope, value)?;
                object.set(scope, key.into(), value);
            }
            Some(object.into())
        }
    }
}

fn v8_value_to_json(
    scope: &mut v8::PinScope<'_, '_>,
    value: v8::Local<'_, v8::Value>,
) -> Result<JsonValue, String> {
    let global = v8::json::stringify(scope, value)
        .ok_or_else(|| "value is not JSON-serializable".to_string())?;
    let text = global.to_rust_string_lossy(scope);
    serde_json::from_str(&text).map_err(|e| format!("failed to deserialize JSON value: {e}"))
}

fn value_to_error_text(
    scope: &mut v8::PinScope<'_, '_>,
    value: v8::Local<'_, v8::Value>,
) -> String {
    value
        .to_string(scope)
        .map(|s| s.to_rust_string_lossy(scope))
        .unwrap_or_else(|| "unknown JavaScript exception".to_string())
}
