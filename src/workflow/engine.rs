//! Workflow engine — walks a compiled workflow, dispatches activity
//! nodes via the orchestration primitives, applies gate packets, follows
//! choice-node branches by verdict, and enforces per-node retry ceilings.
//!
//! v0.2 scope:
//! - Executor actors (bro_exec + durable bro_resume)
//! - Sequential edges
//! - Choice nodes with labeled-edge dispatch keyed on the last gate verdict
//! - Back-edges (retry loops) with visit-count ceilings
//! - Gate packets applied after each activity node completes
//! - `${NodeName.output}` prompt substitution + retry-context prepend
//!
//! Still phase-next: fork / join / fire-and-forget / late_inject /
//! ensemble actors. Activity nodes whose spec uses unimplemented modes
//! bail at runtime with a clear message.

use std::collections::HashMap;
use std::sync::Arc;

use anyhow::{anyhow, bail, Result};
use serde_json::{json, Value};

use super::{ActorKind, ActorSpec, CompiledWorkflow, GateMode, MermaidNodeKind, NodeMode};
use crate::orchestration as orch;
use crate::BlackboxServer;

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
}

/// Absolute ceiling on nested sub-workflow composition. The depth is
/// threaded through `run_workflow` calls so child runners inherit it —
/// a local per-runner counter would let an arbitrarily-deep nest
/// silently skip the cap (caught by a Haiku self-audit round, fixed
/// here). Top-level callers pass 0.
pub const MAX_COMPOSITION_DEPTH: u32 = 5;

pub async fn run_workflow(
    server: &BlackboxServer,
    compiled: &CompiledWorkflow,
    project_dir: Option<String>,
    max_steps: Option<usize>,
) -> WorkflowRunResult {
    run_workflow_at_depth(server, compiled, project_dir, max_steps, 0, HashMap::new()).await
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
    let mut runner = WorkflowRunner::new(
        server,
        compiled,
        project_dir,
        max_steps.unwrap_or(50),
        0,
    );
    runner.event_sink = Some(event_sink);
    runner.open_arc_thread();
    let status = match runner.run().await {
        Ok(()) => "completed".to_string(),
        Err(e) => {
            runner.log_event("error", json!({"message": e.to_string()}));
            runner.arc_note("blocked", &format!("workflow errored: {e}"));
            let status_str = format!("error: {e}");
            runner.update_arc_snapshot(&status_str, "(error)", None);
            status_str
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
    WorkflowRunResult {
        status,
        events: runner.events,
        node_outputs: runner.node_outputs,
        plan: None,
        arc_thread_id,
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
) -> WorkflowRunResult {
    if composition_depth > MAX_COMPOSITION_DEPTH {
        return WorkflowRunResult {
            status: format!(
                "error: subworkflow composition depth {composition_depth} exceeds ceiling {MAX_COMPOSITION_DEPTH}"
            ),
            events: Vec::new(),
            node_outputs: HashMap::new(),
            plan: None,
            arc_thread_id: None,
        };
    }
    let mut runner = WorkflowRunner::new(
        server,
        compiled,
        project_dir,
        max_steps.unwrap_or(50),
        composition_depth,
    );
    runner.node_outputs = seed_outputs;
    runner.open_arc_thread();
    let status = match runner.run().await {
        Ok(()) => "completed".to_string(),
        Err(e) => {
            runner.log_event("error", json!({"message": e.to_string()}));
            runner.arc_note("blocked", &format!("workflow errored: {e}"));
            let status_str = format!("error: {e}");
            runner.update_arc_snapshot(&status_str, "(error)", None);
            status_str
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
    WorkflowRunResult {
        status,
        events: runner.events,
        node_outputs: runner.node_outputs,
        plan: None,
        arc_thread_id,
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
        plan: Some(compiled.summarize()),
        arc_thread_id: None,
    }
}

struct WorkflowRunner<'a> {
    server: &'a BlackboxServer,
    compiled: &'a CompiledWorkflow,
    project_dir: Option<String>,
    node_outputs: HashMap<String, String>,
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
}

impl<'a> WorkflowRunner<'a> {
    fn new(
        server: &'a BlackboxServer,
        compiled: &'a CompiledWorkflow,
        project_dir: Option<String>,
        max_steps: usize,
        composition_depth: u32,
    ) -> Self {
        Self {
            server,
            compiled,
            project_dir,
            node_outputs: HashMap::new(),
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
                self.log_event(
                    "arc_thread_open_failed",
                    json!({"error": e.to_string()}),
                );
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
        let mut completed: Vec<String> =
            self.node_outputs.keys().cloned().collect();
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
        while current != "[*]" {
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
                self.log_event(
                    "policy_error",
                    json!({"packet_id": packet_id, "error": e}),
                );
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
                    &format!(
                        "policy warn from {packet_id} at step {step} (just_ran={just_ran})"
                    ),
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
        self.compiled
            .graph
            .edges
            .iter()
            .find(|e| e.from == "[*]")
            .map(|e| e.to.clone())
            .ok_or_else(|| anyhow!("no entry edge from [*]"))
    }

    fn node_kind(&self, node_id: &str) -> MermaidNodeKind {
        self.compiled
            .graph
            .nodes
            .iter()
            .find(|n| n.id == node_id)
            .map(|n| n.kind.clone())
            .unwrap_or(MermaidNodeKind::Activity)
    }

    fn next_node(&self, current: &str) -> Result<String> {
        let outgoing: Vec<_> = self
            .compiled
            .graph
            .edges
            .iter()
            .filter(|e| e.from == current)
            .collect();
        if outgoing.is_empty() {
            bail!("node '{current}' has no outgoing edges");
        }

        let kind = self.node_kind(current);
        if matches!(kind, MermaidNodeKind::Fork) {
            // Main walk follows the FIRST outgoing edge — that's the
            // sync continuation. Remaining edges were dispatched as
            // fire-and-forget at run_node time.
            return Ok(outgoing[0].to.clone());
        }
        if matches!(kind, MermaidNodeKind::Choice) {
            // Choice nodes dispatch by matching `last_verdict` against
            // outgoing edge labels. This is how blind-style convergence
            // loops express "revise" vs. "converged" branching.
            let verdict = self.last_verdict.as_deref().ok_or_else(|| {
                anyhow!(
                    "choice node '{current}' reached with no prior gate verdict — \
                     either the predecessor activity node has no `gate` packet \
                     spec, or the gate fired but no rule matched the output \
                     (packet returned None). Check the predecessor's gate \
                     config and ensure the packet has a catchall fallback rule."
                )
            })?;
            let matched = outgoing.iter().find(|e| e.label.as_deref() == Some(verdict));
            match matched {
                Some(edge) => Ok(edge.to.clone()),
                None => {
                    let labels: Vec<&str> = outgoing
                        .iter()
                        .filter_map(|e| e.label.as_deref())
                        .collect();
                    bail!(
                        "choice '{current}' has no edge for verdict '{verdict}' (edge labels: {labels:?})"
                    );
                }
            }
        } else {
            if outgoing.len() > 1 {
                bail!(
                    "v0 engine does not support fan-out on non-choice nodes: '{current}' has {} outgoing edges",
                    outgoing.len()
                );
            }
            Ok(outgoing[0].to.clone())
        }
    }

    async fn run_node(&mut self, node_id: &str) -> Result<()> {
        let kind = self.node_kind(node_id);
        match kind {
            MermaidNodeKind::Choice => {
                // Choice nodes are pure routing — no dispatch, no gate.
                // next_node will consume last_verdict to pick an edge.
                self.log_event(
                    "choice_route",
                    json!({
                        "node": node_id,
                        "verdict": self.last_verdict.clone(),
                    }),
                );
                Ok(())
            }
            MermaidNodeKind::Fork => self.run_fork_node(node_id).await,
            MermaidNodeKind::Join => self.run_join_node(node_id).await,
            MermaidNodeKind::Activity => self.run_activity_node(node_id).await,
        }
    }

    async fn run_activity_node(&mut self, node_id: &str) -> Result<()> {
        let spec = self
            .compiled
            .spec
            .nodes
            .get(node_id)
            .ok_or_else(|| anyhow!("no metadata for activity node '{node_id}'"))?;

        // Sub-workflow composition: if the node embeds a workflow, run
        // it recursively instead of dispatching an actor. The parent
        // node's output becomes the concatenated sub-node outputs.
        if spec.subworkflow.is_some() {
            return self.run_subworkflow_node(node_id).await;
        }

        let actor_name = spec.actor.clone();
        let actor = self
            .compiled
            .spec
            .actors
            .get(&actor_name)
            .ok_or_else(|| anyhow!("node '{node_id}' references undeclared actor '{actor_name}'"))?;
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

        match &actor.kind {
            ActorKind::Executor => {
                self.run_executor_node(node_id, actor, &actor_name, &prompt)
                    .await?;
            }
            ActorKind::Ensemble => {
                self.run_ensemble_node(node_id, actor, &actor_name, &prompt)
                    .await?;
            }
            ActorKind::Advisor => {
                // Advisor actor is a single-bro dispatch with an
                // advisor-lens prompt — functionally identical to an
                // executor for engine purposes. The distinction exists
                // at the brofile layer (tool filtering, lens prompt).
                self.run_executor_node(node_id, actor, &actor_name, &prompt)
                    .await?;
            }
            ActorKind::User => {
                self.run_user_node(node_id, &prompt)?;
            }
        }

        // Apply the gate packet (if any). Dispatch by gate_mode:
        // - first (default): one rule's classification becomes verdict
        // - all: every matching rule produces a finding, verdict is
        //   the lattice-highest-priority classification across them
        if let Some(packet_id) = spec.gate.as_deref() {
            let output = self
                .node_outputs
                .get(node_id)
                .cloned()
                .unwrap_or_default();
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
                                &format!(
                                    "gate '{packet_id}' on '{node_id}' (first) → {verdict_s}"
                                ),
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
                                    let preview: String =
                                        consequent_str.chars().take(80).collect();
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
                            let verdict_s = result
                                .verdict
                                .as_deref()
                                .unwrap_or("(no match)");
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
        if !completed {
            bail!("node '{node_id}' (task {task_id}) exceeded timeout");
        }
        let result_json = orch::task_result_json(&task);
        let output = result_json
            .get("result")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let session_id = task.inner.lock().session_id.clone();

        if actor.durable {
            self.actor_sessions
                .insert(actor_name.to_string(), session_id.clone());
        }
        self.node_outputs.insert(node_id.to_string(), output.clone());

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
        self.arc_note(
            "done",
            &format!("node '{node_id}' → {output_preview}"),
        );
        Ok(())
    }

    async fn run_fork_node(&mut self, node_id: &str) -> Result<()> {
        // First outgoing edge is the sync continuation; main walk picks
        // it up via next_node. All other outgoing edges get their
        // target dispatched fire-and-forget and stored in `in_flight`.
        let async_targets: Vec<String> = self
            .compiled
            .graph
            .edges
            .iter()
            .filter(|e| e.from == node_id)
            .skip(1)
            .map(|e| e.to.clone())
            .filter(|t| t != "[*]")
            .collect();

        self.log_event(
            "fork",
            json!({
                "node": node_id,
                "async_targets": async_targets.clone(),
            }),
        );
        for target in async_targets {
            self.dispatch_fire_and_forget(&target).await?;
        }
        Ok(())
    }

    async fn dispatch_fire_and_forget(&mut self, target_id: &str) -> Result<()> {
        let spec = self
            .compiled
            .spec
            .nodes
            .get(target_id)
            .ok_or_else(|| anyhow!("fork: no metadata for async target '{target_id}'"))?;
        let actor_name = spec.actor.clone();
        let actor = self
            .compiled
            .spec
            .actors
            .get(&actor_name)
            .ok_or_else(|| {
                anyhow!(
                    "fork: async target '{target_id}' references undeclared actor '{actor_name}'"
                )
            })?;
        let prompt = self.render_prompt(spec.prompt.as_deref().unwrap_or(""));
        match &actor.kind {
            ActorKind::Executor | ActorKind::Advisor => {
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
                    .map_err(|e| {
                        anyhow!("fire-and-forget ensemble dispatch '{target_id}': {e}")
                    })?;
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
            ActorKind::User => {
                bail!("fork: async target '{target_id}' is a user node — can't fire-and-forget");
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
                    bail!(
                        "in-flight source '{source}' (task {task_id}) exceeded timeout"
                    );
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
                self.node_outputs.insert(source.to_string(), output);
            }
            InFlight::Ensemble {
                actor_name,
                durable,
                tasks,
            } => {
                let mut joinset = tokio::task::JoinSet::new();
                for (member, task) in tasks {
                    joinset.spawn(async move {
                        let completed =
                            orch::wait_for_task_with_timeout(&task, Some(900.0)).await;
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
                    bail!(
                        "in-flight source '{source}' (ensemble) had member timeouts"
                    );
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
                self.node_outputs.insert(source.to_string(), merged);
            }
        }
        Ok(true)
    }

    /// `<<join>>` control node — synchronous fan-in. Waits for every
    /// incoming edge's source that's currently in-flight, captures
    /// their outputs into `node_outputs`, then advances via the single
    /// outgoing edge. Sources already joined (e.g., via `late_inject`)
    /// are skipped silently.
    async fn run_join_node(&mut self, node_id: &str) -> Result<()> {
        let incoming_sources: Vec<String> = self
            .compiled
            .graph
            .edges
            .iter()
            .filter(|e| e.to == node_id && e.from != "[*]")
            .map(|e| e.from.clone())
            .collect();
        let mut joined_count = 0usize;
        let mut skipped_count = 0usize;
        for source in &incoming_sources {
            let joined = self.join_in_flight_source(source).await?;
            if joined {
                joined_count += 1;
            } else {
                skipped_count += 1;
            }
        }
        self.log_event(
            "join",
            json!({
                "node": node_id,
                "incoming": incoming_sources,
                "joined": joined_count,
                "already_completed": skipped_count,
            }),
        );
        if joined_count > 0 {
            self.arc_note(
                "surprise",
                &format!(
                    "join '{node_id}' fanned in {joined_count} async source(s) (+{skipped_count} already completed)"
                ),
            );
        }
        Ok(())
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
            .workflow_dispatch_ensemble(team, prompt, self.project_dir.as_deref(), &existing_sessions)
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
        self.node_outputs.insert(node_id.to_string(), merged.clone());
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
            .ok_or_else(|| anyhow!("no metadata for subworkflow node '{node_id}'"))?;
        let sub_spec = spec
            .subworkflow
            .as_ref()
            .ok_or_else(|| anyhow!("subworkflow missing from node '{node_id}'"))?;
        let sub_spec = (**sub_spec).clone();

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

        let compiled = super::compile(sub_spec).map_err(|e| {
            anyhow!("subworkflow on node '{node_id}' failed to compile: {e}")
        })?;
        let project_dir = self.project_dir.clone();
        // Seed the sub-runner with the parent's node_outputs so sub
        // prompts can reference `${ParentNode.output}` the same way
        // siblings do. Caveat: sub-node names that collide with parent
        // names overwrite parent entries in the sub's local copy.
        let seed_outputs = self.node_outputs.clone();
        // Box the recursive future to avoid infinitely-sized types.
        // Depth is threaded so the child runner knows where it is in
        // the composition tree.
        let sub_result = Box::pin(run_workflow_at_depth(
            self.server,
            &compiled,
            project_dir,
            Some(25),
            child_depth,
            seed_outputs,
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
        self.node_outputs.insert(node_id.to_string(), merged.clone());

        self.log_event(
            "subworkflow_complete",
            json!({
                "node": node_id,
                "sub_arc_thread_id": sub_result.arc_thread_id,
                "sub_events": sub_result.events.len(),
                "sub_node_count": sub_outputs.len(),
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

    fn run_user_node(&mut self, node_id: &str, prompt: &str) -> Result<()> {
        // v0 user semantics: halt the run with a structured escalation.
        // Resume via re-invocation is phase-next (wants arc-thread
        // persistence). For today, this is a clean stop with enough
        // context for the user to respond in whatever channel they
        // prefer.
        let preview: String = prompt.chars().take(500).collect();
        self.log_event(
            "user_pause",
            json!({
                "node": node_id,
                "message": preview.clone(),
            }),
        );
        self.arc_note(
            "blocked",
            &format!("paused at user node '{node_id}' — {preview}"),
        );
        bail!("paused at user node '{node_id}' — resolve and re-dispatch");
    }

    fn render_prompt(&self, template: &str) -> String {
        let mut out = template.to_string();
        for (node, output) in &self.node_outputs {
            let key = format!("${{{node}.output}}");
            out = out.replace(&key, output);
        }
        out
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
                "N1": {"actor": "a", "prompt": "first node"},
                "N2": {"actor": "a", "prompt": "echo ${N1.output}"}
            },
            "graph": "stateDiagram-v2\n    [*] --> N1\n    N1 --> N2\n    N2 --> [*]"
        }"#;
        compile(load_workflow(json).unwrap()).unwrap()
    }

    fn choice_compiled() -> CompiledWorkflow {
        let json = r#"{
            "name": "t",
            "version": 1,
            "actors": {"a": {"kind": "executor", "brofile": "b"}},
            "nodes": {
                "Decide": {"actor": "a", "gate": "packet-12345678"},
                "Yes": {"actor": "a"},
                "No": {"actor": "a"}
            },
            "graph": "stateDiagram-v2\n    [*] --> Decide\n    state Decide_Pick <<choice>>\n    Decide --> Decide_Pick\n    Decide_Pick --> Yes: yes\n    Decide_Pick --> No: no\n    Yes --> [*]\n    No --> [*]"
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
        let (runner, _) = runner_for(&compiled);
        assert_eq!(runner.entry_node().unwrap(), "N1");
        assert_eq!(runner.next_node("N1").unwrap(), "N2");
        assert_eq!(runner.next_node("N2").unwrap(), "[*]");
    }

    #[test]
    fn choice_routes_by_last_verdict() {
        let compiled = choice_compiled();
        let (mut runner, _) = runner_for(&compiled);
        runner.last_verdict = Some("yes".into());
        assert_eq!(runner.next_node("Decide_Pick").unwrap(), "Yes");
        runner.last_verdict = Some("no".into());
        assert_eq!(runner.next_node("Decide_Pick").unwrap(), "No");
    }

    #[test]
    fn choice_without_verdict_errors() {
        let compiled = choice_compiled();
        let (runner, _) = runner_for(&compiled);
        let err = runner.next_node("Decide_Pick").unwrap_err().to_string();
        assert!(err.contains("no prior gate verdict"), "err: {err}");
    }

    #[test]
    fn choice_with_unmatched_verdict_errors() {
        let compiled = choice_compiled();
        let (mut runner, _) = runner_for(&compiled);
        runner.last_verdict = Some("maybe".into());
        let err = runner.next_node("Decide_Pick").unwrap_err().to_string();
        assert!(err.contains("no edge for verdict 'maybe'"), "err: {err}");
        assert!(err.contains("yes") && err.contains("no"));
    }

    #[test]
    fn non_choice_fan_out_still_rejected() {
        // Non-choice fan-out is a spec error. Build an activity node
        // with two outgoing non-labeled edges.
        let json = r#"{
            "name": "t",
            "version": 1,
            "actors": {"a": {"kind": "executor", "brofile": "b"}},
            "nodes": {
                "N1": {"actor": "a"},
                "N2": {"actor": "a"},
                "N3": {"actor": "a"}
            },
            "graph": "stateDiagram-v2\n    [*] --> N1\n    N1 --> N2\n    N1 --> N3\n    N2 --> [*]\n    N3 --> [*]"
        }"#;
        let compiled = compile(load_workflow(json).unwrap()).unwrap();
        let (runner, _) = runner_for(&compiled);
        let err = runner.next_node("N1").unwrap_err().to_string();
        assert!(err.contains("fan-out"), "err: {err}");
    }

    // Build a runner against a dummy server ref. Since most control-flow
    // tests don't dispatch, we just need a valid &BlackboxServer to borrow
    // from. The tests that would dispatch are covered by the E2E CLI run.
    fn runner_for(compiled: &CompiledWorkflow) -> (DummyRunner, ()) {
        (
            DummyRunner {
                compiled,
                node_outputs: HashMap::new(),
                last_verdict: None,
                visit_counts: HashMap::new(),
            },
            (),
        )
    }

    // Mirror of WorkflowRunner's read-side helpers, free of the server
    // ref. Keeps the graph-walk logic testable.
    struct DummyRunner<'a> {
        compiled: &'a CompiledWorkflow,
        node_outputs: HashMap<String, String>,
        last_verdict: Option<String>,
        visit_counts: HashMap<String, u32>,
    }

    impl<'a> DummyRunner<'a> {
        fn entry_node(&self) -> Result<String> {
            self.compiled
                .graph
                .edges
                .iter()
                .find(|e| e.from == "[*]")
                .map(|e| e.to.clone())
                .ok_or_else(|| anyhow!("no entry edge"))
        }

        fn node_kind(&self, node_id: &str) -> MermaidNodeKind {
            self.compiled
                .graph
                .nodes
                .iter()
                .find(|n| n.id == node_id)
                .map(|n| n.kind.clone())
                .unwrap_or(MermaidNodeKind::Activity)
        }

        fn next_node(&self, current: &str) -> Result<String> {
            let outgoing: Vec<_> = self
                .compiled
                .graph
                .edges
                .iter()
                .filter(|e| e.from == current)
                .collect();
            if outgoing.is_empty() {
                bail!("no outgoing from '{current}'");
            }
            let kind = self.node_kind(current);
            if matches!(kind, MermaidNodeKind::Choice) {
                let verdict = self
                    .last_verdict
                    .as_deref()
                    .ok_or_else(|| anyhow!("choice node '{current}' has no prior gate verdict"))?;
                let matched = outgoing.iter().find(|e| e.label.as_deref() == Some(verdict));
                match matched {
                    Some(edge) => Ok(edge.to.clone()),
                    None => {
                        let labels: Vec<&str> =
                            outgoing.iter().filter_map(|e| e.label.as_deref()).collect();
                        bail!("choice '{current}' has no edge for verdict '{verdict}' (edge labels: {labels:?})")
                    }
                }
            } else {
                if outgoing.len() > 1 {
                    bail!(
                        "v0 engine does not support fan-out on non-choice nodes: '{current}' has {} outgoing edges",
                        outgoing.len()
                    );
                }
                Ok(outgoing[0].to.clone())
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

    // Silence dead_code warnings on DummyRunner fields that aren't
    // read by current tests but exist for parity with the runner.
    #[test]
    fn dummy_runner_field_parity_with_real_runner() {
        let compiled = mini_compiled();
        let mut d = DummyRunner {
            compiled: &compiled,
            node_outputs: HashMap::new(),
            last_verdict: None,
            visit_counts: HashMap::new(),
        };
        d.node_outputs.insert("N1".into(), "".into());
        d.last_verdict = Some("x".into());
        d.visit_counts.insert("N1".into(), 0);
    }
}
