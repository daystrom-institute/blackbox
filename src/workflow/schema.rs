//! Workflow metadata schema. Control flow is encoded directly on each
//! node via `NodeSpec.next` (a tagged [`NodeTransition`]); there is no
//! separate graph string. Cross-validation lives in [`super::compile`].

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;

use super::context::VarsSchema;
use super::ops::HookOp;
use super::wait::WaitSpec;
use crate::orchestration::allocator::RuntimeRequest;
use crate::orchestration::atoms::types::SupervisionPlanOverride;
use crate::orchestration::providers::Capability;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Workflow {
    pub name: String,
    pub version: u32,
    pub actors: HashMap<String, ActorSpec>,
    /// Workflow-local bindings to reusable atoms. Nodes reference
    /// these by id through `NodeSpec.atom`; the standalone atom
    /// manifest remains the reusable contract.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub atom_bindings: HashMap<String, AtomBinding>,
    pub nodes: HashMap<String, NodeSpec>,
    /// Entry node id — the first node executed when the arc starts.
    /// Must reference a key in `nodes`.
    pub start: String,
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
    /// Optional runtime allocation defaults for executor dispatch.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runtime: Option<RuntimeRequest>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct AtomBinding {
    /// Optional self-identifying id. The map key remains canonical; if
    /// present, compile validation requires it to match the key.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    pub atom_ref: String,
    /// When true, repeated visits to this binding resume the prior
    /// resumable atom invocation instead of starting a fresh one.
    #[serde(default)]
    pub durable: bool,
    #[serde(default)]
    pub compaction_anchor: bool,
    /// Provider capability requirements retained from ActorSpec donor
    /// shape. Effects such as filesystem/network live on atom policy,
    /// not here.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub requires: Vec<Capability>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limits: Option<AtomBindingLimits>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub supervision_override: Option<SupervisionPlanOverride>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trace_override: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub portal: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct AtomBindingLimits {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub writes_files: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dispatches_runs: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_depth: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub uses_network: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SleepSpec {
    /// Duration to sleep in milliseconds.
    pub duration_ms: u64,
}

/// Engine-level actor kinds. Two values: `executor` for single-bro
/// dispatch, `ensemble` for team broadcast. Persona / role / contract
/// (advisor, planner, triager, facilitator, specialist, reviewer, …)
/// is a workflow-author concern carried by the brofile lens + prompt
/// + on_exit `parse_json` validation, not an engine type.
///
/// `user` is intentionally NOT an engine kind. Human-in-the-loop and
/// agent-in-the-loop both work through the same primitives ensemble
/// dispatches use: post to a whiteboard via the `whiteboard_*` MCP
/// surface, and the arc resumes via `wait_for_phase` when the board
/// transitions. See `WORKFLOWS.md` § Whiteboards.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ActorKind {
    /// Single-bro worker. Dispatched via `bro_exec` (or `bro_resume`
    /// when the actor is durable). Used for every single-bro role
    /// the workflow declares — implementer, fixer, triager, planner,
    /// facilitator, advisor, aggregator, synthesizer, etc. The
    /// brofile + prompt carry the persona.
    Executor,
    /// Ensemble broadcast. Dispatched via `bro_broadcast` +
    /// `bro_when_all`. Each member runs the same prompt; outputs are
    /// labeled and concatenated. When the node has an associated
    /// whiteboard board, each member's STRICT-JSON output is also
    /// auto-posted to the board.
    Ensemble,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct NodeSpec {
    /// Name of the actor to invoke (must exist in `Workflow.actors`).
    /// Optional only when `subworkflow` or `wait` is set.
    #[serde(default)]
    pub actor: String,
    /// Name of an atom binding to invoke (must exist in
    /// `Workflow.atom_bindings`). Mutually exclusive with `actor`,
    /// `subworkflow`, `wait`, `foreach`, and `matrix`.
    #[serde(default)]
    pub atom: String,
    /// Structured atom input arguments. Values are resolved through
    /// `workflow::context::resolve_arg_value` so whole-string
    /// `${vars.x}` templates preserve JSON type.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub atom_args: Option<Value>,
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
    /// Sync (caller waits) vs. fire-and-forget. Fire-and-forget is
    /// only meaningful on nodes reached as the main walk continuation;
    /// nodes spawned by a `Fork` transition are always fire-and-forget.
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
    /// Reference a workflow installed in the daemon registry by id.
    /// Mutually exclusive with `subworkflow`. Resolved at dispatch
    /// time (not compile time) so the registry can change without
    /// recompiling parents that name it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subworkflow_ref: Option<String>,
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

    // ── hooks + Wait ──────────────────────────────────────
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
    /// Sleep declaration — deterministic timer node for polling loops.
    /// Mutually exclusive with actor, atom, wait, subworkflow, foreach,
    /// and matrix. Cancellation is observed while sleeping.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sleep: Option<SleepSpec>,
    /// Names of nodes whose in-flight outputs must be joined before
    /// this node's body runs. Used for fork → fan-in. The engine
    /// awaits each listed node's task (single or ensemble), records
    /// its output, and only then proceeds with on_enter / actor
    /// dispatch / wait / subworkflow. Empty/None = no fan-in.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub wait_for: Vec<String>,

    /// Dynamic-N synchronous fanout/fanin. The node expands `items`
    /// from the current ArcContext, runs the referenced child
    /// sub-workflow once per item, collects ordered item results, then
    /// continues through the ordinary `next` transition.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub foreach: Option<ForeachSpec>,
    /// Cartesian-product sugar over [`ForeachSpec`]. Axes are evaluated
    /// at dispatch time in declaration order, materialized into item
    /// objects, then passed through the foreach execution path.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub matrix: Option<MatrixSpec>,

    /// What to do when the actor's dispatched task terminates with a
    /// non-success outcome (timed out / failed / cancelled). Default
    /// `halt` matches the engine's pre-existing behavior — bail and
    /// fail the arc. Setting `continue` records the task envelope into
    /// `actor_results.<NodeId>` and proceeds to `next` so downstream
    /// nodes can branch on the outcome (e.g. PostOutcome rendering a
    /// red badge instead of crashing the arc). Only meaningful on
    /// nodes with a real `actor` reference.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub actor_failure: Option<ActorFailureMode>,

    /// Control-flow successor for this node. Required on every node;
    /// the arc terminates when control reaches a node whose
    /// transition is `Terminal`.
    pub next: NodeTransition,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum ActorFailureMode {
    /// Bail the arc on actor failure (default — preserves legacy semantics).
    #[default]
    Halt,
    /// Record the task envelope and continue to `next`. Downstream
    /// gates / templates inspect `actor_results.<NodeId>.status` etc.
    Continue,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ForeachSpec {
    /// Resolved at dispatch with `workflow::context::resolve_arg_value`.
    /// Must resolve to a JSON array; inline arrays and `${vars.*}`
    /// whole-value templates are both valid.
    pub items: Value,
    /// Child-local var name that receives the current item.
    pub as_var: String,
    /// Optional child-local var name that receives the zero-based item
    /// index.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub index_as: Option<String>,
    /// Optional key template rendered against a transient per-item
    /// context after `as_var` and `index_as` are bound. Runtime
    /// enforces non-empty, unique rendered keys.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
    /// Inline child workflow. Exactly one of `subworkflow` or
    /// `subworkflow_ref` must be present.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subworkflow: Option<Box<Workflow>>,
    /// Installed child workflow id. Exactly one of `subworkflow` or
    /// `subworkflow_ref` must be present.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subworkflow_ref: Option<String>,
    /// Parent vars copied into each child arc.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub imports: Vec<String>,
    /// Parent-context paths extracted into each child arc under local
    /// names, mirroring subworkflow import_renames.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub import_renames: HashMap<String, String>,
    /// Child terminal vars copied into each item result's `exports`
    /// object. These are not promoted directly into parent vars.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub exports: Vec<String>,
    /// Bounded child concurrency. Defaults to 1 at runtime and may not
    /// exceed `MAX_FOREACH_PARALLELISM`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parallelism: Option<usize>,
    pub collect: ForeachCollect,
    #[serde(default)]
    pub on_item_failure: ItemFailurePolicy,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MatrixSpec {
    pub axes: Vec<MatrixAxis>,
    /// Child-local var name that receives the materialized axis object.
    pub as_var: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub index_as: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subworkflow: Option<Box<Workflow>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subworkflow_ref: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub imports: Vec<String>,
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub import_renames: HashMap<String, String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub exports: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parallelism: Option<usize>,
    pub collect: ForeachCollect,
    #[serde(default)]
    pub on_item_failure: ItemFailurePolicy,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MatrixAxis {
    pub name: String,
    /// Same resolution rules as [`ForeachSpec::items`]. Must resolve to
    /// a JSON array at dispatch.
    pub values: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ForeachCollect {
    pub into_var: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ItemFailurePolicy {
    /// Stop dispatching new items and cancel/drain according to the
    /// runtime's current fanout mode. This is the fail-fast default.
    #[default]
    Halt,
    /// Stop dispatching new items after the first failure, drain any
    /// already-started children, collect their results, then fail the
    /// parent node.
    CollectThenHalt,
    /// Keep dispatching siblings and collect both successes and failures.
    Continue,
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

/// Per-node control-flow successor. Tagged enum: serialized JSON looks
/// like `{"type": "goto", "to": "Foo"}`, `{"type": "branch", ...}`,
/// `{"type": "fork", ...}`, or `{"type": "terminal"}`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum NodeTransition {
    /// Linear advance to a single named node. Use this for both
    /// forward edges and back-edges (cycles).
    Goto { to: String },
    /// Multi-way branch keyed by the gate verdict (default selector).
    /// `cases` keys SHOULD match the gate packet's classification
    /// lattice; verdicts not enumerated fall through to `default` if
    /// set, else error.
    Branch {
        #[serde(default)]
        on: BranchSelector,
        cases: HashMap<String, String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        default: Option<String>,
    },
    /// Fan-out: dispatch every name in `branches` fire-and-forget,
    /// then advance the main walk to `continue_to`. To wait for the
    /// branches before running a downstream node, set that node's
    /// `wait_for` field.
    Fork {
        branches: Vec<String>,
        continue_to: String,
    },
    /// Terminal sink — the arc ends successfully when control reaches
    /// a node whose transition is `Terminal`. Default for stub specs;
    /// real specs always set `next` explicitly.
    #[default]
    Terminal,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum BranchSelector {
    /// Match `cases` against the most recent gate verdict. Default.
    #[default]
    GateVerdict,
}

/// Parse a workflow from a JSON string. (YAML loader is a trivial
/// one-line extension once `serde_yaml` is added to Cargo.toml.)
#[allow(dead_code)] // test-only entry point; tests use this through the workflow::load_workflow reexport
pub fn load_workflow(src: &str) -> Result<Workflow> {
    serde_json::from_str(src).context("workflow JSON parse failed")
}

