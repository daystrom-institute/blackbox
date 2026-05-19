//! Hook operations — named, sandboxed side effects that fire on
//! node-enter / node-exit / arc-exit / arc-cancel.
//!
//! Every op:
//! - takes its args as `Value` (rendered through the ArcContext
//!   templater before execution so `${vars.x}` works in any arg)
//! - is gated by an optional `when: PacketRef` (packet evaluated
//!   against the flattened ArcContext)
//! - declares its `on_failure` mode (halt | warn | ignore)
//! - returns either `OpEffect::None`, a vars mutation, or an
//!   arc-meta mutation
//!
//! Ops are NOT decision-makers — they cannot change which next node
//! runs. That stays with the gate packet at the node level. A misuse
//! that would do that should be refactored into a node + gate.

mod auto_digest;
mod json_ops;
mod system_events;
mod vector;
mod worktree;

use std::fs;
use std::path::PathBuf;
use std::process::Stdio;

use anyhow::{Result, anyhow, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use tokio::io::AsyncWriteExt;

use super::context::{ArcContext, VarsSchema, resolve_arg_value};
use auto_digest::{
    exec_aggregate_auto_edge_votes, exec_append_knowledge_link, exec_apply_entry,
    exec_extract_candidate_pairs, exec_log_reject, exec_read_session, exec_surface_to_inbox,
    exec_validate_schema, exec_write_semantic_edge,
};
use json_ops::{coerce_json_value, ensure_objectish_json, exec_parse_json, normalize_array_field};
use system_events::{exec_require_identity, exec_system_event_compact};
use vector::{exec_compact_vector_partitions, exec_read_vector_status, exec_rebuild_hnsw};
use worktree::{exec_worktree_create, exec_worktree_remove};

/// Side-effect declaration. Lives in `NodeSpec.on_enter`,
/// `NodeSpec.on_exit`, `Workflow.on_arc_exit`, `Workflow.on_arc_cancel`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HookOp {
    /// Named operation kind.
    pub op: OpKind,
    /// Op-specific arguments. Rendered through ArcContext templater
    /// before execution.
    #[serde(default)]
    pub args: Value,
    /// Optional packet id; op fires only if packet verdict is `allow`.
    /// Absent = always fire.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub when: Option<String>,
    /// What to do if the op itself errors.
    #[serde(default)]
    pub on_failure: OnFailure,
    /// For ops that produce a value (ParseJson, ForgejoIssueFetch, …),
    /// the var key to write the result into.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub into_var: Option<String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum OpKind {
    SetVar,
    /// Set a variable only when it is currently missing or null.
    /// Useful for optional workflow inputs that still appear in prompts.
    DefaultVar,
    IncVar,
    AppendVar,
    MergeVar,
    ParseJson,
    /// Normalize architecture-pathology survey atom requests before
    /// dispatching them. LLM survey output sometimes stringifies nested
    /// `survey_json` objects; atom schemas require structured objects.
    NormalizeArchPathologyAtomRequests,
    Shell,
    /// Write an Architecture Pathology correction plan markdown file under
    /// `<project>/design/refactor/plans/<slug>.md` from reviewed plan JSON.
    WriteArchPathologyPlan,
    WorktreeCreate,
    WorktreeRemove,
    SetMeta,
    /// Request an external identity mapping for a bro through the EventHub.
    /// Calls `EventHub::require_identity`; emits `bro.identity.required` once
    /// per daemon lifetime per key when the mapping is missing (pending dedup).
    /// Writes `{ "status": "ready", "identity": <ExternalIdentity> }` or
    /// `{ "status": "pending", "identity": null }` into `vars[into_var]`.
    RequireIdentity,
    /// Apply system-event journal and outbox retention compaction through the
    /// EventHub. Writes the compaction report into `vars[into_var]`.
    SystemEventCompact,
    /// Generic HTTP request → JSON-decoded response body. Workflow
    /// authors compose URL/headers/body from `${env.X}` + `${vars.X}`
    /// to express any code-host integration (issue fetch, PR create,
    /// PR comment, …) without baking platform-specific ops into the
    /// engine. Captures the response into `vars[into_var]` when set.
    HttpJson,
    /// Find the first element of an array variable whose nested field
    /// equals a target value, write into `vars[into_var]`. Writes
    /// `Value::Null` (not Err) when no match is found, so downstream
    /// `IsNull` / `IsNonNull` packet predicates can branch cleanly.
    /// Composable primitive that lets workflow authors express
    /// "find existing PR for this branch" / "find label by name" / etc
    /// without a code-host-specific search op AND without relying on
    /// upstream API filters that may be broken or absent.
    FindFirst,
    /// Outbound MCP tool call (sibling of HttpJson but speaking MCP
    /// JSON-RPC instead of REST). Resolves `args.server` against the
    /// blackbox MCP registry (global `~/.bro/mcp.json` + project
    /// overlay), opens a transient client (stdio child-process or
    /// streamable HTTP), invokes `args.tool` with `args.arguments`,
    /// and captures the result into `vars[into_var]`. Tool-level
    /// errors (`is_error: true`) become op failures so `on_failure`
    /// fires.
    ///
    /// Why an op kind, not just a dispatched bro: this lets the engine
    /// inject deterministic MCP results (`sast_run`, `sast_findings`,
    /// `bbox_thread`, etc) into hooks at known boundaries — analyzer
    /// arc creates a work-item thread before dispatch, fixer arc
    /// re-runs SAST on its branch before opening a PR, reviewer arc
    /// reads finding context the analyzer captured. Without this, the
    /// arc has to dispatch a bro to make every grounding call, which
    /// is both expensive and non-deterministic.
    McpCall,
    /// Read vector partition metrics and expose max deleted-ratio state
    /// for compaction-policy gates.
    ReadVectorStatus,
    /// Marker hook for pausing search traffic before a vector rebuild.
    ///
    /// V1 is intentionally observable-only: vector reads serve from the
    /// in-memory partition snapshot while `vectors::rebuild(route)` rebuilds
    /// from WAL under the partition lock. If search becomes more concurrent or
    /// moves out of process, this hook is where real quiescence belongs.
    QuiesceSearch,
    /// Rebuild one vector partition's HNSW from WAL.
    RebuildHnsw,
    /// Compact every vector partition whose deleted-ratio policy says it is
    /// eligible. `args.max_partitions` can cap the batch for operator-run arcs.
    CompactVectorPartitions,
    /// Marker hook for the atomic swap step.
    ///
    /// V1 is intentionally observable-only: `vectors::rebuild(route)` already
    /// swaps the rebuilt in-memory partition and rewrites derived files from
    /// WAL, so there is no separate file-system rename step for the workflow to
    /// perform yet.
    SwapAtomic,
    /// Load a transcript session through bbox_messages for auto-digest arcs.
    ReadSession,
    /// Validate auto-digest candidate JSON shape before packet gating.
    ValidateSchema,
    /// Apply an auto-digest candidate through the knowledge MCP tools.
    ApplyEntry,
    /// Append an authored edge to `KnowledgeEntry.links` via bbox_knowledge_link.
    AppendKnowledgeLink,
    /// Scan for semantic auto-edge candidate pairs.
    ExtractCandidatePairs,
    /// Aggregate three classifier votes into a compact gate entity.
    AggregateAutoEdgeVotes,
    /// Write a reviewed semantic edge (REFERENCES or DESCRIBES).
    WriteSemanticEdge,
    /// Surface an auto-digest candidate for operator review.
    SurfaceToInbox,
    /// Record an auto-digest rejection.
    LogReject,
    /// Observable marker for the index-drop step of a schema migration.
    /// Emits a structured trace event; actual document deletion happens in
    /// the paired `SchemaMigrationRebuild` op which calls `bbox_reindex`.
    SchemaMigrationDrop,
    /// Run a full tantivy rebuild via `bbox_reindex(full=true)`. This is the
    /// live runner for schema-migration-arc's Rebuild node — the workflow
    /// owns the drop+rebuild lifecycle rather than relying on startup-time
    /// `reset_index_on_schema_mismatch` alone.
    SchemaMigrationRebuild,
    /// Compute an eval drift score from captured shell output and write
    /// `drift_pp` into `vars[into_var]`. Reads `args.suite_output` (the
    /// `{exit_code, stdout, stderr, parsed}` blob captured by a Shell op)
    /// and extracts a percentage-points drift figure for the drift-policy
    /// gate in the nightly-eval-arc Decide node.
    ScoreEvalOutput,
    /// Pick the first element of a `vars[array_var]` array into
    /// `vars[into_var]`. Writes `Value::Null` when the array is absent or
    /// empty so downstream `IsNull` predicates can short-circuit cleanly.
    PickFirst,
    /// Engine-state op handled by `WorkflowRunner`: poll a supervised primary
    /// invocation through its wrapper-owned attachment lineage and write the
    /// bounded snapshot into `vars[into_var]`.
    PollAttachedInvocation,
    /// Engine-state op handled by `WorkflowRunner`: apply a typed advisor
    /// action through code-owned lineage and compatibility checks.
    ExecuteSupervisionAction,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, Default, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum OnFailure {
    #[default]
    Halt,
    Warn,
    Ignore,
}

/// Result of executing one HookOp. The runner applies these to its
/// ArcContext.
#[derive(Debug)]
pub enum OpEffect {
    /// No-op (Shell with no into_var, etc.).
    None,
    /// Write `value` into `vars[key]` (schema-validated).
    SetVar { key: String, value: Value },
    /// Update meta.worktree (set to Some / None).
    SetWorktree(Option<String>),
    /// Update the runner's project_dir (and ctx.meta.project_dir).
    /// Used when the arc's project context is determined dynamically
    /// (e.g. resolved from a Slack channel→project binding) rather
    /// than fixed at arc start.
    SetProjectDir(Option<String>),
}

/// Execute one HookOp against the given context. Stateless variant —
/// ops that require daemon state (e.g. `require_identity`) will error.
/// Use `execute_op_with_hub` from the engine where state is available.
#[cfg(test)]
pub async fn execute_op(
    hook: &HookOp,
    ctx: &ArcContext,
    schema: Option<&VarsSchema>,
) -> Result<OpEffect> {
    execute_op_with_hub(hook, ctx, schema, None).await
}

/// State-aware variant of `execute_op`. The `hub` parameter is
/// `Some` when called from the engine (which has access to
/// `SharedState`) and `None` in stateless test/preview contexts.
/// Ops that require the hub return an error when it is absent.
pub async fn execute_op_with_hub(
    hook: &HookOp,
    ctx: &ArcContext,
    schema: Option<&VarsSchema>,
    hub: Option<&crate::system_events::SharedEventHub>,
) -> Result<OpEffect> {
    let _ = schema;
    let rendered_args = resolve_arg_value(ctx, &hook.args)
        .map_err(|e| anyhow!("op {:?}: arg render failed: {e}", hook.op))?;
    match hook.op {
        OpKind::SetVar => exec_set_var(&rendered_args),
        OpKind::DefaultVar => exec_default_var(&rendered_args, ctx),
        OpKind::IncVar => exec_inc_var(&rendered_args, ctx),
        OpKind::AppendVar => exec_append_var(&rendered_args, ctx),
        OpKind::MergeVar => exec_merge_var(&rendered_args, ctx),
        OpKind::ParseJson => exec_parse_json(&rendered_args, hook.into_var.as_deref()),
        OpKind::NormalizeArchPathologyAtomRequests => {
            exec_normalize_arch_pathology_atom_requests(&rendered_args, hook.into_var.as_deref())
        }
        OpKind::Shell => exec_shell(&rendered_args, hook.into_var.as_deref(), ctx).await,
        OpKind::WriteArchPathologyPlan => {
            exec_write_arch_pathology_plan(&rendered_args, hook.into_var.as_deref())
        }
        OpKind::WorktreeCreate => exec_worktree_create(&rendered_args, ctx).await,
        OpKind::WorktreeRemove => exec_worktree_remove(&rendered_args, ctx).await,
        OpKind::SetMeta => exec_set_meta(&rendered_args),
        OpKind::HttpJson => exec_http_json(&rendered_args, hook.into_var.as_deref()).await,
        OpKind::FindFirst => exec_find_first(&rendered_args, hook.into_var.as_deref()),
        OpKind::McpCall => exec_mcp_call(&rendered_args, hook.into_var.as_deref(), ctx).await,
        OpKind::ReadVectorStatus => {
            exec_read_vector_status(&rendered_args, hook.into_var.as_deref())
        }
        OpKind::QuiesceSearch => Ok(OpEffect::None),
        OpKind::RebuildHnsw => exec_rebuild_hnsw(&rendered_args),
        OpKind::CompactVectorPartitions => {
            exec_compact_vector_partitions(&rendered_args, hook.into_var.as_deref())
        }
        OpKind::SwapAtomic => Ok(OpEffect::None),
        OpKind::ReadSession => {
            exec_read_session(&rendered_args, hook.into_var.as_deref(), ctx).await
        }
        OpKind::ValidateSchema => exec_validate_schema(&rendered_args, hook.into_var.as_deref()),
        OpKind::ApplyEntry => exec_apply_entry(&rendered_args, hook.into_var.as_deref(), ctx).await,
        OpKind::AppendKnowledgeLink => {
            exec_append_knowledge_link(&rendered_args, hook.into_var.as_deref(), ctx).await
        }
        OpKind::ExtractCandidatePairs => {
            exec_extract_candidate_pairs(&rendered_args, hook.into_var.as_deref(), ctx).await
        }
        OpKind::AggregateAutoEdgeVotes => {
            exec_aggregate_auto_edge_votes(&rendered_args, hook.into_var.as_deref())
        }
        OpKind::WriteSemanticEdge => {
            exec_write_semantic_edge(&rendered_args, hook.into_var.as_deref(), ctx).await
        }
        OpKind::SurfaceToInbox => exec_surface_to_inbox(&rendered_args, ctx).await,
        OpKind::LogReject => exec_log_reject(&rendered_args, ctx).await,
        OpKind::SchemaMigrationDrop => Ok(exec_schema_migration_drop(ctx)),
        OpKind::SchemaMigrationRebuild => {
            exec_schema_migration_rebuild(hook.into_var.as_deref(), ctx).await
        }
        OpKind::ScoreEvalOutput => {
            exec_score_eval_output(&rendered_args, hook.into_var.as_deref(), ctx)
        }
        OpKind::PickFirst => exec_pick_first(&rendered_args, hook.into_var.as_deref(), ctx),
        OpKind::PollAttachedInvocation => {
            bail!("poll_attached_invocation op requires workflow engine state")
        }
        OpKind::ExecuteSupervisionAction => {
            bail!("execute_supervision_action op requires workflow engine state")
        }
        OpKind::SystemEventCompact => {
            let hub = hub.ok_or_else(|| {
                anyhow!(
                    "system_event_compact op requires EventHub — not available in stateless context"
                )
            })?;
            exec_system_event_compact(&rendered_args, hook.into_var.as_deref(), hub)
        }
        OpKind::RequireIdentity => {
            let hub = hub.ok_or_else(|| {
                anyhow!(
                    "require_identity op requires EventHub — not available in stateless context"
                )
            })?;
            exec_require_identity(&rendered_args, hook.into_var.as_deref(), hub).await
        }
    }
}

async fn call_blackbox_tool(tool: &str, arguments: Value, ctx: &ArcContext) -> Result<Value> {
    let arguments = arguments
        .as_object()
        .cloned()
        .ok_or_else(|| anyhow!("blackbox tool arguments must be an object"))?;
    crate::mcp_client::call_tool(
        "blackbox",
        tool,
        arguments,
        300,
        ctx.meta.project_dir.as_deref(),
        ctx.meta
            .worktree
            .as_deref()
            .or(ctx.meta.project_dir.as_deref()),
    )
    .await
    .map_err(|e| anyhow!("McpCall 'blackbox.{tool}': {e}"))
}

async fn exec_mcp_call(args: &Value, into_var: Option<&str>, ctx: &ArcContext) -> Result<OpEffect> {
    let server = args.get("server").and_then(|v| v.as_str()).ok_or_else(|| {
        anyhow!("McpCall requires args.server (MCP registry name, e.g. 'biofilter' or 'blackbox')")
    })?;
    let tool = args.get("tool").and_then(|v| v.as_str()).ok_or_else(|| {
        anyhow!("McpCall requires args.tool (tool name as the server registers it)")
    })?;
    // arguments — must be an object (or absent → empty).
    let arguments = match args.get("arguments") {
        Some(Value::Object(m)) => m.clone(),
        Some(Value::Null) | None => serde_json::Map::new(),
        Some(other) => bail!("McpCall args.arguments must be an object, got {other:?}"),
    };
    let timeout_secs = args
        .get("timeout_secs")
        .and_then(|v| v.as_u64())
        .unwrap_or(300);
    let project_dir = ctx.meta.project_dir.as_deref();
    // cwd resolution: explicit args.cwd → arc's active worktree →
    // arc's project_dir → None (daemon's cwd, almost never what you
    // want for tools that read project state). MCP servers like
    // biofilter resolve their root via `process.cwd()` so this
    // matters for stdio transports.
    let cwd_owned: Option<String> = args
        .get("cwd")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .or_else(|| ctx.meta.worktree.clone())
        .or_else(|| ctx.meta.project_dir.clone());
    let result = crate::mcp_client::call_tool(
        server,
        tool,
        arguments,
        timeout_secs,
        project_dir,
        cwd_owned.as_deref(),
    )
    .await
    .map_err(|e| anyhow!("McpCall '{server}.{tool}': {e}"))?;
    match into_var {
        Some(k) => Ok(OpEffect::SetVar {
            key: k.to_string(),
            value: result,
        }),
        None => Ok(OpEffect::None),
    }
}

fn exec_find_first(args: &Value, into_var: Option<&str>) -> Result<OpEffect> {
    let into = into_var.ok_or_else(|| anyhow!("FindFirst requires into_var on the HookOp spec"))?;
    let arr = args
        .get("from")
        .ok_or_else(|| anyhow!("FindFirst requires args.from (array)"))?;
    let where_obj = args
        .get("where")
        .and_then(|v| v.as_object())
        .ok_or_else(|| anyhow!("FindFirst requires args.where (object of field→value)"))?;
    let items: &[Value] = match arr {
        Value::Array(a) => a.as_slice(),
        Value::Null => &[],
        other => bail!("FindFirst args.from must be array or null, got {other:?}"),
    };
    let mut found: Value = Value::Null;
    'outer: for item in items {
        for (k, expected) in where_obj {
            // Walk dotted path inside the element.
            let actual = walk_dotted(item, k);
            if actual.as_ref() != Some(expected) {
                continue 'outer;
            }
        }
        found = item.clone();
        break;
    }
    Ok(OpEffect::SetVar {
        key: into.to_string(),
        value: found,
    })
}

fn walk_dotted(root: &Value, path: &str) -> Option<Value> {
    let mut cur = root.clone();
    for seg in path.split('.') {
        cur = match &cur {
            Value::Object(m) => m.get(seg).cloned()?,
            Value::Array(a) => {
                let idx: usize = seg.parse().ok()?;
                a.get(idx).cloned()?
            }
            _ => return None,
        };
    }
    Some(cur)
}

async fn exec_http_json(args: &Value, into_var: Option<&str>) -> Result<OpEffect> {
    // Parse + execute via the shared HTTP-fetch primitive — same shape
    // the daemon-level poller consumes. The op is a thin wrapper that
    // also handles `into_var` capture; everything else (request build,
    // status classification, response decoding) lives in http_fetch.
    let mut spec = crate::orchestration::http_fetch::HttpFetchSpec::from_args(args)?;
    // `secret_headers` resolves `secret:<name>` refs at request time so
    // raw token values never appear in vars, logs, or JSON artifacts.
    // Values are merged into spec.headers AFTER from_args so they override
    // any same-named key in the normal `headers` field.
    if let Some(secret_hdrs) = args.get("secret_headers").and_then(|v| v.as_object()) {
        for (header_name, raw_val) in secret_hdrs {
            let raw_str = raw_val
                .as_str()
                .ok_or_else(|| anyhow!("secret_headers.{header_name} must be a string"))?;
            let resolved = resolve_secret_header_value(raw_str)
                .map_err(|e| anyhow!("secret_headers.{header_name}: {e}"))?;
            spec.headers.insert(header_name.clone(), resolved);
        }
    }
    let result = spec.execute().await?;
    match into_var {
        Some(k) => Ok(OpEffect::SetVar {
            key: k.to_string(),
            value: result.value,
        }),
        None => Ok(OpEffect::None),
    }
}

/// Resolve a header value that contains a `secret:<name>` reference.
/// The `secret:<name>` substring is replaced with the resolved secret value.
/// Example: `"token secret:my-token"` → `"token <actual-value>"`.
/// Rejects values that contain no `secret:` reference — use the normal
/// `headers` field for static values.
fn resolve_secret_header_value(value: &str) -> Result<String> {
    let Some(start) = value.find("secret:") else {
        bail!(
            "secret_headers value must contain a 'secret:<name>' reference; \
             use the 'headers' field for static values"
        )
    };
    let rest = &value[start + "secret:".len()..];
    let end = rest.find(|c: char| c.is_whitespace()).unwrap_or(rest.len());
    let name = &rest[..end];
    // resolve() validates the name and returns an error without exposing the value.
    let secret = blackbox::secrets::resolve(name)
        .map_err(|_| anyhow!("secret_headers: secret '{name}' could not be resolved"))?;
    let prefix = &value[..start];
    let suffix = &rest[end..];
    Ok(format!("{}{}{}", prefix, secret.expose(), suffix))
}

// ── Built-in op implementations ──────────────────────────────────

fn exec_set_var(args: &Value) -> Result<OpEffect> {
    // Two arg shapes accepted:
    //   { "key": "name", "value": <any> }
    //   { "name": <any>, "other_name": <any>, ... }   (bulk form)
    if let Some(obj) = args.as_object() {
        if let (Some(Value::String(k)), Some(v)) = (obj.get("key"), obj.get("value")) {
            return Ok(OpEffect::SetVar {
                key: k.clone(),
                value: v.clone(),
            });
        }
        // Bulk form: every key is a var name. We can only return one
        // effect here; the runner treats SetVar as a single mutation,
        // so bulk form is implemented by emitting multiple effects via
        // the special `Bulk` shape — keep it simple, require `{key,
        // value}` for now.
        if obj.len() == 1 {
            let (k, v) = obj.iter().next().unwrap();
            return Ok(OpEffect::SetVar {
                key: k.clone(),
                value: v.clone(),
            });
        }
    }
    bail!("SetVar args must be {{key,value}} or a single-entry object, got: {args}")
}

fn exec_default_var(args: &Value, ctx: &ArcContext) -> Result<OpEffect> {
    let key = args
        .get("key")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("DefaultVar requires args.key (string)"))?;
    let value = args
        .get("value")
        .ok_or_else(|| anyhow!("DefaultVar requires args.value"))?;
    if ctx.vars.get(key).is_some_and(|v| !v.is_null()) {
        return Ok(OpEffect::None);
    }
    Ok(OpEffect::SetVar {
        key: key.to_string(),
        value: value.clone(),
    })
}

fn exec_inc_var(args: &Value, ctx: &ArcContext) -> Result<OpEffect> {
    let key = args
        .get("key")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("IncVar requires args.key (string)"))?;
    let by = args.get("by").and_then(|v| v.as_i64()).unwrap_or(1);
    let current = ctx.vars.get(key).and_then(|v| v.as_i64()).unwrap_or(0);
    Ok(OpEffect::SetVar {
        key: key.to_string(),
        value: json!(current + by),
    })
}

fn exec_append_var(args: &Value, ctx: &ArcContext) -> Result<OpEffect> {
    let key = args
        .get("key")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("AppendVar requires args.key (string)"))?;
    let value = args
        .get("value")
        .ok_or_else(|| anyhow!("AppendVar requires args.value"))?;
    let mut arr = match ctx.vars.get(key).cloned() {
        Some(Value::Array(a)) => a,
        Some(Value::Null) | None => Vec::new(),
        Some(other) => {
            bail!("AppendVar: vars[{key}] is {other:?}, not an array");
        }
    };
    arr.push(value.clone());
    Ok(OpEffect::SetVar {
        key: key.to_string(),
        value: Value::Array(arr),
    })
}

fn exec_merge_var(args: &Value, ctx: &ArcContext) -> Result<OpEffect> {
    let key = args
        .get("key")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("MergeVar requires args.key (string)"))?;
    let value = args
        .get("value")
        .and_then(|v| v.as_object())
        .ok_or_else(|| anyhow!("MergeVar requires args.value (object)"))?;
    let mut merged = match ctx.vars.get(key).cloned() {
        Some(Value::Object(m)) => m,
        Some(Value::Null) | None => Map::new(),
        Some(other) => bail!("MergeVar: vars[{key}] is {other:?}, not an object"),
    };
    for (k, v) in value {
        merged.insert(k.clone(), v.clone());
    }
    Ok(OpEffect::SetVar {
        key: key.to_string(),
        value: Value::Object(merged),
    })
}

fn exec_normalize_arch_pathology_atom_requests(
    args: &Value,
    into_var: Option<&str>,
) -> Result<OpEffect> {
    let into =
        into_var.ok_or_else(|| anyhow!("NormalizeArchPathologyAtomRequests requires into_var"))?;
    let requests = args
        .get("requests")
        .ok_or_else(|| anyhow!("NormalizeArchPathologyAtomRequests requires args.requests"))?;
    let defaults = args
        .get("defaults")
        .and_then(Value::as_object)
        .ok_or_else(|| {
            anyhow!("NormalizeArchPathologyAtomRequests requires args.defaults object")
        })?;
    let requests = coerce_json_value(requests);
    let requests = requests.as_array().ok_or_else(|| {
        anyhow!("NormalizeArchPathologyAtomRequests requires requests to be an array")
    })?;

    let allowed_atoms = [
        "atom:java-architecture-role-behavior-coherence@v1",
        "atom:java-architecture-responsibility-bleed@v1",
        "atom:java-architecture-conceptual-duplicate-discovery@v1",
        "atom:java-architecture-anemic-data-remote-behavior@v1",
        "atom:java-architecture-scoped-context-capture@v1",
        "atom:java-architecture-framework-contract-violation@v1",
        "atom:java-architecture-test-implied-architecture@v1",
        "atom:java-architecture-transcript-anchored-pressure@v1",
    ];

    let mut normalized = Vec::with_capacity(requests.len());
    for (idx, request) in requests.iter().enumerate() {
        let request = coerce_json_value(request);
        let request = request
            .as_object()
            .ok_or_else(|| anyhow!("atom request #{idx} must be an object, got {request:?}"))?;
        let atom_ref = request
            .get("atom_ref")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow!("atom request #{idx} missing atom_ref"))?;
        if !allowed_atoms.contains(&atom_ref) {
            bail!("atom request #{idx} uses unsupported atom_ref '{atom_ref}'");
        }

        let mut request_args = request
            .get("args")
            .map(coerce_json_value)
            .unwrap_or_else(|| Value::Object(Map::new()));
        let request_args_obj = request_args.as_object_mut().ok_or_else(|| {
            anyhow!("atom request #{idx} args must be an object after normalization")
        })?;
        for key in [
            "project_dir",
            "scope_filter",
            "target_loci",
            "operator_hints",
            "layer_model_path",
            "target_context_window",
            "whole_project_mode",
            "whiteboard_id",
        ] {
            if !request_args_obj.contains_key(key) {
                if let Some(value) = defaults.get(key) {
                    request_args_obj.insert(key.to_string(), value.clone());
                }
            }
        }

        let survey_json = request_args_obj
            .remove("survey_json")
            .map(|v| ensure_objectish_json(coerce_json_value(&v)))
            .transpose()?
            .or_else(|| defaults.get("survey_json").cloned())
            .map(ensure_objectish_json)
            .transpose()?
            .unwrap_or_else(|| json!({}));
        request_args_obj.insert("survey_json".to_string(), survey_json);

        normalize_array_field(request_args_obj, defaults, "target_loci");
        normalize_array_field(request_args_obj, defaults, "operator_hints");

        normalized.push(json!({
            "atom_ref": atom_ref,
            "args": Value::Object(request_args_obj.clone()),
        }));
    }

    Ok(OpEffect::SetVar {
        key: into.to_string(),
        value: Value::Array(normalized),
    })
}

/// Execute a shell command. When `into_var` is provided the op captures
/// `{exit_code, stdout, stderr, parsed}` into `vars[into_var]` and does NOT
/// fail on a non-zero exit code — the caller decides what to do with the
/// exit code via downstream ops or packet gates. Without `into_var` the
/// original behaviour is preserved: non-zero exit aborts the op.
async fn exec_shell(args: &Value, into_var: Option<&str>, ctx: &ArcContext) -> Result<OpEffect> {
    let argv = args
        .get("argv")
        .and_then(|v| v.as_array())
        .ok_or_else(|| anyhow!("Shell requires args.argv (array of strings)"))?;
    if argv.is_empty() {
        bail!("Shell argv is empty");
    }
    let strs: Vec<String> = argv
        .iter()
        .map(|v| {
            v.as_str()
                .map(|s| s.to_string())
                .ok_or_else(|| anyhow!("Shell argv entries must be strings"))
        })
        .collect::<Result<Vec<_>>>()?;
    let cwd = args
        .get("cwd")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .or_else(|| ctx.meta.worktree.clone())
        .or_else(|| ctx.meta.project_dir.clone());
    let stdin_payload = args
        .get("stdin")
        .map(shell_stdin_payload)
        .transpose()
        .map_err(|e| anyhow!("Shell stdin: {e}"))?;
    let mut cmd = tokio::process::Command::new(&strs[0]);
    cmd.args(&strs[1..])
        .stdin(if stdin_payload.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(d) = &cwd {
        cmd.current_dir(d);
    }
    if let Some(env) = args.get("env").and_then(|v| v.as_object()) {
        for (key, value) in env {
            let value = value
                .as_str()
                .ok_or_else(|| anyhow!("Shell env.{key} must be a string"))?;
            cmd.env(key, value);
        }
    }
    if let Some(secret_env) = args.get("secret_env").and_then(|v| v.as_object()) {
        for (key, raw_value) in secret_env {
            let raw_value = raw_value
                .as_str()
                .ok_or_else(|| anyhow!("Shell secret_env.{key} must be a string"))?;
            let resolved = resolve_secret_header_value(raw_value)
                .map_err(|e| anyhow!("Shell secret_env.{key}: {e}"))?;
            cmd.env(key, resolved);
        }
    }
    let output = if let Some(payload) = stdin_payload {
        let mut child = cmd
            .spawn()
            .map_err(|e| anyhow!("Shell spawn failed: {e}"))?;
        if let Some(mut stdin) = child.stdin.take() {
            stdin
                .write_all(payload.as_bytes())
                .await
                .map_err(|e| anyhow!("Shell stdin write failed: {e}"))?;
        }
        child
            .wait_with_output()
            .await
            .map_err(|e| anyhow!("Shell wait failed: {e}"))?
    } else {
        cmd.output()
            .await
            .map_err(|e| anyhow!("Shell spawn failed: {e}"))?
    };

    let exit_code = output.status.code().unwrap_or(-1);
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr_str = String::from_utf8_lossy(&output.stderr).to_string();

    // When into_var is set: always capture output, never fail on non-zero exit.
    // Downstream ops (ScoreEvalOutput, packet gates) decide what to do with it.
    if let Some(key) = into_var {
        let parsed: Value = serde_json::from_str(stdout.trim()).unwrap_or(Value::Null);
        return Ok(OpEffect::SetVar {
            key: key.to_string(),
            value: json!({
                "exit_code": exit_code,
                "stdout": stdout,
                "stderr": stderr_str,
                "parsed": parsed,
            }),
        });
    }

    if !output.status.success() {
        bail!("Shell {strs:?} exited with {exit_code}: {stderr_str}",);
    }
    Ok(OpEffect::None)
}

fn shell_stdin_payload(value: &Value) -> Result<String> {
    match value {
        Value::String(s) => Ok(s.clone()),
        other => Ok(serde_json::to_string(other)?),
    }
}

fn exec_write_arch_pathology_plan(args: &Value, into_var: Option<&str>) -> Result<OpEffect> {
    let project_dir = args
        .get("project_dir")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("WriteArchPathologyPlan requires args.project_dir"))?;
    let slug = args
        .get("slug")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("WriteArchPathologyPlan requires args.slug"))?;
    let scope = args
        .get("scope")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("WriteArchPathologyPlan requires args.scope"))?;
    let baseline_commit = args
        .get("baseline_commit")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("WriteArchPathologyPlan requires args.baseline_commit"))?;
    let target_context_window = args
        .get("target_context_window")
        .and_then(|v| v.as_i64())
        .unwrap_or(10_000);
    let plan = args
        .get("plan")
        .and_then(|v| v.as_object())
        .ok_or_else(|| anyhow!("WriteArchPathologyPlan requires args.plan object"))?;

    let project_root = PathBuf::from(project_dir)
        .canonicalize()
        .map_err(|e| anyhow!("canonicalize project_dir {project_dir}: {e}"))?;
    let slug = arch_pathology_slug(slug);
    let out_dir = project_root.join("design").join("refactor").join("plans");
    fs::create_dir_all(&out_dir).map_err(|e| {
        anyhow!(
            "create correction-plan directory {}: {e}",
            out_dir.display()
        )
    })?;
    let out_path = out_dir.join(format!("{slug}.md"));

    let title = plan
        .get("title")
        .and_then(|v| v.as_str())
        .filter(|s| !s.trim().is_empty())
        .map(str::to_owned)
        .unwrap_or_else(|| format!("Architecture Correction Plan: {scope}"));
    let brief = plan
        .get("brief")
        .and_then(|v| v.as_str())
        .filter(|s| !s.trim().is_empty())
        .unwrap_or("Architecture pathology correction plan.");
    let criteria = plan
        .get("acceptance_criteria")
        .cloned()
        .unwrap_or_else(|| Value::Array(Vec::new()));
    let plan_rel = out_path
        .strip_prefix(&project_root)
        .unwrap_or(&out_path)
        .to_string_lossy()
        .replace('\\', "/");
    let today = chrono::Utc::now().date_naive();
    let body = format!(
        concat!(
            "---\n",
            "title: \"{}\"\n",
            "kind: correction-plan\n",
            "lifecycle: proposed\n",
            "corpus: project-refactor\n",
            "topic:\n",
            "  - refactor-plan\n",
            "  - architecture\n",
            "date: {}\n",
            "baseline_commit: {}\n",
            "generated_by: arch-pathology\n",
            "scope: \"{}\"\n",
            "brief: \"{}\"\n",
            "---\n\n",
            "# {}\n\n",
            "## Diagnosis Summary\n\n{}\n\n",
            "## Evidence\n\n{}\n\n",
            "## Remediation Plan\n\n{}\n\n",
            "## Acceptance Criteria\n\n{}\n\n",
            "## Deferred\n\n{}\n\n",
            "## Dispatch Payload\n\n{}\n"
        ),
        yaml_quote(&title),
        today,
        baseline_commit.trim(),
        yaml_quote(scope),
        yaml_quote(brief),
        title,
        arch_pathology_markdown(
            plan.get("diagnosis_summary"),
            "No diagnosis survived review."
        ),
        arch_pathology_markdown(plan.get("evidence"), "No evidence was retained."),
        arch_pathology_markdown(
            plan.get("remediation_plan"),
            "No remediation slices were retained."
        ),
        arch_pathology_criteria_markdown(&criteria),
        arch_pathology_markdown(
            plan.get("deferred"),
            "No deferred candidates were recorded."
        ),
        arch_pathology_dispatch_payload(
            project_root.to_string_lossy().as_ref(),
            &plan_rel,
            &criteria,
            target_context_window
        )
    );
    fs::write(&out_path, body)
        .map_err(|e| anyhow!("write correction plan {}: {e}", out_path.display()))?;

    let result = json!({
        "plan_path": plan_rel,
        "absolute_plan_path": out_path.to_string_lossy(),
        "acceptance_criteria": criteria,
    });
    if let Some(key) = into_var {
        Ok(OpEffect::SetVar {
            key: key.to_string(),
            value: result,
        })
    } else {
        Ok(OpEffect::None)
    }
}

fn arch_pathology_slug(raw: &str) -> String {
    let mut slug = String::new();
    let mut prev_dash = false;
    for ch in raw.trim().chars().flat_map(char::to_lowercase) {
        if ch.is_ascii_alphanumeric() || matches!(ch, '.' | '_' | '-') {
            slug.push(ch);
            prev_dash = false;
        } else if !prev_dash {
            slug.push('-');
            prev_dash = true;
        }
    }
    let slug = slug
        .trim_matches(|c| matches!(c, '-' | '.' | '_'))
        .to_string();
    if slug.is_empty() {
        "architecture-correction-plan".to_string()
    } else {
        slug
    }
}

fn yaml_quote(raw: &str) -> String {
    raw.replace('\\', "\\\\").replace('"', "\\\"")
}

fn arch_pathology_markdown(value: Option<&Value>, fallback: &str) -> String {
    match value {
        Some(Value::String(s)) if !s.trim().is_empty() => s.trim().to_string(),
        Some(Value::Array(items)) if !items.is_empty() => items
            .iter()
            .map(|item| match item {
                Value::String(s) => format!("- {s}"),
                other => format!("- `{}`", serde_json::to_string(other).unwrap_or_default()),
            })
            .collect::<Vec<_>>()
            .join("\n"),
        _ => fallback.to_string(),
    }
}

fn arch_pathology_criteria_markdown(criteria: &Value) -> String {
    let Some(items) = criteria.as_array() else {
        return "- AP-1: The reviewed correction plan has at least one concrete acceptance criterion before PD dispatch.".to_string();
    };
    if items.is_empty() {
        return "- AP-1: The reviewed correction plan has at least one concrete acceptance criterion before PD dispatch.".to_string();
    }
    items
        .iter()
        .enumerate()
        .map(|(idx, item)| {
            let default_id = format!("AP-{}", idx + 1);
            if let Some(obj) = item.as_object() {
                let id = obj
                    .get("id")
                    .and_then(|v| v.as_str())
                    .unwrap_or(&default_id);
                let text = obj
                    .get("criterion_text")
                    .or_else(|| obj.get("text"))
                    .and_then(|v| v.as_str())
                    .filter(|s| !s.trim().is_empty())
                    .map(str::to_string)
                    .unwrap_or_else(|| serde_json::to_string(item).unwrap_or_default());
                format!("- {id}: {text}")
            } else {
                format!("- {default_id}: {item}")
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn arch_pathology_dispatch_payload(
    project_dir: &str,
    plan_rel: &str,
    criteria: &Value,
    target_context_window: i64,
) -> String {
    let criteria = criteria.as_array().cloned().unwrap_or_default();
    let payload = json!({
        "workflow_id": "phase-decompose-main-edit",
        "project_dir": project_dir,
        "initial_vars": {
            "phase_doc_path": plan_rel,
            "phase_doc_text": "<full correction plan text>",
            "project_dir": project_dir,
            "target_context_window": target_context_window,
            "epoch": 0,
            "max_epochs": 3,
            "acceptance_criteria": criteria,
        }
    });
    format!(
        "```json\n{}\n```",
        serde_json::to_string_pretty(&payload).unwrap_or_default()
    )
}

fn exec_set_meta(args: &Value) -> Result<OpEffect> {
    // Mutable meta keys: `worktree`, `project_dir`. Other meta fields
    // are arc-intrinsic and immutable.
    let key = args
        .get("key")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("SetMeta requires args.key"))?;
    let value = args.get("value").cloned().unwrap_or(Value::Null);
    match key {
        "worktree" => {
            let v = match value {
                Value::String(s) => Some(s),
                Value::Null => None,
                other => bail!("SetMeta worktree must be string or null, got {other:?}"),
            };
            Ok(OpEffect::SetWorktree(v))
        }
        "project_dir" => {
            let v = match value {
                Value::String(s) if s.is_empty() => None,
                Value::String(s) => Some(s),
                Value::Null => None,
                other => bail!("SetMeta project_dir must be string or null, got {other:?}"),
            };
            Ok(OpEffect::SetProjectDir(v))
        }
        other => bail!("SetMeta: unsupported key '{other}' (mutable keys: worktree, project_dir)"),
    }
}

// ── Schema migration ops ─────────────────────────────────────────────────────

/// Observable marker for the index-drop node. Logs intent and returns None;
/// the actual document deletion is performed by the following
/// `SchemaMigrationRebuild` op via a full `bbox_reindex`.
fn exec_schema_migration_drop(ctx: &ArcContext) -> OpEffect {
    tracing::info!(
        arc_id = %ctx.meta.arc_id,
        project = ?ctx.meta.project_dir,
        "schema_migration_drop: marking index for full rebuild"
    );
    OpEffect::None
}

/// Full tantivy rebuild via `bbox_reindex(full=true)`. Captures a JSON
/// summary into `vars[into_var]` when set.
async fn exec_schema_migration_rebuild(
    into_var: Option<&str>,
    ctx: &ArcContext,
) -> Result<OpEffect> {
    let result = call_blackbox_tool("bbox_reindex", json!({"full": true}), ctx).await?;
    tracing::info!(
        arc_id = %ctx.meta.arc_id,
        "schema_migration_rebuild: full reindex complete"
    );
    match into_var {
        Some(k) => Ok(OpEffect::SetVar {
            key: k.to_string(),
            value: result,
        }),
        None => Ok(OpEffect::None),
    }
}

// ── Eval score op ────────────────────────────────────────────────────────────

/// Parse captured shell output from a `RunSuite` step and compute `drift_pp`.
///
/// Reads `args.from` (or `args.suite_output`) — a `{exit_code, stdout, stderr,
/// parsed}` blob captured by the preceding Shell op — and extracts:
///   1. `parsed.drift_pp`   if the script emitted a JSON summary
///   2. Exit-code heuristic: non-zero exit → assume minor drift (5 pp)
///   3. Default 0.0          when neither signal is present
///
/// Writes `{drift_pp, suite_exit_code, raw_stdout}` into `vars[into_var]`.
fn exec_score_eval_output(
    args: &Value,
    into_var: Option<&str>,
    ctx: &ArcContext,
) -> Result<OpEffect> {
    let into = into_var.unwrap_or("suite_score");
    let suite_output = args
        .get("from")
        .or_else(|| args.get("suite_output"))
        .or_else(|| ctx.vars.get("suite_output"))
        .cloned()
        .unwrap_or(Value::Null);

    let exit_code = suite_output
        .get("exit_code")
        .and_then(Value::as_i64)
        .unwrap_or(0);
    let stdout = suite_output
        .get("stdout")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();

    // Extract drift_pp from the parsed JSON block first, then from stdout as
    // a last-resort inline search for `"drift_pp": N`.
    let drift_pp: f64 = suite_output
        .get("parsed")
        .and_then(|p| p.get("drift_pp"))
        .and_then(Value::as_f64)
        .or_else(|| {
            // Try to parse stdout directly as JSON
            serde_json::from_str::<Value>(&stdout)
                .ok()
                .and_then(|v| v.get("drift_pp").and_then(Value::as_f64))
        })
        .unwrap_or({
            // Exit-code heuristic: non-zero → minor drift signal
            if exit_code != 0 { 5.0 } else { 0.0 }
        });

    Ok(OpEffect::SetVar {
        key: into.to_string(),
        value: json!({
            "drift_pp": drift_pp,
            "suite_exit_code": exit_code,
            "raw_stdout": stdout,
        }),
    })
}

// ── PickFirst op ─────────────────────────────────────────────────────────────

/// Pick the first element of an array into `vars[into_var]`.
///
/// Reads the array from:
/// - `args.from` if it is already an array value (e.g. a rendered template),
/// - `vars[args.array]` if `args.array` is a simple var name,
/// - `vars[args.array.path.…]` via dotted-path walk when the var is a nested
///   object (e.g. `"array": "candidate_pairs.candidates"`).
///
/// Writes `Value::Null` when the array is absent or empty.
fn exec_pick_first(args: &Value, into_var: Option<&str>, ctx: &ArcContext) -> Result<OpEffect> {
    let into = into_var.ok_or_else(|| anyhow!("PickFirst requires into_var"))?;
    let resolved: Option<Value> = if let Some(from) = args.get("from") {
        Some(from.clone())
    } else if let Some(path) = args.get("array").and_then(Value::as_str) {
        // Walk dotted path inside vars.
        let mut cur = Value::Object(ctx.vars.clone());
        for seg in path.split('.') {
            cur = match cur {
                Value::Object(m) => m.get(seg).cloned().unwrap_or(Value::Null),
                Value::Array(a) => {
                    if let Ok(i) = seg.parse::<usize>() {
                        a.get(i).cloned().unwrap_or(Value::Null)
                    } else {
                        Value::Null
                    }
                }
                _ => Value::Null,
            };
        }
        Some(cur)
    } else {
        None
    };
    let first = resolved
        .as_ref()
        .and_then(Value::as_array)
        .and_then(|a| a.first())
        .cloned()
        .unwrap_or(Value::Null);
    Ok(OpEffect::SetVar {
        key: into.to_string(),
        value: first,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workflow::context::{ArcContext, ArcMeta};

    #[tokio::test]
    async fn normalize_arch_pathology_atom_requests_parses_nested_survey_json() {
        let ctx = ArcContext::new(ArcMeta::default());
        let hook = HookOp {
            op: OpKind::NormalizeArchPathologyAtomRequests,
            args: json!({
                "requests": [
                    {
                        "atom_ref": "atom:java-architecture-role-behavior-coherence@v1",
                        "args": {
                            "survey_json": "{\"focus\":\"view layer\"}",
                            "target_loci": "webapp/src/main/java"
                        }
                    }
                ],
                "defaults": {
                    "project_dir": "/repo",
                    "scope_filter": ".",
                    "target_loci": [],
                    "operator_hints": ["hint"],
                    "layer_model_path": "",
                    "survey_json": {"survey_summary": "fallback"},
                    "target_context_window": 10000,
                    "whole_project_mode": true,
                    "whiteboard_id": "board-1"
                }
            }),
            when: None,
            on_failure: OnFailure::Halt,
            into_var: Some("atom_requests".into()),
        };
        let effect = execute_op(&hook, &ctx, None).await.unwrap();
        match effect {
            OpEffect::SetVar { key, value } => {
                assert_eq!(key, "atom_requests");
                assert_eq!(value[0]["args"]["survey_json"]["focus"], "view layer");
                assert_eq!(value[0]["args"]["project_dir"], "/repo");
                assert_eq!(
                    value[0]["args"]["target_loci"],
                    json!(["webapp/src/main/java"])
                );
                assert_eq!(value[0]["args"]["operator_hints"], json!(["hint"]));
            }
            _ => panic!("expected SetVar"),
        }
    }

    #[tokio::test]
    async fn find_first_returns_match() {
        let ctx = ArcContext::new(ArcMeta::default());
        let hook = HookOp {
            op: OpKind::FindFirst,
            args: json!({
                "from": [
                    {"head": {"ref": "feat/x"}, "number": 1},
                    {"head": {"ref": "fix/issue-42"}, "number": 2},
                    {"head": {"ref": "fix/issue-99"}, "number": 3}
                ],
                "where": {"head.ref": "fix/issue-42"}
            }),
            when: None,
            on_failure: OnFailure::Halt,
            into_var: Some("matched".into()),
        };
        let effect = execute_op(&hook, &ctx, None).await.unwrap();
        match effect {
            OpEffect::SetVar { key, value } => {
                assert_eq!(key, "matched");
                assert_eq!(value, json!({"head": {"ref": "fix/issue-42"}, "number": 2}));
            }
            _ => panic!("expected SetVar"),
        }
    }

    #[tokio::test]
    async fn find_first_returns_null_on_no_match() {
        let ctx = ArcContext::new(ArcMeta::default());
        let hook = HookOp {
            op: OpKind::FindFirst,
            args: json!({
                "from": [{"head": {"ref": "feat/x"}}],
                "where": {"head.ref": "absent"}
            }),
            when: None,
            on_failure: OnFailure::Halt,
            into_var: Some("matched".into()),
        };
        let effect = execute_op(&hook, &ctx, None).await.unwrap();
        match effect {
            OpEffect::SetVar { value, .. } => assert_eq!(value, Value::Null),
            _ => panic!("expected SetVar"),
        }
    }

    #[tokio::test]
    async fn find_first_handles_null_input() {
        let ctx = ArcContext::new(ArcMeta::default());
        let hook = HookOp {
            op: OpKind::FindFirst,
            args: json!({
                "from": null,
                "where": {"x": 1}
            }),
            when: None,
            on_failure: OnFailure::Halt,
            into_var: Some("matched".into()),
        };
        let effect = execute_op(&hook, &ctx, None).await.unwrap();
        match effect {
            OpEffect::SetVar { value, .. } => assert_eq!(value, Value::Null),
            _ => panic!("expected SetVar"),
        }
    }

    #[tokio::test]
    async fn set_var_writes_value() {
        let ctx = ArcContext::new(ArcMeta::default());
        let hook = HookOp {
            op: OpKind::SetVar,
            args: json!({"key": "x", "value": 42}),
            when: None,
            on_failure: OnFailure::Halt,
            into_var: None,
        };
        let effect = execute_op(&hook, &ctx, None).await.unwrap();
        match effect {
            OpEffect::SetVar { key, value } => {
                assert_eq!(key, "x");
                assert_eq!(value, json!(42));
            }
            _ => panic!("expected SetVar effect"),
        }
    }

    #[tokio::test]
    async fn default_var_only_writes_missing_value() {
        let mut ctx = ArcContext::new(ArcMeta::default());
        let hook = HookOp {
            op: OpKind::DefaultVar,
            args: json!({"key": "sub_unit", "value": {}}),
            when: None,
            on_failure: OnFailure::Halt,
            into_var: None,
        };
        let effect = execute_op(&hook, &ctx, None).await.unwrap();
        match effect {
            OpEffect::SetVar { key, value } => {
                assert_eq!(key, "sub_unit");
                assert_eq!(value, json!({}));
            }
            _ => panic!("expected SetVar effect"),
        }

        ctx.vars
            .insert("sub_unit".to_string(), json!({"sub_unit_id": "su-1"}));
        let effect = execute_op(&hook, &ctx, None).await.unwrap();
        assert!(matches!(effect, OpEffect::None));
    }

    #[tokio::test]
    async fn inc_var_increments() {
        let mut ctx = ArcContext::new(ArcMeta::default());
        ctx.vars.insert("counter".into(), json!(5));
        let hook = HookOp {
            op: OpKind::IncVar,
            args: json!({"key": "counter", "by": 3}),
            when: None,
            on_failure: OnFailure::Halt,
            into_var: None,
        };
        let effect = execute_op(&hook, &ctx, None).await.unwrap();
        match effect {
            OpEffect::SetVar { key, value } => {
                assert_eq!(key, "counter");
                assert_eq!(value, json!(8));
            }
            _ => panic!("expected SetVar effect"),
        }
    }

    #[tokio::test]
    async fn parse_json_strips_code_fence() {
        let ctx = ArcContext::new(ArcMeta::default());
        let hook = HookOp {
            op: OpKind::ParseJson,
            args: json!({"from": "```json\n{\"x\": 1}\n```"}),
            when: None,
            on_failure: OnFailure::Halt,
            into_var: Some("parsed".into()),
        };
        let effect = execute_op(&hook, &ctx, None).await.unwrap();
        match effect {
            OpEffect::SetVar { key, value } => {
                assert_eq!(key, "parsed");
                assert_eq!(value, json!({"x": 1}));
            }
            _ => panic!("expected SetVar effect"),
        }
    }

    #[tokio::test]
    async fn parse_json_extracts_fenced_block_after_prose_preamble() {
        // LLMs commonly precede the structured JSON with prose
        // ("Here's the result:\n\n```json\n{...}\n```"). Earlier
        // strip_code_fence required the fence opener on line 1.
        // Now it falls back to first-fenced-block-anywhere when the
        // first line isn't a fence.
        let ctx = ArcContext::new(ArcMeta::default());
        let body = "Scoring meatiness on the top candidates.\n\nEmitting reply now.\n\n```json\n{\"scout_charters\": [{\"scout_id\": \"s1\"}]}\n```";
        let hook = HookOp {
            op: OpKind::ParseJson,
            args: json!({ "from": body }),
            when: None,
            on_failure: OnFailure::Halt,
            into_var: Some("parsed".into()),
        };
        let effect = execute_op(&hook, &ctx, None).await.unwrap();
        match effect {
            OpEffect::SetVar { key, value } => {
                assert_eq!(key, "parsed");
                assert_eq!(value, json!({"scout_charters": [{"scout_id": "s1"}]}));
            }
            _ => panic!("expected SetVar effect"),
        }
    }

    #[tokio::test]
    async fn parse_json_extracts_inline_object_after_prose_preamble() {
        let ctx = ArcContext::new(ArcMeta::default());
        let body = "Acknowledged - single discovery, no task tracker needed.\n\n{\"tldr\":\"ok\",\"leads_entity_refs\":[]}";
        let hook = HookOp {
            op: OpKind::ParseJson,
            args: json!({ "from": body }),
            when: None,
            on_failure: OnFailure::Halt,
            into_var: Some("parsed".into()),
        };
        let effect = execute_op(&hook, &ctx, None).await.unwrap();
        match effect {
            OpEffect::SetVar { key, value } => {
                assert_eq!(key, "parsed");
                assert_eq!(value, json!({"tldr": "ok", "leads_entity_refs": []}));
            }
            _ => panic!("expected SetVar effect"),
        }
    }

    #[tokio::test]
    async fn parse_json_extracts_first_balanced_object_before_trailing_text() {
        let ctx = ArcContext::new(ArcMeta::default());
        let body = "{\"triage_verdict\":\"needs_decompose\",\"evidence_bundle\":{\"degraded\":{\"unresolved_refs\":[]}}}} trailing";
        let hook = HookOp {
            op: OpKind::ParseJson,
            args: json!({ "from": body }),
            when: None,
            on_failure: OnFailure::Halt,
            into_var: Some("parsed".into()),
        };
        let effect = execute_op(&hook, &ctx, None).await.unwrap();
        match effect {
            OpEffect::SetVar { key, value } => {
                assert_eq!(key, "parsed");
                assert_eq!(
                    value,
                    json!({
                        "triage_verdict": "needs_decompose",
                        "evidence_bundle": {"degraded": {"unresolved_refs": []}}
                    })
                );
            }
            _ => panic!("expected SetVar effect"),
        }
    }

    #[tokio::test]
    async fn parse_json_repairs_missing_trailing_delimiters_when_enabled() {
        let ctx = ArcContext::new(ArcMeta::default());
        let body = "{\"sub_units\":[{\"sub_unit_id\":\"su-1\"}],\"recompose_contract\":{\"leftover_acceptance_ids\":[]}";
        let hook = HookOp {
            op: OpKind::ParseJson,
            args: json!({
                "from": body,
                "repair_missing_closing_delimiters": true
            }),
            when: None,
            on_failure: OnFailure::Halt,
            into_var: Some("parsed".into()),
        };
        let effect = execute_op(&hook, &ctx, None).await.unwrap();
        match effect {
            OpEffect::SetVar { key, value } => {
                assert_eq!(key, "parsed");
                assert_eq!(
                    value,
                    json!({
                        "sub_units": [{"sub_unit_id": "su-1"}],
                        "recompose_contract": {"leftover_acceptance_ids": []}
                    })
                );
            }
            _ => panic!("expected SetVar effect"),
        }
    }

    #[tokio::test]
    async fn parse_json_does_not_repair_missing_trailing_delimiters_by_default() {
        let ctx = ArcContext::new(ArcMeta::default());
        let hook = HookOp {
            op: OpKind::ParseJson,
            args: json!({
                "from": "{\"sub_units\":[{\"sub_unit_id\":\"su-1\"}]"
            }),
            when: None,
            on_failure: OnFailure::Halt,
            into_var: Some("parsed".into()),
        };
        let err = execute_op(&hook, &ctx, None).await.unwrap_err();
        assert!(
            err.to_string().contains("input did not parse as JSON"),
            "unexpected error: {err:#}"
        );
    }

    #[tokio::test]
    async fn shell_runs_command() {
        let ctx = ArcContext::new(ArcMeta::default());
        let hook = HookOp {
            op: OpKind::Shell,
            args: json!({"argv": ["true"]}),
            when: None,
            on_failure: OnFailure::Halt,
            into_var: None,
        };
        let effect = execute_op(&hook, &ctx, None).await.unwrap();
        assert!(matches!(effect, OpEffect::None));
    }

    #[tokio::test]
    async fn worktree_create_reuses_existing_branch() {
        // Regression: a previous arc died and left `fix/issue-N`
        // around. The next arc tries WorktreeCreate with the same
        // branch name. Old behavior: hard-fail with `git worktree add
        // -b <branch>` saying the branch exists. New: detect the
        // free branch and reuse it.
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        std::fs::create_dir(&repo).unwrap();
        let git = |args: &[&str]| {
            std::process::Command::new("git")
                .arg("-C")
                .arg(&repo)
                .args(args)
                .output()
                .unwrap()
        };
        // Initial repo with a commit on main.
        git(&["init", "-q", "-b", "main"]);
        git(&["config", "user.email", "t@t.t"]);
        git(&["config", "user.name", "t"]);
        std::fs::write(repo.join("a.txt"), "x").unwrap();
        git(&["add", "a.txt"]);
        git(&["commit", "-q", "-m", "init"]);
        // Create a stray branch as if a prior arc left it behind.
        git(&["branch", "fix/issue-42"]);

        let meta = ArcMeta {
            project_dir: Some(repo.to_string_lossy().into_owned()),
            ..Default::default()
        };
        let ctx = ArcContext::new(meta);

        let wt_path = tmp.path().join("wt-arc-1");
        let hook = HookOp {
            op: OpKind::WorktreeCreate,
            args: json!({
                "path":   wt_path.to_string_lossy(),
                "branch": "fix/issue-42",
                "base":   "main",
            }),
            when: None,
            on_failure: OnFailure::Halt,
            into_var: None,
        };
        let effect = execute_op(&hook, &ctx, None).await.unwrap();
        match effect {
            OpEffect::SetWorktree(Some(p)) => {
                assert_eq!(p, wt_path.to_string_lossy());
            }
            other => panic!(
                "expected SetWorktree(Some), got {:?}",
                std::mem::discriminant(&other)
            ),
        }
        // The worktree should be on the reused branch.
        let head = std::process::Command::new("git")
            .arg("-C")
            .arg(&wt_path)
            .args(["symbolic-ref", "--short", "HEAD"])
            .output()
            .unwrap();
        let head_branch = String::from_utf8_lossy(&head.stdout).trim().to_string();
        assert_eq!(head_branch, "fix/issue-42");
    }

    #[tokio::test]
    async fn worktree_create_fails_when_branch_in_use() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        std::fs::create_dir(&repo).unwrap();
        let git = |args: &[&str]| {
            std::process::Command::new("git")
                .arg("-C")
                .arg(&repo)
                .args(args)
                .output()
                .unwrap()
        };
        git(&["init", "-q", "-b", "main"]);
        git(&["config", "user.email", "t@t.t"]);
        git(&["config", "user.name", "t"]);
        std::fs::write(repo.join("a.txt"), "x").unwrap();
        git(&["add", "a.txt"]);
        git(&["commit", "-q", "-m", "init"]);

        // Existing worktree on the contested branch.
        let occupied = tmp.path().join("wt-occupied");
        git(&[
            "worktree",
            "add",
            "-b",
            "fix/issue-42",
            occupied.to_str().unwrap(),
        ]);

        let meta = ArcMeta {
            project_dir: Some(repo.to_string_lossy().into_owned()),
            ..Default::default()
        };
        let ctx = ArcContext::new(meta);

        let wt_path = tmp.path().join("wt-conflicting");
        let hook = HookOp {
            op: OpKind::WorktreeCreate,
            args: json!({
                "path":   wt_path.to_string_lossy(),
                "branch": "fix/issue-42",
                "base":   "main",
            }),
            when: None,
            on_failure: OnFailure::Halt,
            into_var: None,
        };
        let err = execute_op(&hook, &ctx, None).await.unwrap_err();
        assert!(
            err.to_string().contains("already checked out"),
            "expected concurrent-arc error, got: {err}"
        );
    }

    #[tokio::test]
    async fn shell_failure_propagates() {
        let ctx = ArcContext::new(ArcMeta::default());
        let hook = HookOp {
            op: OpKind::Shell,
            args: json!({"argv": ["false"]}),
            when: None,
            on_failure: OnFailure::Halt,
            into_var: None,
        };
        let result = execute_op(&hook, &ctx, None).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn args_render_via_template() {
        let mut ctx = ArcContext::new(ArcMeta::default());
        ctx.vars.insert("issue".into(), json!(42));
        let hook = HookOp {
            op: OpKind::SetVar,
            args: json!({"key": "branch", "value": "fix/issue-${vars.issue}"}),
            when: None,
            on_failure: OnFailure::Halt,
            into_var: None,
        };
        let effect = execute_op(&hook, &ctx, None).await.unwrap();
        match effect {
            OpEffect::SetVar { key, value } => {
                assert_eq!(key, "branch");
                assert_eq!(value, json!("fix/issue-42"));
            }
            _ => panic!("expected SetVar"),
        }
    }

    // ── Shell with into_var captures output without failing on non-zero ────────

    #[tokio::test]
    async fn shell_into_var_captures_output_on_success() {
        let ctx = ArcContext::new(ArcMeta::default());
        let hook = HookOp {
            op: OpKind::Shell,
            args: json!({"argv": ["echo", "hello"]}),
            when: None,
            on_failure: OnFailure::Halt,
            into_var: Some("out".into()),
        };
        let effect = execute_op(&hook, &ctx, None).await.unwrap();
        match effect {
            OpEffect::SetVar { key, value } => {
                assert_eq!(key, "out");
                assert_eq!(value["exit_code"], json!(0));
                assert!(value["stdout"].as_str().unwrap().contains("hello"));
            }
            _ => panic!("expected SetVar"),
        }
    }

    #[tokio::test]
    async fn shell_into_var_captures_output_on_nonzero_exit() {
        let ctx = ArcContext::new(ArcMeta::default());
        let hook = HookOp {
            op: OpKind::Shell,
            args: json!({"argv": ["false"]}),
            when: None,
            on_failure: OnFailure::Halt,
            into_var: Some("out".into()),
        };
        // Should NOT fail even though `false` exits non-zero
        let effect = execute_op(&hook, &ctx, None).await.unwrap();
        match effect {
            OpEffect::SetVar { key, value } => {
                assert_eq!(key, "out");
                assert_ne!(value["exit_code"], json!(0));
            }
            _ => panic!("expected SetVar"),
        }
    }

    #[tokio::test]
    async fn shell_into_var_parses_json_stdout() {
        let ctx = ArcContext::new(ArcMeta::default());
        let hook = HookOp {
            op: OpKind::Shell,
            args: json!({"argv": ["echo", "{\"drift_pp\": 7.5]"]}),
            when: None,
            on_failure: OnFailure::Halt,
            into_var: Some("out".into()),
        };
        // Malformed JSON → parsed should be null, but the op still succeeds
        let effect = execute_op(&hook, &ctx, None).await.unwrap();
        assert!(matches!(effect, OpEffect::SetVar { .. }));
    }

    #[tokio::test]
    async fn shell_writes_typed_stdin_payload() {
        let mut ctx = ArcContext::new(ArcMeta::default());
        ctx.vars.insert("issue".into(), json!(42));
        let hook = HookOp {
            op: OpKind::Shell,
            args: json!({
                "argv": ["cat"],
                "stdin": {
                    "issue": "${vars.issue}",
                    "label": "phase-decompose"
                }
            }),
            when: None,
            on_failure: OnFailure::Halt,
            into_var: Some("out".into()),
        };
        let effect = execute_op(&hook, &ctx, None).await.unwrap();
        match effect {
            OpEffect::SetVar { key, value } => {
                assert_eq!(key, "out");
                assert_eq!(value["exit_code"], json!(0));
                assert_eq!(
                    value["parsed"],
                    json!({"issue": 42, "label": "phase-decompose"})
                );
            }
            _ => panic!("expected SetVar"),
        }
    }

    // ── ScoreEvalOutput ────────────────────────────────────────────────────────

    #[tokio::test]
    async fn score_eval_output_reads_drift_from_parsed() {
        let ctx = ArcContext::new(ArcMeta::default());
        let hook = HookOp {
            op: OpKind::ScoreEvalOutput,
            args: json!({
                "from": {
                    "exit_code": 0,
                    "stdout": "",
                    "stderr": "",
                    "parsed": {"drift_pp": 9.5}
                }
            }),
            when: None,
            on_failure: OnFailure::Halt,
            into_var: Some("score".into()),
        };
        let effect = execute_op(&hook, &ctx, None).await.unwrap();
        match effect {
            OpEffect::SetVar { key, value } => {
                assert_eq!(key, "score");
                assert_eq!(value["drift_pp"], json!(9.5));
                assert_eq!(value["suite_exit_code"], json!(0));
            }
            _ => panic!("expected SetVar"),
        }
    }

    #[tokio::test]
    async fn score_eval_output_falls_back_to_stdout_json() {
        let ctx = ArcContext::new(ArcMeta::default());
        let hook = HookOp {
            op: OpKind::ScoreEvalOutput,
            args: json!({
                "from": {
                    "exit_code": 0,
                    "stdout": "{\"drift_pp\": 3.2}",
                    "stderr": "",
                    "parsed": null
                }
            }),
            when: None,
            on_failure: OnFailure::Halt,
            into_var: None,
        };
        let effect = execute_op(&hook, &ctx, None).await.unwrap();
        match effect {
            OpEffect::SetVar { key, value } => {
                assert_eq!(key, "suite_score");
                let dp = value["drift_pp"].as_f64().unwrap();
                assert!((dp - 3.2).abs() < 0.001);
            }
            _ => panic!("expected SetVar"),
        }
    }

    #[tokio::test]
    async fn score_eval_output_exit_code_heuristic() {
        let ctx = ArcContext::new(ArcMeta::default());
        let hook = HookOp {
            op: OpKind::ScoreEvalOutput,
            args: json!({
                "from": {
                    "exit_code": 1,
                    "stdout": "",
                    "stderr": "",
                    "parsed": null
                }
            }),
            when: None,
            on_failure: OnFailure::Halt,
            into_var: None,
        };
        let effect = execute_op(&hook, &ctx, None).await.unwrap();
        match effect {
            OpEffect::SetVar { value, .. } => {
                let dp = value["drift_pp"].as_f64().unwrap();
                assert!(
                    (dp - 5.0).abs() < 0.001,
                    "non-zero exit should yield 5pp heuristic"
                );
            }
            _ => panic!("expected SetVar"),
        }
    }

    #[tokio::test]
    async fn score_eval_output_reads_from_ctx_vars() {
        let mut ctx = ArcContext::new(ArcMeta::default());
        ctx.vars.insert(
            "suite_output".into(),
            json!({
                "exit_code": 0,
                "stdout": "",
                "stderr": "",
                "parsed": {"drift_pp": 1.5}
            }),
        );
        let hook = HookOp {
            op: OpKind::ScoreEvalOutput,
            args: json!({}),
            when: None,
            on_failure: OnFailure::Halt,
            into_var: Some("score".into()),
        };
        let effect = execute_op(&hook, &ctx, None).await.unwrap();
        match effect {
            OpEffect::SetVar { value, .. } => {
                assert_eq!(value["drift_pp"], json!(1.5));
            }
            _ => panic!("expected SetVar"),
        }
    }

    // ── PickFirst ──────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn pick_first_from_inline_array() {
        let ctx = ArcContext::new(ArcMeta::default());
        let hook = HookOp {
            op: OpKind::PickFirst,
            args: json!({"from": [{"id": "a"}, {"id": "b"}]}),
            when: None,
            on_failure: OnFailure::Halt,
            into_var: Some("first".into()),
        };
        let effect = execute_op(&hook, &ctx, None).await.unwrap();
        match effect {
            OpEffect::SetVar { key, value } => {
                assert_eq!(key, "first");
                assert_eq!(value["id"], json!("a"));
            }
            _ => panic!("expected SetVar"),
        }
    }

    #[tokio::test]
    async fn pick_first_from_vars_array() {
        let mut ctx = ArcContext::new(ArcMeta::default());
        ctx.vars.insert("items".into(), json!([1, 2, 3]));
        let hook = HookOp {
            op: OpKind::PickFirst,
            args: json!({"array": "items"}),
            when: None,
            on_failure: OnFailure::Halt,
            into_var: Some("first".into()),
        };
        let effect = execute_op(&hook, &ctx, None).await.unwrap();
        match effect {
            OpEffect::SetVar { value, .. } => assert_eq!(value, json!(1)),
            _ => panic!("expected SetVar"),
        }
    }

    #[tokio::test]
    async fn pick_first_from_nested_vars_dotted_path() {
        let mut ctx = ArcContext::new(ArcMeta::default());
        ctx.vars.insert(
            "candidate_pairs".into(),
            json!({"candidates": [{"ref": "knowledge:abc"}, {"ref": "knowledge:def"}]}),
        );
        let hook = HookOp {
            op: OpKind::PickFirst,
            args: json!({"array": "candidate_pairs.candidates"}),
            when: None,
            on_failure: OnFailure::Halt,
            into_var: Some("candidate".into()),
        };
        let effect = execute_op(&hook, &ctx, None).await.unwrap();
        match effect {
            OpEffect::SetVar { value, .. } => assert_eq!(value["ref"], json!("knowledge:abc")),
            _ => panic!("expected SetVar"),
        }
    }

    #[tokio::test]
    async fn pick_first_empty_array_yields_null() {
        let ctx = ArcContext::new(ArcMeta::default());
        let hook = HookOp {
            op: OpKind::PickFirst,
            args: json!({"from": []}),
            when: None,
            on_failure: OnFailure::Halt,
            into_var: Some("first".into()),
        };
        let effect = execute_op(&hook, &ctx, None).await.unwrap();
        match effect {
            OpEffect::SetVar { value, .. } => assert_eq!(value, Value::Null),
            _ => panic!("expected SetVar"),
        }
    }

    // ── SchemaMigrationDrop is observable-only ─────────────────────────────────

    #[tokio::test]
    async fn schema_migration_drop_returns_none() {
        let ctx = ArcContext::new(ArcMeta::default());
        let hook = HookOp {
            op: OpKind::SchemaMigrationDrop,
            args: json!({}),
            when: None,
            on_failure: OnFailure::Halt,
            into_var: None,
        };
        let effect = execute_op(&hook, &ctx, None).await.unwrap();
        assert!(matches!(effect, OpEffect::None));
    }

    // ── require_identity ─────────────────────────────────────────────────────

    fn test_hub_for_ops() -> (crate::system_events::SharedEventHub, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let es = crate::system_events::EventStore::new_at(dir.path().join("journal"));
        let os = crate::system_events::OutboxStore::new(dir.path().join("outbox")).unwrap();
        let rd = dir.path().join("reactions");
        let id = dir.path().join("identities");
        (
            std::sync::Arc::new(crate::system_events::EventHub::new(es, os, rd, id)),
            dir,
        )
    }

    #[tokio::test]
    async fn system_event_compact_op_writes_report() {
        let (hub, _dir) = test_hub_for_ops();
        let ctx = ArcContext::new(ArcMeta::default());
        let hook = HookOp {
            op: OpKind::SystemEventCompact,
            args: json!({"now": "2026-05-14T00:00:00Z"}),
            when: None,
            on_failure: OnFailure::Halt,
            into_var: Some("compaction".into()),
        };
        let effect = execute_op_with_hub(&hook, &ctx, None, Some(&hub))
            .await
            .unwrap();
        match effect {
            OpEffect::SetVar { key, value } => {
                assert_eq!(key, "compaction");
                assert_eq!(value["now"], "2026-05-14T00:00:00Z");
                assert_eq!(value["event_journal"]["before"], 0);
                assert_eq!(value["outbox"]["before"], 0);
            }
            _ => panic!("expected SetVar"),
        }
    }

    #[tokio::test]
    async fn require_identity_pending_on_miss() {
        let (hub, _dir) = test_hub_for_ops();
        let ctx = ArcContext::new(ArcMeta::default());
        let hook = HookOp {
            op: OpKind::RequireIdentity,
            args: json!({
                "scope":    "forgejo",
                "instance": "local-forgejo15",
                "bro":      "keystone-review",
                "provider": "claude",
                "model":    "haiku-4.5"
            }),
            when: None,
            on_failure: OnFailure::Halt,
            into_var: Some("identity_result".into()),
        };
        let effect = execute_op_with_hub(&hook, &ctx, None, Some(&hub))
            .await
            .unwrap();
        match effect {
            OpEffect::SetVar { key, value } => {
                assert_eq!(key, "identity_result");
                assert_eq!(value["status"], "pending");
                assert!(value["identity"].is_null());
            }
            _ => panic!("expected SetVar"),
        }
        let events = hub
            .list_events(None, Some("bro.identity.required"), None, None)
            .unwrap();
        assert_eq!(events.len(), 1, "should have emitted bro.identity.required");
    }

    #[tokio::test]
    async fn require_identity_ready_after_upsert() {
        let (hub, _dir) = test_hub_for_ops();
        let identity = crate::system_events::identity::ExternalIdentity {
            scope: "forgejo".to_string(),
            instance: "local-forgejo15".to_string(),
            subject: "bro:keystone-review".to_string(),
            provider: "claude".to_string(),
            model: "haiku-4.5".to_string(),
            external_user_id: "42".to_string(),
            username: "bro-keystone-review-claude-haiku45".to_string(),
            token_ref: "secret:forgejo-bro-keystone-review".to_string(),
            created_at: crate::util::now_iso(),
            last_verified_at: None,
        };
        hub.identity_registry().upsert(&identity).unwrap();

        let ctx = ArcContext::new(ArcMeta::default());
        let hook = HookOp {
            op: OpKind::RequireIdentity,
            args: json!({
                "scope":    "forgejo",
                "instance": "local-forgejo15",
                "bro":      "keystone-review",
                "provider": "claude",
                "model":    "haiku-4.5"
            }),
            when: None,
            on_failure: OnFailure::Halt,
            into_var: Some("identity_result".into()),
        };
        let effect = execute_op_with_hub(&hook, &ctx, None, Some(&hub))
            .await
            .unwrap();
        match effect {
            OpEffect::SetVar { key, value } => {
                assert_eq!(key, "identity_result");
                assert_eq!(value["status"], "ready");
                assert_eq!(
                    value["identity"]["token_ref"],
                    "secret:forgejo-bro-keystone-review"
                );
            }
            _ => panic!("expected SetVar"),
        }
        let events = hub
            .list_events(None, Some("bro.identity.required"), None, None)
            .unwrap();
        assert!(
            events.is_empty(),
            "no event expected when identity is ready"
        );
    }

    #[tokio::test]
    async fn require_identity_without_hub_errors() {
        let ctx = ArcContext::new(ArcMeta::default());
        let hook = HookOp {
            op: OpKind::RequireIdentity,
            args: json!({
                "scope": "forgejo", "instance": "x", "bro": "y",
                "provider": "claude", "model": "haiku-4.5"
            }),
            when: None,
            on_failure: OnFailure::Halt,
            into_var: None,
        };
        let err = execute_op(&hook, &ctx, None).await.unwrap_err();
        assert!(
            err.to_string().contains("EventHub"),
            "expected EventHub error, got: {err}"
        );
    }

    // ── secret_headers / resolve_secret_header_value ─────────────────────────

    #[test]
    fn resolve_secret_header_value_with_env_fallback() {
        // Use the env var fallback (BLACKBOX_SECRET_TEST_HTTP_TOKEN).
        // SAFETY: single-threaded test context; no other threads access this var.
        unsafe { std::env::set_var("BLACKBOX_SECRET_TEST_HTTP_TOKEN", "tok-abc123") };
        let result = resolve_secret_header_value("token secret:test-http-token").unwrap();
        assert_eq!(result, "token tok-abc123");
        // SAFETY: same as above.
        unsafe { std::env::remove_var("BLACKBOX_SECRET_TEST_HTTP_TOKEN") };
    }

    #[test]
    fn resolve_secret_header_value_rejects_no_secret_ref() {
        let err = resolve_secret_header_value("static-plain-value").unwrap_err();
        assert!(
            err.to_string().contains("secret:"),
            "expected rejection, got: {err}"
        );
    }

    #[test]
    fn resolve_secret_header_value_missing_secret_errors_without_value() {
        let err = resolve_secret_header_value("Bearer secret:definitely-not-set-xyz").unwrap_err();
        let msg = err.to_string();
        // Must not contain any resolved value (there is none), but must name the key.
        assert!(
            msg.contains("definitely-not-set-xyz"),
            "expected secret name in error: {msg}"
        );
        assert!(
            !msg.contains("Bearer"),
            "error must not expose header prefix: {msg}"
        );
    }

    #[test]
    fn resolve_secret_header_value_bare_secret_ref() {
        // SAFETY: single-threaded test context; no other threads access this var.
        unsafe { std::env::set_var("BLACKBOX_SECRET_BARE_TOKEN", "raw-value") };
        let result = resolve_secret_header_value("secret:bare-token").unwrap();
        assert_eq!(result, "raw-value");
        // SAFETY: same as above.
        unsafe { std::env::remove_var("BLACKBOX_SECRET_BARE_TOKEN") };
    }
}
