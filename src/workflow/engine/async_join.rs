use std::collections::HashMap;

use anyhow::{Result, anyhow, bail};
use serde_json::json;

use super::super::ActorKind;
use super::{InFlight, WorkflowRunner};
use crate::orchestration as orch;

impl<'a> WorkflowRunner<'a> {
    pub(super) async fn dispatch_fire_and_forget(&mut self, target_id: &str) -> Result<()> {
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
                        &[],
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
    pub(super) async fn join_late_inject(&mut self, node_id: &str) -> Result<()> {
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
    pub(super) async fn join_in_flight_source(&mut self, source: &str) -> Result<bool> {
        let entry = match self.in_flight.remove(source) {
            Some(e) => e,
            None => return Ok(false),
        };
        let timeout_secs = self.compiled.spec.node_timeout_secs(source);
        match entry {
            InFlight::Single {
                actor_name,
                durable,
                task,
            } => {
                let task_id = task.inner.lock().id.clone();
                let completed = orch::wait_for_task_with_timeout(&task, Some(timeout_secs)).await;
                if !completed {
                    bail!(
                        "in-flight source '{source}' (task {task_id}) exceeded timeout ({timeout_secs}s)"
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
                        let completed =
                            orch::wait_for_task_with_timeout(&task, Some(timeout_secs)).await;
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
                    bail!(
                        "in-flight source '{source}' (ensemble) had member timeouts ({timeout_secs}s)"
                    );
                }
                outs.sort_by(|a, b| a.0.cmp(&b.0));
                // Board-bound fire-and-forget ensembles auto-apply at
                // join time — same contract as the synchronous path.
                let board_template = self
                    .compiled
                    .spec
                    .nodes
                    .get(source)
                    .and_then(|n| n.board.clone());
                if let Some(template) = board_template {
                    let board_id = self.render_prompt(&template);
                    self.apply_board_actions(source, &board_id, &outs);
                }
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
}
