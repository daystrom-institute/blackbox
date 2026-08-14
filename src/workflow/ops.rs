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

mod arch_pathology;
mod auto_digest;
mod eval_score;
mod external;
mod json_ops;
mod pathology_common;
mod perf_pathology;
mod schema_migration;
mod system_events;
#[cfg(test)]
mod tests;
mod vars;
mod vector;
mod worktree;

use anyhow::{Result, anyhow, bail};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::context::{ArcContext, VarsSchema, resolve_arg_value};
use arch_pathology::{exec_normalize_arch_pathology_atom_requests, exec_write_arch_pathology_plan};
use auto_digest::{
    exec_aggregate_auto_edge_votes, exec_append_knowledge_link, exec_apply_entry,
    exec_extract_candidate_pairs, exec_log_reject, exec_read_session, exec_surface_to_inbox,
    exec_validate_schema, exec_write_semantic_edge,
};
use eval_score::exec_score_eval_output;
use external::{exec_http_json, exec_mcp_call, exec_shell, parse_shell_allowlist_arg};
use json_ops::exec_parse_json;
use perf_pathology::{exec_normalize_perf_pathology_atom_requests, exec_write_perf_pathology_plan};
use schema_migration::{exec_schema_migration_drop, exec_schema_migration_rebuild};
use system_events::{exec_require_identity, exec_system_event_compact};
use vars::{
    exec_append_var, exec_default_var, exec_find_first, exec_inc_var, exec_merge_var,
    exec_pick_first, exec_set_meta, exec_set_var,
};
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
    /// Perf-pathology sibling of `NormalizeArchPathologyAtomRequests`: normalize
    /// performance-pathology survey atom requests (perf inherit-keys carry
    /// `hot_paths` / `baseline_refs`) before dispatch.
    NormalizePerfPathologyAtomRequests,
    Shell,
    /// Write an Architecture Pathology correction plan markdown file under
    /// `<project>/design/refactor/plans/<slug>.md` from reviewed plan JSON.
    WriteArchPathologyPlan,
    /// Write a Performance Pathology correction plan markdown file under
    /// `<project>/design/refactor/perf/plans/<slug>.md` from reviewed plan JSON
    /// (`kind: performance-correction-plan`, `PP-*` acceptance criteria).
    WritePerfPathologyPlan,
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
        OpKind::NormalizePerfPathologyAtomRequests => {
            exec_normalize_perf_pathology_atom_requests(&rendered_args, hook.into_var.as_deref())
        }
        OpKind::Shell => {
            // Per-op allowlist override: `args.allowlist` on the Shell
            // HookOp itself (a plain string array, or a `${vars.x}`
            // template that resolves to one). `exec_shell` enforces this
            // IN ADDITION to `ctx.meta.shell_allowlist` (the
            // workflow-level list, copied onto ArcMeta at arc-construction
            // time from `Workflow.shell_allowlist`): intersection
            // semantics, a per-op list can only narrow the workflow
            // policy, never widen it. A malformed `args.allowlist` (not
            // an array, or a non-string entry) fails the op instead of
            // silently falling back to "no per-op restriction" - see
            // `parse_shell_allowlist_arg`.
            let allowlist = parse_shell_allowlist_arg(&rendered_args)?;
            exec_shell(
                &rendered_args,
                hook.into_var.as_deref(),
                ctx,
                allowlist.as_deref(),
            )
            .await
        }
        OpKind::WriteArchPathologyPlan => {
            exec_write_arch_pathology_plan(&rendered_args, hook.into_var.as_deref())
        }
        OpKind::WritePerfPathologyPlan => {
            exec_write_perf_pathology_plan(&rendered_args, hook.into_var.as_deref())
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

// ── Built-in op implementations ──────────────────────────────────

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
