//! Workflow engine — walks a compiled workflow, dispatches activity
//! nodes via the orchestration primitives, applies gate packets, follows
//! `next` transitions (goto / branch / fork / terminal), and enforces
//! per-node retry ceilings.
//!
//! v0.3 scope:
//! - Executor / Ensemble / Advisor / User / Noop actors
//! - Per-node `NodeTransition` (goto / branch-by-verdict / fork / terminal)
//! - Back-edges (cycles) expressed as plain Goto with retry-budget caps
//! - Gate packets applied after each activity node completes
//! - Fork = fire-and-forget side dispatch + main-walk continuation
//! - `wait_for` = explicit fan-in: await listed in-flight sources at
//!   node entry before running its body
//! - `${NodeName.output}` prompt substitution + retry-context prepend

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use anyhow::{anyhow, bail, Result};
use serde_json::{json, Map, Value};
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

use super::context::{resolve_arg_value, ArcContext, ArcMeta, SignalRef};
use super::ops::{execute_op, HookOp, OnFailure, OpEffect};
use super::wait::{canonicalize_correlation, PendingWait, WaitSpec};
use super::{
    ActorFailureMode, ActorKind, ActorSpec, CompiledWorkflow, ForeachSpec, GateMode,
    ItemFailurePolicy, MatrixSpec, NodeMode, NodeTransition, Workflow,
};
use crate::orchestration as orch;
use crate::BlackboxServer;

/// Sentinel value returned by `next_node` when the arc has reached a
/// `Terminal` transition. The main run loop exits on this value.
const TERMINAL_SENTINEL: &str = "__terminal__";

/// Handle to a dispatched-but-not-yet-waited-on task. Fork nodes
/// register these for their async branches; a later `late_inject` pulls
/// them out to join.
enum InFlight {
    Single {
        actor_name: String,
        durable: bool,
        task: Arc<orch::Task>,
    },
    Ensemble {
        actor_name: String,
        durable: bool,
        tasks: Vec<(String, Arc<orch::Task>)>,
    },
}

#[derive(Debug, serde::Serialize)]
pub struct WorkflowRunResult {
    pub status: String,
    pub events: Vec<Value>,
    pub node_outputs: HashMap<String, String>,
    /// Final ArcContext vars at terminal state. Populated by hook
    /// SetVar / IncVar / Forgejo ops, plus subworkflow imports.
    #[serde(default, skip_serializing_if = "Map::is_empty")]
    pub vars: Map<String, Value>,
    /// Final arc id (for resume / signal targeting on suspended arcs).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub arc_id: String,
    /// Populated only on `--dry-run`: the textual summary from
    /// [`crate::workflow::CompiledWorkflow::summarize`] for eyeballing
    /// the plan before any dispatch fires.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub plan: Option<String>,
    /// The `bbox_thread` opened for this arc at run start. Every
    /// structured event the engine emits is also written as a
    /// `bbox_note` against this thread, so arcs are queryable /
    /// auditable via the normal knowledge + notes surface.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub arc_thread_id: Option<String>,
    /// Final actor → session_id map at terminal state. Surfaces the
    /// session ids spawned for each `kind: executor` actor so
    /// post-arc capture (e.g., the Slack thread continuity store)
    /// can persist them keyed by an external correlation.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub actor_sessions: HashMap<String, String>,
}

/// Absolute ceiling on nested sub-workflow composition. The depth is
/// threaded through `run_workflow` calls so child runners inherit it —
/// a local per-runner counter would let an arbitrarily-deep nest
/// silently skip the cap (caught by a Haiku self-audit round, fixed
/// here). Top-level callers pass 0.
pub const MAX_COMPOSITION_DEPTH: u32 = 5;

/// Maximum number of materialized items a foreach/matrix node may run
/// in one expansion. Kept as a hard engine guard for the v1 primitive,
/// matching the fixed composition-depth ceiling above.
pub const MAX_FOREACH_ITEMS: usize = 256;

/// Maximum concurrent child sub-workflows for foreach/matrix fanout.
pub const MAX_FOREACH_PARALLELISM: usize = 16;

/// Strip `_actor_session.<actor>` magic keys out of `initial_vars` and
/// return them as an actor → session_id map. Callers (e.g., the Slack
/// webhook ingress) inject these to seed `actor_sessions` so the
/// runner's first executor dispatch resumes the named session instead
/// of starting fresh. Removing the keys before `seed_vars` runs keeps
/// them out of the workflow's typed-vars schema and out of node
/// templates.
fn extract_actor_session_seeds(vars: &mut Map<String, Value>) -> HashMap<String, String> {
    let mut out = HashMap::new();
    let keys: Vec<String> = vars
        .keys()
        .filter(|k| k.starts_with("_actor_session."))
        .cloned()
        .collect();
    for k in keys {
        if let Some(Value::String(s)) = vars.remove(&k) {
            if let Some(actor) = k.strip_prefix("_actor_session.") {
                out.insert(actor.to_string(), s);
            }
        }
    }
    out
}

pub async fn run_workflow(
    server: &BlackboxServer,
    compiled: &CompiledWorkflow,
    project_dir: Option<String>,
    max_steps: Option<usize>,
) -> WorkflowRunResult {
    run_workflow_with_initial_vars(server, compiled, project_dir, max_steps, Map::new()).await
}

/// Variant of [`run_workflow`] that seeds initial vars into the
/// arc's context — used by webhook router (`{route: start_arc, …}`)
/// and by the meta-loop's `bbox_orchestrate_run(initial_vars=…)` MCP
/// path.
pub async fn run_workflow_with_initial_vars(
    server: &BlackboxServer,
    compiled: &CompiledWorkflow,
    project_dir: Option<String>,
    max_steps: Option<usize>,
    initial_vars: Map<String, Value>,
) -> WorkflowRunResult {
    run_workflow_at_depth(
        server,
        compiled,
        project_dir,
        max_steps,
        0,
        HashMap::new(),
        initial_vars,
        None,
    )
    .await
}

/// Streaming variant: runs the workflow while forwarding every
/// `log_event` to the provided sender. The final `WorkflowRunResult`
/// is still returned synchronously so the HTTP handler can emit it
/// as the terminal SSE frame. Sender is dropped on exit, which
/// signals end-of-stream to consumers via `recv() -> None`.
pub async fn run_workflow_streaming(
    server: &BlackboxServer,
    compiled: &CompiledWorkflow,
    project_dir: Option<String>,
    max_steps: Option<usize>,
    event_sink: tokio::sync::mpsc::UnboundedSender<Value>,
) -> WorkflowRunResult {
    run_workflow_streaming_with_vars(
        server,
        compiled,
        project_dir,
        max_steps,
        Map::new(),
        event_sink,
    )
    .await
}

pub async fn run_workflow_streaming_with_vars(
    server: &BlackboxServer,
    compiled: &CompiledWorkflow,
    project_dir: Option<String>,
    max_steps: Option<usize>,
    initial_vars: Map<String, Value>,
    event_sink: tokio::sync::mpsc::UnboundedSender<Value>,
) -> WorkflowRunResult {
    run_workflow_streaming_with_vars_inner(
        server,
        compiled,
        project_dir,
        max_steps,
        initial_vars,
        event_sink,
        None,
    )
    .await
}

/// Streaming variant with a caller-minted arc id. Used by async MCP
/// orchestration so `bro_orchestrate_run` can return a pollable
/// `arcId` immediately and still stream events into the backing task.
pub async fn run_workflow_streaming_with_vars_and_arc_id(
    server: &BlackboxServer,
    compiled: &CompiledWorkflow,
    project_dir: Option<String>,
    max_steps: Option<usize>,
    initial_vars: Map<String, Value>,
    event_sink: tokio::sync::mpsc::UnboundedSender<Value>,
    arc_id: String,
) -> WorkflowRunResult {
    run_workflow_streaming_with_vars_inner(
        server,
        compiled,
        project_dir,
        max_steps,
        initial_vars,
        event_sink,
        Some(arc_id),
    )
    .await
}

async fn run_workflow_streaming_with_vars_inner(
    server: &BlackboxServer,
    compiled: &CompiledWorkflow,
    project_dir: Option<String>,
    max_steps: Option<usize>,
    mut initial_vars: Map<String, Value>,
    event_sink: tokio::sync::mpsc::UnboundedSender<Value>,
    arc_id_override: Option<String>,
) -> WorkflowRunResult {
    let actor_session_seeds = extract_actor_session_seeds(&mut initial_vars);
    let mut runner = WorkflowRunner::new(
        server,
        compiled,
        project_dir,
        max_steps.unwrap_or(50),
        0,
        None,
        arc_id_override,
    );
    runner.event_sink = Some(event_sink);
    for (actor, session) in actor_session_seeds {
        runner.actor_sessions.insert(actor, session);
    }
    if let Err(e) = runner
        .ctx
        .seed_vars(initial_vars, compiled.spec.vars_schema.as_ref())
    {
        return WorkflowRunResult {
            status: format!("error: initial_vars seed failed: {e}"),
            events: vec![json!({
                "kind": "error",
                "data": {"message": format!("initial_vars: {e}")},
            })],
            node_outputs: HashMap::new(),
            vars: Map::new(),
            arc_id: runner.ctx.meta.arc_id.clone(),
            plan: None,
            arc_thread_id: None,
            actor_sessions: HashMap::new(),
        };
    }
    // Required-vars enforcement at arc start. seed_vars validates kind
    // for whatever was passed; this catches keys declared `required:
    // true` that were absent from initial_vars OR seeded as null.
    if let Some(schema) = compiled.spec.vars_schema.as_ref() {
        let missing = runner.ctx.missing_required_vars(schema);
        if !missing.is_empty() {
            let msg = format!("initial_vars missing required keys: {missing:?}");
            return WorkflowRunResult {
                status: format!("error: {msg}"),
                events: vec![json!({"kind": "error", "data": {"message": msg.clone()}})],
                node_outputs: HashMap::new(),
                vars: Map::new(),
                arc_id: runner.ctx.meta.arc_id.clone(),
                plan: None,
                arc_thread_id: None,
                actor_sessions: HashMap::new(),
            };
        }
    }
    runner.open_arc_thread();
    let mut status = match runner.run().await {
        Ok(()) => "completed".to_string(),
        Err(e) => {
            let msg = e.to_string();
            // "arc cancelled" is the canonical sentinel emitted by
            // the runner when its CancellationToken trips. Surface
            // it as a first-class terminal outcome (not an error) so
            // on_arc_cancel hooks fire and consumers can distinguish
            // a manual cancel from a runtime error.
            if msg == "arc cancelled" {
                runner.log_event("cancelled_terminal", json!({"message": msg}));
                runner.arc_note("blocked", "workflow cancelled");
                let status_str = "cancelled".to_string();
                runner.update_arc_snapshot(&status_str, "(cancelled)", None);
                status_str
            } else {
                runner.log_event("error", json!({"message": msg.clone()}));
                runner.arc_note("blocked", &format!("workflow errored: {msg}"));
                let status_str = format!("error: {msg}");
                runner.update_arc_snapshot(&status_str, "(error)", None);
                status_str
            }
        }
    };
    let arc_thread_id = runner.arc_thread_id.clone();
    if matches!(status.as_str(), "completed") {
        runner.arc_note(
            "done",
            &format!(
                "workflow {} (v{}) completed in {} event(s)",
                runner.compiled.spec.name,
                runner.compiled.spec.version,
                runner.events.len()
            ),
        );
    }
    let final_outcome = runner.ctx.meta.arc_outcome.clone().unwrap_or_else(|| {
        if status == "completed" {
            "success".into()
        } else if status.starts_with("cancelled") {
            "cancelled".into()
        } else {
            "failed".into()
        }
    });
    runner.ctx.meta.arc_outcome = Some(final_outcome);
    runner.run_arc_exit_hooks().await;
    // Surface terminal-hook halt failures in `status` + the
    // running_arcs snapshot. Operators polling /orchestrate/peek or
    // reading WorkflowRunResult.status would otherwise see stale
    // "completed" while meta.arc_outcome silently records the
    // cleanup failure.
    if let Some(outcome) = runner.ctx.meta.arc_outcome.clone() {
        if outcome.starts_with("failed") && !status.starts_with("error") {
            status = format!("error: {outcome}");
            runner.update_arc_snapshot(&status, "(error)", None);
        }
    }
    // Release the arc's cancel-token registry entry. Doing this
    // before moving fields out of the runner avoids needing a Drop
    // impl (which would conflict with the by-value field moves into
    // WorkflowRunResult below).
    server.unregister_arc_cancel_token(&runner.ctx.meta.arc_id);
    let actor_sessions = runner.actor_sessions.clone();
    WorkflowRunResult {
        status,
        events: runner.events,
        node_outputs: runner.node_outputs,
        vars: runner.ctx.vars,
        arc_id: runner.ctx.meta.arc_id,
        plan: None,
        arc_thread_id,
        actor_sessions,
    }
}

/// Internal entry point that tracks nested-composition depth and
/// seeds initial node_outputs. Use [`run_workflow`] at top level;
/// `run_subworkflow_node` calls this directly with the parent's
/// `node_outputs` as seed so sub-workflow templates can reference
/// parent nodes via `${ParentNode.output}` identically to sibling
/// references.
pub async fn run_workflow_at_depth(
    server: &BlackboxServer,
    compiled: &CompiledWorkflow,
    project_dir: Option<String>,
    max_steps: Option<usize>,
    composition_depth: u32,
    seed_outputs: HashMap<String, String>,
    initial_vars: Map<String, Value>,
    parent_arc_id: Option<String>,
) -> WorkflowRunResult {
    run_workflow_at_depth_with_cancel(
        server,
        compiled,
        project_dir,
        max_steps,
        composition_depth,
        seed_outputs,
        initial_vars,
        parent_arc_id,
        None,
        None,
    )
    .await
}

/// Internal nested-runner entry point that can chain the new arc's
/// cancellation token to a parent arc or fanout group token.
async fn run_workflow_at_depth_with_cancel(
    server: &BlackboxServer,
    compiled: &CompiledWorkflow,
    project_dir: Option<String>,
    max_steps: Option<usize>,
    composition_depth: u32,
    seed_outputs: HashMap<String, String>,
    mut initial_vars: Map<String, Value>,
    parent_arc_id: Option<String>,
    parent_cancel_token: Option<CancellationToken>,
    arc_id_override: Option<String>,
) -> WorkflowRunResult {
    let actor_session_seeds = extract_actor_session_seeds(&mut initial_vars);
    if composition_depth > MAX_COMPOSITION_DEPTH {
        return WorkflowRunResult {
            status: format!(
                "error: subworkflow composition depth {composition_depth} exceeds ceiling {MAX_COMPOSITION_DEPTH}"
            ),
            events: Vec::new(),
            node_outputs: HashMap::new(),
            vars: Map::new(),
            arc_id: String::new(),
            plan: None,
            arc_thread_id: None,
            actor_sessions: HashMap::new(),
        };
    }
    let mut runner = WorkflowRunner::new(
        server,
        compiled,
        project_dir,
        max_steps.unwrap_or(50),
        composition_depth,
        parent_cancel_token.as_ref(),
        arc_id_override,
    );
    runner.node_outputs = seed_outputs.clone();
    for (actor, session) in actor_session_seeds {
        runner.actor_sessions.insert(actor, session);
    }
    // Mirror seeded outputs into the typed channel as JSON strings.
    for (k, v) in &seed_outputs {
        runner.ctx.set_output(k, Value::String(v.clone()));
    }
    runner.ctx.meta.parent_arc_id = parent_arc_id;
    if let Err(e) = runner
        .ctx
        .seed_vars(initial_vars, compiled.spec.vars_schema.as_ref())
    {
        return WorkflowRunResult {
            status: format!("error: initial_vars seed failed: {e}"),
            events: vec![json!({
                "kind": "error",
                "data": {"message": format!("initial_vars: {e}")},
            })],
            node_outputs: HashMap::new(),
            vars: Map::new(),
            arc_id: runner.ctx.meta.arc_id.clone(),
            plan: None,
            arc_thread_id: None,
            actor_sessions: HashMap::new(),
        };
    }
    // Required-vars enforcement at sub-arc start. Catches subworkflow
    // imports that omitted a required key OR fed it as null.
    if let Some(schema) = compiled.spec.vars_schema.as_ref() {
        let missing = runner.ctx.missing_required_vars(schema);
        if !missing.is_empty() {
            let msg = format!("subworkflow imports missing required keys: {missing:?}");
            return WorkflowRunResult {
                status: format!("error: {msg}"),
                events: vec![json!({"kind": "error", "data": {"message": msg.clone()}})],
                node_outputs: HashMap::new(),
                vars: Map::new(),
                arc_id: runner.ctx.meta.arc_id.clone(),
                plan: None,
                arc_thread_id: None,
                actor_sessions: HashMap::new(),
            };
        }
    }
    runner.open_arc_thread();
    let mut status = match runner.run().await {
        Ok(()) => "completed".to_string(),
        Err(e) => {
            let msg = e.to_string();
            // "arc cancelled" is the canonical sentinel emitted by
            // the runner when its CancellationToken trips. Surface
            // it as a first-class terminal outcome (not an error) so
            // on_arc_cancel hooks fire and consumers can distinguish
            // a manual cancel from a runtime error.
            if msg == "arc cancelled" {
                runner.log_event("cancelled_terminal", json!({"message": msg}));
                runner.arc_note("blocked", "workflow cancelled");
                let status_str = "cancelled".to_string();
                runner.update_arc_snapshot(&status_str, "(cancelled)", None);
                status_str
            } else {
                runner.log_event("error", json!({"message": msg.clone()}));
                runner.arc_note("blocked", &format!("workflow errored: {msg}"));
                let status_str = format!("error: {msg}");
                runner.update_arc_snapshot(&status_str, "(error)", None);
                status_str
            }
        }
    };
    let arc_thread_id = runner.arc_thread_id.clone();
    if matches!(status.as_str(), "completed") {
        runner.arc_note(
            "done",
            &format!(
                "workflow {} (v{}) completed in {} event(s)",
                runner.compiled.spec.name,
                runner.compiled.spec.version,
                runner.events.len()
            ),
        );
    }
    let final_outcome = runner.ctx.meta.arc_outcome.clone().unwrap_or_else(|| {
        if status == "completed" {
            "success".into()
        } else if status.starts_with("cancelled") {
            "cancelled".into()
        } else {
            "failed".into()
        }
    });
    runner.ctx.meta.arc_outcome = Some(final_outcome);
    runner.run_arc_exit_hooks().await;
    // Surface terminal-hook halt failures in `status` + the
    // running_arcs snapshot. Operators polling /orchestrate/peek or
    // reading WorkflowRunResult.status would otherwise see stale
    // "completed" while meta.arc_outcome silently records the
    // cleanup failure.
    if let Some(outcome) = runner.ctx.meta.arc_outcome.clone() {
        if outcome.starts_with("failed") && !status.starts_with("error") {
            status = format!("error: {outcome}");
            runner.update_arc_snapshot(&status, "(error)", None);
        }
    }
    server.unregister_arc_cancel_token(&runner.ctx.meta.arc_id);
    let actor_sessions = runner.actor_sessions.clone();
    WorkflowRunResult {
        status,
        events: runner.events,
        node_outputs: runner.node_outputs,
        vars: runner.ctx.vars,
        arc_id: runner.ctx.meta.arc_id,
        plan: None,
        arc_thread_id,
        actor_sessions,
    }
}

/// Return a dry-run plan without dispatching anything. Useful as a
/// `bro orchestrate run --dry-run` mode: parse, validate, summarize,
/// return — no bros spawned, no sessions touched.
pub fn dry_run(compiled: &CompiledWorkflow) -> WorkflowRunResult {
    WorkflowRunResult {
        status: "dry_run".into(),
        events: Vec::new(),
        node_outputs: HashMap::new(),
        vars: Map::new(),
        arc_id: String::new(),
        plan: Some(compiled.summarize()),
        arc_thread_id: None,
        actor_sessions: HashMap::new(),
    }
}

struct WorkflowRunner<'a> {
    server: &'a BlackboxServer,
    compiled: &'a CompiledWorkflow,
    project_dir: Option<String>,
    /// Legacy string-shaped output channel. Mirrored from `ctx.outputs`
    /// at every set so existing callers (CLI, server endpoints,
    /// existing tests) keep working. New code should read `ctx`.
    node_outputs: HashMap<String, String>,
    /// Primary engine state: vars, typed outputs, meta, signals.
    ctx: ArcContext,
    actor_sessions: HashMap<String, String>,
    /// Per-ensemble member session continuity: key is
    /// `<actor_name>::<member_name>`. Populated when the ensemble
    /// actor is durable.
    ensemble_sessions: HashMap<String, HashMap<String, String>>,
    /// Nodes dispatched asynchronously by a prior fork — keyed by the
    /// async target's node id. Consumed by later `late_inject` at the
    /// downstream node's entry.
    in_flight: HashMap<String, InFlight>,
    visit_counts: HashMap<String, u32>,
    last_verdict: Option<String>,
    events: Vec<Value>,
    max_steps: usize,
    arc_thread_id: Option<String>,
    /// Nesting depth for sub-workflow composition. Threaded through
    /// recursive `run_workflow_at_depth` calls so a chain of nested
    /// sub-workflows can't silently bypass the ceiling.
    composition_depth: u32,
    /// Optional live event channel — every `log_event` also sends
    /// through this sender. Used by the SSE streaming endpoint so
    /// clients see events as they happen rather than waiting for the
    /// terminal WorkflowRunResult. None for plain blocking runs.
    event_sink: Option<tokio::sync::mpsc::UnboundedSender<Value>>,
    /// Cancellation handle. Triggered by `bro_arc_cancel` MCP, the
    /// `cancel_arc` routing verdict, or the cancel CLI subcommand.
    /// The runner observes the token between node iterations and
    /// inside Wait suspensions; both bail with `arc cancelled`.
    cancel_token: tokio_util::sync::CancellationToken,
}

#[derive(Clone)]
struct FanoutRuntime {
    kind: &'static str,
    items: Vec<Value>,
    as_var: String,
    index_as: Option<String>,
    key_template: Option<String>,
    child_workflow: Workflow,
    imports: Vec<String>,
    import_renames: HashMap<String, String>,
    exports: Vec<String>,
    parallelism: usize,
    collect_into: String,
    failure_policy: ItemFailurePolicy,
}

#[derive(Clone)]
struct FanoutChildPlan {
    index: usize,
    key: String,
    item: Value,
    initial_vars: Map<String, Value>,
}

struct FanoutChildOutcome {
    index: usize,
    key: String,
    item: Value,
    status: String,
    exports: Map<String, Value>,
    outputs: Map<String, Value>,
    arc_id: String,
    arc_thread_id: Option<String>,
    error: Option<String>,
}

impl FanoutChildOutcome {
    fn is_failure(&self) -> bool {
        self.status != "completed"
    }

    fn into_value(self) -> Value {
        let mut out = Map::new();
        out.insert("index".into(), json!(self.index));
        out.insert("key".into(), Value::String(self.key));
        out.insert("item".into(), self.item);
        out.insert("status".into(), Value::String(self.status));
        out.insert("exports".into(), Value::Object(self.exports));
        out.insert("outputs".into(), Value::Object(self.outputs));
        if !self.arc_id.is_empty() {
            out.insert("arc_id".into(), Value::String(self.arc_id));
        }
        if let Some(thread_id) = self.arc_thread_id {
            out.insert("arc_thread_id".into(), Value::String(thread_id));
        }
        if let Some(error) = self.error {
            out.insert("error".into(), Value::String(error));
        }
        Value::Object(out)
    }
}

impl<'a> WorkflowRunner<'a> {
    fn new(
        server: &'a BlackboxServer,
        compiled: &'a CompiledWorkflow,
        project_dir: Option<String>,
        max_steps: usize,
        composition_depth: u32,
        parent_cancel_token: Option<&CancellationToken>,
        arc_id_override: Option<String>,
    ) -> Self {
        let arc_id =
            arc_id_override.unwrap_or_else(|| format!("arc-{}", uuid::Uuid::new_v4().simple()));
        let ctx = ArcContext::new(ArcMeta {
            arc_id: arc_id.clone(),
            workflow_name: compiled.spec.name.clone(),
            workflow_version: compiled.spec.version,
            started_at: crate::util::now_iso(),
            project_dir: project_dir.clone(),
            worktree: None,
            arc_outcome: None,
            parent_arc_id: None,
            composition_depth,
        });
        let cancel_token = match parent_cancel_token {
            Some(parent) => server.register_arc_cancel_token_child(&arc_id, parent),
            None => server.register_arc_cancel_token(&arc_id),
        };
        Self {
            server,
            compiled,
            project_dir,
            node_outputs: HashMap::new(),
            ctx,
            actor_sessions: HashMap::new(),
            ensemble_sessions: HashMap::new(),
            in_flight: HashMap::new(),
            visit_counts: HashMap::new(),
            last_verdict: None,
            events: Vec::new(),
            max_steps,
            arc_thread_id: None,
            composition_depth,
            event_sink: None,
            cancel_token,
        }
    }

    /// Mirror a string output into both legacy and typed channels.
    fn record_output(&mut self, node_id: &str, text: String) {
        self.ctx.set_output(node_id, Value::String(text.clone()));
        self.node_outputs.insert(node_id.to_string(), text);
    }

    /// Apply a single OpEffect to the runner state. Used by hook
    /// execution to centralize logging + schema validation.
    fn apply_op_effect(&mut self, effect: OpEffect) -> Result<()> {
        match effect {
            OpEffect::None => {}
            OpEffect::SetVar { key, value } => {
                self.ctx
                    .set_var(&key, value, self.compiled.spec.vars_schema.as_ref())?;
            }
            OpEffect::SetWorktree(path) => {
                self.ctx.meta.worktree = path;
            }
            OpEffect::SetProjectDir(path) => {
                self.ctx.meta.project_dir = path.clone();
                self.project_dir = path;
            }
        }
        Ok(())
    }

    /// Run a list of HookOps against the current ArcContext.
    /// Per-op `when` packet evaluates against the flattened entity;
    /// failure handled per `on_failure`.
    async fn run_hooks(&mut self, hooks: &[HookOp], lifecycle: &str) -> Result<()> {
        for (idx, hook) in hooks.iter().enumerate() {
            // Gate evaluation. Three outcomes:
            //   - Ok(Some(v)) → fire iff verdict reads as "allow"
            //   - Ok(None)    → no rule matched; treat as "deny" (skip op)
            //   - Err(e)      → packet evaluation itself failed; routed
            //                   through the op's `on_failure` policy so a
            //                   misspelled packet ref can't masquerade as
            //                   a clean skip.
            if let Some(packet_id) = &hook.when {
                let entity = self.ctx.flatten();
                let op_kind = format!("{:?}", hook.op);
                match self.server.apply_workflow_policy(packet_id, &entity) {
                    Ok(Some(verdict)) => {
                        if !is_allow_verdict(&verdict) {
                            self.log_event(
                                "hook_gated_out",
                                json!({
                                    "lifecycle": lifecycle,
                                    "index": idx,
                                    "op": op_kind,
                                    "packet_id": packet_id,
                                    "verdict": verdict,
                                }),
                            );
                            continue;
                        }
                    }
                    Ok(None) => {
                        self.log_event(
                            "hook_gated_out",
                            json!({
                                "lifecycle": lifecycle,
                                "index": idx,
                                "op": op_kind,
                                "packet_id": packet_id,
                                "reason": "no_match",
                            }),
                        );
                        continue;
                    }
                    Err(e) => {
                        self.log_event(
                            "hook_gate_error",
                            json!({
                                "lifecycle": lifecycle,
                                "index": idx,
                                "packet_id": packet_id,
                                "error": e.to_string(),
                            }),
                        );
                        match hook.on_failure {
                            OnFailure::Halt => {
                                bail!("hook {lifecycle}#{idx} {op_kind} gate '{packet_id}': {e}");
                            }
                            OnFailure::Warn => {
                                self.arc_note(
                                    "surprise",
                                    &format!(
                                        "hook {lifecycle}#{idx} {op_kind} gate '{packet_id}' errored: {e}"
                                    ),
                                );
                                continue;
                            }
                            OnFailure::Ignore => continue,
                        }
                    }
                }
            }
            let op_kind = format!("{:?}", hook.op);
            match execute_op(hook, &self.ctx, self.compiled.spec.vars_schema.as_ref()).await {
                Ok(effect) => {
                    if let Err(e) = self.apply_op_effect(effect) {
                        match hook.on_failure {
                            OnFailure::Halt => {
                                bail!("hook {lifecycle}#{idx} {op_kind} effect-apply: {e}");
                            }
                            OnFailure::Warn => {
                                self.arc_note(
                                    "surprise",
                                    &format!("hook {lifecycle}#{idx} {op_kind} effect: {e}"),
                                );
                            }
                            OnFailure::Ignore => {}
                        }
                        continue;
                    }
                    self.log_event(
                        "hook_ok",
                        json!({
                            "lifecycle": lifecycle,
                            "index": idx,
                            "op": op_kind,
                        }),
                    );
                }
                Err(e) => match hook.on_failure {
                    OnFailure::Halt => {
                        bail!("hook {lifecycle}#{idx} {op_kind}: {e}");
                    }
                    OnFailure::Warn => {
                        self.log_event(
                            "hook_failed",
                            json!({
                                "lifecycle": lifecycle,
                                "index": idx,
                                "op": op_kind,
                                "error": e.to_string(),
                                "policy": "warn",
                            }),
                        );
                        self.arc_note(
                            "surprise",
                            &format!("hook {lifecycle}#{idx} {op_kind} warned: {e}"),
                        );
                    }
                    OnFailure::Ignore => {
                        self.log_event(
                            "hook_failed",
                            json!({
                                "lifecycle": lifecycle,
                                "index": idx,
                                "op": op_kind,
                                "error": e.to_string(),
                                "policy": "ignore",
                            }),
                        );
                    }
                },
            }
        }
        Ok(())
    }

    /// Workflow-level `on_arc_cancel` + `on_arc_exit` invocation. Run
    /// at terminal state. on_arc_cancel runs ONLY when the outcome is
    /// `cancelled`; on_arc_exit runs in every terminal state. Each
    /// hook's `on_failure` policy still applies — `halt` rewrites the
    /// arc outcome to `failed` and the error surfaces in arc_outcome
    /// + the engine's events log so cleanup failures aren't silent.
    /// `warn` and `ignore` keep the original outcome intact.
    async fn run_arc_exit_hooks(&mut self) {
        let outcome = self.ctx.meta.arc_outcome.clone().unwrap_or_default();
        if outcome == "cancelled" && !self.compiled.spec.on_arc_cancel.is_empty() {
            let cancel_hooks = self.compiled.spec.on_arc_cancel.clone();
            if let Err(e) = self.run_hooks(&cancel_hooks, "arc_cancel").await {
                self.log_event(
                    "arc_cancel_hook_error",
                    json!({"error": e.to_string(), "rewrites_outcome": true}),
                );
                self.ctx.meta.arc_outcome = Some(format!("failed: arc_cancel hook halted: {e}"));
            }
        }
        if !self.compiled.spec.on_arc_exit.is_empty() {
            let exit_hooks = self.compiled.spec.on_arc_exit.clone();
            if let Err(e) = self.run_hooks(&exit_hooks, "arc_exit").await {
                self.log_event(
                    "arc_exit_hook_error",
                    json!({"error": e.to_string(), "rewrites_outcome": true}),
                );
                // Don't downgrade an already-failed outcome — the
                // original failure is more informative than the
                // cleanup failure.
                if !self
                    .ctx
                    .meta
                    .arc_outcome
                    .as_deref()
                    .is_some_and(|o| o.starts_with("failed"))
                {
                    self.ctx.meta.arc_outcome = Some(format!("failed: arc_exit hook halted: {e}"));
                }
            }
        }
    }

    /// Open a `bbox_thread(kind=work_item)` for this arc. Silent on
    /// failure — an arc still runs even if thread persistence is
    /// unavailable, it just won't be queryable via the threads/notes
    /// surface afterward.
    fn open_arc_thread(&mut self) {
        let params = crate::threads::ThreadParams {
            action: "open".into(),
            name: Some(format!("wf-{}", self.compiled.spec.name)),
            id: None,
            topic: Some(format!(
                "workflow arc: {} (v{})",
                self.compiled.spec.name, self.compiled.spec.version
            )),
            project: self.project_dir.clone(),
            session_id: None,
            provider: None,
            session_name: None,
            handoff_doc: None,
            note: None,
            target: None,
            target_type: None,
            edge: None,
            promoted_to: None,
            kind: Some("work_item".into()),
        };
        let result = {
            let mut threads = self.server.state.threads.write();
            threads.thread(&params)
        };
        match result {
            Ok(msg) => {
                let thread_id = msg
                    .split_whitespace()
                    .find(|t| t.starts_with("thread-"))
                    .map(|s| s.to_string());
                if let Some(id) = thread_id {
                    self.log_event(
                        "arc_thread_opened",
                        json!({"thread_id": id, "topic": format!("workflow arc: {}", self.compiled.spec.name)}),
                    );
                    self.arc_thread_id = Some(id);
                }
            }
            Err(e) => {
                self.log_event("arc_thread_open_failed", json!({"error": e.to_string()}));
            }
        }
    }

    /// Register/update this arc's live snapshot in the daemon's
    /// running_arcs registry for observability via /orchestrate/peek.
    /// Called at every node boundary + at start/finish with the
    /// appropriate status. Silent on missing arc_thread_id (the
    /// snapshot key).
    fn update_arc_snapshot(&self, status: &str, just_ran: &str, next: Option<&str>) {
        let Some(thread_id) = self.arc_thread_id.as_deref() else {
            return;
        };
        let now = crate::util::now_iso();
        let mut completed: Vec<String> = self.node_outputs.keys().cloned().collect();
        completed.sort();
        let mut in_flight: Vec<String> = self.in_flight.keys().cloned().collect();
        in_flight.sort();
        let visit_counts: std::collections::HashMap<String, u32> = self
            .visit_counts
            .iter()
            .filter(|(k, _)| !k.starts_with("__"))
            .map(|(k, v)| (k.clone(), *v))
            .collect();
        let mut map = self.server.state.running_arcs.write();
        let existing_started = map.get(thread_id).map(|s| s.started_at.clone());
        let snapshot = crate::ArcSnapshot {
            arc_thread_id: thread_id.to_string(),
            workflow_name: self.compiled.spec.name.clone(),
            workflow_version: self.compiled.spec.version,
            status: status.to_string(),
            current_node: next.map(|s| s.to_string()).or(Some(just_ran.to_string())),
            completed_nodes: completed,
            in_flight_nodes: in_flight,
            last_verdict: self.last_verdict.clone(),
            visit_counts,
            started_at: existing_started.unwrap_or_else(|| now.clone()),
            updated_at: now,
        };
        map.insert(thread_id.to_string(), snapshot);
    }

    /// Write a structured note on the arc thread. Kind MUST be one of
    /// the 7 canonical note kinds: dispute, assumption, surprise,
    /// followup, blocked, learned, done. No-op if no arc thread is
    /// open.
    fn arc_note(&self, kind: &str, body: &str) {
        let Some(thread_id) = self.arc_thread_id.as_deref() else {
            return;
        };
        let params = crate::notes::NoteParams {
            kind: kind.into(),
            body: body.into(),
            task_id: None,
            session_id: None,
            project: self.project_dir.clone(),
            thread_id: Some(thread_id.to_string()),
            provider: None,
            bro: None,
        };
        let mut notes = self.server.state.notes.write();
        let _ = notes.create(&params);
    }

    async fn run(&mut self) -> Result<()> {
        let entry = self.entry_node()?;
        self.log_event(
            "start",
            json!({
                "workflow": self.compiled.spec.name,
                "version": self.compiled.spec.version,
                "entry_node": entry,
            }),
        );
        self.update_arc_snapshot("running", "(start)", Some(&entry));
        let mut current = entry;
        let mut steps = 0usize;
        while current != TERMINAL_SENTINEL {
            if self.cancel_token.is_cancelled() {
                self.log_event(
                    "cancelled",
                    json!({"steps": steps, "next_was": current.clone()}),
                );
                bail!("arc cancelled");
            }
            if steps >= self.max_steps {
                bail!("workflow exceeded max_steps ({})", self.max_steps);
            }
            steps += 1;
            self.run_node(&current).await?;
            let next = self.next_node(&current)?;
            // Compaction anchor — emit a rolling summary after each
            // boundary so an observer can reconstruct arc state
            // without reading every per-node event.
            self.write_compaction_anchor(steps, &current, &next);
            // Update the in-flight arc snapshot for /orchestrate/peek.
            self.update_arc_snapshot("running", &current, Some(&next));
            // Arc-level policy packet: advisor-as-packet, evaluates
            // arc state mechanically. Halt/escalate/warn verdicts act
            // on the arc without needing an LLM advisor round.
            self.apply_policy_packet(steps, &current, &next)?;
            current = next;
        }
        self.log_event("complete", json!({"steps": steps}));
        self.update_arc_snapshot("completed", "(end)", None);
        Ok(())
    }

    fn apply_policy_packet(&mut self, step: usize, just_ran: &str, next: &str) -> Result<()> {
        let Some(packet_id) = self.compiled.spec.policy_packet.clone() else {
            return Ok(());
        };
        let mut completed: Vec<&String> = self.node_outputs.keys().collect();
        completed.sort();
        let mut in_flight: Vec<&String> = self.in_flight.keys().collect();
        in_flight.sort();
        let entity = json!({
            "step": step,
            "just_ran": just_ran,
            "next": next,
            "completed": completed,
            "completed_count": completed.len(),
            "in_flight": in_flight,
            "in_flight_count": in_flight.len(),
            "last_verdict": self.last_verdict,
            "visit_counts": self.visit_counts,
        });
        let verdict = match self.server.apply_workflow_policy(&packet_id, &entity) {
            Ok(v) => v,
            Err(e) => {
                self.log_event("policy_error", json!({"packet_id": packet_id, "error": e}));
                return Ok(());
            }
        };
        let Some(v) = verdict else {
            return Ok(());
        };
        self.log_event(
            "policy_verdict",
            json!({
                "packet_id": packet_id.clone(),
                "verdict": v.clone(),
                "step": step,
            }),
        );
        match v.as_str() {
            "halt" => {
                self.arc_note(
                    "blocked",
                    &format!("policy halt from {packet_id} at step {step} (just_ran={just_ran})"),
                );
                bail!(
                    "policy packet {packet_id} halted arc at step {step} (just_ran='{just_ran}')"
                );
            }
            "escalate" => {
                self.arc_note(
                    "blocked",
                    &format!(
                        "policy escalate from {packet_id} at step {step} (just_ran={just_ran})"
                    ),
                );
            }
            "warn" => {
                self.arc_note(
                    "surprise",
                    &format!("policy warn from {packet_id} at step {step} (just_ran={just_ran})"),
                );
            }
            _ => {
                // Treat unknown verdicts as continue; this is the
                // conservative choice. A packet with a typo in its
                // lattice shouldn't accidentally halt the arc.
            }
        }
        Ok(())
    }

    /// Rolling summary of arc state written as a `learned` note on the
    /// arc thread. Consumers filter on the `ANCHOR` prefix and take the
    /// most recent — this is the pattern daystrom calls a
    /// `compaction_anchor` on its overmind vertex. Ours is lighter:
    /// plain notes, no dedicated field, consumers scan-and-take-latest.
    fn write_compaction_anchor(&self, step: usize, just_ran: &str, next: &str) {
        let mut completed: Vec<&String> = self.node_outputs.keys().collect();
        completed.sort();
        let mut in_flight: Vec<&String> = self.in_flight.keys().collect();
        in_flight.sort();
        let mut visits: Vec<String> = self
            .visit_counts
            .iter()
            .map(|(k, v)| format!("{k}={v}"))
            .collect();
        visits.sort();
        let verdict = self.last_verdict.as_deref().unwrap_or("(none)");
        let body = format!(
            "ANCHOR [step {step}, just-ran='{just_ran}', next='{next}']: completed={completed:?} in_flight={in_flight:?} verdict={verdict} visits=[{}]",
            visits.join(", ")
        );
        self.arc_note("learned", &body);
    }

    fn entry_node(&self) -> Result<String> {
        Ok(self.compiled.spec.start.clone())
    }

    fn next_node(&self, current: &str) -> Result<String> {
        let node = self
            .compiled
            .spec
            .nodes
            .get(current)
            .ok_or_else(|| anyhow!("no metadata for node '{current}'"))?;
        match &node.next {
            NodeTransition::Terminal => Ok(TERMINAL_SENTINEL.to_string()),
            NodeTransition::Goto { to } => Ok(to.clone()),
            NodeTransition::Fork { continue_to, .. } => Ok(continue_to.clone()),
            NodeTransition::Branch { cases, default, .. } => {
                let Some(verdict) = self.last_verdict.as_deref() else {
                    if let Some(d) = default {
                        return Ok(d.clone());
                    }
                    bail!(
                        "branch node '{current}' reached with no prior gate verdict — \
                         either the node has no `gate` packet spec, the gate fired \
                         but no rule matched (packet returned None), or the predecessor's \
                         transition didn't run a gate. Ensure the gate has a catchall \
                         fallback rule, or set `default` on the branch."
                    );
                };
                if let Some(target) = cases.get(verdict) {
                    return Ok(target.clone());
                }
                if let Some(d) = default {
                    return Ok(d.clone());
                }
                let mut labels: Vec<&str> = cases.keys().map(String::as_str).collect();
                labels.sort();
                bail!(
                    "branch '{current}' has no case for verdict '{verdict}' (cases: {labels:?}, no default)"
                );
            }
        }
    }

    async fn run_node(&mut self, node_id: &str) -> Result<()> {
        // wait_for: explicit fan-in. Join any listed in-flight sources
        // before running the node body so their outputs are available
        // for prompt rendering / gate evaluation.
        let wait_for: Vec<String> = self
            .compiled
            .spec
            .nodes
            .get(node_id)
            .map(|n| n.wait_for.clone())
            .unwrap_or_default();
        if !wait_for.is_empty() {
            let mut joined = 0usize;
            let mut already = 0usize;
            for src in &wait_for {
                if self.join_in_flight_source(src).await? {
                    joined += 1;
                } else {
                    already += 1;
                }
            }
            self.log_event(
                "fan_in",
                json!({
                    "node": node_id,
                    "wait_for": wait_for,
                    "joined": joined,
                    "already_completed": already,
                }),
            );
        }
        self.run_activity_node(node_id).await
    }

    async fn run_fork_dispatch(&mut self, node_id: &str) -> Result<()> {
        let branches: Vec<String> = self
            .compiled
            .spec
            .nodes
            .get(node_id)
            .and_then(|n| match &n.next {
                NodeTransition::Fork { branches, .. } => Some(branches.clone()),
                _ => None,
            })
            .unwrap_or_default();
        if branches.is_empty() {
            return Ok(());
        }
        self.log_event(
            "fork",
            json!({
                "node": node_id,
                "branches": branches.clone(),
            }),
        );
        for target in branches {
            self.dispatch_fire_and_forget(&target).await?;
        }
        Ok(())
    }

    async fn run_activity_node(&mut self, node_id: &str) -> Result<()> {
        let spec = self
            .compiled
            .spec
            .nodes
            .get(node_id)
            .ok_or_else(|| anyhow!("no metadata for activity node '{node_id}'"))?
            .clone();

        // on_enter hooks fire BEFORE every node body — including
        // subworkflow descents, Wait registrations, and actor
        // dispatches. They set up state (worktree, vars, branch
        // names) and are the right place to run setup ops.
        if !spec.on_enter.is_empty() {
            self.run_hooks(&spec.on_enter, &format!("{node_id}/on_enter"))
                .await?;
        }

        // Dynamic fanout: foreach/matrix nodes own child
        // sub-workflow dispatch and collection; they otherwise pass
        // through the ordinary on_exit/gate/next boundary.
        if spec.foreach.is_some() || spec.matrix.is_some() {
            self.run_dynamic_fanout_node(node_id, &spec).await?;
            self.run_node_exit_hooks(&spec.on_exit, node_id).await?;
            self.apply_node_gate(node_id, &spec).await;
            return Ok(());
        }

        // Wait node — suspend the arc on a signal. Mutually exclusive
        // with subworkflow + actor.
        if let Some(wait_spec) = spec.wait.clone() {
            self.run_wait_node(node_id, &wait_spec).await?;
            self.run_node_exit_hooks(&spec.on_exit, node_id).await?;
            self.apply_node_gate(node_id, &spec).await;
            return Ok(());
        }

        // Sub-workflow composition: if the node embeds a workflow OR
        // references one by id, run it recursively instead of
        // dispatching an actor. The parent node's output becomes the
        // concatenated sub-node outputs.
        if spec.subworkflow.is_some() || spec.subworkflow_ref.is_some() {
            self.run_subworkflow_node(node_id).await?;
            self.run_node_exit_hooks(&spec.on_exit, node_id).await?;
            self.apply_node_gate(node_id, &spec).await;
            return Ok(());
        }

        let actor_name = spec.actor.clone();

        // Hook-only / pure-routing node: no actor declared. The
        // rendered prompt becomes the node's captured output (so
        // downstream `${NodeName.output}` references stay legal),
        // hooks have already fired around this point, gate runs as
        // usual. Fork side-dispatch fires here too.
        if actor_name.is_empty() {
            let raw_template = spec.prompt.as_deref().unwrap_or("");
            let prompt = self.render_prompt(raw_template);
            self.record_output(node_id, prompt.clone());
            self.log_event(
                "node_complete_hookless",
                json!({"node": node_id, "output_bytes": prompt.len()}),
            );
            if matches!(spec.next, NodeTransition::Fork { .. }) {
                self.run_fork_dispatch(node_id).await?;
            }
            self.run_node_exit_hooks(&spec.on_exit, node_id).await?;
            self.apply_node_gate(node_id, &spec).await;
            return Ok(());
        }

        let actor = self.compiled.spec.actors.get(&actor_name).ok_or_else(|| {
            anyhow!("node '{node_id}' references undeclared actor '{actor_name}'")
        })?;
        // Fire-and-forget on the main walk: dispatch and store the
        // handle, then advance without waiting. Downstream late_inject
        // consumers will join.
        if matches!(spec.mode, NodeMode::FireAndForget) {
            self.dispatch_fire_and_forget(node_id).await?;
            return Ok(());
        }

        // Late-inject join — if this node's spec references an
        // in-flight source, wait for it and fold its output into
        // node_outputs before rendering this node's prompt template.
        self.join_late_inject(node_id).await?;

        // Retry ceiling — every visit bumps the count; if we exceed the
        // node's `retry.max_generations`, halt the arc. This is the
        // circuit-breaker from daystrom's generation-tracking pattern.
        let visit_count = {
            let c = self.visit_counts.entry(node_id.to_string()).or_insert(0);
            *c += 1;
            *c
        };
        if let Some(retry) = &spec.retry {
            if visit_count > retry.max_generations {
                bail!(
                    "node '{node_id}' exceeded retry ceiling ({} generations; visited {visit_count} times)",
                    retry.max_generations
                );
            }
        }

        let raw_template = spec.prompt.as_deref().unwrap_or("");
        let mut prompt = self.render_prompt(raw_template);
        if visit_count > 1 {
            // Prepend retry context so the retried bro sees the prior
            // gate verdict. Durable actors also see their own prior
            // turn via session continuity; non-durable actors get the
            // verdict string as the only signal.
            let verdict = self.last_verdict.as_deref().unwrap_or("(no verdict)");
            prompt = format!(
                "[retry — attempt {visit_count}, prior gate verdict: {verdict}]\n\n{prompt}"
            );
        }

        let actor_failure = spec.actor_failure.unwrap_or_default();
        match &actor.kind {
            ActorKind::Executor => {
                self.run_executor_node(node_id, actor, &actor_name, &prompt, actor_failure)
                    .await?;
            }
            ActorKind::Ensemble => {
                self.run_ensemble_node(node_id, actor, &actor_name, &prompt)
                    .await?;
            }
        }

        // Fork dispatch: if this activity node's `next` is a Fork,
        // spawn the side-branches fire-and-forget AFTER the main
        // body has captured its output.
        if matches!(spec.next, NodeTransition::Fork { .. }) {
            self.run_fork_dispatch(node_id).await?;
        }

        // on_exit hooks — fire AFTER actor return but BEFORE gate so
        // the gate sees normalized output (e.g. after a ParseJson
        // hook stuffs structured data into vars).
        self.run_node_exit_hooks(&spec.on_exit, node_id).await?;

        // Apply the gate packet (if any). Dispatch by gate_mode:
        // - first (default): one rule's classification becomes verdict
        // - all: every matching rule produces a finding, verdict is
        //   the lattice-highest-priority classification across them
        if let Some(packet_id) = spec.gate.as_deref() {
            let output = self.node_outputs.get(node_id).cloned().unwrap_or_default();
            let mode = spec.gate_mode.clone().unwrap_or_default();
            match mode {
                GateMode::First => {
                    match self.server.apply_workflow_gate(packet_id, &output, node_id) {
                        Ok(verdict) => {
                            self.last_verdict = verdict.clone();
                            self.log_event(
                                "gate_applied",
                                json!({
                                    "node": node_id,
                                    "packet_id": packet_id,
                                    "mode": "first",
                                    "verdict": verdict.clone(),
                                }),
                            );
                            let verdict_s = verdict.as_deref().unwrap_or("(no match)");
                            self.arc_note(
                                "learned",
                                &format!("gate '{packet_id}' on '{node_id}' (first) → {verdict_s}"),
                            );
                        }
                        Err(e) => {
                            self.log_event(
                                "gate_error",
                                json!({
                                    "node": node_id,
                                    "packet_id": packet_id,
                                    "mode": "first",
                                    "error": e,
                                }),
                            );
                        }
                    }
                }
                GateMode::All => {
                    match self
                        .server
                        .apply_workflow_gate_all(packet_id, &output, node_id)
                    {
                        Ok(result) => {
                            self.last_verdict = result.verdict.clone();
                            let finding_previews: Vec<String> = result
                                .findings
                                .iter()
                                .map(|f| {
                                    let consequent_str = format!("{:?}", f.consequent);
                                    let preview: String = consequent_str.chars().take(80).collect();
                                    format!("{}[{}]: {preview}", f.classification, f.rule_id)
                                })
                                .collect();
                            self.log_event(
                                "gate_applied",
                                json!({
                                    "node": node_id,
                                    "packet_id": packet_id,
                                    "mode": "all",
                                    "verdict": result.verdict.clone(),
                                    "finding_count": result.findings.len(),
                                    "findings": finding_previews.clone(),
                                }),
                            );
                            let verdict_s = result.verdict.as_deref().unwrap_or("(no match)");
                            self.arc_note(
                                "learned",
                                &format!(
                                    "gate '{packet_id}' on '{node_id}' (all) → verdict={verdict_s}, {} finding(s): [{}]",
                                    result.findings.len(),
                                    finding_previews.join("; ")
                                ),
                            );
                        }
                        Err(e) => {
                            self.log_event(
                                "gate_error",
                                json!({
                                    "node": node_id,
                                    "packet_id": packet_id,
                                    "mode": "all",
                                    "error": e,
                                }),
                            );
                        }
                    }
                }
            }
        }
        Ok(())
    }

    async fn run_executor_node(
        &mut self,
        node_id: &str,
        actor: &ActorSpec,
        actor_name: &str,
        prompt: &str,
        failure_mode: ActorFailureMode,
    ) -> Result<()> {
        let brofile = actor
            .brofile
            .as_deref()
            .ok_or_else(|| anyhow!("executor actor '{actor_name}' missing brofile"))?;
        let existing_session = if actor.durable {
            self.actor_sessions.get(actor_name).cloned()
        } else {
            None
        };
        self.log_event(
            "node_dispatch",
            json!({
                "node": node_id,
                "actor": actor_name,
                "brofile": brofile,
                "mode": if existing_session.is_some() { "resume" } else { "exec" },
                "prompt_bytes": prompt.len(),
                "visit": self.visit_counts.get(node_id).copied().unwrap_or(0),
            }),
        );
        let task = self
            .server
            .workflow_dispatch_executor(
                brofile,
                prompt,
                self.project_dir.as_deref(),
                existing_session.as_deref(),
            )
            .await
            .map_err(|e| anyhow!("dispatch for node '{node_id}': {e}"))?;

        let task_id = {
            let inner = task.inner.lock();
            inner.id.clone()
        };
        let completed = orch::wait_for_task_with_timeout(&task, Some(900.0)).await;
        // Record full task envelope into actor_results regardless of
        // outcome so downstream nodes can branch on `status`, surface
        // `result` text, or stash `taskId` for state-machine bookkeeping.
        // Timeout uses the snapshot variant (sets `timed_out: true`); a
        // completed task uses the canonical task_result_json.
        let result_json = if completed {
            orch::task_result_json(&task)
        } else {
            orch::timeout_snapshot_json(&task)
        };
        self.ctx.record_actor_result(node_id, result_json.clone());

        let session_id = task.inner.lock().session_id.clone();
        if actor.durable {
            self.actor_sessions
                .insert(actor_name.to_string(), session_id.clone());
        }

        let task_status = result_json
            .get("status")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let success = completed && task_status == "completed";

        let output = result_json
            .get("result")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        // Always record string output so legacy `${outputs.NodeId}`
        // templates stay populated even on failure (best-effort: the
        // `result` field on a failed/timed-out task is the last
        // assistant message before termination).
        self.record_output(node_id, output.clone());

        if !success {
            let reason = if !completed { "timeout" } else { "task_failed" };
            self.log_event(
                "node_actor_failure",
                json!({
                    "node": node_id,
                    "task_id": task_id,
                    "session_id": session_id,
                    "reason": reason,
                    "task_status": task_status,
                    "failure_mode": failure_mode,
                }),
            );
            match failure_mode {
                ActorFailureMode::Halt => {
                    if !completed {
                        bail!("node '{node_id}' (task {task_id}) exceeded timeout");
                    } else {
                        bail!(
                            "node '{node_id}' (task {task_id}) terminated with status={task_status}"
                        );
                    }
                }
                ActorFailureMode::Continue => {
                    self.arc_note(
                        "surprise",
                        &format!(
                            "node '{node_id}' actor failed (reason={reason}, status={task_status}); continuing per actor_failure=continue"
                        ),
                    );
                    return Ok(());
                }
            }
        }

        let output_preview: String = output.chars().take(160).collect();
        self.log_event(
            "node_complete",
            json!({
                "node": node_id,
                "task_id": task_id,
                "session_id": session_id,
                "output_bytes": output.len(),
                "output_preview": output_preview.clone(),
            }),
        );
        self.arc_note("done", &format!("node '{node_id}' → {output_preview}"));
        Ok(())
    }

    async fn run_dynamic_fanout_node(
        &mut self,
        node_id: &str,
        spec: &super::schema::NodeSpec,
    ) -> Result<()> {
        let runtime = self.materialize_fanout_runtime(node_id, spec)?;
        let total = runtime.items.len();
        if total > MAX_FOREACH_ITEMS {
            bail!(
                "node '{node_id}' {} materialized {total} items, above ceiling {MAX_FOREACH_ITEMS}",
                runtime.kind
            );
        }
        let child_depth = self.composition_depth + 1;
        if child_depth > MAX_COMPOSITION_DEPTH {
            bail!(
                "{} recursion would exceed ceiling {MAX_COMPOSITION_DEPTH} at node '{node_id}' (current depth {}, child would be {child_depth})",
                runtime.kind,
                self.composition_depth
            );
        }

        let compiled = super::compile(runtime.child_workflow.clone()).map_err(|e| {
            anyhow!(
                "{} child on node '{node_id}' failed to compile: {e}",
                runtime.kind
            )
        })?;
        crate::validate_workflow_capabilities(&compiled, &self.server.state).map_err(|e| {
            anyhow!(
                "{} child on node '{node_id}' capability validation: {e}",
                runtime.kind
            )
        })?;

        let plans = self.build_fanout_child_plans(node_id, &runtime)?;
        let mut results: Vec<Option<Value>> = vec![None; total];
        let mut first_failure: Option<String> = None;
        let seed_outputs = self.node_outputs.clone();
        let project_dir = self.project_dir.clone();
        let parent_arc_id = self.ctx.meta.arc_id.clone();
        let group_cancel = self.cancel_token.child_token();
        let effective_parallelism = runtime.parallelism.max(1).min(MAX_FOREACH_PARALLELISM);
        let mut joinset = tokio::task::JoinSet::new();
        let mut next_to_start = 0usize;
        let mut stop_new_dispatch = false;

        self.log_event(
            "fanout_begin",
            json!({
                "node": node_id,
                "kind": runtime.kind,
                "items": total,
                "parallelism": runtime.parallelism,
                "effective_parallelism": effective_parallelism,
                "collect_into": runtime.collect_into,
                "on_item_failure": runtime.failure_policy,
            }),
        );

        while next_to_start < total || !joinset.is_empty() {
            if self.cancel_token.is_cancelled() {
                group_cancel.cancel();
                bail!("arc cancelled");
            }

            while !stop_new_dispatch
                && next_to_start < total
                && joinset.len() < effective_parallelism
            {
                let plan = plans[next_to_start].clone();
                self.log_event(
                    "fanout_item_start",
                    json!({
                        "node": node_id,
                        "index": plan.index,
                        "key": plan.key,
                    }),
                );
                let state = self.server.state.clone();
                let compiled_for_child = compiled.clone();
                let project_dir_for_child = project_dir.clone();
                let seed_outputs_for_child = seed_outputs.clone();
                let parent_arc_id_for_child = parent_arc_id.clone();
                let cancel_for_child = group_cancel.clone();
                let exports_for_child = runtime.exports.clone();
                joinset.spawn_blocking(move || {
                    let runtime = tokio::runtime::Builder::new_current_thread()
                        .enable_all()
                        .build();
                    let Ok(runtime) = runtime else {
                        return FanoutChildOutcome {
                            index: plan.index,
                            key: plan.key,
                            item: plan.item,
                            status: "error".into(),
                            exports: Map::new(),
                            outputs: Map::new(),
                            arc_id: String::new(),
                            arc_thread_id: None,
                            error: Some("failed to build fanout child runtime".into()),
                        };
                    };
                    runtime.block_on(run_fanout_child_owned(
                        state,
                        compiled_for_child,
                        project_dir_for_child,
                        child_depth,
                        seed_outputs_for_child,
                        parent_arc_id_for_child,
                        cancel_for_child,
                        plan,
                        exports_for_child,
                    ))
                });
                next_to_start += 1;
            }

            if joinset.is_empty() {
                break;
            }
            let outcome = match joinset.join_next().await {
                Some(Ok(outcome)) => outcome,
                Some(Err(err)) => {
                    first_failure.get_or_insert_with(|| format!("fanout child task join: {err}"));
                    stop_new_dispatch = true;
                    group_cancel.cancel();
                    continue;
                }
                None => break,
            };
            let failed = outcome.is_failure();
            let failure_msg = outcome
                .error
                .clone()
                .unwrap_or_else(|| format!("item {} status {}", outcome.index, outcome.status));
            self.log_event(
                if failed {
                    "fanout_item_failed"
                } else {
                    "fanout_item_complete"
                },
                json!({
                    "node": node_id,
                    "index": outcome.index,
                    "key": outcome.key,
                    "status": outcome.status,
                    "error": outcome.error,
                }),
            );
            let idx = outcome.index;
            results[idx] = Some(outcome.into_value());
            if failed
                && first_failure.is_none()
                && !matches!(runtime.failure_policy, ItemFailurePolicy::Continue)
            {
                first_failure = Some(failure_msg);
                stop_new_dispatch = true;
                if matches!(runtime.failure_policy, ItemFailurePolicy::Halt) {
                    group_cancel.cancel();
                }
            }
        }

        if stop_new_dispatch {
            for idx in next_to_start..total {
                if results[idx].is_none() {
                    results[idx] = Some(fanout_skipped_value(idx, &plans[idx]));
                }
            }
        }

        let collected = Value::Array(results.into_iter().flatten().collect());
        self.ctx.set_var(
            &runtime.collect_into,
            collected.clone(),
            self.compiled.spec.vars_schema.as_ref(),
        )?;
        self.record_output(node_id, collected.to_string());
        self.log_event(
            "fanout_complete",
            json!({
                "node": node_id,
                "kind": runtime.kind,
                "items": total,
                "collected": collected.as_array().map(Vec::len).unwrap_or_default(),
                "collect_into": runtime.collect_into,
                "failed": first_failure.is_some(),
            }),
        );
        self.arc_note(
            if first_failure.is_some() {
                "blocked"
            } else {
                "done"
            },
            &format!(
                "{} node '{node_id}' collected {} item result(s) into vars.{}",
                runtime.kind,
                collected.as_array().map(Vec::len).unwrap_or_default(),
                runtime.collect_into
            ),
        );

        if let Some(error) = first_failure {
            bail!(
                "{} node '{node_id}' failed under {:?}: {error}",
                runtime.kind,
                runtime.failure_policy
            );
        }
        Ok(())
    }

    fn materialize_fanout_runtime(
        &self,
        node_id: &str,
        spec: &super::schema::NodeSpec,
    ) -> Result<FanoutRuntime> {
        match (&spec.foreach, &spec.matrix) {
            (Some(foreach), None) => self.materialize_foreach_runtime(node_id, foreach),
            (None, Some(matrix)) => self.materialize_matrix_runtime(node_id, matrix),
            _ => bail!("node '{node_id}' expected exactly one of foreach or matrix"),
        }
    }

    fn materialize_foreach_runtime(
        &self,
        node_id: &str,
        spec: &ForeachSpec,
    ) -> Result<FanoutRuntime> {
        let items_value = resolve_arg_value(&self.ctx, &spec.items)
            .map_err(|e| anyhow!("foreach node '{node_id}' items: {e}"))?;
        let items = items_value
            .as_array()
            .cloned()
            .ok_or_else(|| anyhow!("foreach node '{node_id}' items resolved to non-array"))?;
        Ok(FanoutRuntime {
            kind: "foreach",
            items,
            as_var: spec.as_var.clone(),
            index_as: spec.index_as.clone(),
            key_template: spec.key.clone(),
            child_workflow: self.resolve_fanout_child_workflow(
                node_id,
                spec.subworkflow.as_deref(),
                spec.subworkflow_ref.as_deref(),
            )?,
            imports: spec.imports.clone(),
            import_renames: spec.import_renames.clone(),
            exports: spec.exports.clone(),
            parallelism: spec.parallelism.unwrap_or(1),
            collect_into: spec.collect.into_var.clone(),
            failure_policy: spec.on_item_failure.clone(),
        })
    }

    fn materialize_matrix_runtime(
        &self,
        node_id: &str,
        spec: &MatrixSpec,
    ) -> Result<FanoutRuntime> {
        let mut axes: Vec<(String, Vec<Value>)> = Vec::new();
        for axis in &spec.axes {
            let values = resolve_arg_value(&self.ctx, &axis.values)
                .map_err(|e| anyhow!("matrix node '{node_id}' axis '{}': {e}", axis.name))?;
            let values = values.as_array().cloned().ok_or_else(|| {
                anyhow!(
                    "matrix node '{node_id}' axis '{}' resolved to non-array",
                    axis.name
                )
            })?;
            axes.push((axis.name.clone(), values));
        }
        let items = expand_matrix_items(node_id, &axes)?;
        Ok(FanoutRuntime {
            kind: "matrix",
            items,
            as_var: spec.as_var.clone(),
            index_as: spec.index_as.clone(),
            key_template: spec.key.clone(),
            child_workflow: self.resolve_fanout_child_workflow(
                node_id,
                spec.subworkflow.as_deref(),
                spec.subworkflow_ref.as_deref(),
            )?,
            imports: spec.imports.clone(),
            import_renames: spec.import_renames.clone(),
            exports: spec.exports.clone(),
            parallelism: spec.parallelism.unwrap_or(1),
            collect_into: spec.collect.into_var.clone(),
            failure_policy: spec.on_item_failure.clone(),
        })
    }

    fn resolve_fanout_child_workflow(
        &self,
        node_id: &str,
        inline: Option<&Workflow>,
        reference: Option<&str>,
    ) -> Result<Workflow> {
        if let Some(inline) = inline {
            return Ok(inline.clone());
        }
        let id =
            reference.ok_or_else(|| anyhow!("fanout node '{node_id}' missing child workflow"))?;
        self.server
            .resolve_workflow_by_id(id)
            .ok_or_else(|| anyhow!("subworkflow_ref '{id}' on fanout node '{node_id}' not in registry — install via bro_workflow_install"))
    }

    fn build_fanout_child_plans(
        &mut self,
        node_id: &str,
        runtime: &FanoutRuntime,
    ) -> Result<Vec<FanoutChildPlan>> {
        let mut plans = Vec::with_capacity(runtime.items.len());
        let mut keys = HashSet::new();
        for (idx, item) in runtime.items.iter().cloned().enumerate() {
            let mut initial_vars: Map<String, Value> = Map::new();
            for k in &runtime.imports {
                if let Some(v) = self.ctx.vars.get(k) {
                    initial_vars.insert(k.clone(), v.clone());
                } else {
                    self.log_event(
                        "fanout_import_missing",
                        json!({"node": node_id, "index": idx, "import": k}),
                    );
                }
            }
            for (local_name, parent_path) in &runtime.import_renames {
                match self.ctx.resolve(parent_path) {
                    Some(v) => {
                        initial_vars.insert(local_name.clone(), v);
                    }
                    None => {
                        self.log_event(
                            "fanout_rename_unresolved",
                            json!({
                                "node": node_id,
                                "index": idx,
                                "local": local_name,
                                "parent_path": parent_path,
                            }),
                        );
                    }
                }
            }
            initial_vars.insert(runtime.as_var.clone(), item.clone());
            if let Some(index_as) = &runtime.index_as {
                initial_vars.insert(index_as.clone(), json!(idx));
            }

            let key = if let Some(template) = &runtime.key_template {
                let mut item_ctx = self.ctx.clone();
                item_ctx.vars.insert(runtime.as_var.clone(), item.clone());
                if let Some(index_as) = &runtime.index_as {
                    item_ctx.vars.insert(index_as.clone(), json!(idx));
                }
                item_ctx.render_template(template)
            } else {
                idx.to_string()
            };
            if key.trim().is_empty() {
                bail!(
                    "{} node '{node_id}' item {idx} rendered an empty key",
                    runtime.kind
                );
            }
            if !keys.insert(key.clone()) {
                bail!(
                    "{} node '{node_id}' rendered duplicate item key '{key}'",
                    runtime.kind
                );
            }
            plans.push(FanoutChildPlan {
                index: idx,
                key,
                item,
                initial_vars,
            });
        }
        Ok(plans)
    }

    async fn dispatch_fire_and_forget(&mut self, target_id: &str) -> Result<()> {
        let spec = self
            .compiled
            .spec
            .nodes
            .get(target_id)
            .ok_or_else(|| anyhow!("fork: no metadata for async target '{target_id}'"))?;
        let actor_name = spec.actor.clone();
        let actor = self.compiled.spec.actors.get(&actor_name).ok_or_else(|| {
            anyhow!("fork: async target '{target_id}' references undeclared actor '{actor_name}'")
        })?;
        let prompt = self.render_prompt(spec.prompt.as_deref().unwrap_or(""));
        match &actor.kind {
            ActorKind::Executor => {
                let brofile = actor.brofile.as_deref().ok_or_else(|| {
                    anyhow!("async target '{target_id}' executor missing brofile")
                })?;
                let existing = if actor.durable {
                    self.actor_sessions.get(&actor_name).cloned()
                } else {
                    None
                };
                let task = self
                    .server
                    .workflow_dispatch_executor(
                        brofile,
                        &prompt,
                        self.project_dir.as_deref(),
                        existing.as_deref(),
                    )
                    .await
                    .map_err(|e| anyhow!("fire-and-forget dispatch '{target_id}': {e}"))?;
                self.log_event(
                    "fire_and_forget",
                    json!({
                        "node": target_id,
                        "actor": actor_name,
                        "brofile": brofile,
                        "task_id": task.inner.lock().id.clone(),
                    }),
                );
                self.in_flight.insert(
                    target_id.to_string(),
                    InFlight::Single {
                        actor_name,
                        durable: actor.durable,
                        task,
                    },
                );
            }
            ActorKind::Ensemble => {
                let team = actor
                    .team
                    .as_deref()
                    .ok_or_else(|| anyhow!("async target '{target_id}' ensemble missing team"))?;
                let existing = if actor.durable {
                    self.ensemble_sessions
                        .get(&actor_name)
                        .cloned()
                        .unwrap_or_default()
                } else {
                    HashMap::new()
                };
                let tasks = self
                    .server
                    .workflow_dispatch_ensemble(
                        team,
                        &prompt,
                        self.project_dir.as_deref(),
                        &existing,
                    )
                    .await
                    .map_err(|e| anyhow!("fire-and-forget ensemble dispatch '{target_id}': {e}"))?;
                self.log_event(
                    "fire_and_forget_ensemble",
                    json!({
                        "node": target_id,
                        "actor": actor_name,
                        "team": team,
                        "members": tasks.iter().map(|(m, _)| m.clone()).collect::<Vec<_>>(),
                    }),
                );
                self.in_flight.insert(
                    target_id.to_string(),
                    InFlight::Ensemble {
                        actor_name,
                        durable: actor.durable,
                        tasks,
                    },
                );
            }
        }
        Ok(())
    }

    /// If the given node has a `late_inject` spec, wait on the source
    /// node's in-flight task(s), capture the output into
    /// `node_outputs[source]`, and update session maps so any durable
    /// actor's future visits resume correctly. Idempotent if no
    /// late_inject is declared or the source is already joined.
    async fn join_late_inject(&mut self, node_id: &str) -> Result<()> {
        let late_inject = match self
            .compiled
            .spec
            .nodes
            .get(node_id)
            .and_then(|s| s.late_inject.as_ref())
        {
            Some(li) => li.clone(),
            None => return Ok(()),
        };
        let source = late_inject.from;
        let joined = self.join_in_flight_source(&source).await?;
        if joined {
            self.log_event(
                "late_inject_join",
                json!({"node": node_id, "source": source}),
            );
            self.arc_note(
                "surprise",
                &format!("node '{node_id}' late-joined async source '{source}'"),
            );
        } else {
            self.log_event(
                "late_inject_skip",
                json!({
                    "node": node_id,
                    "source": source,
                    "reason": "no in-flight handle",
                }),
            );
        }
        Ok(())
    }

    /// Wait on a single in-flight source node's task(s), capture its
    /// output into `node_outputs[source]`, update session maps if the
    /// actor was durable. Returns `Ok(true)` when a handle was joined,
    /// `Ok(false)` when the source had no in-flight handle (already
    /// joined, or never dispatched). Shared path between `late_inject`
    /// and explicit `<<join>>` control nodes.
    async fn join_in_flight_source(&mut self, source: &str) -> Result<bool> {
        let entry = match self.in_flight.remove(source) {
            Some(e) => e,
            None => return Ok(false),
        };
        match entry {
            InFlight::Single {
                actor_name,
                durable,
                task,
            } => {
                let task_id = task.inner.lock().id.clone();
                let completed = orch::wait_for_task_with_timeout(&task, Some(900.0)).await;
                if !completed {
                    bail!("in-flight source '{source}' (task {task_id}) exceeded timeout");
                }
                let result_json = orch::task_result_json(&task);
                let output = result_json
                    .get("result")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let session_id = task.inner.lock().session_id.clone();
                if durable {
                    self.actor_sessions.insert(actor_name, session_id.clone());
                }
                self.record_output(source, output);
            }
            InFlight::Ensemble {
                actor_name,
                durable,
                tasks,
            } => {
                let mut joinset = tokio::task::JoinSet::new();
                for (member, task) in tasks {
                    joinset.spawn(async move {
                        let completed = orch::wait_for_task_with_timeout(&task, Some(900.0)).await;
                        let result_json = orch::task_result_json(&task);
                        let output = result_json
                            .get("result")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string();
                        let session_id = task.inner.lock().session_id.clone();
                        (member, completed, output, session_id)
                    });
                }
                let mut outs: Vec<(String, String)> = Vec::new();
                let mut sessions: HashMap<String, String> = HashMap::new();
                let mut timed_out = false;
                while let Some(res) = joinset.join_next().await {
                    let (member, completed, output, session_id) =
                        res.map_err(|e| anyhow!("ensemble join: {e}"))?;
                    if !completed {
                        timed_out = true;
                    }
                    sessions.insert(member.clone(), session_id);
                    outs.push((member, output));
                }
                if timed_out {
                    bail!("in-flight source '{source}' (ensemble) had member timeouts");
                }
                outs.sort_by(|a, b| a.0.cmp(&b.0));
                let merged = outs
                    .iter()
                    .map(|(m, o)| format!("── {m} ──\n{o}"))
                    .collect::<Vec<_>>()
                    .join("\n\n");
                if durable {
                    self.ensemble_sessions.insert(actor_name, sessions);
                }
                self.record_output(source, merged);
            }
        }
        Ok(true)
    }

    async fn run_ensemble_node(
        &mut self,
        node_id: &str,
        actor: &ActorSpec,
        actor_name: &str,
        prompt: &str,
    ) -> Result<()> {
        let team = actor
            .team
            .as_deref()
            .ok_or_else(|| anyhow!("ensemble actor '{actor_name}' missing team"))?;
        let ensemble_key = actor_name.to_string();
        let existing_sessions = if actor.durable {
            self.ensemble_sessions
                .get(&ensemble_key)
                .cloned()
                .unwrap_or_default()
        } else {
            HashMap::new()
        };
        self.log_event(
            "node_dispatch",
            json!({
                "node": node_id,
                "actor": actor_name,
                "team": team,
                "kind": "ensemble",
                "prompt_bytes": prompt.len(),
                "visit": self.visit_counts.get(node_id).copied().unwrap_or(0),
            }),
        );
        let tasks = self
            .server
            .workflow_dispatch_ensemble(
                team,
                prompt,
                self.project_dir.as_deref(),
                &existing_sessions,
            )
            .await
            .map_err(|e| anyhow!("dispatch for node '{node_id}': {e}"))?;

        // Wait for every member concurrently, then collect outputs.
        let mut joinset = tokio::task::JoinSet::new();
        for (member_name, task) in tasks.iter() {
            let member_name = member_name.clone();
            let task_clone = task.clone();
            joinset.spawn(async move {
                let completed = orch::wait_for_task_with_timeout(&task_clone, Some(900.0)).await;
                let result_json = orch::task_result_json(&task_clone);
                let output = result_json
                    .get("result")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let session_id = task_clone.inner.lock().session_id.clone();
                let task_id = task_clone.inner.lock().id.clone();
                (member_name, completed, output, session_id, task_id)
            });
        }
        let mut member_outputs: Vec<(String, String)> = Vec::new();
        let mut member_sessions: HashMap<String, String> = HashMap::new();
        let mut any_timeout = false;
        while let Some(res) = joinset.join_next().await {
            let (member, completed, output, session_id, task_id) =
                res.map_err(|e| anyhow!("ensemble join: {e}"))?;
            if !completed {
                any_timeout = true;
                self.log_event(
                    "ensemble_member_timeout",
                    json!({
                        "node": node_id,
                        "member": member,
                        "task_id": task_id,
                    }),
                );
            }
            member_sessions.insert(member.clone(), session_id.clone());
            let preview: String = output.chars().take(160).collect();
            self.log_event(
                "ensemble_member_complete",
                json!({
                    "node": node_id,
                    "member": member,
                    "task_id": task_id,
                    "session_id": session_id,
                    "output_bytes": output.len(),
                    "output_preview": preview,
                }),
            );
            member_outputs.push((member, output));
        }
        if any_timeout {
            bail!("node '{node_id}' had one or more ensemble-member timeouts");
        }
        // Stable order (by member name) so prompt substitution is
        // deterministic across re-runs.
        member_outputs.sort_by(|a, b| a.0.cmp(&b.0));
        let merged = member_outputs
            .iter()
            .map(|(m, o)| format!("── {m} ──\n{o}"))
            .collect::<Vec<_>>()
            .join("\n\n");
        // Mirror into ctx.outputs so downstream `${NodeName.output}`
        // templates resolve consistently (see `record_output` —
        // executor + ensemble + in-flight-join all use the same path).
        self.record_output(node_id, merged.clone());
        if actor.durable {
            self.ensemble_sessions.insert(ensemble_key, member_sessions);
        }
        let member_names: Vec<String> = member_outputs.iter().map(|(m, _)| m.clone()).collect();
        self.log_event(
            "node_complete",
            json!({
                "node": node_id,
                "members": member_names.clone(),
                "merged_bytes": merged.len(),
            }),
        );
        self.arc_note(
            "done",
            &format!(
                "ensemble node '{node_id}' completed — {} members ({})",
                member_names.len(),
                member_names.join(", ")
            ),
        );
        Ok(())
    }

    /// Recursively run a sub-workflow embedded in a node. The parent
    /// node's output becomes the concatenated labeled outputs of every
    /// activity node in the sub-workflow (stable order). The sub-arc
    /// runs in its own compiled context but under the same server and
    /// project_dir — so it opens its own arc thread, applies its own
    /// gates, etc. Depth-limited to prevent runaway recursion.
    async fn run_subworkflow_node(&mut self, node_id: &str) -> Result<()> {
        let spec = self
            .compiled
            .spec
            .nodes
            .get(node_id)
            .ok_or_else(|| anyhow!("no metadata for subworkflow node '{node_id}'"))?
            .clone();
        let sub_spec = if let Some(inline) = &spec.subworkflow {
            (**inline).clone()
        } else if let Some(id) = &spec.subworkflow_ref {
            self.server
                .resolve_workflow_by_id(id)
                .ok_or_else(|| anyhow!("subworkflow_ref '{id}' on node '{node_id}' not in registry — install via bro_workflow_install"))?
        } else {
            bail!("subworkflow missing from node '{node_id}' (neither inline nor ref set)");
        };

        // Depth is threaded through `run_workflow_at_depth`. Check at
        // dispatch time so the error surfaces here (with node context)
        // rather than inside the child runner's error return.
        let child_depth = self.composition_depth + 1;
        if child_depth > MAX_COMPOSITION_DEPTH {
            bail!(
                "subworkflow recursion would exceed ceiling {MAX_COMPOSITION_DEPTH} at node '{node_id}' (current depth {}, child would be {child_depth})",
                self.composition_depth
            );
        }

        self.log_event(
            "subworkflow_begin",
            json!({
                "node": node_id,
                "sub_name": sub_spec.name.clone(),
                "sub_version": sub_spec.version,
                "parent_depth": self.composition_depth,
                "child_depth": child_depth,
            }),
        );
        self.arc_note(
            "learned",
            &format!(
                "node '{node_id}' entering sub-workflow '{}' (depth {child_depth}/{MAX_COMPOSITION_DEPTH})",
                sub_spec.name
            ),
        );

        let compiled = super::compile(sub_spec)
            .map_err(|e| anyhow!("subworkflow on node '{node_id}' failed to compile: {e}"))?;
        // Capability validation on the resolved sub-spec. Without this
        // a brofile/team can change after install and a referenced
        // subworkflow silently dispatches against a now-incapable
        // provider. Ref-resolution is dispatch-time, so this check
        // must happen here, not at parent compile.
        crate::validate_workflow_capabilities(&compiled, &self.server.state)
            .map_err(|e| anyhow!("subworkflow on node '{node_id}' capability validation: {e}"))?;
        let project_dir = self.project_dir.clone();
        // Seed the sub-runner with the parent's node_outputs so sub
        // prompts can reference `${ParentNode.output}` the same way
        // siblings do. Caveat: sub-node names that collide with parent
        // names overwrite parent entries in the sub's local copy.
        let seed_outputs = self.node_outputs.clone();

        // Compute initial_vars for the sub: explicit imports list +
        // import_renames (extractor expressions on parent context).
        // import_renames takes precedence; missing import keys are a
        // soft warning (sub may be tolerant).
        let mut initial_vars: Map<String, Value> = Map::new();
        for k in &spec.imports {
            if let Some(v) = self.ctx.vars.get(k) {
                initial_vars.insert(k.clone(), v.clone());
            } else {
                self.log_event(
                    "subworkflow_import_missing",
                    json!({"node": node_id, "import": k}),
                );
            }
        }
        for (local_name, parent_path) in &spec.import_renames {
            match self.ctx.resolve(parent_path) {
                Some(v) => {
                    initial_vars.insert(local_name.clone(), v);
                }
                None => {
                    self.log_event(
                        "subworkflow_rename_unresolved",
                        json!({
                            "node": node_id,
                            "local": local_name,
                            "parent_path": parent_path,
                        }),
                    );
                }
            }
        }

        let parent_arc_id = self.ctx.meta.arc_id.clone();
        // Box the recursive future to avoid infinitely-sized types.
        // Depth is threaded so the child runner knows where it is in
        // the composition tree.
        let sub_result = Box::pin(run_workflow_at_depth_with_cancel(
            self.server,
            &compiled,
            project_dir,
            Some(25),
            child_depth,
            seed_outputs,
            initial_vars,
            Some(parent_arc_id),
            Some(self.cancel_token.clone()),
            None,
        ))
        .await;

        if !sub_result.status.starts_with("completed") {
            bail!(
                "subworkflow '{}' did not complete cleanly: {}",
                compiled.spec.name,
                sub_result.status
            );
        }

        // Merge sub-node outputs into a single labeled string — same
        // shape as ensemble output so downstream templates can consume
        // it consistently. Filter OUT seeded parent outputs (keys not
        // in the sub's own node set) so the merge reflects only what
        // the sub produced, not what it received as input context.
        let sub_node_names: std::collections::HashSet<String> =
            compiled.spec.nodes.keys().cloned().collect();
        let mut sub_outputs: Vec<(String, String)> = sub_result
            .node_outputs
            .into_iter()
            .filter(|(k, _)| sub_node_names.contains(k))
            .collect();
        sub_outputs.sort_by(|a, b| a.0.cmp(&b.0));
        let merged = sub_outputs
            .iter()
            .map(|(n, o)| format!("── sub:{n} ──\n{o}"))
            .collect::<Vec<_>>()
            .join("\n\n");
        self.record_output(node_id, merged.clone());

        // Promote declared exports from sub vars back into parent.
        // Missing exports are a runtime error — declared contract.
        for k in &spec.exports {
            match sub_result.vars.get(k) {
                Some(v) => {
                    if let Err(e) =
                        self.ctx
                            .set_var(k, v.clone(), self.compiled.spec.vars_schema.as_ref())
                    {
                        bail!(
                            "subworkflow '{}' export '{k}' parent-write rejected: {e}",
                            compiled.spec.name
                        );
                    }
                    self.log_event(
                        "subworkflow_export",
                        json!({"node": node_id, "key": k, "value": v.clone()}),
                    );
                }
                None => {
                    bail!(
                        "subworkflow '{}' did not export declared key '{k}' (have: {:?})",
                        compiled.spec.name,
                        sub_result.vars.keys().collect::<Vec<_>>()
                    );
                }
            }
        }

        self.log_event(
            "subworkflow_complete",
            json!({
                "node": node_id,
                "sub_arc_thread_id": sub_result.arc_thread_id,
                "sub_events": sub_result.events.len(),
                "sub_node_count": sub_outputs.len(),
                "exports_promoted": spec.exports.len(),
                "merged_bytes": merged.len(),
            }),
        );
        self.arc_note(
            "done",
            &format!(
                "sub-workflow '{}' completed ({} sub-nodes) — arc={}",
                compiled.spec.name,
                sub_outputs.len(),
                sub_result.arc_thread_id.as_deref().unwrap_or("?")
            ),
        );
        Ok(())
    }

    fn render_prompt(&self, template: &str) -> String {
        // Use the new ArcContext templater. It handles ${vars.x},
        // ${outputs.NodeName.field}, ${meta.x}, ${last_signal.x}
        // AND the legacy ${NodeName.output} form, so existing prompts
        // keep working.
        self.ctx.render_template(template)
    }

    async fn run_node_exit_hooks(&mut self, hooks: &[HookOp], node_id: &str) -> Result<()> {
        if hooks.is_empty() {
            return Ok(());
        }
        self.run_hooks(hooks, &format!("{node_id}/on_exit")).await
    }

    /// Extracted gate evaluation. Reads the just-completed node's
    /// output AND the full ArcContext flatten so rules can reference
    /// `vars.x`, `last_signal.name`, etc. — not just the node output
    /// string. The legacy first-mode and all-mode handlers still
    /// receive the output string for backward compatibility; the
    /// flattened entity is added under `node_output_json` and via
    /// the gate-entity helpers in `apply_workflow_gate_*`.
    async fn apply_node_gate(&mut self, node_id: &str, spec: &super::schema::NodeSpec) {
        let Some(packet_id) = spec.gate.as_deref() else {
            return;
        };
        // Build the canonical gate entity once, then both first- and
        // all-mode dispatchers use it.
        let entity = self.ctx.flatten_for_gate(node_id);
        let mode = spec.gate_mode.clone().unwrap_or_default();
        match mode {
            GateMode::First => {
                match self
                    .server
                    .apply_workflow_gate_entity(packet_id, &entity, node_id)
                {
                    Ok(verdict) => {
                        self.last_verdict = verdict.clone();
                        self.log_event(
                            "gate_applied",
                            json!({
                                "node": node_id,
                                "packet_id": packet_id,
                                "mode": "first",
                                "verdict": verdict.clone(),
                            }),
                        );
                        let verdict_s = verdict.as_deref().unwrap_or("(no match)");
                        self.arc_note(
                            "learned",
                            &format!("gate '{packet_id}' on '{node_id}' (first) → {verdict_s}"),
                        );
                    }
                    Err(e) => {
                        self.log_event(
                            "gate_error",
                            json!({
                                "node": node_id,
                                "packet_id": packet_id,
                                "mode": "first",
                                "error": e,
                            }),
                        );
                    }
                }
            }
            GateMode::All => {
                match self
                    .server
                    .apply_workflow_gate_all_entity(packet_id, &entity, node_id)
                {
                    Ok(result) => {
                        self.last_verdict = result.verdict.clone();
                        let finding_previews: Vec<String> = result
                            .findings
                            .iter()
                            .map(|f| {
                                let consequent_str = format!("{:?}", f.consequent);
                                let preview: String = consequent_str.chars().take(80).collect();
                                format!("{}[{}]: {preview}", f.classification, f.rule_id)
                            })
                            .collect();
                        self.log_event(
                            "gate_applied",
                            json!({
                                "node": node_id,
                                "packet_id": packet_id,
                                "mode": "all",
                                "verdict": result.verdict.clone(),
                                "finding_count": result.findings.len(),
                                "findings": finding_previews.clone(),
                            }),
                        );
                        let verdict_s = result.verdict.as_deref().unwrap_or("(no match)");
                        self.arc_note(
                            "learned",
                            &format!(
                                "gate '{packet_id}' on '{node_id}' (all) → verdict={verdict_s}, {} finding(s): [{}]",
                                result.findings.len(),
                                finding_previews.join("; ")
                            ),
                        );
                    }
                    Err(e) => {
                        self.log_event(
                            "gate_error",
                            json!({
                                "node": node_id,
                                "packet_id": packet_id,
                                "mode": "all",
                                "error": e,
                            }),
                        );
                    }
                }
            }
        }
    }

    /// Wait node — register pending waits in the server's WaitStore,
    /// suspend on a Notify, resume when a matching signal arrives.
    /// Updates `ctx.last_signal` + `signal_history` before returning.
    async fn run_wait_node(&mut self, node_id: &str, spec: &WaitSpec) -> Result<()> {
        // Clear any prior signal so a stale reference can't leak.
        self.ctx.clear_last_signal();

        let mut registered_ids: Vec<(String, String)> = Vec::new();
        let resolved_slot: Arc<parking_lot::Mutex<Option<SignalRef>>> =
            Arc::new(parking_lot::Mutex::new(None));
        let notify = Arc::new(Notify::new());
        let arc_id = self.ctx.meta.arc_id.clone();

        for (idx, wait_signal) in spec.any_of.iter().enumerate() {
            // Resolve correlation tuple via Selector against the
            // current ArcContext flatten.
            let context_entity = self.ctx.flatten();
            let mut correlation = Map::new();
            for (k, sel) in &wait_signal.correlate {
                let v = sel
                    .evaluate(&context_entity)
                    .map_err(|e| anyhow!("Wait correlation eval for '{k}': {e}"))?;
                correlation.insert(k.clone(), v);
            }
            let wait_id = format!("{node_id}#{idx}");
            self.log_event(
                "wait_registered",
                json!({
                    "node": node_id,
                    "wait_id": wait_id,
                    "signal": wait_signal.signal,
                    "correlation_canonical": canonicalize_correlation(&correlation),
                }),
            );
            self.server.wait_store().register(PendingWait {
                arc_id: arc_id.clone(),
                wait_id: wait_id.clone(),
                signal: wait_signal.signal.clone(),
                correlation,
                notify: notify.clone(),
                resolved: resolved_slot.clone(),
            });
            registered_ids.push((arc_id.clone(), wait_id));
        }

        // Block on Notify with optional timeout, OR an arc-cancel
        // signal. Without the cancel arm, a parked arc on a
        // long-timeout wait would only release on signal arrival or
        // timeout — a manual cancel would have no observation point.
        // The store's match_and_take pops the first matching wait
        // and inserts the SignalRef into resolved_slot before
        // notifying.
        let timeout = spec.timeout;
        let cancel_token = self.cancel_token.clone();
        enum WaitOutcome {
            Resolved,
            Cancelled,
            TimedOut,
        }
        let outcome = match timeout {
            Some(d) => tokio::select! {
                _ = notify.notified() => WaitOutcome::Resolved,
                _ = cancel_token.cancelled() => WaitOutcome::Cancelled,
                _ = tokio::time::sleep(d) => WaitOutcome::TimedOut,
            },
            None => tokio::select! {
                _ = notify.notified() => WaitOutcome::Resolved,
                _ = cancel_token.cancelled() => WaitOutcome::Cancelled,
            },
        };

        // Cancel sibling waits — only the first to fire wins; the
        // others must be removed from the store so a later signal
        // doesn't accidentally resume a completed arc.
        for (arc, wid) in &registered_ids {
            self.server.wait_store().cancel(arc, wid);
        }

        if matches!(outcome, WaitOutcome::Cancelled) {
            self.log_event(
                "wait_cancelled",
                json!({
                    "node": node_id,
                    "registered_waits": registered_ids
                        .iter()
                        .map(|(_, w)| w.clone())
                        .collect::<Vec<_>>(),
                }),
            );
            bail!("arc cancelled");
        }
        let waited = matches!(outcome, WaitOutcome::Resolved);

        if !waited {
            // Timeout — synthesize a __timeout__ signal payload so the
            // gate can branch on it (`In{name, [pr-merged, __timeout__]}`).
            let sig = SignalRef {
                name: "__timeout__".into(),
                payload: json!({
                    "expired": spec.any_of.iter().map(|s| s.signal.clone()).collect::<Vec<_>>(),
                }),
                correlation: Map::new(),
                received_at: crate::util::now_iso(),
            };
            self.log_event(
                "wait_timeout",
                json!({
                    "node": node_id,
                    "expired_signals": sig.payload["expired"].clone(),
                }),
            );
            self.ctx.record_signal(sig.clone());
            self.record_output(node_id, serde_json::to_string(&sig).unwrap_or_default());
            self.arc_note(
                "surprise",
                &format!("Wait '{node_id}' timed out after {:?}", timeout),
            );
            return Ok(());
        }

        let sig = resolved_slot
            .lock()
            .clone()
            .ok_or_else(|| anyhow!("Wait '{node_id}' notified but resolved slot empty"))?;
        self.log_event(
            "wait_resolved",
            json!({
                "node": node_id,
                "signal": sig.name,
                "correlation": sig.correlation,
            }),
        );
        self.record_output(node_id, serde_json::to_string(&sig).unwrap_or_default());
        self.ctx.record_signal(sig.clone());
        self.arc_note(
            "done",
            &format!("Wait '{node_id}' resolved by signal '{}'", sig.name),
        );
        Ok(())
    }

    fn log_event(&mut self, kind: &str, data: Value) {
        let ev = json!({
            "kind": kind,
            "data": data,
            "timestamp": crate::util::now_iso(),
        });
        if let Some(tx) = &self.event_sink {
            let _ = tx.send(ev.clone());
        }
        self.events.push(ev);
    }
}

async fn run_fanout_child_owned(
    state: Arc<crate::SharedState>,
    compiled: CompiledWorkflow,
    project_dir: Option<String>,
    child_depth: u32,
    seed_outputs: HashMap<String, String>,
    parent_arc_id: String,
    parent_cancel_token: CancellationToken,
    plan: FanoutChildPlan,
    exports: Vec<String>,
) -> FanoutChildOutcome {
    let server = BlackboxServer::new(state);
    run_fanout_child(
        &server,
        compiled,
        project_dir,
        child_depth,
        seed_outputs,
        parent_arc_id,
        parent_cancel_token,
        plan,
        exports,
    )
    .await
}

async fn run_fanout_child(
    server: &BlackboxServer,
    compiled: CompiledWorkflow,
    project_dir: Option<String>,
    child_depth: u32,
    seed_outputs: HashMap<String, String>,
    parent_arc_id: String,
    parent_cancel_token: CancellationToken,
    plan: FanoutChildPlan,
    exports: Vec<String>,
) -> FanoutChildOutcome {
    let result = Box::pin(run_workflow_at_depth_with_cancel(
        server,
        &compiled,
        project_dir,
        Some(25),
        child_depth,
        seed_outputs,
        plan.initial_vars,
        Some(parent_arc_id),
        Some(parent_cancel_token),
        None,
    ))
    .await;

    let mut export_values = Map::new();
    let mut error = None;
    if result.status.starts_with("completed") {
        for key in &exports {
            match result.vars.get(key) {
                Some(value) => {
                    export_values.insert(key.clone(), value.clone());
                }
                None => {
                    error = Some(format!(
                        "child workflow '{}' did not export declared key '{key}'",
                        compiled.spec.name
                    ));
                    break;
                }
            }
        }
    } else {
        error = Some(result.status.clone());
    }

    let outputs = result
        .node_outputs
        .into_iter()
        .map(|(key, value)| (key, Value::String(value)))
        .collect();
    let status = if error.is_some() {
        if result.status == "cancelled" {
            "cancelled".to_string()
        } else {
            "error".to_string()
        }
    } else {
        "completed".to_string()
    };
    FanoutChildOutcome {
        index: plan.index,
        key: plan.key,
        item: plan.item,
        status,
        exports: export_values,
        outputs,
        arc_id: result.arc_id,
        arc_thread_id: result.arc_thread_id,
        error,
    }
}

fn fanout_skipped_value(index: usize, plan: &FanoutChildPlan) -> Value {
    json!({
        "index": index,
        "key": plan.key,
        "item": plan.item,
        "status": "skipped",
        "exports": {},
        "outputs": {},
        "error": "not dispatched after earlier item failure"
    })
}

fn expand_matrix_items(node_id: &str, axes: &[(String, Vec<Value>)]) -> Result<Vec<Value>> {
    let mut items: Vec<Map<String, Value>> = vec![Map::new()];
    for (axis_name, values) in axes {
        let projected = items
            .len()
            .checked_mul(values.len())
            .ok_or_else(|| anyhow!("matrix node '{node_id}' item count overflow"))?;
        if projected > MAX_FOREACH_ITEMS {
            bail!(
                "matrix node '{node_id}' would materialize {projected} items, above ceiling {MAX_FOREACH_ITEMS}"
            );
        }
        let mut next = Vec::with_capacity(projected);
        for base in &items {
            for value in values {
                let mut item = base.clone();
                item.insert(axis_name.clone(), value.clone());
                next.push(item);
            }
        }
        items = next;
    }
    Ok(items.into_iter().map(Value::Object).collect())
}

/// True when a hook-gating packet returned a verdict that means
/// "fire the op." We accept the canonical positive classifications
/// from the lattices most likely to be used for hook gating:
/// `allow`, `pass`, `proceed`, `delete`, `keep`. Conservative — an
/// unknown verdict (e.g. `flag`, `manual`) does NOT permit firing.
fn is_allow_verdict(verdict: &str) -> bool {
    matches!(
        verdict,
        "allow" | "pass" | "proceed" | "fire" | "ok" | "delete" | "keep" | "yes" | "true"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workflow::{compile, load_workflow};

    fn mini_compiled() -> CompiledWorkflow {
        let json = r#"{
            "name": "t",
            "version": 1,
            "actors": {"a": {"kind": "executor", "brofile": "b"}},
            "nodes": {
                "N1": {"actor": "a", "prompt": "first node", "next": {"type": "goto", "to": "N2"}},
                "N2": {"actor": "a", "prompt": "echo ${N1.output}", "next": {"type": "terminal"}}
            },
            "start": "N1"
        }"#;
        compile(load_workflow(json).unwrap()).unwrap()
    }

    fn branch_compiled() -> CompiledWorkflow {
        let json = r#"{
            "name": "t",
            "version": 1,
            "actors": {"a": {"kind": "executor", "brofile": "b"}},
            "nodes": {
                "Decide": {
                    "actor": "a",
                    "gate": "packet-12345678",
                    "next": {
                        "type": "branch",
                        "cases": {"yes": "Yes", "no": "No"}
                    }
                },
                "Yes": {"actor": "a", "next": {"type": "terminal"}},
                "No":  {"actor": "a", "next": {"type": "terminal"}}
            },
            "start": "Decide"
        }"#;
        compile(load_workflow(json).unwrap()).unwrap()
    }

    #[test]
    fn render_prompt_substitutes_node_outputs() {
        let mut outputs = HashMap::new();
        outputs.insert("N1".to_string(), "hello world".to_string());
        let rendered = render_with(&outputs, "echo ${N1.output}");
        assert_eq!(rendered, "echo hello world");
    }

    #[test]
    fn entry_walk_and_sequential_transitions() {
        let compiled = mini_compiled();
        let runner = runner_for(&compiled);
        assert_eq!(runner.entry_node().unwrap(), "N1");
        assert_eq!(runner.next_node("N1").unwrap(), "N2");
        assert_eq!(runner.next_node("N2").unwrap(), TERMINAL_SENTINEL);
    }

    #[test]
    fn branch_routes_by_last_verdict() {
        let compiled = branch_compiled();
        let mut runner = runner_for(&compiled);
        runner.last_verdict = Some("yes".into());
        assert_eq!(runner.next_node("Decide").unwrap(), "Yes");
        runner.last_verdict = Some("no".into());
        assert_eq!(runner.next_node("Decide").unwrap(), "No");
    }

    #[test]
    fn branch_without_verdict_errors() {
        let compiled = branch_compiled();
        let runner = runner_for(&compiled);
        let err = runner.next_node("Decide").unwrap_err().to_string();
        assert!(err.contains("no prior gate verdict"), "err: {err}");
    }

    #[test]
    fn branch_with_unmatched_verdict_errors() {
        let compiled = branch_compiled();
        let mut runner = runner_for(&compiled);
        runner.last_verdict = Some("maybe".into());
        let err = runner.next_node("Decide").unwrap_err().to_string();
        assert!(err.contains("no case for verdict 'maybe'"), "err: {err}");
        assert!(err.contains("yes") && err.contains("no"));
    }

    #[test]
    fn branch_with_default_falls_through() {
        let json = r#"{
            "name": "t",
            "version": 1,
            "actors": {"a": {"kind": "executor", "brofile": "b"}},
            "nodes": {
                "Decide": {
                    "actor": "a",
                    "gate": "packet-12345678",
                    "next": {
                        "type": "branch",
                        "cases": {"yes": "Yes"},
                        "default": "Fallback"
                    }
                },
                "Yes": {"actor": "a", "next": {"type": "terminal"}},
                "Fallback": {"actor": "a", "next": {"type": "terminal"}}
            },
            "start": "Decide"
        }"#;
        let compiled = compile(load_workflow(json).unwrap()).unwrap();
        let mut runner = runner_for(&compiled);
        runner.last_verdict = Some("anything-else".into());
        assert_eq!(runner.next_node("Decide").unwrap(), "Fallback");
    }

    #[test]
    fn branch_with_default_handles_missing_verdict() {
        let json = r#"{
            "name": "t",
            "version": 1,
            "actors": {"a": {"kind": "executor", "brofile": "b"}},
            "nodes": {
                "Decide": {
                    "actor": "a",
                    "gate": "packet-12345678",
                    "next": {
                        "type": "branch",
                        "cases": {"yes": "Yes"},
                        "default": "Fallback"
                    }
                },
                "Yes": {"actor": "a", "next": {"type": "terminal"}},
                "Fallback": {"actor": "a", "next": {"type": "terminal"}}
            },
            "start": "Decide"
        }"#;
        let compiled = compile(load_workflow(json).unwrap()).unwrap();
        let runner = runner_for(&compiled);
        assert_eq!(runner.next_node("Decide").unwrap(), "Fallback");
    }

    fn runner_for(compiled: &CompiledWorkflow) -> DummyRunner<'_> {
        DummyRunner {
            compiled,
            last_verdict: None,
        }
    }

    // Mirror of WorkflowRunner's read-side helpers, free of the server
    // ref. Keeps the transition-walk logic testable without spinning
    // up the daemon.
    struct DummyRunner<'a> {
        compiled: &'a CompiledWorkflow,
        last_verdict: Option<String>,
    }

    impl<'a> DummyRunner<'a> {
        fn entry_node(&self) -> Result<String> {
            Ok(self.compiled.spec.start.clone())
        }

        fn next_node(&self, current: &str) -> Result<String> {
            let node = self
                .compiled
                .spec
                .nodes
                .get(current)
                .ok_or_else(|| anyhow!("no metadata for node '{current}'"))?;
            match &node.next {
                NodeTransition::Terminal => Ok(TERMINAL_SENTINEL.to_string()),
                NodeTransition::Goto { to } => Ok(to.clone()),
                NodeTransition::Fork { continue_to, .. } => Ok(continue_to.clone()),
                NodeTransition::Branch { cases, default, .. } => {
                    let Some(verdict) = self.last_verdict.as_deref() else {
                        if let Some(d) = default {
                            return Ok(d.clone());
                        }
                        bail!("branch '{current}' has no prior gate verdict");
                    };
                    if let Some(t) = cases.get(verdict) {
                        return Ok(t.clone());
                    }
                    if let Some(d) = default {
                        return Ok(d.clone());
                    }
                    let mut labels: Vec<&str> = cases.keys().map(String::as_str).collect();
                    labels.sort();
                    bail!("branch '{current}' has no case for verdict '{verdict}' (cases: {labels:?})")
                }
            }
        }
    }

    fn render_with(outputs: &HashMap<String, String>, template: &str) -> String {
        let mut out = template.to_string();
        for (node, output) in outputs {
            let key = format!("${{{node}.output}}");
            out = out.replace(&key, output);
        }
        out
    }
}
