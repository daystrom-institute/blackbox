use std::sync::Arc;

use crate::crons;
use crate::orchestration as orch;
use crate::orchestration::providers::Provider;
use crate::pollers;
use crate::server::BlackboxServer;
use crate::server::routes::{SignalDispatchOrigin, signal_arc_dispatch, webhook_replay_inner};
use crate::server::state::{SIGNAL_LOG_CAP, SignalEvent, WEBHOOK_LOG_CAP, WebhookDelivery};
use crate::server::workflow_capabilities::validate_workflow_capabilities;
use crate::system_memory;
use crate::tools::bro_helpers::extract_and_compile_workflow;
use crate::tools::bro_runtime_params::{
    ArcCancelParams, ArcResultParams, ArcSignalParams, ArcStatusParams, CronInstallParams,
    CronRemoveParams, CronUpcomingParams, OrchestrateAuthorParams, OrchestrateRunParams,
    PollerInstallParams, PollerRemoveParams, SignalsParams, WebhookDeliveriesParams,
    WebhookInstallParams, WebhookRemoveParams, WebhookReplayParams, WorkflowInstallParams,
    WorkflowRemoveParams,
};
use crate::webhooks;
use crate::workflow;
use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::CallToolResult;
use rmcp::{tool, tool_router};
use serde_json::Value;

pub(crate) fn router() -> ToolRouter<BlackboxServer> {
    BlackboxServer::orchestrate_tools()
}

#[tool_router(router = orchestrate_tools)]
impl BlackboxServer {
    #[tool(
        name = "bro_orchestrate_author",
        description = "Compile a prose charter into a validated workflow spec. Dispatches an authoring LLM with the sm-workflow-orchestration runbook + a minimal reference example, parses its JSON response, cross-validates via the engine's compile step, retries once on compile failure with the error appended, and returns the validated spec — ready to pass to `bro_orchestrate_run`. Closes the authoring loop: operators describe the arc in prose, get a JSON spec back (with per-node `next` transitions), dispatch without hand-writing the graph."
    )]
    pub(crate) async fn bro_orchestrate_author(
        &self,
        Parameters(p): Parameters<OrchestrateAuthorParams>,
    ) -> CallToolResult {
        // Caller-supplied few-shot corpus: exemplars teach the house
        // grammar, the preamble carries domain ground truth. The budget
        // bounds the FULLY RENDERED sections (bodies + framing labels +
        // preamble), not just exemplar bytes - otherwise a thousand
        // tiny exemplars or a fat preamble walks around the cap while
        // still blowing the authoring context.
        const AUTHORING_INPUT_BUDGET_BYTES: usize = 64 * 1024;
        const MAX_EXEMPLARS: usize = 16;
        let exemplars = p.exemplars.unwrap_or_default();
        if exemplars.len() > MAX_EXEMPLARS {
            return Self::err_text(&format!(
                "{} exemplars passed; the cap is {MAX_EXEMPLARS} — pick the most representative ones",
                exemplars.len()
            ));
        }
        let exemplar_section = if exemplars.is_empty() {
            String::new()
        } else {
            let joined = exemplars
                .iter()
                .enumerate()
                .map(|(i, e)| format!("--- exemplar {} ---\n{e}", i + 1))
                .collect::<Vec<_>>()
                .join("\n\n");
            format!(
                "\n=== CALLER EXEMPLARS (prefer their idioms over the generic example) ===\n{joined}\n"
            )
        };
        let preamble_section = p
            .preamble
            .as_deref()
            .map(|d| format!("\n=== DOMAIN PREAMBLE (treat as ground truth) ===\n{d}\n"))
            .unwrap_or_default();
        let rendered_input_bytes = exemplar_section
            .len()
            .saturating_add(preamble_section.len());
        if rendered_input_bytes > AUTHORING_INPUT_BUDGET_BYTES {
            return Self::err_text(&format!(
                "exemplars + preamble render to {rendered_input_bytes} bytes, over the {AUTHORING_INPUT_BUDGET_BYTES}-byte budget — trim them"
            ));
        }

        // Load the runbook + a reference example.
        let runbook = match system_memory::get("sm-workflow-orchestration") {
            Some(sm) => sm.content.as_str(),
            None => {
                return Self::err_text(
                    "sm-workflow-orchestration runbook not found — internal error",
                );
            }
        };
        let reference_example = include_str!("../../examples/workflows/e2e-gated.json");
        let hint_line = p
            .hint
            .as_deref()
            .map(|h| format!("\nShape hint: match the `{h}` pattern from the runbook if it fits the charter.\n"))
            .unwrap_or_default();

        let base_prompt = format!(
            "You are a workflow spec compiler. Convert a prose charter into a validated workflow JSON spec for the blackbox `bro_orchestrate_run` engine.\n\n\
=== REFERENCE RUNBOOK ===\n{runbook}\n\n\
=== REFERENCE EXAMPLE (e2e-gated.json) ===\n{reference_example}\n{exemplar_section}{preamble_section}\n\
=== CHARTER ===\n{charter}\n{hint_line}\n\
=== OUTPUT INSTRUCTIONS ===\n\
Output ONLY the JSON workflow spec — no preamble, no prose explanation, no trailing commentary. Start with `{{` and end with `}}`. You may wrap in ```json fences; the parser handles both.\n\n\
Constraints:\n\
- Use actor kinds only from {{executor, ensemble}}. Persona / role / contract (advisor, triager, planner, facilitator, specialist, …) is the brofile lens + prompt + on_exit `parse_json` validator — not an engine type.\n\
- Cross-reference every `actor` field in nodes to a declared actor name.\n\
- Every activity node in the graph must have a matching entry in `nodes`.\n\
- Every `nodes` entry (except ones with `subworkflow`) needs an `actor`.\n\
- Top-level `start` names the entry node; every node carries a `next` clause whose `type` is one of `goto` / `branch` / `fork` / `terminal`. There is no `graph` string.\n\
- If you reference a gate or policy packet ID, use a placeholder like `packet-TODO` — the operator will fill it in after compilation.\n\
- Do NOT invent new actor kinds or graph primitives.\n",
            charter = p.charter,
        );

        let first_task = match self
            .workflow_dispatch_executor(
                &p.brofile,
                &base_prompt,
                p.project_dir.as_deref(),
                None,
                None,
                None,
                &[],
            )
            .await
        {
            Ok(t) => t,
            Err(e) => return Self::err_text(&format!("authoring dispatch failed: {e}")),
        };
        let completed = orch::wait_for_task_with_timeout(&first_task, Some(600.0)).await;
        if !completed {
            return Self::err_text("authoring dispatch timed out");
        }
        let first_output = orch::task_result_json(&first_task)
            .get("result")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let first_session_id = first_task.inner.lock().session_id.clone();
        let first_task_id = first_task.inner.lock().id.clone();

        // Try to compile. If it fails, retry once with the error.
        match extract_and_compile_workflow(&first_output) {
            Ok(spec) => Self::ok_json(&serde_json::json!({
                "workflow": spec,
                "attempts": 1,
                "author_session_id": first_session_id,
            })),
            Err(first_err) => {
                let retry_prompt = format!(
                    "Your previous spec failed validation with this error:\n\n{first_err}\n\nRevise and output the corrected JSON spec. Same output rules — no preamble, no trailing prose."
                );
                // Resume the same session so the LLM sees its prior output.
                let retry_task = match self
                    .workflow_dispatch_executor(
                        &p.brofile,
                        &retry_prompt,
                        p.project_dir.as_deref(),
                        Some(&first_session_id),
                        Some(&first_task_id),
                        None,
                        &[],
                    )
                    .await
                {
                    Ok(t) => t,
                    Err(e) => {
                        return Self::err_text(&format!(
                            "authoring retry dispatch failed: {e}; first error: {first_err}"
                        ));
                    }
                };
                let retry_completed =
                    orch::wait_for_task_with_timeout(&retry_task, Some(600.0)).await;
                if !retry_completed {
                    return Self::err_text(&format!(
                        "authoring retry timed out; first error: {first_err}"
                    ));
                }
                let retry_output = orch::task_result_json(&retry_task)
                    .get("result")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                match extract_and_compile_workflow(&retry_output) {
                    Ok(spec) => Self::ok_json(&serde_json::json!({
                        "workflow": spec,
                        "attempts": 2,
                        "author_session_id": first_session_id,
                        "first_error": first_err,
                    })),
                    Err(second_err) => Self::err_text(&format!(
                        "authoring failed after 2 attempts. First error: {first_err} | Second error: {second_err}"
                    )),
                }
            }
        }
    }

    pub(crate) fn spawn_workflow_task(
        &self,
        compiled: workflow::CompiledWorkflow,
        project_dir: Option<String>,
        max_steps: Option<usize>,
        initial_vars: serde_json::Map<String, Value>,
    ) -> (Arc<orch::Task>, String) {
        let task_id = uuid::Uuid::new_v4().to_string();
        let arc_id = format!("arc-{}", uuid::Uuid::new_v4().simple());
        let workflow_name = compiled.spec.name.clone();
        let task = orch::spawn_in_process_task(
            task_id.clone(),
            Provider::Workflow,
            arc_id.clone(),
            project_dir.clone(),
            self.state.store_dir.clone(),
            self.state.task_store.clone(),
            self.state.tail_tx.clone(),
            Some(self.state.roster_events()),
            Some(format!("workflow::{workflow_name}")),
            None,
            Some(self.state.system_events.clone()),
            // orchestrate.rs is the workflow executor's harness-task
            // spawn — same source class as the surrounding
            // workflow_runtime.rs dispatch path.
            bro_core::Origin::Workflow,
        );
        orch::push_in_process_event(
            &task,
            serde_json::json!({
                "kind": "workflow_task_started",
                "data": {
                    "workflow": workflow_name,
                    "arc_id": arc_id,
                },
                "timestamp": crate::util::now_iso(),
            }),
            &self.state.tail_tx,
        );
        let state = self.state.clone();
        let task_for_run = task.clone();
        let arc_for_run = arc_id.clone();
        tokio::spawn(async move {
            let server = BlackboxServer::new(state.clone());
            let (event_tx, mut event_rx) = tokio::sync::mpsc::unbounded_channel();
            let task_for_events = task_for_run.clone();
            let tail_for_events = state.tail_tx.clone();
            let event_forwarder = tokio::spawn(async move {
                let mut count = 0usize;
                while let Some(event) = event_rx.recv().await {
                    count += 1;
                    orch::push_in_process_event(&task_for_events, event, &tail_for_events);
                }
                count
            });
            let result = workflow::run_workflow_streaming_with_vars_and_arc_id(
                &server,
                &compiled,
                project_dir,
                max_steps,
                initial_vars,
                event_tx,
                arc_for_run.clone(),
            )
            .await;
            let streamed_count = event_forwarder.await.unwrap_or(0);
            let status = if result.status == "completed" {
                orch::TaskStatus::Completed
            } else if result.status == "cancelled" {
                orch::TaskStatus::Cancelled
            } else {
                orch::TaskStatus::Failed
            };
            if streamed_count == 0 {
                for event in &result.events {
                    orch::push_in_process_event(&task_for_run, event.clone(), &state.tail_tx);
                }
            }
            // Events live in the task event log (streamed above or replayed
            // by the fallback loop) and are readable via bro_status tail.
            // Duplicating them in the result envelope made bro_wait return
            // an ~80KB escaped blob for any nontrivial arc (gap-55be3518);
            // the envelope keeps only the structured fields.
            let mut result = result;
            result.events = Vec::new();
            let result_text = serde_json::to_string(&result).unwrap_or_else(|err| {
                serde_json::json!({
                    "status": "serialization_error",
                    "error": err.to_string()
                })
                .to_string()
            });
            let stderr = (status == orch::TaskStatus::Failed).then(|| result.status.clone());
            orch::finish_in_process_task(
                &task_for_run,
                status,
                Some(result_text),
                stderr,
                &state.task_store,
                &state.store_dir,
                &state.tail_tx,
                Some(state.system_events.clone()),
            );
        });
        (task, arc_id)
    }

    #[tool(
        name = "bro_orchestrate_run",
        description = "Dispatch a workflow as a pollable task. Takes a full spec (actors, nodes with per-node `next` transitions: goto / branch / fork / terminal) and returns {taskId, arcId, status} immediately by default; poll with bro_status(task_id=...), await with bro_wait(task_id=...), or inspect arc state with bro_arc_status(arc_id=...). Pass await_completion=true only when the caller intentionally wants blocking behavior. Pass dry_run=true to validate + summarize without dispatching any bros. Run and dry-run both validate `subworkflow_ref` seams strictly: every ref must be installed and its imports/exports must type-check against the child schema. Workflows declaring `admission` enforce at most one non-terminal arc per key; a duplicate start errors with the holding arc named."
    )]
    pub(crate) async fn bro_orchestrate_run(
        &self,
        Parameters(p): Parameters<OrchestrateRunParams>,
    ) -> CallToolResult {
        let spec: workflow::Workflow = match serde_json::from_value(p.workflow) {
            Ok(s) => s,
            Err(e) => {
                return Self::err_text(&format!("workflow parse failed: {e}"));
            }
        };
        let compiled = match workflow::compile(spec) {
            Ok(c) => c,
            Err(e) => return Self::err_text(&format!("workflow compile failed: {e}")),
        };
        // Capability validation — walk every actor's brofile/team →
        // provider and verify the actor's `requires` capabilities are
        // covered. Hard fail rather than silent route-around.
        if let Err(e) = validate_workflow_capabilities(&compiled, &self.state) {
            return Self::err_text(&format!("workflow capability validation failed: {e}"));
        }
        // Author-time seam validation, strict form: at run (and
        // dry-run) every subworkflow_ref must resolve and type-check
        // its imports/exports against the installed child's schema. A
        // dry-run that blesses an unresolvable or mistyped seam is the
        // exact gap this closes.
        if let Err(e) = workflow::validate_subworkflow_refs(
            &compiled.spec,
            &|id: &str| self.resolve_workflow_by_id(id),
            true,
        ) {
            return Self::err_text(&format!("subworkflow_ref validation failed: {e}"));
        }
        if p.dry_run.unwrap_or(false) {
            let result = workflow::engine::dry_run(&compiled);
            return Self::ok_json(&serde_json::to_value(&result).unwrap_or_default());
        }
        let initial_vars = p.initial_vars.unwrap_or_default();
        let (task, arc_id) =
            self.spawn_workflow_task(compiled, p.project_dir, p.max_steps, initial_vars);
        if p.await_completion.unwrap_or(false) {
            let completed = orch::wait_for_task_with_timeout(&task, p.timeout_seconds).await;
            let mut out = if completed {
                orch::task_result_json(&task)
            } else {
                orch::timeout_snapshot_json(&task)
            };
            out["arcId"] = Value::String(arc_id);
            return Self::ok_json(&out);
        }
        let inner = task.inner.lock();
        Self::ok_json(&serde_json::json!({
            "taskId": inner.id,
            "sessionId": inner.session_id,
            "arcId": arc_id,
            "status": "running",
            "poll": {
                "status_tool": "bro_status",
                "wait_tool": "bro_wait",
                "arc_status_tool": "bro_arc_status"
            }
        }))
    }

    #[tool(
        name = "bro_arc_signal",
        description = "Resolve a pending Wait by signal name + correlation tuple. Same dispatch path that the webhook router uses for `signal_arc` verdicts — surfaced as MCP so an operator can manually advance an arc that's blocked on an external event."
    )]
    pub(crate) async fn bro_arc_signal(
        &self,
        Parameters(p): Parameters<ArcSignalParams>,
    ) -> CallToolResult {
        let correlation = p.correlate.unwrap_or_default();
        let payload = p
            .payload
            .unwrap_or_else(|| Value::Object(correlation.clone()));
        let result = signal_arc_dispatch(
            &self.state,
            &p.signal,
            correlation,
            payload,
            SignalDispatchOrigin::Direct,
            None,
        )
        .await;
        Self::ok_json(&result)
    }

    #[tool(
        name = "bro_arc_status",
        description = "Read-only structured query against active and recently-finished arcs. Returns the current ArcSnapshot (current_node, completed_nodes, in_flight_nodes, last_verdict, visit_counts, started_at) plus pending-wait registrations for the arc."
    )]
    pub(crate) async fn bro_arc_status(
        &self,
        Parameters(p): Parameters<ArcStatusParams>,
    ) -> CallToolResult {
        if p.arc_id.is_none() {
            // Default: all running.
            let map = self.state.running_arcs.read();
            return Self::ok_json(&serde_json::json!({
                "snapshots": map.values().collect::<Vec<_>>(),
                "pending_waits": self.state.wait_store.snapshot(),
            }));
        }
        let map = self.state.running_arcs.read();
        let wanted = p.arc_id.unwrap_or_default();
        let snap = map
            .values()
            .find(|s| s.arc_id == wanted || s.arc_thread_id == wanted)
            .cloned();
        let waits = self
            .state
            .wait_store
            .snapshot()
            .into_iter()
            .filter(|w| {
                w.arc_id == wanted
                    || snap
                        .as_ref()
                        .map(|snapshot| w.arc_id == snapshot.arc_id)
                        .unwrap_or(false)
            })
            .collect::<Vec<_>>();
        Self::ok_json(&serde_json::json!({
            "snapshot": snap,
            "pending_waits": waits,
        }))
    }

    #[tool(
        name = "bro_arc_result",
        description = "Read a completed workflow arc's structured result without the event-log bulk: `structuredExit` (vars._structured_exit), final `vars` (optionally filtered by `keys`), `arcThreadId`, and `actorSessions`. Accepts the arcId from bro_orchestrate_run or the workflow task id. `include_node_outputs=true` adds per-node prose. Covers task-backed arcs (bro_orchestrate_run); webhook/SSE-ingress arcs are not task-backed."
    )]
    pub(crate) async fn bro_arc_result(
        &self,
        Parameters(p): Parameters<ArcResultParams>,
    ) -> CallToolResult {
        let task = {
            let store = self.state.task_store.read();
            store.all_tasks().into_iter().find(|t| {
                let inner = t.inner.lock();
                inner.provider == Provider::Workflow
                    && (inner.session_id == p.arc_id || inner.id == p.arc_id)
            })
        };
        let Some(task) = task else {
            return Self::err_text(&format!(
                "no workflow task found for arc/task id {:?}; only bro_orchestrate_run arcs are \
                 task-backed (webhook/SSE-ingress arcs are not). For live state use \
                 bro_arc_status; for the audit trail use bbox_notes(thread_id=<arc_thread_id>).",
                p.arc_id
            ));
        };
        let inner = task.inner.lock();
        let mut out = serde_json::json!({
            "taskId": inner.id,
            "arcId": inner.session_id,
            "status": inner.status,
        });
        if !inner.status.is_terminal() {
            out["hint"] = Value::String(
                "arc still running; result vars are available at terminal state — poll \
                 bro_status(task_id=...) or inspect live position with bro_arc_status"
                    .to_string(),
            );
            return Self::ok_json(&out);
        }
        let Some(msg) = inner.last_assistant_message.as_deref() else {
            out["hint"] =
                Value::String("task terminal but no result envelope was captured".to_string());
            return Self::ok_json(&out);
        };
        let Ok(parsed) = serde_json::from_str::<Value>(msg) else {
            out["hint"] =
                Value::String("task result is not a JSON WorkflowRunResult envelope".to_string());
            return Self::ok_json(&out);
        };
        out["workflowStatus"] = parsed.get("status").cloned().unwrap_or(Value::Null);
        out["structuredExit"] = parsed
            .get("structured_exit")
            .cloned()
            .unwrap_or(Value::Null);
        if let Some(thread) = parsed.get("arc_thread_id") {
            out["arcThreadId"] = thread.clone();
        }
        if let Some(sessions) = parsed.get("actor_sessions") {
            out["actorSessions"] = sessions.clone();
        }
        if let Some(Value::Object(vars)) = parsed.get("vars") {
            let filtered: serde_json::Map<String, Value> = match &p.keys {
                Some(keys) => vars
                    .iter()
                    .filter(|(k, _)| keys.iter().any(|w| w == *k))
                    .map(|(k, v)| (k.clone(), v.clone()))
                    .collect(),
                None => vars.clone(),
            };
            out["vars"] = Value::Object(filtered);
        }
        if p.include_node_outputs.unwrap_or(false)
            && let Some(node_outputs) = parsed.get("node_outputs")
        {
            out["nodeOutputs"] = node_outputs.clone();
        }
        Self::ok_json(&out)
    }

    #[tool(
        name = "bro_arc_cancel",
        description = "Cancel a running workflow arc by id. Trips the arc's cancellation token; the runner observes between node iterations and inside Wait suspensions, bails out with status `cancelled`, runs `on_arc_cancel` (if declared) followed by `on_arc_exit`, and writes a `blocked` note (`workflow cancelled`) on the arc's thread. Returns `{cancelled: true|false}` — false means no token registered for that arc id (already terminated, never started, or wrong id)."
    )]
    pub(crate) async fn bro_arc_cancel(
        &self,
        Parameters(p): Parameters<ArcCancelParams>,
    ) -> CallToolResult {
        let cancelled = self.state.cancel_arc(&p.arc_id);
        Self::ok_json(&serde_json::json!({
            "arc_id": p.arc_id,
            "cancelled": cancelled,
        }))
    }

    #[tool(
        name = "bro_signals",
        description = "Recent signal-dispatch events as a bounded ring buffer (last ~200). Every call to the signal router records one entry: (timestamp, signal, correlation, outcome, matched_arc_id, matched_wait_id, idle_pending). `outcome` is `matched` (resolved a wait) or `no_matching_wait` (fell idle); on idle, `idle_pending` carries the pending-with-same-signal snapshot at dispatch time so the diff between what arrived and what was waiting is one read away. Filter by `signal=` (exact match) and `since=` (ISO timestamp). Replaces the journalctl|grep workflow for debugging webhook → routing → signal → wait paths."
    )]
    pub(crate) async fn bro_signals(
        &self,
        Parameters(p): Parameters<SignalsParams>,
    ) -> CallToolResult {
        let log = self.state.signal_log.read();
        let limit = p.limit.unwrap_or(50).min(SIGNAL_LOG_CAP);
        let mut out: Vec<&SignalEvent> = log
            .iter()
            .filter(|e| match &p.signal {
                Some(s) => e.signal == *s,
                None => true,
            })
            .filter(|e| match &p.since {
                Some(ts) => e.timestamp.as_str() >= ts.as_str(),
                None => true,
            })
            .filter(|e| match &p.outcome {
                Some(o) => e.outcome == *o,
                None => true,
            })
            .collect();
        // Newest first.
        out.reverse();
        out.truncate(limit);
        Self::ok_json(&serde_json::json!({
            "events": out,
            "total_in_buffer": log.len(),
            "buffer_capacity": SIGNAL_LOG_CAP,
        }))
    }

    #[tool(
        name = "bro_webhook_replay",
        description = "Replay an arbitrary payload through an installed webhook's extractor + routing packet WITHOUT dispatching the verdict. Returns the extracted entity, the routing verdict's classification, and the resolved consequent (after `${entity.X}` substitution). Skips signature verification — same path as the HTTP `/webhook/:name/replay` endpoint, surfaced as MCP so routing-rule iteration happens inside the tool surface. Records the replay into the same delivery ring buffer (`source: replay`) so `bro_webhook_deliveries` shows it."
    )]
    pub(crate) async fn bro_webhook_replay(
        &self,
        Parameters(p): Parameters<WebhookReplayParams>,
    ) -> CallToolResult {
        let headers = p
            .headers
            .unwrap_or_default()
            .into_iter()
            .map(|(k, v)| (k.to_lowercase(), v))
            .collect();
        match webhook_replay_inner(&self.state, &p.name, &p.body, &headers) {
            Ok(v) => Self::ok_json(&v),
            Err((status, msg)) => {
                Self::err_text(&format!("replay failed ({}): {msg}", status.as_u16()))
            }
        }
    }

    #[tool(
        name = "bro_webhook_deliveries",
        description = "Recent webhook deliveries as a bounded ring buffer (last ~200). Each entry: (received_at, webhook_name, source, headers, extracted_entity, verdict_classification, response_status, response_body). `source` is `webhook` for live deliveries and `replay` for the no-signature replay endpoint. `verdict_classification` echoes how the routing packet classified the event (`start_arc` / `signal_arc` / `cancel_arc` / `ignore` / `dead_letter` / `no_match` / `duplicate_dropped` / `error`). Filter by `name=` (webhook name) and `since=` (ISO timestamp). Replaces poking the upstream code-host's hook-task table or grepping the daemon's tracing log to debug routing-rule misses."
    )]
    pub(crate) async fn bro_webhook_deliveries(
        &self,
        Parameters(p): Parameters<WebhookDeliveriesParams>,
    ) -> CallToolResult {
        let log = self.state.webhook_delivery_log.read();
        let limit = p.limit.unwrap_or(50).min(WEBHOOK_LOG_CAP);
        let mut out: Vec<&WebhookDelivery> = log
            .iter()
            .filter(|d| match &p.name {
                Some(n) => d.webhook_name == *n,
                None => true,
            })
            .filter(|d| match &p.since {
                Some(ts) => d.received_at.as_str() >= ts.as_str(),
                None => true,
            })
            .filter(|d| match &p.verdict_classification {
                Some(v) => d.verdict_classification == *v,
                None => true,
            })
            .collect();
        // Newest first.
        out.reverse();
        out.truncate(limit);
        Self::ok_json(&serde_json::json!({
            "deliveries": out,
            "total_in_buffer": log.len(),
            "buffer_capacity": WEBHOOK_LOG_CAP,
        }))
    }

    #[tool(
        name = "bro_webhook_install",
        description = "Install a webhook endpoint reachable at POST /webhook/<name>. Signature verification, extractor projection, and routing-packet dispatch are mechanical at the daemon. Routing packets must already be operator-installed in the global packet store."
    )]
    // migration debt: webhook/poller spec persists belong on a StorePersister; tracked in thread-935b467d.
    #[allow(clippy::disallowed_methods)]
    pub(crate) async fn bro_webhook_install(
        &self,
        Parameters(p): Parameters<WebhookInstallParams>,
    ) -> CallToolResult {
        let spec: webhooks::WebhookSpec = match Self::parse_spec(p.spec, "webhook") {
            Ok(s) => s,
            Err(r) => return r,
        };
        // Reject schemes that aren't safe under the daemon's bind
        // (today: SignatureScheme::None requires loopback). Defense
        // in depth — verify_signature also enforces, but rejecting
        // here keeps the on-disk registry clean.
        if let Err(e) = webhooks::install_check(&spec.signature, self.state.bind_is_loopback) {
            return Self::err_text(&format!("webhook install rejected: {e}"));
        }
        // Persist for restart durability.
        let dir = self.state.store_dir.join("webhooks");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join(format!("{}.json", spec.name));
        if let Err(e) = std::fs::write(
            &path,
            serde_json::to_string_pretty(&spec).unwrap_or_default(),
        ) {
            return Self::err_text(&format!("webhook persist failed: {e}"));
        }
        self.state.webhooks.install(spec.clone());
        Self::ok_json(&serde_json::json!({
            "status": "installed",
            "name": spec.name,
            "endpoint": format!("/webhook/{}", spec.name),
        }))
    }

    #[tool(
        name = "bro_webhook_list",
        description = "List installed webhook endpoints with their signature scheme + routing packet."
    )]
    pub(crate) async fn bro_webhook_list(&self) -> CallToolResult {
        let list = self.state.webhooks.list();
        Self::ok_json(&serde_json::json!({"webhooks": list}))
    }

    #[tool(
        name = "bro_webhook_remove",
        description = "Remove an installed webhook by name: drops it from the in-memory registry (POST /webhook/<name> starts 404ing immediately) and deletes its persisted spec file so it does not reload on daemon restart. Deletes the persisted file BEFORE mutating the in-memory registry, so a file-delete failure leaves the webhook fully installed rather than half-removed."
    )]
    #[allow(clippy::disallowed_methods)]
    pub(crate) async fn bro_webhook_remove(
        &self,
        Parameters(p): Parameters<WebhookRemoveParams>,
    ) -> CallToolResult {
        if self.state.webhooks.get(&p.name).is_none() {
            return Self::err_text(&format!("webhook '{}' not found", p.name));
        }
        // Delete the persisted file FIRST: if this fails, the webhook
        // stays fully installed (registry untouched) rather than a
        // half-removed state (registry cleared, stale file on disk that
        // would silently respawn the endpoint on the next daemon restart).
        let path = self
            .state
            .store_dir
            .join("webhooks")
            .join(format!("{}.json", p.name));
        match std::fs::remove_file(&path) {
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Self::err_text(&format!("webhook persisted file remove failed: {e}")),
        }
        self.state.webhooks.remove(&p.name);
        Self::ok_json(&serde_json::json!({"status": "removed", "name": p.name}))
    }

    #[tool(
        name = "bro_poller_install",
        description = "Install a scheduled HTTP-source poller that converges on the same routing pipeline as webhook ingress. Use when the upstream doesn't push (no webhook capability) or the daemon has no public ingress. Spec carries: name, every_seconds (>= BBOX_POLLER_MIN_INTERVAL_SECS, default 5), source (HttpFetchSpec), optional iterate (Selector — array path to explode response into N events), per-event extractor, optional dedup_id_path (Selector for stable id, in-memory recent-seen ring per poller), routing_packet, optional default_project_dir. Persisted to disk + tick loop spawned immediately; reinstall replaces the running task."
    )]
    // migration debt: webhook/poller spec persists belong on a StorePersister; tracked in thread-935b467d.
    #[allow(clippy::disallowed_methods)]
    pub(crate) async fn bro_poller_install(
        &self,
        Parameters(p): Parameters<PollerInstallParams>,
    ) -> CallToolResult {
        let spec: pollers::PollerSpec = match Self::parse_spec(p.spec, "poller") {
            Ok(s) => s,
            Err(r) => return r,
        };
        // Persist for restart durability.
        let dir = self.state.store_dir.join("pollers");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join(format!("{}.json", spec.name));
        if let Err(e) = std::fs::write(
            &path,
            serde_json::to_string_pretty(&spec).unwrap_or_default(),
        ) {
            return Self::err_text(&format!("poller persist failed: {e}"));
        }
        self.state.pollers.install(spec.clone());
        let handle = pollers::spawn_loop(self.state.clone(), spec.clone());
        self.state.pollers.track_handle(&spec.name, handle);
        Self::ok_json(&serde_json::json!({
            "status": "installed",
            "name": spec.name,
            "every_seconds": spec.every_seconds,
        }))
    }

    #[tool(
        name = "bro_poller_list",
        description = "List installed pollers with their schedule + source URL + routing packet."
    )]
    pub(crate) async fn bro_poller_list(&self) -> CallToolResult {
        let list = self.state.pollers.list();
        Self::ok_json(&serde_json::json!({"pollers": list}))
    }

    #[tool(
        name = "bro_poller_remove",
        description = "Remove an installed poller by name: aborts its running tick-loop task immediately, clears its dedup ring, and deletes its persisted spec file so it does not respawn on daemon restart. Deletes the persisted file BEFORE mutating the in-memory registry, so a file-delete failure leaves the poller fully installed rather than half-removed."
    )]
    #[allow(clippy::disallowed_methods)]
    pub(crate) async fn bro_poller_remove(
        &self,
        Parameters(p): Parameters<PollerRemoveParams>,
    ) -> CallToolResult {
        let existed = self.state.pollers.list().iter().any(|s| s.name == p.name);
        if !existed {
            return Self::err_text(&format!("poller '{}' not found", p.name));
        }
        // Delete the persisted file FIRST: if this fails, the poller
        // stays fully installed (registry untouched, tick loop still
        // running) rather than a half-removed state (registry cleared,
        // stale file on disk that would silently respawn the poller on
        // the next daemon restart).
        let path = self
            .state
            .store_dir
            .join("pollers")
            .join(format!("{}.json", p.name));
        match std::fs::remove_file(&path) {
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Self::err_text(&format!("poller persisted file remove failed: {e}")),
        }
        self.state.pollers.remove(&p.name);
        Self::ok_json(&serde_json::json!({"status": "removed", "name": p.name}))
    }

    #[tool(
        name = "bro_cron_install",
        description = "Install a calendar-driven cron inlet — sibling of webhook + poller. Same routing pipeline (extractor → routing packet → dispatch_routed_event), different trigger source: wall-clock schedule, no fetch. Spec: name, schedule (6-field cron expr `sec min hour dom mon dow`), optional payload (operator-supplied entity fields), optional concurrency cap (default 1, set 0 to disable), routing_packet, optional default_project_dir. Synthetic entity fields `cron_name` + `tick_at` are merged in at tick time so routing rules can discriminate."
    )]
    // migration debt: webhook/poller spec persists belong on a StorePersister; tracked in thread-935b467d.
    #[allow(clippy::disallowed_methods)]
    pub(crate) async fn bro_cron_install(
        &self,
        Parameters(p): Parameters<CronInstallParams>,
    ) -> CallToolResult {
        let spec: crons::CronSpec = match Self::parse_spec(p.spec, "cron") {
            Ok(s) => s,
            Err(r) => return r,
        };
        if let Err(e) = crons::validate_schedule(&spec.schedule) {
            return Self::err_text(&format!("cron schedule invalid: {e}"));
        }
        let dir = self.state.store_dir.join("crons");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join(format!("{}.json", spec.name));
        if let Err(e) = std::fs::write(
            &path,
            serde_json::to_string_pretty(&spec).unwrap_or_default(),
        ) {
            return Self::err_text(&format!("cron persist failed: {e}"));
        }
        self.state.crons.install(spec.clone());
        let handle = crons::spawn_loop(self.state.clone(), spec.clone());
        self.state.crons.track_handle(&spec.name, handle);
        Self::ok_json(&serde_json::json!({
            "status": "installed",
            "name": spec.name,
            "schedule": spec.schedule,
            "concurrency": spec.concurrency,
        }))
    }

    #[tool(
        name = "bro_cron_list",
        description = "List installed crons with schedule + concurrency cap + routing packet."
    )]
    pub(crate) async fn bro_cron_list(&self) -> CallToolResult {
        let list = self.state.crons.list();
        Self::ok_json(&serde_json::json!({"crons": list}))
    }

    #[tool(
        name = "bro_cron_remove",
        description = "Remove an installed cron by name: aborts its running tick loop immediately, clears in-flight concurrency state, and deletes the persisted spec file so it does not respawn on daemon restart. Deletes the persisted file BEFORE mutating the in-memory registry, so a file-delete failure leaves the cron fully installed rather than half-removed. A cron installed via bbox_artifact_install (kind=\"cron\") is catalog-managed and gets re-materialized on the next catalog sync; remove it with bbox_artifact_remove instead."
    )]
    #[allow(clippy::disallowed_methods)]
    pub(crate) async fn bro_cron_remove(
        &self,
        Parameters(p): Parameters<CronRemoveParams>,
    ) -> CallToolResult {
        let existed = self.state.crons.list().iter().any(|c| c.name == p.name);
        if !existed {
            return Self::err_text(&format!("cron '{}' not found", p.name));
        }
        // Delete the persisted file FIRST: if this fails, the cron stays
        // fully installed (registry untouched) rather than landing in a
        // half-removed state (registry cleared, stale file on disk that
        // would silently respawn the cron on the next daemon restart).
        let path = self
            .state
            .store_dir
            .join("crons")
            .join(format!("{}.json", p.name));
        match std::fs::remove_file(&path) {
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Self::err_text(&format!("cron persisted file remove failed: {e}")),
        }
        self.state.crons.remove(&p.name);
        Self::ok_json(&serde_json::json!({"status": "removed", "name": p.name}))
    }

    #[tool(
        name = "bro_cron_upcoming",
        description = "Compute the next N scheduled times for a cron expression as RFC3339 strings. Pure function — does not touch the registry."
    )]
    pub(crate) async fn bro_cron_upcoming(
        &self,
        Parameters(p): Parameters<CronUpcomingParams>,
    ) -> CallToolResult {
        let n = p.count.unwrap_or(5).clamp(1, 100);
        match crons::upcoming_times(&p.schedule, n) {
            Ok(times) => Self::ok_json(&serde_json::json!({
                "schedule": p.schedule,
                "upcoming": times,
            })),
            Err(e) => Self::err_text(&format!("schedule '{}': {e}", p.schedule)),
        }
    }

    #[tool(
        name = "bro_workflow_install",
        description = "Install a workflow spec by id so it can be referenced by name from webhook routing verdicts (`{route: start_arc, workflow: <id>}`) and other lookup paths. Compile-validated before install; capability tags enforced. `subworkflow_ref` seams are validated against already-installed children (required imports covered, exports declared in the child schema); refs not installed yet come back as `warnings` rather than refusals so install order stays free."
    )]
    // migration debt: webhook/poller spec persists belong on a StorePersister; tracked in thread-935b467d.
    #[allow(clippy::disallowed_methods)]
    pub(crate) async fn bro_workflow_install(
        &self,
        Parameters(p): Parameters<WorkflowInstallParams>,
    ) -> CallToolResult {
        let spec: workflow::Workflow = match Self::parse_spec(p.spec, "workflow") {
            Ok(s) => s,
            Err(r) => return r,
        };
        let compiled = match workflow::compile(spec.clone()) {
            Ok(c) => c,
            Err(e) => return Self::err_text(&format!("workflow compile failed: {e}")),
        };
        if let Err(e) = validate_workflow_capabilities(&compiled, &self.state) {
            return Self::err_text(&format!("capability validation failed: {e}"));
        }
        // Author-time seam validation for subworkflow_ref: refs that
        // resolve get their imports/exports contract checked NOW
        // instead of when a live arc reaches the node. Unresolved refs
        // are warnings at install time (a parent may legitimately be
        // installed before its children) and hard errors at run time.
        let ref_warnings = match workflow::validate_subworkflow_refs(
            &spec,
            &|id: &str| self.resolve_workflow_by_id(id),
            false,
        ) {
            Ok(w) => w,
            Err(e) => return Self::err_text(&format!("subworkflow_ref validation failed: {e}")),
        };
        let id = p.id.unwrap_or_else(|| spec.name.clone());
        let dir = self.state.store_dir.join("workflows");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join(format!("{id}.json"));
        if let Err(e) = std::fs::write(
            &path,
            serde_json::to_string_pretty(&spec).unwrap_or_default(),
        ) {
            return Self::err_text(&format!("workflow persist failed: {e}"));
        }
        self.state
            .workflow_registry
            .write()
            .insert(id.clone(), spec);
        Self::ok_json(&serde_json::json!({
            "status": "installed",
            "id": id,
            "warnings": ref_warnings,
        }))
    }

    #[tool(
        name = "bro_workflow_list",
        description = "List installed workflow specs by id."
    )]
    pub(crate) async fn bro_workflow_list(&self) -> CallToolResult {
        let map = self.state.workflow_registry.read();
        let names: Vec<String> = map.keys().cloned().collect();
        Self::ok_json(&serde_json::json!({"workflows": names}))
    }

    #[tool(
        name = "bro_workflow_remove",
        description = "Remove an installed workflow by registry id: deletes it from the registry and its persisted spec file so webhook/poller/cron routing verdicts and subworkflow_ref lookups can no longer resolve it. Refuses when any running_arcs entry is still non-terminal (status \"running\") for either this registry id or the resolved spec's own name, unless force=true. Does not cancel or otherwise touch arcs already dispatched from this workflow (use bro_arc_cancel for that). Deletes the persisted file BEFORE mutating the in-memory registry, so a file-delete failure leaves the workflow fully installed rather than half-removed. A workflow installed via bbox_artifact_install (kind=\"workflow\") is catalog-managed and gets re-materialized on the next catalog sync; remove it with bbox_artifact_remove instead."
    )]
    #[allow(clippy::disallowed_methods)]
    pub(crate) async fn bro_workflow_remove(
        &self,
        Parameters(p): Parameters<WorkflowRemoveParams>,
    ) -> CallToolResult {
        // Hold the registry write lock across resolve + running-arc check
        // + file delete + registry mutation: a concurrent
        // bro_workflow_install for this id, or any lookup path that
        // resolves it by name (webhook/poller/cron routing verdicts,
        // subworkflow_ref), blocks until this call finishes rather than
        // interleaving with it.
        //
        // Residual window this does NOT close: a dispatch that already
        // resolved this workflow (read the registry) before we acquired
        // this write lock, but whose ArcSnapshot has not yet landed in
        // `running_arcs` (a separate lock, populated by the engine after
        // dispatch begins, not atomically with the registry read) is
        // invisible to the check below no matter how these two locks are
        // ordered. Closing that fully would need admission bookkeeping
        // at resolve time, which lives in engine-owned code out of this
        // change's scope.
        let mut registry = self.state.workflow_registry.write();
        let Some(spec) = registry.get(&p.id).cloned() else {
            return Self::err_text(&format!("workflow '{}' not found", p.id));
        };
        if !p.force.unwrap_or(false) {
            // Correlated against BOTH the registry id and the resolved
            // spec's own `name` field: an install can pin a custom id
            // via WorkflowInstallParams.id, but `ArcSnapshot.workflow_name`
            // always records `compiled.spec.name`, so checking the id
            // alone misses live arcs whenever the two diverge.
            let running: Vec<String> = self
                .state
                .running_arcs
                .read()
                .values()
                .filter(|s| {
                    s.status == "running"
                        && (s.workflow_name == p.id || s.workflow_name == spec.name)
                })
                .map(|s| s.arc_id.clone())
                .collect();
            if !running.is_empty() {
                return Self::err_text(&format!(
                    "workflow '{}' has {} non-terminal arc(s) still running ({}); pass force=true to remove anyway",
                    p.id,
                    running.len(),
                    running.join(", ")
                ));
            }
        }
        // Delete the persisted file FIRST: if this fails, the workflow
        // stays fully installed (registry untouched) rather than a
        // half-removed state (registry cleared, stale file on disk that
        // would silently reload on the next daemon restart).
        let path = self
            .state
            .store_dir
            .join("workflows")
            .join(format!("{}.json", p.id));
        match std::fs::remove_file(&path) {
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => {
                return Self::err_text(&format!("workflow persisted file remove failed: {e}"));
            }
        }
        registry.remove(&p.id);
        Self::ok_json(&serde_json::json!({"status": "removed", "id": p.id}))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    use crate::server::state::SharedState;

    fn test_server(tmp: &tempfile::TempDir) -> BlackboxServer {
        BlackboxServer::new(Arc::new(SharedState::for_test(tmp.path())))
    }

    #[tokio::test]
    async fn workflow_spawn_returns_pollable_task() {
        use crate::workflow::{compile, load_workflow};
        let tmp = tempfile::tempdir().unwrap();
        let server = test_server(&tmp);
        let json = r#"{
        "name": "pollable-workflow",
        "version": 1,
        "actors": {},
        "nodes": {
            "Only": {
                "actor": "",
                "prompt": "done",
                "next": {"type": "terminal"}
            }
        },
        "start": "Only"
    }"#;
        let compiled = compile(load_workflow(json).unwrap()).unwrap();
        let (task, arc_id) =
            server.spawn_workflow_task(compiled, None, Some(5), serde_json::Map::new());
        {
            let inner = task.inner.lock();
            assert_eq!(inner.provider, Provider::Workflow);
            assert_eq!(inner.session_id, arc_id);
            assert_eq!(inner.status, orch::TaskStatus::Running);
        }
        assert!(orch::wait_for_task_with_timeout(&task, Some(5.0)).await);
        let status = orch::task_status_json(&task, 5);
        assert_eq!(status["status"], "completed");
        assert_eq!(status["provider"], "workflow");
        assert!(status["eventCount"].as_u64().unwrap_or_default() > 1);
        let result: Value = serde_json::from_str(status["result"].as_str().unwrap()).unwrap();
        assert_eq!(result["status"], "completed");
        assert_eq!(result["arc_id"], arc_id);
    }

    #[tokio::test]
    async fn bro_arc_result_returns_structured_exit_without_events() {
        use crate::tools::bro_runtime_params::ArcResultParams;
        use crate::workflow::{compile, load_workflow};
        let tmp = tempfile::tempdir().unwrap();
        let server = test_server(&tmp);
        let json = r#"{
        "name": "structured-exit-workflow",
        "version": 1,
        "actors": {},
        "vars_schema": {
            "_structured_exit": {"kind": "object"},
            "sieve": {"kind": "string"},
            "noise": {"kind": "string"}
        },
        "nodes": {
            "Only": {
                "actor": "",
                "prompt": "done",
                "on_enter": [
                    {"op": "set_var", "args": {"key": "sieve", "value": "kept"}},
                    {"op": "set_var", "args": {"key": "noise", "value": "dropped"}},
                    {"op": "set_var", "args": {"key": "_structured_exit", "value": {"verdict": "ok"}}}
                ],
                "next": {"type": "terminal"}
            }
        },
        "start": "Only"
    }"#;
        let compiled = compile(load_workflow(json).unwrap()).unwrap();
        let (task, arc_id) =
            server.spawn_workflow_task(compiled, None, Some(5), serde_json::Map::new());
        assert!(orch::wait_for_task_with_timeout(&task, Some(5.0)).await);

        // The stored envelope is event-free — events live in the task event
        // log only (gap-55be3518's 81k escaped blob).
        let status = orch::task_status_json(&task, 5);
        let envelope: Value = serde_json::from_str(status["result"].as_str().unwrap()).unwrap();
        assert_eq!(envelope["events"], serde_json::json!([]));
        assert!(status["eventCount"].as_u64().unwrap_or_default() > 1);

        // bro_wait-shaped result lifts structuredExit first-class.
        let result_json = orch::task_result_json(&task);
        assert_eq!(result_json["structuredExit"]["verdict"], "ok");

        // bro_arc_result by arc id, vars filtered to the requested keys,
        // node outputs withheld unless asked for.
        let r = server
            .bro_arc_result(Parameters(ArcResultParams {
                arc_id: arc_id.clone(),
                keys: Some(vec!["sieve".into()]),
                include_node_outputs: None,
            }))
            .await;
        let text = r.content[0].as_text().expect("text content").text.clone();
        let v: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(v["arcId"], arc_id);
        assert_eq!(v["workflowStatus"], "completed");
        assert_eq!(v["structuredExit"]["verdict"], "ok");
        assert_eq!(v["vars"]["sieve"], "kept");
        assert!(v["vars"].get("noise").is_none());
        assert!(v.get("nodeOutputs").is_none());

        // Task-id lookup works too; unknown ids fail with guidance.
        let by_task = server
            .bro_arc_result(Parameters(ArcResultParams {
                arc_id: status["taskId"].as_str().unwrap().to_string(),
                keys: None,
                include_node_outputs: Some(true),
            }))
            .await;
        let text = by_task.content[0].as_text().unwrap().text.clone();
        let v: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(v["vars"]["noise"], "dropped");
        assert!(v.get("nodeOutputs").is_some());

        let missing = server
            .bro_arc_result(Parameters(ArcResultParams {
                arc_id: "arc-doesnotexist".into(),
                keys: None,
                include_node_outputs: None,
            }))
            .await;
        assert_eq!(missing.is_error, Some(true));
    }

    #[tokio::test]
    async fn bro_arc_cancel_trips_a_parked_wait_arc() {
        // End-to-end cancel: spawn an arc that immediately parks on a
        // long-timeout Wait, cancel it via the SharedState, observe
        // that run() returns with status=cancelled. No LLM dispatch
        // needed — the arc is hook-only and immediately blocks on the
        // wait.
        use crate::workflow::{compile, engine, load_workflow};
        let tmp = tempfile::tempdir().unwrap();
        let server = test_server(&tmp);

        let json = r#"{
        "name": "cancel-smoke",
        "version": 1,
        "actors": {},
        "nodes": {
            "WaitFor": {
                "actor": "",
                "wait": {
                    "any_of": [{"signal": "never-arrives"}],
                    "timeout": "30s"
                },
                "next": {"type": "terminal"}
            }
        },
        "start": "WaitFor"
    }"#;
        let compiled = compile(load_workflow(json).unwrap()).unwrap();

        // Spawn the arc on a background task — it'll park inside the
        // Wait until either the timeout fires or our cancel trips.
        let server_state = server.state.clone();
        let run_handle = tokio::spawn(async move {
            let server2 = BlackboxServer::new(server_state);
            engine::run_workflow_with_initial_vars(
                &server2,
                &compiled,
                None,
                Some(50),
                serde_json::Map::new(),
            )
            .await
        });

        // Give the runner a moment to register the wait + cancel
        // token, then observe the registered token and trip it. Yield
        // a few times to let the task progress past wait registration
        // without hard-coding a timing assumption.
        for _ in 0..50 {
            tokio::task::yield_now().await;
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            let token_count = server.state.arc_cancel_tokens.read().len();
            if token_count > 0 {
                break;
            }
        }

        // Cancel every registered arc (test fixture only spawns one).
        let arc_ids: Vec<String> = server
            .state
            .arc_cancel_tokens
            .read()
            .keys()
            .cloned()
            .collect();
        assert!(
            !arc_ids.is_empty(),
            "expected an arc cancel token to be registered after dispatch"
        );
        for arc_id in &arc_ids {
            let cancelled = server.state.cancel_arc(arc_id);
            assert!(cancelled, "cancel_arc returned false for live arc {arc_id}");
        }

        // The runner should release the wait and return with
        // status=cancelled.
        let result = tokio::time::timeout(std::time::Duration::from_secs(5), run_handle)
            .await
            .expect("runner did not exit within 5s of cancel")
            .expect("runner panicked");
        assert_eq!(result.status, "cancelled", "got: {}", result.status);

        // Token should have been unregistered at terminus.
        assert!(
            server.state.arc_cancel_tokens.read().is_empty(),
            "cancel token still registered after arc terminated"
        );
    }

    // ── bro_webhook_remove ────────────────────────────────────────

    #[tokio::test]
    async fn bro_webhook_remove_deletes_registry_entry_file_and_survives_restart() {
        let tmp = tempfile::tempdir().unwrap();
        let server = test_server(&tmp);
        let spec = serde_json::json!({
            "name": "wh-remove-test",
            "signature": {"kind": "none"},
            "extractor": {"outputs": {}},
            "routing_packet": "packet-test"
        });
        let install = server
            .bro_webhook_install(Parameters(WebhookInstallParams { spec }))
            .await;
        assert!(
            !install.is_error.unwrap_or(false),
            "install failed: {:?}",
            install.content
        );
        let persisted = server
            .state
            .store_dir
            .join("webhooks")
            .join("wh-remove-test.json");
        assert!(persisted.exists(), "expected webhook spec to be persisted");
        assert!(server.state.webhooks.get("wh-remove-test").is_some());

        let remove = server
            .bro_webhook_remove(Parameters(WebhookRemoveParams {
                name: "wh-remove-test".into(),
            }))
            .await;
        assert!(
            !remove.is_error.unwrap_or(false),
            "remove failed: {:?}",
            remove.content
        );
        assert!(server.state.webhooks.get("wh-remove-test").is_none());
        assert!(!persisted.exists());

        // Restart durability: a fresh `load_all` over the persisted
        // directory (what the daemon does on boot) must not see the
        // removed webhook.
        let reloaded = webhooks::load_all(&server.state.store_dir.join("webhooks"));
        assert!(
            !reloaded.iter().any(|w| w.name == "wh-remove-test"),
            "removed webhook reappeared on simulated restart"
        );
    }

    #[tokio::test]
    async fn bro_webhook_remove_unknown_name_errors() {
        let tmp = tempfile::tempdir().unwrap();
        let server = test_server(&tmp);
        let remove = server
            .bro_webhook_remove(Parameters(WebhookRemoveParams {
                name: "does-not-exist".into(),
            }))
            .await;
        assert!(remove.is_error.unwrap_or(false));
    }

    // ── bro_poller_remove ─────────────────────────────────────────

    #[tokio::test]
    async fn bro_poller_remove_deletes_registry_entry_file_and_survives_restart() {
        let tmp = tempfile::tempdir().unwrap();
        let server = test_server(&tmp);
        let spec = serde_json::json!({
            "name": "poller-remove-test",
            // Long enough that no tick fires during the test.
            "every_seconds": 3600,
            "source": {"url": "http://example.invalid"},
            "extractor": {"outputs": {}},
            "routing_packet": "packet-test"
        });
        let install = server
            .bro_poller_install(Parameters(PollerInstallParams { spec }))
            .await;
        assert!(
            !install.is_error.unwrap_or(false),
            "install failed: {:?}",
            install.content
        );
        let persisted = server
            .state
            .store_dir
            .join("pollers")
            .join("poller-remove-test.json");
        assert!(persisted.exists(), "expected poller spec to be persisted");
        assert_eq!(server.state.pollers.list().len(), 1);

        let remove = server
            .bro_poller_remove(Parameters(PollerRemoveParams {
                name: "poller-remove-test".into(),
            }))
            .await;
        assert!(
            !remove.is_error.unwrap_or(false),
            "remove failed: {:?}",
            remove.content
        );
        assert!(server.state.pollers.list().is_empty());
        assert!(!persisted.exists());

        // Restart durability: a fresh `load_all` over the persisted
        // directory must not see the removed poller.
        let reloaded = pollers::load_all(&server.state.store_dir.join("pollers"));
        assert!(
            !reloaded.iter().any(|p| p.name == "poller-remove-test"),
            "removed poller reappeared on simulated restart"
        );
    }

    #[tokio::test]
    async fn bro_poller_remove_unknown_name_errors() {
        let tmp = tempfile::tempdir().unwrap();
        let server = test_server(&tmp);
        let remove = server
            .bro_poller_remove(Parameters(PollerRemoveParams {
                name: "does-not-exist".into(),
            }))
            .await;
        assert!(remove.is_error.unwrap_or(false));
    }

    // ── bro_cron_remove ──────────────────────────────────────────

    #[tokio::test]
    async fn bro_cron_remove_deletes_registry_entry_and_file() {
        let tmp = tempfile::tempdir().unwrap();
        let server = test_server(&tmp);
        let spec = serde_json::json!({
            "name": "test-cron-remove",
            "schedule": "0 0 9 * * *",
            "routing_packet": "packet-test",
        });
        let install = server
            .bro_cron_install(Parameters(CronInstallParams { spec }))
            .await;
        assert!(!install.is_error.unwrap_or(false), "install failed");
        let store_dir = server.state.store_dir.clone();
        let persisted = store_dir.join("crons").join("test-cron-remove.json");
        assert!(persisted.exists(), "expected cron spec to be persisted");
        assert_eq!(server.state.crons.list().len(), 1);

        let remove = server
            .bro_cron_remove(Parameters(CronRemoveParams {
                name: "test-cron-remove".into(),
            }))
            .await;
        assert!(
            !remove.is_error.unwrap_or(false),
            "remove failed: {:?}",
            remove.content
        );
        assert!(server.state.crons.list().is_empty());
        assert!(
            !persisted.exists(),
            "expected persisted cron file to be deleted"
        );

        // Restart durability: a fresh `load_all` over the persisted
        // directory must not see the removed cron.
        let reloaded = crons::load_all(&store_dir.join("crons"));
        assert!(
            !reloaded.iter().any(|c| c.name == "test-cron-remove"),
            "removed cron reappeared on simulated restart"
        );
    }

    #[tokio::test]
    async fn bro_cron_remove_unknown_name_errors() {
        let tmp = tempfile::tempdir().unwrap();
        let server = test_server(&tmp);
        let remove = server
            .bro_cron_remove(Parameters(CronRemoveParams {
                name: "does-not-exist".into(),
            }))
            .await;
        assert!(remove.is_error.unwrap_or(false));
    }

    // ── bro_workflow_remove ──────────────────────────────────────

    fn minimal_workflow_spec(name: &str) -> Value {
        serde_json::json!({
            "name": name,
            "version": 1,
            "actors": {},
            "nodes": {
                "Only": {"actor": "", "prompt": "done", "next": {"type": "terminal"}}
            },
            "start": "Only"
        })
    }

    #[tokio::test]
    async fn bro_workflow_remove_deletes_registry_entry_and_file() {
        let tmp = tempfile::tempdir().unwrap();
        let server = test_server(&tmp);
        let install = server
            .bro_workflow_install(Parameters(WorkflowInstallParams {
                id: Some("wf-remove-me".into()),
                spec: minimal_workflow_spec("wf-remove-me"),
            }))
            .await;
        assert!(!install.is_error.unwrap_or(false), "install failed");
        let persisted = server
            .state
            .store_dir
            .join("workflows")
            .join("wf-remove-me.json");
        assert!(persisted.exists());
        assert!(
            server
                .state
                .workflow_registry
                .read()
                .contains_key("wf-remove-me")
        );

        let remove = server
            .bro_workflow_remove(Parameters(WorkflowRemoveParams {
                id: "wf-remove-me".into(),
                force: None,
            }))
            .await;
        assert!(
            !remove.is_error.unwrap_or(false),
            "remove failed: {:?}",
            remove.content
        );
        assert!(
            !server
                .state
                .workflow_registry
                .read()
                .contains_key("wf-remove-me")
        );
        assert!(!persisted.exists());
    }

    #[tokio::test]
    async fn bro_workflow_remove_unknown_id_errors() {
        let tmp = tempfile::tempdir().unwrap();
        let server = test_server(&tmp);
        let remove = server
            .bro_workflow_remove(Parameters(WorkflowRemoveParams {
                id: "does-not-exist".into(),
                force: None,
            }))
            .await;
        assert!(remove.is_error.unwrap_or(false));
    }

    #[tokio::test]
    async fn bro_workflow_remove_refuses_when_arc_running() {
        let tmp = tempfile::tempdir().unwrap();
        let server = test_server(&tmp);
        server
            .bro_workflow_install(Parameters(WorkflowInstallParams {
                id: Some("wf-in-flight".into()),
                spec: minimal_workflow_spec("wf-in-flight"),
            }))
            .await;
        // Simulate an in-flight arc: running_arcs snapshots persist for
        // the arc's lifetime (see workflow::engine::arc_state), keyed by
        // arc_thread_id, distinguished from terminal states by
        // status == "running".
        server.state.running_arcs.write().insert(
            "thread-in-flight".into(),
            crate::server::state::ArcSnapshot {
                arc_id: "arc-in-flight".into(),
                arc_thread_id: "thread-in-flight".into(),
                workflow_name: "wf-in-flight".into(),
                workflow_version: 1,
                status: "running".into(),
                current_node: Some("Only".into()),
                completed_nodes: vec![],
                in_flight_nodes: vec![],
                last_verdict: None,
                visit_counts: std::collections::HashMap::new(),
                admission_key: None,
                started_at: "2026-08-14T00:00:00Z".into(),
                updated_at: "2026-08-14T00:00:00Z".into(),
            },
        );

        let refused = server
            .bro_workflow_remove(Parameters(WorkflowRemoveParams {
                id: "wf-in-flight".into(),
                force: None,
            }))
            .await;
        assert!(refused.is_error.unwrap_or(false));
        assert!(
            server
                .state
                .workflow_registry
                .read()
                .contains_key("wf-in-flight"),
            "workflow must not be removed while an arc is running"
        );

        let forced = server
            .bro_workflow_remove(Parameters(WorkflowRemoveParams {
                id: "wf-in-flight".into(),
                force: Some(true),
            }))
            .await;
        assert!(
            !forced.is_error.unwrap_or(false),
            "force=true should override the running-arc refusal"
        );
        assert!(
            !server
                .state
                .workflow_registry
                .read()
                .contains_key("wf-in-flight")
        );
    }
}
