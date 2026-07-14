use std::collections::HashMap;

use anyhow::{Result, anyhow, bail};
use serde_json::json;

use super::super::ActorSpec;
use super::WorkflowRunner;
use crate::orchestration as orch;
use crate::whiteboards;

impl<'a> WorkflowRunner<'a> {
    pub(super) async fn run_ensemble_node(
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
        let timeout_secs = self.compiled.spec.node_timeout_secs(node_id);
        let mut joinset = tokio::task::JoinSet::new();
        for (member_name, task) in tasks.iter() {
            let member_name = member_name.clone();
            let task_clone = task.clone();
            joinset.spawn(async move {
                let completed =
                    orch::wait_for_task_with_timeout(&task_clone, Some(timeout_secs)).await;
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
                        "timeout_secs": timeout_secs,
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
            bail!("node '{node_id}' had one or more ensemble-member timeouts ({timeout_secs}s)");
        }
        // Stable order (by member name) so prompt substitution is
        // deterministic across re-runs.
        member_outputs.sort_by(|a, b| a.0.cmp(&b.0));
        // Board binding: parse each member's STRICT-JSON output into
        // typed board actions and apply them engine-side, so a member
        // that wrote the deliberation but forgot the tool call still
        // lands on the board (gap-7fbefe13).
        let board_template = self
            .compiled
            .spec
            .nodes
            .get(node_id)
            .and_then(|n| n.board.clone());
        if let Some(template) = board_template {
            let board_id = self.render_prompt(&template);
            self.apply_board_actions(node_id, &board_id, &member_outputs);
        }
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

    /// Engine-driven whiteboard mutation for a `board`-bound ensemble
    /// node. Each member's output is parsed with
    /// [`whiteboards::parse_board_actions`] and applied through the
    /// registry (same phase/role checks as the `whiteboard_*` tools).
    /// Attribution: the item's `agent_name` when present, else the
    /// member name. Nothing here fails the node — a member that
    /// already posted via tools returns prose (parse skip), and a
    /// phase-illegal action is the registry's refusal to log, not an
    /// arc error. Mechanical enforcement of board contracts stays with
    /// gate packets over `whiteboard_summarize` counts.
    pub(super) fn apply_board_actions(
        &mut self,
        node_id: &str,
        board_id: &str,
        member_outputs: &[(String, String)],
    ) {
        for (member, output) in member_outputs {
            let items = match whiteboards::parse_board_actions(output) {
                Ok(items) => items,
                Err(e) => {
                    self.log_event(
                        "board_autoapply_skipped",
                        json!({
                            "node": node_id,
                            "board": board_id,
                            "member": member,
                            "reason": e.to_string(),
                        }),
                    );
                    continue;
                }
            };
            for item in items {
                let agent = item.agent_name.as_deref().unwrap_or(member.as_str());
                match self
                    .server
                    .state
                    .whiteboards
                    .apply_action(board_id, agent, &item.action)
                {
                    Ok(applied) => {
                        self.log_event(
                            "board_autoapply",
                            json!({
                                "node": node_id,
                                "board": board_id,
                                "member": member,
                                "agent": agent,
                                "action": item.action.kind(),
                                "result": applied,
                            }),
                        );
                    }
                    Err(e) => {
                        self.log_event(
                            "board_autoapply_failed",
                            json!({
                                "node": node_id,
                                "board": board_id,
                                "member": member,
                                "agent": agent,
                                "action": item.action.kind(),
                                "reason": e.to_string(),
                            }),
                        );
                        self.arc_note(
                            "surprise",
                            &format!(
                                "board auto-apply failed on node '{node_id}' ({agent} {} → {board_id}): {e}",
                                item.action.kind()
                            ),
                        );
                    }
                }
            }
        }
    }
}
