use anyhow::{Result, anyhow, bail};
use std::sync::Arc;

use bro_capabilities::CellLoadRequest;
use serde_json::{Value, json};

use crate::workflow::context::resolve_arg_value;
use crate::workflow::ops::{HookOp, OnFailure, OpEffect, OpKind, execute_op_with_hub};

use super::WorkflowRunner;

impl WorkflowRunner<'_> {
    /// Apply a single OpEffect to the runner state. Used by hook execution to
    /// centralize logging + schema validation.
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

    async fn execute_poll_attached_invocation_op(&self, hook: &HookOp) -> Result<OpEffect> {
        let rendered_args = resolve_arg_value(&self.ctx, &hook.args)
            .map_err(|e| anyhow!("op {:?}: arg render failed: {e}", hook.op))?;
        let primary_invocation_id = rendered_args
            .get("primary_invocation_id")
            .and_then(Value::as_str)
            .filter(|value| !value.trim().is_empty())
            .ok_or_else(|| {
                anyhow!("poll_attached_invocation requires args.primary_invocation_id")
            })?;
        let owner = rendered_args
            .get("owner")
            .and_then(Value::as_str)
            .filter(|value| !value.trim().is_empty())
            .map(str::to_string)
            .or_else(|| {
                self.ctx
                    .vars
                    .get("_atom_owner")
                    .and_then(Value::as_str)
                    .filter(|value| !value.trim().is_empty())
                    .map(str::to_string)
            })
            .unwrap_or_else(|| format!("workflow:{}", self.ctx.meta.arc_id));
        let attempt = rendered_args.get("attempt").and_then(Value::as_u64);
        let tail_policy = match rendered_args.get("tail_policy") {
            Some(Value::Null) | None => None,
            Some(Value::String(value)) if value.trim().is_empty() || value.starts_with("${") => {
                None
            }
            Some(Value::String(value)) => Some(
                serde_json::from_str::<crate::orchestration::atoms::types::SupervisionTailPolicy>(
                    value,
                )
                .map_err(|e| anyhow!("poll_attached_invocation tail_policy: {e}"))?,
            ),
            Some(value) => {
                Some(
                    serde_json::from_value::<
                        crate::orchestration::atoms::types::SupervisionTailPolicy,
                    >(value.clone())
                    .map_err(|e| anyhow!("poll_attached_invocation tail_policy: {e}"))?,
                )
            }
        };
        let value = self
            .server
            .attached_supervision_poll_value_with_tail(
                primary_invocation_id,
                &owner,
                attempt,
                tail_policy.as_ref(),
            )
            .map_err(|e| anyhow!("poll_attached_invocation: {e}"))?;
        Ok(OpEffect::SetVar {
            key: hook
                .into_var
                .as_deref()
                .unwrap_or("attached_invocation")
                .to_string(),
            value,
        })
    }

    async fn execute_supervision_action_op(&self, hook: &HookOp) -> Result<OpEffect> {
        let rendered_args = resolve_arg_value(&self.ctx, &hook.args)
            .map_err(|e| anyhow!("op {:?}: arg render failed: {e}", hook.op))?;
        let primary_invocation_id = rendered_args
            .get("primary_invocation_id")
            .and_then(Value::as_str)
            .filter(|value| !value.trim().is_empty())
            .ok_or_else(|| {
                anyhow!("execute_supervision_action requires args.primary_invocation_id")
            })?;
        let owner = rendered_args
            .get("owner")
            .and_then(Value::as_str)
            .filter(|value| !value.trim().is_empty())
            .map(str::to_string)
            .or_else(|| {
                self.ctx
                    .vars
                    .get("_atom_owner")
                    .and_then(Value::as_str)
                    .filter(|value| !value.trim().is_empty())
                    .map(str::to_string)
            })
            .unwrap_or_else(|| format!("workflow:{}", self.ctx.meta.arc_id));
        let attempt = rendered_args.get("attempt").and_then(Value::as_u64);
        let mut action = rendered_args
            .get("action")
            .cloned()
            .ok_or_else(|| anyhow!("execute_supervision_action requires args.action"))?;
        if action.get("max_attempts").is_none()
            && let Some(max_attempts) = rendered_args.get("max_attempts").cloned()
            && let Some(obj) = action.as_object_mut()
        {
            obj.insert("max_attempts".to_string(), max_attempts);
        }
        let value = self
            .server
            .execute_supervision_action_value(primary_invocation_id, &owner, attempt, action)
            .await
            .map_err(|e| anyhow!("execute_supervision_action: {e}"))?;
        Ok(OpEffect::SetVar {
            key: hook
                .into_var
                .as_deref()
                .unwrap_or("supervision_action_result")
                .to_string(),
            value,
        })
    }

    async fn execute_cell_run_op(&self, hook: &HookOp) -> Result<OpEffect> {
        let rendered_args = resolve_arg_value(&self.ctx, &hook.args)
            .map_err(|e| anyhow!("op {:?}: arg render failed: {e}", hook.op))?;
        let handle = rendered_args
            .get("handle")
            .and_then(Value::as_str)
            .filter(|value| !value.trim().is_empty())
            .ok_or_else(|| anyhow!("cell_run requires args.handle"))?;
        let input_json = rendered_args.get("input").cloned().unwrap_or(Value::Null);
        let loaded = crate::cells::load_cell(
            &self.server.state.artifacts.read(),
            CellLoadRequest {
                handle: handle.to_string(),
            },
        )
        .map_err(|e| anyhow!("cell_run load: {e}"))?;
        let contract: bro_script::CellContract = serde_json::from_value(loaded.contract_json)
            .map_err(|e| anyhow!("cell_run contract parse: {e}"))?;
        let caps = bro_script::Capabilities {
            atoms: Arc::new(crate::orchestration::capabilities::DaemonAtoms {
                state: self.server.state.clone(),
            }),
            refactor: Arc::new(crate::orchestration::capabilities::DaemonRefactor::new(
                self.server.state.clone(),
            )),
            tools: None,
            kv: Arc::new(bro_harness::capabilities::KvStore::default()),
        };
        let runtime =
            bro_script::ScriptRuntime::new(caps, bro_script::SupervisionPolicy::default())
                .await
                .map_err(|e| anyhow!("cell_run runtime init: {e:#}"))?;
        let output = runtime
            .call_cell(loaded.source, contract, input_json)
            .await
            .map_err(|e| anyhow!("cell_run failed: {e:#}"))?;
        let value: Value =
            serde_json::from_str(&output).map_err(|e| anyhow!("cell_run output parse: {e}"))?;
        Ok(OpEffect::SetVar {
            key: hook
                .into_var
                .as_deref()
                .unwrap_or("cell_result")
                .to_string(),
            value,
        })
    }

    /// Run a list of HookOps against the current ArcContext. Per-op `when`
    /// packet evaluates against the flattened entity; failure is handled per
    /// `on_failure`.
    pub(super) async fn run_hooks(&mut self, hooks: &[HookOp], lifecycle: &str) -> Result<()> {
        for (idx, hook) in hooks.iter().enumerate() {
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
            let op_result = match hook.op {
                OpKind::PollAttachedInvocation => {
                    self.execute_poll_attached_invocation_op(hook).await
                }
                OpKind::ExecuteSupervisionAction => self.execute_supervision_action_op(hook).await,
                OpKind::CellRun => self.execute_cell_run_op(hook).await,
                _ => {
                    execute_op_with_hub(
                        hook,
                        &self.ctx,
                        self.compiled.spec.vars_schema.as_ref(),
                        Some(&self.server.state.system_events),
                    )
                    .await
                }
            };
            match op_result {
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

    /// Workflow-level `on_arc_cancel` + `on_arc_exit` invocation.
    pub(super) async fn run_arc_exit_hooks(&mut self) {
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
}

fn is_allow_verdict(verdict: &str) -> bool {
    matches!(
        verdict,
        "allow" | "pass" | "proceed" | "fire" | "ok" | "delete" | "keep" | "yes" | "true"
    )
}
