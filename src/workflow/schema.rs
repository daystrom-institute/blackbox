//! Workflow metadata schema. The embedded mermaid graph lives in
//! `Workflow.graph` as a string; the parser/validator lives in
//! [`super::mermaid`] and [`super`].

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

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
    /// boundary. The packet's entity is `{ completed: [...], in_flight:
    /// [...], last_verdict: ..., visit_counts: {...}, step: N }`. Its
    /// classifications are interpreted as arc-level verdicts:
    ///
    /// - `halt` — stop the arc immediately (error exit)
    /// - `escalate` — write a `blocked` note and continue
    /// - `warn` — write a `surprise` note and continue
    /// - any other value — no-op (treated as continue)
    ///
    /// This is the "advisor as packet" pattern — mechanical arc-health
    /// evaluation without an LLM in the decision loop.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub policy_packet: Option<String>,
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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeSpec {
    /// Name of the actor to invoke (must exist in `Workflow.actors`).
    /// Optional only when `subworkflow` is set — a sub-workflow node
    /// brings its own actors from the embedded spec.
    #[serde(default)]
    pub actor: String,
    /// Prompt template. Supports `${var}` interpolation against workflow
    /// variables + prior-node outputs. The parser accepts the raw string
    /// here; template evaluation happens at dispatch time (not parse
    /// time).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt: Option<String>,
    /// Packet id whose verdict gates advance/retry/halt after this node
    /// returns. Absent = no mechanical gate; the caller advances on
    /// successful return.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gate: Option<String>,
    /// Sync (caller waits) vs. fire-and-forget (caller advances
    /// immediately). Defaults to sync.
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
    /// sub-workflow to completion instead of dispatching an actor;
    /// the sub-workflow's per-node outputs are concatenated (with
    /// member labels) and stored as this node's output. The `actor`
    /// field becomes optional for subworkflow nodes — the sub-spec
    /// brings its own actors.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subworkflow: Option<Box<Workflow>>,
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
