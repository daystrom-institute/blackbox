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
    ActorSpec, CompiledWorkflow, ForeachSpec, GateMode, ItemFailurePolicy, MatrixSpec,
    NodeTransition, Workflow,
};
use crate::orchestration as orch;
use crate::server::state::BlackboxServer;
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
    let run_result = runner.run().await;
    finish_arc_run(runner, run_result).await
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
    let run_result = runner.run().await;
    finish_arc_run(runner, run_result).await
}

/// Shared terminal epilogue for every arc-runner path (top-level,
/// nested, checkpoint-resume): classify the run result, fire arc-exit
/// hooks, reconcile the peek snapshot, release the cancel token, drop
/// the durable checkpoint, and assemble the WorkflowRunResult.
async fn finish_arc_run(
    mut runner: WorkflowRunner<'_>,
    run_result: Result<()>,
) -> WorkflowRunResult {
    let mut status = match run_result {
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
    runner
        .server
        .unregister_arc_cancel_token(&runner.ctx.meta.arc_id);
    // Terminal state: drop the durable checkpoint (no-op for sub-arcs).
    runner.remove_checkpoint().await;
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
    /// Steps consumed so far against `max_steps`. A field (not a run()
    /// local) so Wait-node checkpoints can persist the budget position.
    steps: usize,
    arc_thread_id: Option<String>,
    /// Set by checkpoint resume when re-entering a Wait node whose
    /// on_enter hooks already ran before the daemon restart. Consumed
    /// (cleared) by the first `run_activity_node` that matches it.
    resume_skip_on_enter: Option<String>,
    /// Absolute deadline restored from a Waiting checkpoint. Consumed by
    /// the re-entered wait node so it opens the REMAINING window rather
    /// than restarting the full configured timeout.
    resume_wait_deadline: Option<String>,
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
            shell_allowlist: compiled.spec.shell_allowlist.clone(),
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
            steps: 0,
            arc_thread_id: None,
            resume_skip_on_enter: None,
            resume_wait_deadline: None,
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
        self.run_from(entry).await
    }

    /// The node-walk loop, entered at `start_node` with `self.steps`
    /// already positioned (0 for fresh arcs; the checkpointed budget
    /// position on rehydration resume).
    async fn run_from(&mut self, start_node: String) -> Result<()> {
        let mut current = start_node;
        // Durable position before the first body runs: a crash inside
        // a node body rehydrates as `interrupted` at that node instead
        // of vanishing. SKIPPED when resuming into a parked wait node -
        // overwriting the safe Waiting checkpoint with Running before
        // the wait re-registers would turn a second crash in that
        // window into an interruption of a perfectly resumable arc.
        if self.resume_skip_on_enter.as_deref() != Some(current.as_str()) {
            self.write_checkpoint(
                super::arc_store::ArcCheckpointStatus::Running,
                &current,
            )
            .await;
        }
        while current != TERMINAL_SENTINEL {
            if self.cancel_token.is_cancelled() {
                self.log_event(
                    "cancelled",
                    json!({"steps": self.steps, "next_was": current.clone()}),
                );
                bail!("arc cancelled");
            }
            if self.steps >= self.max_steps {
                bail!("workflow exceeded max_steps ({})", self.max_steps);
            }
            self.steps += 1;
            self.run_node(&current).await?;
            let next = self.next_node(&current)?;
            let steps = self.steps;
            // Compaction anchor — emit a rolling summary after each
            // boundary so an observer can reconstruct arc state
            // without reading every per-node event.
            self.write_compaction_anchor(steps, &current, &next);
            // Update the in-flight arc snapshot for /orchestrate/peek.
            self.update_arc_snapshot("running", &current, Some(&next));
            // Durable boundary: the completed node's effects (outputs,
            // vars, signals) are on disk before the next body runs.
            if next != TERMINAL_SENTINEL {
                self.write_checkpoint(
                    super::arc_store::ArcCheckpointStatus::Running,
                    &next,
                )
                .await;
            }
            // Arc-level policy packet: advisor-as-packet, evaluates
            // arc state mechanically. Halt/escalate/warn verdicts act
            // on the arc without needing an LLM advisor round.
            self.apply_policy_packet(steps, &current, &next)?;
            current = next;
        }
        self.log_event("complete", json!({"steps": self.steps}));
        self.emit_arc_system_event(
            crate::system_events::types::SystemEventKind::WorkflowArcCompleted,
            json!({"arc_id": self.ctx.meta.arc_id, "steps": self.steps}),
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
mod async_join;
mod ensemble_nodes;
mod fanout;
mod hooks;
mod node_dispatch;
mod provider_events;
mod rehydrate;
mod subworkflow_nodes;
#[cfg(test)]
mod tests;
mod wait_nodes;

pub(crate) use rehydrate::rehydrate_arcs;
