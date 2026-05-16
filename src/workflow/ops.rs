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

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;

use anyhow::{Result, anyhow, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use tokio::io::AsyncWriteExt;

use crate::chunker::{EdgeConfidence, EdgeProvenance};
use crate::entity_ref::EntityRef;

use super::context::{ArcContext, VarsSchema, resolve_arg_value};

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
    Shell,
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
        OpKind::Shell => exec_shell(&rendered_args, hook.into_var.as_deref(), ctx).await,
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

fn exec_system_event_compact(
    args: &Value,
    into_var: Option<&str>,
    hub: &crate::system_events::SharedEventHub,
) -> Result<OpEffect> {
    let now = args
        .get("now")
        .and_then(Value::as_str)
        .map(ToOwned::to_owned)
        .unwrap_or_else(crate::util::now_iso);
    let report = hub.compact_with_now(&now)?;
    Ok(OpEffect::SetVar {
        key: into_var
            .unwrap_or("system_event_compaction_result")
            .to_string(),
        value: serde_json::to_value(report)?,
    })
}

async fn exec_read_session(
    args: &Value,
    into_var: Option<&str>,
    ctx: &ArcContext,
) -> Result<OpEffect> {
    let session_id = args
        .get("session_id")
        .and_then(|value| value.as_str())
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| anyhow!("ReadSession requires args.session_id"))?;
    let limit = args
        .get("limit")
        .and_then(|value| value.as_u64())
        .unwrap_or(200);
    let result = call_blackbox_tool(
        "bbox_messages",
        json!({
            "session_id": session_id,
            "limit": limit,
            "max_content_length": 12000u64,
        }),
        ctx,
    )
    .await?;
    Ok(OpEffect::SetVar {
        key: into_var.unwrap_or("session").to_string(),
        value: result,
    })
}

fn exec_validate_schema(args: &Value, into_var: Option<&str>) -> Result<OpEffect> {
    let input = args
        .get("from")
        .ok_or_else(|| anyhow!("ValidateSchema requires args.from"))?;
    let candidate = first_candidate(input)
        .ok_or_else(|| anyhow!("ValidateSchema requires a candidate object or candidates array"))?;
    let required = [
        "title",
        "content",
        "category",
        "scope",
        "source_session",
        "source_query",
        "justification",
        "suggested_approval",
    ];
    for field in required {
        if candidate.get(field).and_then(Value::as_str).is_none() {
            bail!("ValidateSchema candidate missing string field '{field}'");
        }
    }
    let source_files = candidate
        .get("source_files")
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow!("ValidateSchema candidate missing source_files array"))?;
    if source_files.iter().any(|value| value.as_str().is_none()) {
        bail!("ValidateSchema source_files must contain only strings");
    }
    Ok(OpEffect::SetVar {
        key: into_var.unwrap_or("candidate").to_string(),
        value: Value::Object(candidate.clone()),
    })
}

async fn exec_apply_entry(
    args: &Value,
    into_var: Option<&str>,
    ctx: &ArcContext,
) -> Result<OpEffect> {
    let candidate = first_candidate(
        args.get("from")
            .ok_or_else(|| anyhow!("ApplyEntry requires args.from"))?,
    )
    .ok_or_else(|| anyhow!("ApplyEntry requires a candidate object or candidates array"))?;
    let category = candidate
        .get("category")
        .and_then(Value::as_str)
        .unwrap_or("memory");
    let title = candidate.get("title").and_then(Value::as_str);
    let content = candidate
        .get("content")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("ApplyEntry candidate missing content"))?;
    let scope = candidate
        .get("scope")
        .and_then(Value::as_str)
        .unwrap_or("project");
    let project = candidate
        .get("project")
        .and_then(Value::as_str)
        .map(ToOwned::to_owned)
        .or_else(|| ctx.meta.project_dir.clone());
    let mut arguments = Map::new();
    arguments.insert("content".into(), Value::String(content.to_string()));
    if let Some(title) = title {
        arguments.insert("title".into(), Value::String(title.to_string()));
    }
    arguments.insert("scope".into(), Value::String(scope.to_string()));
    if let Some(project) = project {
        arguments.insert("project".into(), Value::String(project));
    }
    let tool = match category {
        "decision" => {
            arguments.insert(
                "rationale".into(),
                Value::String(
                    candidate
                        .get("justification")
                        .and_then(Value::as_str)
                        .unwrap_or("auto-digest candidate")
                        .to_string(),
                ),
            );
            arguments.insert("render".into(), Value::Bool(false));
            "bbox_decide"
        }
        "convention" | "workflow" => {
            arguments.insert("category".into(), Value::String(category.to_string()));
            "bbox_learn"
        }
        _ => {
            arguments.insert("category".into(), Value::String(category.to_string()));
            "bbox_remember"
        }
    };
    let result = call_blackbox_tool(tool, Value::Object(arguments), ctx).await?;
    Ok(OpEffect::SetVar {
        key: into_var.unwrap_or("applied_entry").to_string(),
        value: result,
    })
}

async fn exec_append_knowledge_link(
    args: &Value,
    into_var: Option<&str>,
    ctx: &ArcContext,
) -> Result<OpEffect> {
    let mut arguments = Map::new();
    for key in ["source", "target", "kind"] {
        let value = args
            .get(key)
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow!("AppendKnowledgeLink requires args.{key}"))?;
        arguments.insert(key.into(), Value::String(value.to_string()));
    }
    for key in ["note", "source_arc", "confidence"] {
        if let Some(value) = args.get(key).and_then(Value::as_str) {
            arguments.insert(key.into(), Value::String(value.to_string()));
        }
    }
    let result = call_blackbox_tool("bbox_knowledge_link", Value::Object(arguments), ctx).await?;
    Ok(OpEffect::SetVar {
        key: into_var.unwrap_or("knowledge_link").to_string(),
        value: result,
    })
}

/// Scan the knowledge corpus for entity pairs that may warrant a semantic
/// edge (DESCRIBES or REFERENCES) but have no such edge yet.
///
/// Strategy:
/// 1. Two staggered `bbox_hybrid_search` queries seed a pool of topically
///    relevant entities.
/// 2. Consecutive pairs from the merged, deduplicated pool become candidates,
///    capped at `limit`.
/// 3. Each candidate carries entity refs, labels, and excerpts so the
///    downstream ClassifyVote actor can vote without extra lookups.
async fn exec_extract_candidate_pairs(
    args: &Value,
    into_var: Option<&str>,
    ctx: &ArcContext,
) -> Result<OpEffect> {
    let limit = args.get("limit").and_then(Value::as_u64).unwrap_or(50) as usize;
    let edge_kinds: Vec<String> = args
        .get("edge_kinds")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_else(|| vec!["DESCRIBES".to_string(), "REFERENCES".to_string()]);
    let edge_kind = edge_kinds
        .first()
        .map(|s| s.as_str())
        .unwrap_or("REFERENCES");

    // Two complementary queries to maximise variety in the candidate pool.
    let queries = ["knowledge convention decision", "learned assumption rule"];
    let mut pool: Vec<Value> = Vec::new();
    for query in &queries {
        let result = call_blackbox_tool(
            "bbox_hybrid_search",
            json!({ "query": query, "limit": limit }),
            ctx,
        )
        .await
        .unwrap_or(Value::Null);
        if let Some(hits) = result.get("hits").and_then(Value::as_array) {
            pool.extend(hits.iter().cloned());
        }
    }

    // Deduplicate by entity_ref before pairing.
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    pool.retain(|hit| {
        let r = hit
            .get("entity_ref")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        seen.insert(r)
    });

    // Build consecutive pairs up to `limit`.
    let candidates: Vec<Value> = pool
        .chunks(2)
        .take(limit)
        .filter_map(|chunk| {
            let src = chunk.first()?;
            let tgt = chunk.get(1)?;
            let src_ref = src.get("entity_ref").and_then(Value::as_str)?;
            let tgt_ref = tgt.get("entity_ref").and_then(Value::as_str)?;
            if src_ref == tgt_ref {
                return None;
            }
            Some(json!({
                "source": src_ref,
                "target": tgt_ref,
                "edge_kind": edge_kind,
                "source_label": src.get("label").and_then(Value::as_str).unwrap_or(src_ref),
                "target_label": tgt.get("label").and_then(Value::as_str).unwrap_or(tgt_ref),
                "source_excerpt": src.get("excerpt").and_then(Value::as_str).unwrap_or(""),
                "target_excerpt": tgt.get("excerpt").and_then(Value::as_str).unwrap_or(""),
            }))
        })
        .collect();

    Ok(OpEffect::SetVar {
        key: into_var.unwrap_or("candidate_pairs").to_string(),
        value: json!({
            "limit": limit,
            "scanned": pool.len(),
            "candidates": candidates,
        }),
    })
}

fn exec_aggregate_auto_edge_votes(args: &Value, into_var: Option<&str>) -> Result<OpEffect> {
    let votes = args
        .get("votes")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let mut gate = Map::new();
    for idx in 0..3 {
        let vote = votes
            .get(idx)
            .and_then(|value| value.get("vote"))
            .and_then(Value::as_str)
            .unwrap_or("no");
        gate.insert(format!("vote{}", idx + 1), Value::String(vote.to_string()));
    }
    gate.insert("votes".into(), Value::Array(votes));
    Ok(OpEffect::SetVar {
        key: into_var.unwrap_or("vote_aggregate").to_string(),
        value: Value::Object(gate),
    })
}

async fn exec_write_semantic_edge(
    args: &Value,
    into_var: Option<&str>,
    ctx: &ArcContext,
) -> Result<OpEffect> {
    let source = required_str(args, "source")?;
    let target = required_str(args, "target")?;
    let kind = required_str(args, "kind")?;
    let note = args.get("note").and_then(Value::as_str).unwrap_or("");
    let source_ref = EntityRef::parse(source)?;
    let target_ref = EntityRef::parse(target)?;
    match kind {
        "REFERENCES" => {
            let result = call_blackbox_tool(
                "bbox_knowledge_link",
                json!({
                    "source": source,
                    "target": target,
                    "kind": "REFERENCES",
                    "note": note,
                    "source_arc": ctx.meta.arc_id,
                    "confidence": "heuristic"
                }),
                ctx,
            )
            .await?;
            Ok(OpEffect::SetVar {
                key: into_var.unwrap_or("semantic_edge").to_string(),
                value: result,
            })
        }
        "DESCRIBES" => {
            let project_id = args
                .get("project_id")
                .and_then(Value::as_str)
                .map(str::to_string)
                .or_else(|| project_id_from_ref(&source_ref))
                .or_else(|| project_id_from_ref(&target_ref))
                .ok_or_else(|| {
                    anyhow!("WriteSemanticEdge DESCRIBES requires a project_id-bearing ref")
                })?;
            let mut metadata = BTreeMap::new();
            metadata.insert("source_arc".to_string(), ctx.meta.arc_id.clone());
            if !note.is_empty() {
                metadata.insert("note".to_string(), note.to_string());
            }
            let edge = crate::edge_index::Edge {
                source: source_ref,
                kind: "DESCRIBES".to_string(),
                target: target_ref,
                provenance: EdgeProvenance::Explicit,
                confidence: EdgeConfidence::Heuristic,
                metadata,
            };
            let edges_dir = args
                .get("edges_dir")
                .and_then(Value::as_str)
                .map(PathBuf::from)
                .unwrap_or_else(default_edges_dir);
            let written =
                crate::edge_index::append_explicit_edges(&edges_dir, &project_id, &[edge])?;
            Ok(OpEffect::SetVar {
                key: into_var.unwrap_or("semantic_edge").to_string(),
                value: json!({
                    "status": "ok",
                    "kind": "DESCRIBES",
                    "project_id": project_id,
                    "written": written
                }),
            })
        }
        other => bail!("WriteSemanticEdge unsupported kind `{other}`"),
    }
}

fn required_str<'a>(args: &'a Value, key: &str) -> Result<&'a str> {
    args.get(key)
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| anyhow!("WriteSemanticEdge requires args.{key}"))
}

fn project_id_from_ref(r: &EntityRef) -> Option<String> {
    match r {
        EntityRef::ProjectFile { project_id, .. }
        | EntityRef::ProjectFileV2 { project_id, .. }
        | EntityRef::Symbol { project_id, .. }
        | EntityRef::SymbolV2 { project_id, .. } => Some(project_id.clone()),
        _ => None,
    }
}

fn default_edges_dir() -> PathBuf {
    dirs::home_dir()
        .map(|home| crate::util::blackbox_state_dir(&home).join("edges"))
        .unwrap_or_else(|| PathBuf::from("edges"))
}

async fn exec_surface_to_inbox(args: &Value, ctx: &ArcContext) -> Result<OpEffect> {
    let candidate = first_candidate(
        args.get("from")
            .ok_or_else(|| anyhow!("SurfaceToInbox requires args.from"))?,
    )
    .ok_or_else(|| anyhow!("SurfaceToInbox requires a candidate object or candidates array"))?;
    let title = candidate
        .get("title")
        .and_then(Value::as_str)
        .unwrap_or("(untitled)");
    call_blackbox_tool(
        "bbox_note",
        json!({
            "kind": "followup",
            "project": ctx.meta.project_dir,
            "body": format!("Auto-digest candidate held for review: {title}")
        }),
        ctx,
    )
    .await?;
    Ok(OpEffect::None)
}

async fn exec_log_reject(args: &Value, ctx: &ArcContext) -> Result<OpEffect> {
    let candidate = first_candidate(
        args.get("from")
            .ok_or_else(|| anyhow!("LogReject requires args.from"))?,
    )
    .ok_or_else(|| anyhow!("LogReject requires a candidate object or candidates array"))?;
    let title = candidate
        .get("title")
        .and_then(Value::as_str)
        .unwrap_or("(untitled)");
    call_blackbox_tool(
        "bbox_note",
        json!({
            "kind": "learned",
            "project": ctx.meta.project_dir,
            "body": format!("Auto-digest candidate rejected by entry-quality gate: {title}")
        }),
        ctx,
    )
    .await?;
    Ok(OpEffect::None)
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

fn first_candidate(value: &Value) -> Option<&Map<String, Value>> {
    if let Some(obj) = value.as_object() {
        if let Some(candidate) = obj.get("candidate").and_then(Value::as_object) {
            return Some(candidate);
        }
        if let Some(candidate) = obj
            .get("candidates")
            .and_then(Value::as_array)
            .and_then(|items| items.first())
            .and_then(Value::as_object)
        {
            return Some(candidate);
        }
        return Some(obj);
    }
    value
        .as_array()
        .and_then(|items| items.first())
        .and_then(Value::as_object)
}

fn exec_read_vector_status(args: &Value, into_var: Option<&str>) -> Result<OpEffect> {
    let into = into_var.unwrap_or("vector_status");
    let route_filter = args.get("route").and_then(|value| value.as_str());
    let mut partitions = crate::vectors::metrics();
    if let Some(route) = route_filter {
        partitions.retain(|name, _| name == route);
    }
    let mut max_deleted_route = None::<String>;
    let mut max_deleted_ratio = 0.0f32;
    for (route, metrics) in &partitions {
        if metrics.deleted_ratio >= max_deleted_ratio {
            max_deleted_ratio = metrics.deleted_ratio;
            max_deleted_route = Some(route.clone());
        }
    }
    Ok(OpEffect::SetVar {
        key: into.to_string(),
        value: json!({
            "partitions": partitions,
            "max_deleted_route": max_deleted_route,
            "max_deleted_ratio": max_deleted_ratio,
        }),
    })
}

fn exec_rebuild_hnsw(args: &Value) -> Result<OpEffect> {
    let route = args
        .get("route")
        .and_then(|value| value.as_str())
        .filter(|route| !route.trim().is_empty())
        .ok_or_else(|| anyhow!("RebuildHnsw requires args.route"))?;
    crate::vectors::rebuild(route)?;
    Ok(OpEffect::None)
}

fn exec_compact_vector_partitions(args: &Value, into_var: Option<&str>) -> Result<OpEffect> {
    let max_partitions = args
        .get("max_partitions")
        .and_then(Value::as_u64)
        .map(|value| value as usize);
    let deleted_ratio_threshold = args
        .get("deleted_ratio_threshold")
        .and_then(Value::as_f64)
        .unwrap_or(0.30) as f32;
    let mut candidates = crate::vectors::metrics()
        .into_iter()
        .filter(|(_, metrics)| metrics.deleted_ratio >= deleted_ratio_threshold)
        .collect::<Vec<_>>();
    candidates.sort_by(|a, b| {
        b.1.deleted_ratio
            .total_cmp(&a.1.deleted_ratio)
            .then_with(|| a.0.cmp(&b.0))
    });
    let limit = max_partitions.unwrap_or(usize::MAX);
    let mut stats = Vec::new();
    for (route, before) in candidates.into_iter().take(limit) {
        let started = std::time::Instant::now();
        crate::vectors::rebuild(&route)?;
        let after = crate::vectors::metrics()
            .remove(&route)
            .unwrap_or(before.clone());
        stats.push(json!({
            "route": route,
            "before_wal_records": before.wal_records,
            "after_wal_records": after.wal_records,
            "before_slab_entries": before.active_count + before.deleted_count,
            "after_slab_entries": after.active_count + after.deleted_count,
            "elapsed_ms": started.elapsed().as_millis(),
        }));
    }
    let value = json!({
        "count": stats.len(),
        "routes": stats
            .iter()
            .filter_map(|stat| stat.get("route").and_then(Value::as_str))
            .collect::<Vec<_>>(),
        "stats": stats,
    });
    if let Some(key) = into_var {
        return Ok(OpEffect::SetVar {
            key: key.to_string(),
            value,
        });
    }
    Ok(OpEffect::None)
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

async fn exec_require_identity(
    args: &Value,
    into_var: Option<&str>,
    hub: &crate::system_events::SharedEventHub,
) -> Result<OpEffect> {
    let scope = args
        .get("scope")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("require_identity requires args.scope"))?;
    let instance = args
        .get("instance")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("require_identity requires args.instance"))?;
    let bro = args
        .get("bro")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("require_identity requires args.bro"))?;
    let provider = args
        .get("provider")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("require_identity requires args.provider"))?;
    let model = args
        .get("model")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("require_identity requires args.model"))?;
    let effort = args
        .get("effort")
        .and_then(|v| v.as_str())
        .map(str::to_string);
    let project = args
        .get("project")
        .and_then(|v| v.as_str())
        .map(str::to_string);
    let owner = args
        .get("owner")
        .and_then(|v| v.as_str())
        .map(str::to_string);
    let repo = args
        .get("repo")
        .and_then(|v| v.as_str())
        .map(str::to_string);

    let req = crate::system_events::identity::IdentityRequest {
        scope: scope.to_string(),
        instance: instance.to_string(),
        bro: bro.to_string(),
        provider: provider.to_string(),
        model: model.to_string(),
        effort,
        project,
        owner,
        repo,
    };

    let into = into_var.unwrap_or("identity_result");
    match hub.require_identity(&req).await? {
        Some(identity) => Ok(OpEffect::SetVar {
            key: into.to_string(),
            value: json!({
                "status": "ready",
                "identity": serde_json::to_value(&identity)?
            }),
        }),
        None => Ok(OpEffect::SetVar {
            key: into.to_string(),
            value: json!({ "status": "pending", "identity": null }),
        }),
    }
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

fn exec_parse_json(args: &Value, into_var: Option<&str>) -> Result<OpEffect> {
    let into = into_var.ok_or_else(|| anyhow!("ParseJson requires into_var on the HookOp spec"))?;
    let from = args
        .get("from")
        .ok_or_else(|| anyhow!("ParseJson requires args.from (string or value)"))?;
    let repair_missing_closing_delimiters = args
        .get("repair_missing_closing_delimiters")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let parsed = match from {
        Value::String(s) => {
            let trimmed = s.trim();
            if trimmed.is_empty() {
                Value::Null
            } else {
                // Try fenced ```json blocks first — common LLM output shape.
                let stripped = strip_code_fence(trimmed).unwrap_or(trimmed.to_string());
                match serde_json::from_str(&stripped) {
                    Ok(value) => value,
                    Err(first_err) => {
                        let mut last_err = first_err.to_string();
                        if repair_missing_closing_delimiters {
                            if let Some(repaired) = append_missing_json_closers(&stripped) {
                                match serde_json::from_str(&repaired) {
                                    Ok(value) => {
                                        return Ok(OpEffect::SetVar {
                                            key: into.to_string(),
                                            value,
                                        });
                                    }
                                    Err(err) => last_err = err.to_string(),
                                }
                            }
                        }
                        let candidates =
                            crate::tools::bro_helpers::extract_json_candidates(trimmed);
                        let mut parsed = None;
                        for candidate in candidates {
                            match serde_json::from_str(&candidate) {
                                Ok(value) => {
                                    parsed = Some(value);
                                    break;
                                }
                                Err(err) => {
                                    last_err = err.to_string();
                                    if repair_missing_closing_delimiters {
                                        if let Some(repaired) =
                                            append_missing_json_closers(&candidate)
                                        {
                                            match serde_json::from_str(&repaired) {
                                                Ok(value) => {
                                                    parsed = Some(value);
                                                    break;
                                                }
                                                Err(repair_err) => {
                                                    last_err = repair_err.to_string();
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                        parsed.ok_or_else(|| {
                            anyhow!("ParseJson: input did not parse as JSON: {last_err}")
                        })?
                    }
                }
            }
        }
        // Already structured — pass through.
        other => other.clone(),
    };
    Ok(OpEffect::SetVar {
        key: into.to_string(),
        value: parsed,
    })
}

fn append_missing_json_closers(s: &str) -> Option<String> {
    let mut stack = Vec::new();
    let mut in_string = false;
    let mut escaped = false;

    for ch in s.chars() {
        if in_string {
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == '"' {
                in_string = false;
            }
            continue;
        }

        match ch {
            '"' => in_string = true,
            '{' => stack.push('}'),
            '[' => stack.push(']'),
            '}' | ']' => {
                if stack.pop() != Some(ch) {
                    return None;
                }
            }
            _ => {}
        }
    }

    if in_string || stack.is_empty() {
        return None;
    }

    let mut repaired = s.trim_end().to_string();
    for ch in stack.into_iter().rev() {
        repaired.push(ch);
    }
    Some(repaired)
}

fn strip_code_fence(s: &str) -> Option<String> {
    let lines: Vec<&str> = s.lines().collect();
    if lines.is_empty() {
        return None;
    }
    // Fast path: first line is the fence opener.
    let first = lines[0].trim();
    let opens_fence = first == "```json" || first == "```JSON" || first == "```";
    if opens_fence {
        let last_idx = lines.iter().rposition(|l| l.trim() == "```")?;
        if last_idx == 0 {
            return None;
        }
        return Some(lines[1..last_idx].join("\n"));
    }
    // Fallback: prose preamble before a fenced JSON block. Common LLM
    // output shape — extract the contents of the first ```json (or
    // bare ```) block found in the body. The first matching closing
    // fence after the opener delimits the block.
    let opener_idx = lines.iter().position(|l| {
        let t = l.trim();
        t == "```json" || t == "```JSON" || t == "```"
    })?;
    let closer_idx = lines[opener_idx + 1..]
        .iter()
        .position(|l| l.trim() == "```")?
        + opener_idx
        + 1;
    if closer_idx <= opener_idx + 1 {
        return None;
    }
    Some(lines[opener_idx + 1..closer_idx].join("\n"))
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

async fn exec_worktree_create(args: &Value, ctx: &ArcContext) -> Result<OpEffect> {
    let path = args
        .get("path")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("WorktreeCreate requires args.path"))?
        .to_string();
    let branch = args
        .get("branch")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("WorktreeCreate requires args.branch"))?
        .to_string();
    let base = args
        .get("base")
        .and_then(|v| v.as_str())
        .unwrap_or("main")
        .to_string();
    let repo_root = ctx
        .meta
        .project_dir
        .clone()
        .ok_or_else(|| anyhow!("WorktreeCreate: meta.project_dir not set"))?;

    // Refuse to clobber an existing path. Different issue from the
    // existing-branch case below — a path collision means another arc
    // is concurrently using this exact path or a previous one wasn't
    // cleaned up. Either way, don't silently mutate.
    if Path::new(&path).exists() {
        bail!("WorktreeCreate: path {path} already exists");
    }

    // Branch existence check. Failed/killed arcs leave their branch
    // behind (intentional under `keep-on-fail` cleanup policy), so
    // creating a fresh branch with the same name (`-b`) on the next
    // attempt blows up with `fatal: a branch named '<x>' already
    // exists`. Two real cases:
    //   (a) branch exists AND is checked out in another worktree → real
    //       conflict (concurrent arc on same issue), fail loudly
    //   (b) branch exists but no worktree references it → reuse it
    //       (`git worktree add <path> <branch>` without `-b`)
    //   (c) branch doesn't exist → fresh create (`-b <branch>` against
    //       <base>)
    let branch_status = branch_state(&repo_root, &branch).await?;
    let mut cmd = tokio::process::Command::new("git");
    cmd.arg("-C").arg(&repo_root).arg("worktree").arg("add");
    match branch_status {
        BranchState::AbsentLocal => {
            // Case (c): create fresh.
            cmd.arg("-b").arg(&branch).arg(&path).arg(&base);
        }
        BranchState::PresentFree => {
            // Case (b): reuse existing branch — no `-b`.
            cmd.arg(&path).arg(&branch);
        }
        BranchState::PresentInUse(other_path) => {
            // Case (a): real conflict.
            bail!(
                "WorktreeCreate: branch '{branch}' is already checked out at {other_path} \
                 (concurrent arc on same issue?). Either let the other arc finish, or \
                 retire its worktree, or use a unique branch name (e.g. include \
                 ${{meta.arc_id}} in the name)."
            );
        }
    }
    let output = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await
        .map_err(|e| anyhow!("git worktree add spawn: {e}"))?;
    if !output.status.success() {
        bail!(
            "git worktree add failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    Ok(OpEffect::SetWorktree(Some(path)))
}

/// State of a local branch with respect to worktree usage.
enum BranchState {
    /// Branch does not exist locally.
    AbsentLocal,
    /// Branch exists but is not checked out by any worktree.
    PresentFree,
    /// Branch exists AND another worktree has it checked out (path).
    PresentInUse(String),
}

/// Inspect a branch's worktree-occupancy in `repo_root`.
async fn branch_state(repo_root: &str, branch: &str) -> Result<BranchState> {
    // First: does the branch ref exist at all?
    let exists = tokio::process::Command::new("git")
        .arg("-C")
        .arg(repo_root)
        .arg("show-ref")
        .arg("--verify")
        .arg("--quiet")
        .arg(format!("refs/heads/{branch}"))
        .status()
        .await
        .map_err(|e| anyhow!("git show-ref spawn: {e}"))?
        .success();
    if !exists {
        return Ok(BranchState::AbsentLocal);
    }
    // Second: is some worktree currently checked out on it?
    let porcelain = tokio::process::Command::new("git")
        .arg("-C")
        .arg(repo_root)
        .arg("worktree")
        .arg("list")
        .arg("--porcelain")
        .output()
        .await
        .map_err(|e| anyhow!("git worktree list spawn: {e}"))?;
    if !porcelain.status.success() {
        bail!(
            "git worktree list failed: {}",
            String::from_utf8_lossy(&porcelain.stderr)
        );
    }
    let text = String::from_utf8_lossy(&porcelain.stdout).into_owned();
    // Records are separated by blank lines; each starts with `worktree
    // <path>` and may include `branch refs/heads/<name>`.
    let needle = format!("branch refs/heads/{branch}");
    let mut current_path: Option<String> = None;
    for line in text.lines() {
        if let Some(p) = line.strip_prefix("worktree ") {
            current_path = Some(p.to_string());
            continue;
        }
        if line.trim() == needle {
            if let Some(p) = current_path.take() {
                return Ok(BranchState::PresentInUse(p));
            }
        }
        if line.trim().is_empty() {
            current_path = None;
        }
    }
    Ok(BranchState::PresentFree)
}

async fn exec_worktree_remove(args: &Value, ctx: &ArcContext) -> Result<OpEffect> {
    let path = args
        .get("path")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("WorktreeRemove requires args.path"))?
        .to_string();
    let force = args.get("force").and_then(|v| v.as_bool()).unwrap_or(false);

    if !Path::new(&path).exists() {
        // Treat as no-op — repeated cleanups must be idempotent.
        return Ok(OpEffect::SetWorktree(None));
    }

    // `git worktree remove` is a porcelain that needs to run inside
    // the *parent* repo (the one that owns the worktree), not inside
    // the worktree itself. Default to meta.project_dir; allow the
    // workflow to override via args.repo_root if it knows better.
    let repo_root = args
        .get("repo_root")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .or_else(|| ctx.meta.project_dir.clone())
        .ok_or_else(|| {
            anyhow!(
                "WorktreeRemove: no repo_root resolvable (set args.repo_root or meta.project_dir)"
            )
        })?;

    let mut cmd = tokio::process::Command::new("git");
    cmd.arg("-C")
        .arg(&repo_root)
        .arg("worktree")
        .arg("remove")
        .arg(&path);
    if force {
        cmd.arg("--force");
    }
    let output = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await
        .map_err(|e| anyhow!("git worktree remove spawn: {e}"))?;
    if !output.status.success() {
        // No `rm -rf` fallback — that would erase whatever path the
        // workflow author templated, with no containment guarantee.
        // Operator can clean a non-git-tracked path manually; engine
        // surfaces the git error verbatim so the cause is visible.
        bail!(
            "git worktree remove failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    Ok(OpEffect::SetWorktree(None))
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
        .unwrap_or_else(|| {
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

        let mut meta = ArcMeta::default();
        meta.project_dir = Some(repo.to_string_lossy().into_owned());
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

        let mut meta = ArcMeta::default();
        meta.project_dir = Some(repo.to_string_lossy().into_owned());
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
