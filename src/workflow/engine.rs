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

use std::collections::HashMap;
use std::sync::Arc;

use anyhow::{Result, anyhow, bail};
use serde_json::{Map, Value, json};
use tokio_util::sync::CancellationToken;

use super::context::{ArcContext, ArcMeta};
use super::ops::HookOp;
use super::{
    ActorKind, ActorSpec, CompiledWorkflow, ForeachSpec, GateMode, ItemFailurePolicy, MatrixSpec,
    NodeTransition, Workflow,
};
use crate::orchestration as orch;
use crate::server::state::BlackboxServer;
use crate::server::workflow_capabilities::validate_workflow_capabilities;
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
    /// Optional machine-readable final output for workflow-backed atoms.
    /// Workflows set `vars._structured_exit` or `vars.structured_exit`;
    /// the engine records and surfaces it without parsing node prose.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub structured_exit: Option<Value>,
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

fn workflow_structured_exit(vars: &Map<String, Value>) -> Option<Value> {
    vars.get("_structured_exit")
        .or_else(|| vars.get("structured_exit"))
        .cloned()
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
            structured_exit: None,
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
                structured_exit: None,
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
                runner
                    .emit_arc_system_event(
                        crate::system_events::types::SystemEventKind::WorkflowArcCancelled,
                        json!({"arc_id": runner.ctx.meta.arc_id}),
                    )
                    .await;
                status_str
            } else {
                runner.log_event("error", json!({"message": msg.clone()}));
                runner.arc_note("blocked", &format!("workflow errored: {msg}"));
                let status_str = format!("error: {msg}");
                runner.update_arc_snapshot(&status_str, "(error)", None);
                runner
                    .emit_arc_system_event(
                        crate::system_events::types::SystemEventKind::WorkflowArcFailed,
                        json!({"arc_id": runner.ctx.meta.arc_id, "error": msg}),
                    )
                    .await;
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
    let structured_exit = workflow_structured_exit(&runner.ctx.vars);
    if let Some(value) = &structured_exit {
        runner.log_event("structured_exit", json!({ "value": value }));
    }
    WorkflowRunResult {
        status,
        events: runner.events,
        node_outputs: runner.node_outputs,
        vars: runner.ctx.vars,
        structured_exit,
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
            structured_exit: None,
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
            structured_exit: None,
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
                structured_exit: None,
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
                runner
                    .emit_arc_system_event(
                        crate::system_events::types::SystemEventKind::WorkflowArcCancelled,
                        json!({"arc_id": runner.ctx.meta.arc_id}),
                    )
                    .await;
                status_str
            } else {
                runner.log_event("error", json!({"message": msg.clone()}));
                runner.arc_note("blocked", &format!("workflow errored: {msg}"));
                let status_str = format!("error: {msg}");
                runner.update_arc_snapshot(&status_str, "(error)", None);
                runner
                    .emit_arc_system_event(
                        crate::system_events::types::SystemEventKind::WorkflowArcFailed,
                        json!({"arc_id": runner.ctx.meta.arc_id, "error": msg}),
                    )
                    .await;
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
    let structured_exit = workflow_structured_exit(&runner.ctx.vars);
    if let Some(value) = &structured_exit {
        runner.log_event("structured_exit", json!({ "value": value }));
    }
    WorkflowRunResult {
        status,
        events: runner.events,
        node_outputs: runner.node_outputs,
        vars: runner.ctx.vars,
        structured_exit,
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
        structured_exit: None,
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
    actor_tasks: HashMap<String, String>,
    atom_invocations: HashMap<String, String>,
    /// Per-ensemble member session continuity: key is
    /// `<actor_name>::<member_name>`. Populated when the ensemble
    /// actor is durable.
    ensemble_sessions: HashMap<String, HashMap<String, String>>,
    ensemble_tasks: HashMap<String, HashMap<String, String>>,
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
            actor_tasks: HashMap::new(),
            atom_invocations: HashMap::new(),
            ensemble_sessions: HashMap::new(),
            ensemble_tasks: HashMap::new(),
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

    fn effective_project_dir(&self) -> Option<String> {
        self.ctx
            .meta
            .worktree
            .clone()
            .or_else(|| self.project_dir.clone())
    }

    fn runtime_for_actor(
        &self,
        actor: &ActorSpec,
    ) -> Option<crate::orchestration::allocator::RuntimeRequest> {
        let mut request = actor.runtime.clone();
        if actor.durable || !actor.requires.is_empty() {
            let mut derived = request.unwrap_or_default();
            if actor.durable {
                derived.durable = true;
            }
            derived.capabilities.extend(actor.requires.iter().copied());
            derived.capabilities.sort_by_key(|cap| format!("{cap:?}"));
            derived.capabilities.dedup();
            request = Some(derived);
        }
        request
    }

    fn ensure_not_cancelled(&mut self, node_id: &str, phase: &str) -> Result<()> {
        if self.cancel_token.is_cancelled() {
            self.log_event(
                "cancelled",
                json!({
                    "node": node_id,
                    "phase": phase,
                }),
            );
            bail!("arc cancelled");
        }
        Ok(())
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
        self.emit_arc_system_event(
            crate::system_events::types::SystemEventKind::WorkflowArcStarted,
            json!({
                "arc_id": self.ctx.meta.arc_id,
                "workflow": self.compiled.spec.name,
                "version": self.compiled.spec.version,
            }),
        )
        .await;
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
        self.emit_arc_system_event(
            crate::system_events::types::SystemEventKind::WorkflowArcCompleted,
            json!({"arc_id": self.ctx.meta.arc_id, "steps": steps}),
        )
        .await;
        self.update_arc_snapshot("completed", "(end)", None);
        Ok(())
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
                let existing_task_id = if actor.durable {
                    self.actor_tasks.get(&actor_name).cloned()
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
                        existing_task_id.as_deref(),
                        self.runtime_for_actor(actor),
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
                let existing_tasks = if actor.durable {
                    self.ensemble_tasks
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
                        &existing_tasks,
                        self.runtime_for_actor(actor),
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
                        let inner = task.inner.lock();
                        let session_id = inner.session_id.clone();
                        let task_id = inner.id.clone();
                        (member, completed, output, session_id, task_id)
                    });
                }
                let mut outs: Vec<(String, String)> = Vec::new();
                let mut sessions: HashMap<String, String> = HashMap::new();
                let mut task_ids: HashMap<String, String> = HashMap::new();
                let mut timed_out = false;
                while let Some(res) = joinset.join_next().await {
                    let (member, completed, output, session_id, task_id) =
                        res.map_err(|e| anyhow!("ensemble join: {e}"))?;
                    if !completed {
                        timed_out = true;
                    }
                    sessions.insert(member.clone(), session_id);
                    task_ids.insert(member.clone(), task_id);
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
                    self.ensemble_sessions.insert(actor_name.clone(), sessions);
                    self.ensemble_tasks.insert(actor_name, task_ids);
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
        let existing_tasks = if actor.durable {
            self.ensemble_tasks
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
                &existing_tasks,
                self.runtime_for_actor(actor),
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
        let mut member_tasks: HashMap<String, String> = HashMap::new();
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
            member_tasks.insert(member.clone(), task_id.clone());
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
            self.ensemble_sessions
                .insert(ensemble_key.clone(), member_sessions);
            self.ensemble_tasks.insert(ensemble_key, member_tasks);
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
        self.ensure_not_cancelled(node_id, "subworkflow_entry")?;
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
        validate_workflow_capabilities(&compiled, &self.server.state)
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
    async fn apply_node_gate(
        &mut self,
        node_id: &str,
        spec: &super::schema::NodeSpec,
    ) -> Result<()> {
        let Some(packet_id) = spec.gate.as_deref() else {
            return Ok(());
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
                        bail!("gate '{packet_id}' on node '{node_id}' errored: {e}");
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
                        bail!("gate '{packet_id}' on node '{node_id}' errored: {e}");
                    }
                }
            }
        }
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

    /// Emit a workflow arc system event. Observation-only: emit failures are
    /// logged with tracing::warn and never propagate to the calling arc.
    async fn emit_arc_system_event(
        &self,
        kind: crate::system_events::types::SystemEventKind,
        payload: Value,
    ) {
        let arc_id = self.ctx.meta.arc_id.clone();
        let mut correlation = serde_json::Map::new();
        correlation.insert("arc_id".into(), serde_json::json!(arc_id));
        let draft = crate::system_events::SystemEventDraft {
            kind,
            producer: "workflow.engine".to_string(),
            project: None,
            principal: None,
            subject: None,
            correlation,
            causation_id: None,
            payload,
        };
        if let Err(e) = self.server.state.system_events.emit(draft).await {
            tracing::warn!("workflow arc system event emit failed: {e:#}");
        }
    }
}

mod actor_nodes;
mod arc_state;
mod fanout;
mod hooks;
mod node_dispatch;
mod provider_events;
#[cfg(test)]
mod tests;
mod wait_nodes;
