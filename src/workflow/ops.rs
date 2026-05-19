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
mod external;
mod json_ops;
mod system_events;
mod vars;
mod vector;
mod worktree;

use anyhow::{Result, anyhow, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::context::{ArcContext, VarsSchema, resolve_arg_value};
use arch_pathology::{exec_normalize_arch_pathology_atom_requests, exec_write_arch_pathology_plan};
use auto_digest::{
    exec_aggregate_auto_edge_votes, exec_append_knowledge_link, exec_apply_entry,
    exec_extract_candidate_pairs, exec_log_reject, exec_read_session, exec_surface_to_inbox,
    exec_validate_schema, exec_write_semantic_edge,
};
use external::{exec_http_json, exec_mcp_call, exec_shell};
use json_ops::exec_parse_json;
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

// ── Built-in op implementations ──────────────────────────────────

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
}
