use anyhow::{Result, anyhow, bail};
use serde_json::{Value, json};

use super::super::context::resolve_arg_value;
use super::super::{ActorFailureMode, ActorSpec, AtomBinding, NodeSpec};
use super::WorkflowRunner;
use crate::orchestration as orch;
use crate::tools::bro_params::{AtomInvokeParams, AtomResumeParams, AtomStatusParams};

impl<'a> WorkflowRunner<'a> {
    pub(super) async fn run_executor_node(
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
        let existing_task_id = if actor.durable {
            self.actor_tasks.get(actor_name).cloned()
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
                existing_task_id.as_deref(),
                self.runtime_for_actor(actor),
                &[],
            )
            .await
            .map_err(|e| anyhow!("dispatch for node '{node_id}': {e}"))?;

        let task_id = {
            let inner = task.inner.lock();
            inner.id.clone()
        };
        self.actor_tasks
            .insert(actor_name.to_string(), task_id.clone());
        let timeout_secs = self.compiled.spec.node_timeout_secs(node_id);
        let completed = orch::wait_for_task_with_timeout(&task, Some(timeout_secs)).await;
        // Record full task envelope into actor_results regardless of
        // outcome so downstream nodes can branch on `status`, surface
        // `result` text, or stash `taskId` for state-machine bookkeeping.
        // Timeout uses the snapshot variant (sets `timed_out: true`); a
        // completed task uses the canonical task_result_json.
        let mut result_json = if completed {
            orch::task_result_json(&task)
        } else {
            orch::timeout_snapshot_json(&task)
        };
        result_json["actor"] = Value::String(actor_name.to_string());
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
                        bail!(
                            "node '{node_id}' (task {task_id}) exceeded timeout ({timeout_secs}s)"
                        );
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

    pub(super) async fn run_atom_node(
        &mut self,
        node_id: &str,
        binding: &AtomBinding,
        spec: &NodeSpec,
        visit_count: u32,
    ) -> Result<()> {
        let owner = format!("workflow:{}", self.ctx.meta.arc_id);
        let rendered_prompt = spec.prompt.as_deref().map(|p| self.render_prompt(p));
        let args = if let Some(raw_args) = &spec.atom_args {
            resolve_arg_value(&self.ctx, raw_args)
                .map_err(|e| anyhow!("node '{node_id}' atom_args resolution failed: {e}"))?
        } else if let Some(prompt) = rendered_prompt.as_ref().filter(|p| !p.is_empty()) {
            json!({ "prompt": prompt })
        } else {
            json!({})
        };

        let parent_invocation_id = self
            .ctx
            .vars
            .get("_atom_parent_invocation_id")
            .and_then(Value::as_str)
            .map(str::to_string);
        let existing_invocation_id = if binding.durable {
            self.atom_invocations.get(&spec.atom).cloned()
        } else {
            None
        };

        self.log_event(
            "node_atom_dispatch",
            json!({
                "node": node_id,
                "binding": spec.atom,
                "atom_ref": binding.atom_ref,
                "mode": if existing_invocation_id.is_some() { "resume" } else { "invoke" },
                "visit": visit_count,
            }),
        );

        let value = if let Some(invocation_id) = existing_invocation_id {
            let resume_prompt = rendered_prompt
                .clone()
                .unwrap_or_else(|| serde_json::to_string_pretty(&args).unwrap_or_default());
            match self
                .server
                .atom_resume_value(AtomResumeParams {
                    invocation_id: invocation_id.clone(),
                    prompt: resume_prompt,
                    owner: Some(owner.clone()),
                })
                .await
            {
                Ok(value) => value,
                Err(e) if e.contains("error.not_resumable") => {
                    self.log_event(
                        "node_atom_reinvoke",
                        json!({
                            "node": node_id,
                            "binding": spec.atom,
                            "prior_invocation_id": invocation_id,
                            "reason": e,
                        }),
                    );
                    self.server
                        .atom_invoke_value(
                            AtomInvokeParams {
                                atom: binding.atom_ref.clone(),
                                args: args.clone(),
                                project_dir: self.effective_project_dir(),
                                owner: Some(owner.clone()),
                                parent_invocation_id,
                                runtime: None,
                                supervision_override: binding.supervision_override.clone(),
                                suppress_auto_supervision: false,
                            },
                            binding.limits.as_ref(),
                        )
                        .await
                        .map_err(|e| anyhow!("atom invoke for node '{node_id}': {e}"))?
                }
                Err(e) => return Err(anyhow!("atom resume for node '{node_id}': {e}")),
            }
        } else {
            self.server
                .atom_invoke_value(
                    AtomInvokeParams {
                        atom: binding.atom_ref.clone(),
                        args,
                        project_dir: self.effective_project_dir(),
                        owner: Some(owner.clone()),
                        parent_invocation_id,
                        runtime: None,
                        supervision_override: binding.supervision_override.clone(),
                        suppress_auto_supervision: false,
                    },
                    binding.limits.as_ref(),
                )
                .await
                .map_err(|e| anyhow!("atom invoke for node '{node_id}': {e}"))?
        };

        let invocation_id = value
            .get("invocation_id")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow!("atom node '{node_id}' returned no invocation_id"))?
            .to_string();
        if binding.durable {
            self.atom_invocations
                .insert(spec.atom.clone(), invocation_id.clone());
        }

        if let Some(task_id) = value.get("task_id").and_then(Value::as_str)
            && value.get("arc_id").is_some()
        {
            let task = {
                let task_store = self.server.state.task_store.read();
                task_store.get(task_id)
            };
            if let Some(task) = task {
                let should_wait = {
                    let inner = task.inner.lock();
                    matches!(inner.status, orch::TaskStatus::Running)
                };
                if should_wait {
                    self.log_event(
                        "node_atom_workflow_wait",
                        json!({
                            "node": node_id,
                            "binding": spec.atom,
                            "invocation_id": invocation_id,
                            "task_id": task_id,
                        }),
                    );
                    tokio::select! {
                        _ = task.notify.notified() => {}
                        _ = self.cancel_token.cancelled() => {
                            bail!("arc cancelled")
                        }
                    }
                }
            }
        }

        let status_value = self
            .server
            .atom_status_value(AtomStatusParams {
                invocation_id: invocation_id.clone(),
                owner: Some(owner),
            })
            .map_err(|e| anyhow!("atom status for node '{node_id}': {e}"))?;
        self.ctx.record_actor_result(node_id, status_value.clone());
        let output = serde_json::to_string(&status_value).unwrap_or_default();
        self.record_output(node_id, output.clone());

        let state = status_value
            .get("state")
            .and_then(Value::as_str)
            .unwrap_or("unknown");
        let output_preview: String = output.chars().take(160).collect();
        self.log_event(
            "node_atom_complete",
            json!({
                "node": node_id,
                "binding": spec.atom,
                "atom_ref": binding.atom_ref,
                "invocation_id": invocation_id,
                "state": state,
                "output_bytes": output.len(),
                "output_preview": output_preview.clone(),
            }),
        );
        self.arc_note(
            "done",
            &format!("atom node '{node_id}' ({}) → {state}", binding.atom_ref),
        );
        Ok(())
    }
}
