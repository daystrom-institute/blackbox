//! Workflow metadata schema. The embedded mermaid graph lives in
//! `Workflow.graph` as a string; the parser/validator lives in
//! [`super::mermaid`] and [`super`].

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

use super::context::VarsSchema;
use super::ops::HookOp;
use super::wait::WaitSpec;
use crate::orchestration::providers::Capability;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Workflow {
    pub name: String,
    pub version: u32,
    pub actors: HashMap<String, ActorSpec>,
    pub nodes: HashMap<String, NodeSpec>,
    /// Embedded mermaid state-diagram (as string). Parsed by
    /// [`super::mermaid::parse_mermaid`] during [`super::compile`].
    pub graph: String,
    /// Optional packet ID applied to the arc's own state at each node
    /// boundary. The packet's entity is the flattened ArcContext
    /// (`{vars, outputs, meta, last_signal, …}`). Its classifications
    /// are interpreted as arc-level verdicts:
    ///
    /// - `halt` — stop the arc immediately (error exit)
    /// - `escalate` — write a `blocked` note and continue
    /// - `warn` — write a `surprise` note and continue
    /// - any other value — no-op (treated as continue)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub policy_packet: Option<String>,
    /// Optional schema describing arc variables (kind + required).
    /// Hook writes are validated against this. Initial-vars seeding
    /// at arc start also validates here.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vars_schema: Option<VarsSchema>,
    /// Hooks fired when the arc terminates (success, fail, cancel,
    /// or timeout). `meta.arc_outcome` is set before these run so the
    /// `when` packet can branch on outcome.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub on_arc_exit: Vec<HookOp>,
    /// Hooks fired ONLY when the arc is cancelled (compensating
    /// actions). Run BEFORE on_arc_exit.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub on_arc_cancel: Vec<HookOp>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ActorSpec {
    pub kind: ActorKind,
    /// Brofile name for single-bro actors (executor, advisor).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub brofile: Option<String>,
    /// Team name for ensemble broadcasts.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub team: Option<String>,
    /// When true, the actor's session persists across nodes — the same
    /// session_id is reused when this actor is invoked again later in the
    /// workflow.
    #[serde(default)]
    pub durable: bool,
    /// When true, the CLI writes a rolling summary (compaction anchor) on
    /// the arc's work_item thread at every node boundary this actor
    /// participates in. Enables orchestrator swap-out without losing
    /// strategic memory.
    #[serde(default)]
    pub compaction_anchor: bool,
    /// Required provider capabilities for this actor. Validated at
    /// compile time against the actor's brofile/team → provider/model
    /// catalog. Composition validation is a hard error, not a silent
    /// downgrade.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub requires: Vec<Capability>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ActorKind {
    /// Single-bro worker (dispatched via bro_exec / bro_resume).
    Executor,
    /// Ensemble broadcast (dispatched via bro_broadcast + bro_when_all).
    Ensemble,
    /// Advisor LLM — a single bro used for judgment calls, narrow tool
    /// surface. Distinct from executor mainly by convention / lens.
    Advisor,
    /// Human operator. Invoking a user node means: pause, surface to
    /// inbox, wait for resolve.
    User,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct NodeSpec {
    /// Name of the actor to invoke (must exist in `Workflow.actors`).
    /// Optional only when `subworkflow` or `wait` is set.
    #[serde(default)]
    pub actor: String,
    /// Prompt template. Supports full ArcContext templating:
    /// `${vars.x}`, `${outputs.NodeName.field}`, `${meta.x}`,
    /// `${last_signal.payload.x}`, plus legacy `${NodeName.output}`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt: Option<String>,
    /// Packet id whose verdict gates advance/retry/halt after this node
    /// returns. Absent = no mechanical gate.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gate: Option<String>,
    /// Gate evaluation mode. `first` (default) returns the first rule
    /// whose antecedent holds. `all` evaluates every rule, aggregates
    /// findings, returns lattice-highest-priority classification as
    /// verdict.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gate_mode: Option<GateMode>,
    /// Sync (caller waits) vs. fire-and-forget.
    #[serde(default)]
    pub mode: NodeMode,
    /// Retry ceiling — every retry increments a generation counter; once
    /// exceeded, the node halts the arc.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retry: Option<RetryPolicy>,
    /// Late-inject declaration — when this node starts, augment its
    /// brief with any output now available from another node (typically
    /// a fire-and-forget ensemble review launched in a prior phase).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub late_inject: Option<LateInject>,
    /// Embedded sub-workflow. When present, this node runs the
    /// sub-workflow to completion instead of dispatching an actor.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subworkflow: Option<Box<Workflow>>,
    /// Vars to copy from parent into the subworkflow's fresh
    /// ArcContext. Only meaningful when `subworkflow` is set.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub imports: Vec<String>,
    /// Vars to promote from the subworkflow back to parent on
    /// completion. Only meaningful when `subworkflow` is set. Missing
    /// exports at sub-end are a runtime error.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub exports: Vec<String>,
    /// Optional rename/transform of imports — extractor expressions
    /// applied to the parent context, written into the sub's vars
    /// under the local name. Right-hand side is a path (e.g.
    /// `next_issues.0.number`); evaluated against parent ArcContext.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub import_renames: HashMap<String, String>,

    // ── New: hooks + Wait ──────────────────────────────────────
    /// Hooks fired BEFORE this node's actor dispatch (or before the
    /// Wait registers, or before subworkflow descent). Ops execute
    /// sequentially; failure semantics per-op.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub on_enter: Vec<HookOp>,
    /// Hooks fired AFTER this node's actor returns (or after Wait
    /// resolves, or after subworkflow returns). Run BEFORE the gate
    /// packet evaluates so on_exit can normalize output (e.g.
    /// ParseJson) into the entity the gate sees.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub on_exit: Vec<HookOp>,
    /// Wait declaration — when set, this node suspends the arc until
    /// one of `any_of` signals arrives (or timeout fires). Mutually
    /// exclusive with `actor` and `subworkflow`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wait: Option<WaitSpec>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum GateMode {
    #[default]
    First,
    All,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum NodeMode {
    #[default]
    Sync,
    FireAndForget,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RetryPolicy {
    /// Absolute ceiling on retry generations for this node. Generation 0
    /// is the first attempt; generation >= max triggers halt.
    pub max_generations: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LateInject {
    /// Node id whose output should be folded into this node's brief
    /// when it becomes available.
    pub from: String,
    /// When does the inject happen? `resume_on_return` = when the
    /// source node returns, resume this node with augmented brief at
    /// the next turn boundary. (Only policy defined in v1 — more can
    /// be added without breaking the schema.)
    pub policy: InjectPolicy,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum InjectPolicy {
    /// When the source node returns, resume this node with augmented
    /// brief at the next turn boundary. The source's output is appended
    /// to this node's next prompt as late feedback.
    ResumeOnReturn,
}

/// Parse a workflow from a JSON string. (YAML loader is a trivial
/// one-line extension once `serde_yaml` is added to Cargo.toml.)
pub fn load_workflow(src: &str) -> Result<Workflow> {
    serde_json::from_str(src).context("workflow JSON parse failed")
}
