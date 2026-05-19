use crate::tools::bro_params::{AtomInvokeParams, AtomResumeParams, AtomStatusParams};
use crate::{BlackboxServer, orchestration};

impl BlackboxServer {
    fn supervision_plan_defaults() -> orchestration::atoms::types::SupervisionPlanDefaults {
        use orchestration::atoms::types::{AtomRef, SupervisionClassifierMode};

        orchestration::atoms::types::SupervisionPlanDefaults {
            default_classifier_atom: Some(AtomRef::pinned("supervision-classifier", 1)),
            default_classifier_mode: Some(SupervisionClassifierMode::CadenceOrAlert),
            default_advisor_atom: Some(AtomRef::pinned("supervision-advisor", 1)),
        }
    }

    pub(super) fn normalized_supervision_plan_for_invoke(
        &self,
        manifest: &orchestration::atoms::types::AtomManifest,
        invoke_override: Option<&orchestration::atoms::types::SupervisionPlanOverride>,
    ) -> Result<orchestration::atoms::types::SupervisionPlan, String> {
        let plan = orchestration::atoms::types::SupervisionPlan::normalize(
            manifest.supervision.as_ref(),
            None,
            invoke_override,
            &Self::supervision_plan_defaults(),
        )?;
        Self::reject_unimplemented_runtime_intents(&plan)?;
        Ok(plan)
    }

    fn reject_unimplemented_runtime_intents(
        plan: &orchestration::atoms::types::SupervisionPlan,
    ) -> Result<(), String> {
        if plan.classifier.runtime.is_some()
            || plan.advisor.runtime.is_some()
            || plan.recovery.runtime.is_some()
        {
            return Err(
                "error.unsupported_runtime_allocation: supervision runtime intent is validated but allocator dispatch is not wired yet"
                    .into(),
            );
        }
        Ok(())
    }

    pub(super) async fn start_supervision_for_primary_invocation(
        &self,
        primary_invocation_id: &str,
        primary_task_id: &str,
        owner: &str,
        project_dir: Option<String>,
        plan: &orchestration::atoms::types::SupervisionPlan,
    ) -> Result<serde_json::Value, String> {
        use orchestration::atoms::invocation::SupervisionAttachment;
        use orchestration::atoms::types::{SupervisionAdvisorMode, SupervisionClassifierMode};

        if plan.classifier.mode == SupervisionClassifierMode::None
            && plan.advisor.mode == SupervisionAdvisorMode::None
        {
            return Ok(serde_json::json!({"enabled": false}));
        }

        let supervision_run_id = format!("sup-{}", uuid::Uuid::new_v4().simple());
        let mut attachment = SupervisionAttachment {
            supervision_run_id: supervision_run_id.clone(),
            primary_invocation_id: primary_invocation_id.to_string(),
            primary_task_id: primary_task_id.to_string(),
            classifier_invocation_id: None,
            advisor_invocation_id: None,
            attempt: 1,
        };
        self.state
            .atom_invocation_store
            .write()
            .insert_attachment(attachment.clone());

        let mut classifier_error: Option<String> = None;
        if plan.classifier.mode != SupervisionClassifierMode::None {
            let classifier_ref = plan
                .classifier
                .atom_ref
                .as_ref()
                .ok_or("enabled classifier supervision requires classifier.atom_ref")?
                .render();
            let classifier_result = Box::pin(self.atom_invoke_value(
                AtomInvokeParams {
                    atom: classifier_ref,
                    args: serde_json::json!({
                        "primary_invocation_id": primary_invocation_id,
                        "attempt": 1,
                        "tail_policy": plan.tail_policy,
                        "alerting_classifications": plan.classifier.alerting_classifications,
                    }),
                    project_dir: project_dir.clone(),
                    owner: Some(owner.to_string()),
                    parent_invocation_id: None,
                    runtime: None,
                    supervision_override: None,
                    suppress_auto_supervision: true,
                },
                None,
            ))
            .await;
            match classifier_result {
                Ok(classifier_result) => {
                    attachment.classifier_invocation_id = classifier_result
                        .get("invocation_id")
                        .and_then(serde_json::Value::as_str)
                        .map(str::to_string);
                    self.state
                        .atom_invocation_store
                        .write()
                        .insert_attachment(attachment.clone());
                    if plan.advisor.mode == SupervisionAdvisorMode::OnAlert {
                        self.spawn_on_alert_advisor_watch(
                            attachment.clone(),
                            classifier_result,
                            owner.to_string(),
                            project_dir.clone(),
                            plan.clone(),
                        );
                    }
                }
                Err(err) if !plan.classifier.required => {
                    classifier_error = Some(err);
                }
                Err(err) => return Err(err),
            }
        }

        if plan.advisor.mode == SupervisionAdvisorMode::Always {
            let advisor_ref = plan
                .advisor
                .atom_ref
                .as_ref()
                .ok_or("enabled advisor supervision requires advisor.atom_ref")?
                .render();
            let advisor_result = Box::pin(self.atom_invoke_value(
                AtomInvokeParams {
                    atom: advisor_ref,
                    args: serde_json::json!({
                        "primary_invocation_id": primary_invocation_id,
                        "attempt": 1,
                        "tail_policy": plan.tail_policy,
                        "classifier_findings": [],
                        "acceptance_criteria": "Assess whether the supervised atom completed the requested work correctly and safely.",
                        "attempt_history": [],
                        "allowed_actions": ["accept", "continue_observing", "steer_primary", "cancel_and_retry", "escalate_human", "bail"],
                        "recovery_policy": plan.recovery,
                    }),
                    project_dir,
                    owner: Some(owner.to_string()),
                    parent_invocation_id: None,
                    runtime: None,
                    supervision_override: None,
                    suppress_auto_supervision: true,
                },
                None,
            ))
            .await?;
            attachment.advisor_invocation_id = advisor_result
                .get("invocation_id")
                .and_then(serde_json::Value::as_str)
                .map(str::to_string);
            self.state
                .atom_invocation_store
                .write()
                .insert_attachment(attachment.clone());
        }

        Ok(serde_json::json!({
            "enabled": true,
            "supervision_run_id": supervision_run_id,
            "attempt": 1,
            "classifier_invocation_id": attachment.classifier_invocation_id,
            "advisor_invocation_id": attachment.advisor_invocation_id,
            "classifier_error": classifier_error,
        }))
    }

    fn spawn_on_alert_advisor_watch(
        &self,
        attachment: orchestration::atoms::invocation::SupervisionAttachment,
        classifier_result: serde_json::Value,
        owner: String,
        project_dir: Option<String>,
        plan: orchestration::atoms::types::SupervisionPlan,
    ) {
        let Some(classifier_invocation_id) = attachment.classifier_invocation_id.clone() else {
            return;
        };
        let Some(classifier_task_id) = classifier_result
            .get("task_id")
            .and_then(serde_json::Value::as_str)
            .map(str::to_string)
        else {
            return;
        };
        let state = self.state.clone();
        tokio::spawn(async move {
            let server = BlackboxServer::new(state.clone());
            let task = {
                let task_store = state.task_store.read();
                task_store.get(&classifier_task_id)
            };
            if let Some(task) = task {
                let should_wait = {
                    let inner = task.inner.lock();
                    matches!(inner.status, orchestration::TaskStatus::Running)
                };
                if should_wait {
                    task.notify.notified().await;
                }
            }
            let Ok(status) = server.atom_status_value(AtomStatusParams {
                invocation_id: classifier_invocation_id,
                owner: Some(owner.clone()),
            }) else {
                return;
            };
            let structured = status
                .get("structured_output")
                .cloned()
                .unwrap_or(serde_json::Value::Null);
            let should_advise = structured
                .get("status")
                .and_then(serde_json::Value::as_str)
                .is_some_and(|status| status == "alert" || status == "classifier_failed");
            if !should_advise {
                return;
            }
            let Some(advisor_ref) = plan.advisor.atom_ref.as_ref().map(|atom| atom.render()) else {
                return;
            };
            let advisor_result = Box::pin(server.atom_invoke_value(
                AtomInvokeParams {
                    atom: advisor_ref,
                    args: serde_json::json!({
                        "primary_invocation_id": attachment.primary_invocation_id,
                        "attempt": attachment.attempt,
                        "tail_policy": plan.tail_policy,
                        "classifier_findings": [structured],
                        "acceptance_criteria": "Assess the classifier alert and choose a safe supervision action.",
                        "attempt_history": [],
                        "allowed_actions": ["accept", "continue_observing", "steer_primary", "cancel_and_retry", "escalate_human", "bail"],
                        "recovery_policy": plan.recovery,
                    }),
                    project_dir,
                    owner: Some(owner),
                    parent_invocation_id: None,
                    runtime: None,
                    supervision_override: None,
                    suppress_auto_supervision: true,
                },
                None,
            ))
            .await;
            if let Ok(advisor_result) = advisor_result {
                let mut updated = attachment;
                updated.advisor_invocation_id = advisor_result
                    .get("invocation_id")
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_string);
                state
                    .atom_invocation_store
                    .write()
                    .insert_attachment(updated);
            }
        });
    }

    fn bounded_tail(&self, text: &str, max_bytes: usize) -> String {
        if text.is_empty() {
            return String::new();
        }
        if max_bytes == 0 {
            return String::new();
        }
        if text.len() <= max_bytes {
            return text.to_string();
        }
        let mut start = text.len().saturating_sub(max_bytes);
        while !text.is_char_boundary(start) {
            start += 1;
        }
        text[start..].to_string()
    }

    fn resolve_attachment_for_poll(
        &self,
        primary_invocation_id: &str,
        attempt: Option<u64>,
    ) -> Result<orchestration::atoms::invocation::SupervisionAttachment, String> {
        let store = self.state.atom_invocation_store.read();
        let attachments = store.attachments_for_primary(primary_invocation_id);
        if attachments.is_empty() {
            return Err(format!(
                "attachment not found for primary invocation: {primary_invocation_id}"
            ));
        }
        match attempt {
            Some(expected) => attachments
                .iter()
                .find(|attached| attached.attempt == expected)
                .cloned()
                .ok_or_else(|| {
                    format!(
                        "attachment not found for primary invocation {primary_invocation_id} attempt {expected}"
                    )
                }),
            None => attachments
                .into_iter()
                .max_by_key(|attached| attached.attempt)
                .ok_or_else(|| {
                    format!(
                        "attachment not found for primary invocation: {primary_invocation_id}"
                    )
                }),
        }
    }

    fn authorize_attachment_read(
        &self,
        attachment: &orchestration::atoms::invocation::SupervisionAttachment,
        owner: &str,
    ) -> Result<(), String> {
        let store = self.state.atom_invocation_store.read();
        let lineage = [
            attachment.primary_invocation_id.as_str(),
            attachment.classifier_invocation_id.as_deref().unwrap_or(""),
            attachment.advisor_invocation_id.as_deref().unwrap_or(""),
        ];
        for inv_id in lineage {
            if inv_id.is_empty() {
                continue;
            }
            match store.get(inv_id) {
                Some(inv) if inv.is_owner(owner) => return Ok(()),
                Some(_) => {}
                None => {}
            }
        }
        Err("error.forbidden: caller is not authorized for this attachment lineage".into())
    }

    #[cfg(test)]
    pub(crate) fn attached_supervision_poll_value(
        &self,
        primary_invocation_id: &str,
        owner: &str,
        attempt: Option<u64>,
    ) -> Result<serde_json::Value, String> {
        self.attached_supervision_poll_value_with_tail(primary_invocation_id, owner, attempt, None)
    }

    pub(crate) fn attached_supervision_poll_value_with_tail(
        &self,
        primary_invocation_id: &str,
        owner: &str,
        attempt: Option<u64>,
        tail_policy: Option<&orchestration::atoms::types::SupervisionTailPolicy>,
    ) -> Result<serde_json::Value, String> {
        use orchestration::atoms::types::SupervisionTailPolicy;

        let default_limits = SupervisionTailPolicy::default();
        let limits = tail_policy.unwrap_or(&default_limits);
        let event_limit = usize::try_from(limits.events).unwrap_or(usize::MAX);
        let note_limit = usize::try_from(limits.notes).unwrap_or(usize::MAX);
        let text_limit = usize::try_from(limits.assistant_bytes).unwrap_or(usize::MAX);
        let report_limit = usize::try_from(limits.reports).unwrap_or(usize::MAX);
        let attachment = self.resolve_attachment_for_poll(primary_invocation_id, attempt)?;
        self.authorize_attachment_read(&attachment, owner)?;

        let store = self.state.atom_invocation_store.read();
        let Some(mut primary_invocation) = store.get(&attachment.primary_invocation_id).cloned()
        else {
            return Err(format!(
                "attachment references missing primary invocation: {}",
                attachment.primary_invocation_id
            ));
        };
        drop(store);

        self.refresh_atom_invocation_from_task(&mut primary_invocation);
        self.state
            .atom_invocation_store
            .write()
            .update(primary_invocation.clone());

        let task_id = primary_invocation.task_id().ok_or_else(|| {
            "error.internal: primary invocation has no associated task_id".to_string()
        })?;

        let task = {
            let task_store = self.state.task_store.read();
            task_store.get(&task_id)
        };
        let (
            task_status,
            supervision_snapshot,
            assistant_tail,
            latest_report,
            provider_events,
            task_notes,
            elapsed_ms,
        ) = if let Some(task) = task {
            let now_ms = orchestration::now_ms();
            {
                let mut inner = task.inner.lock();
                inner.supervision.observe_stall(now_ms);
            }
            let mut task_status = orchestration::task_status_json(&task, event_limit);
            let inner = task.inner.lock();
            let elapsed_ms = now_ms.saturating_sub(inner.started_at);
            if let Some(obj) = task_status.as_object_mut() {
                obj.insert(
                    "supervision".to_string(),
                    inner.supervision.snapshot(now_ms),
                );
            }

            let task_event_start = inner.events.len().saturating_sub(event_limit);
            let helper_events = inner.events[task_event_start..].to_vec();
            let notes = self
                .state
                .notes
                .read()
                .all()
                .iter()
                .rev()
                .filter(|note| note.task_id.as_deref() == Some(&task_id))
                .take(note_limit)
                .map(|note| {
                    serde_json::json!({
                        "id": note.id,
                        "kind": note.kind.as_ref(),
                        "created_at": note.created_at,
                        "body": self.bounded_tail(&note.body, text_limit),
                    })
                })
                .collect::<Vec<_>>();
            let latest_report = if report_limit > 0 {
                inner.report.as_ref().map(orchestration::BroReport::to_json)
            } else {
                None
            };
            let assistant_tail = inner
                .last_assistant_message
                .as_deref()
                .map(|m| self.bounded_tail(m, text_limit))
                .unwrap_or_default();
            (
                task_status,
                inner.supervision.snapshot(now_ms),
                assistant_tail,
                latest_report,
                helper_events,
                notes,
                elapsed_ms,
            )
        } else {
            let attached_task = serde_json::json!({
                "taskId": task_id,
                "status": "missing",
                "error": "linked task record not found",
                "supervision": serde_json::json!({"ok": false, "event_count": 0}),
            });
            let snapshot = orchestration::supervision::SupervisionState::default()
                .snapshot(orchestration::now_ms());
            (
                attached_task,
                snapshot,
                String::new(),
                None,
                Vec::new(),
                Vec::new(),
                0,
            )
        };
        Ok(serde_json::json!({
            "invocation": primary_invocation.to_trace_envelope(),
            "task": task_status,
            "attempt_metadata": {
                "supervision_run_id": attachment.supervision_run_id,
                "primary_invocation_id": attachment.primary_invocation_id,
                "primary_task_id": attachment.primary_task_id,
                "classifier_invocation_id": attachment.classifier_invocation_id,
                "advisor_invocation_id": attachment.advisor_invocation_id,
                "attempt": attachment.attempt,
            },
            "supervision": supervision_snapshot,
            "elapsed_ms": elapsed_ms,
            "recent_provider_events": serde_json::Value::Array(provider_events),
            "task_notes": serde_json::Value::Array(task_notes),
            "latest_bro_report": latest_report,
            "assistant_tail": assistant_tail,
        }))
    }

    pub(crate) async fn execute_supervision_action_value(
        &self,
        primary_invocation_id: &str,
        owner: &str,
        attempt: Option<u64>,
        action: serde_json::Value,
    ) -> Result<serde_json::Value, String> {
        use orchestration::atoms::invocation::{AtomHandle, InvocationStatus};

        let attachment = self.resolve_attachment_for_poll(primary_invocation_id, attempt)?;
        self.authorize_attachment_read(&attachment, owner)?;
        let action_kind = action
            .get("action")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| "action.action must be a string".to_string())?;
        let mut primary_invocation = {
            let store = self.state.atom_invocation_store.read();
            store
                .get(&attachment.primary_invocation_id)
                .cloned()
                .ok_or_else(|| {
                    format!(
                        "attachment references missing primary invocation: {}",
                        attachment.primary_invocation_id
                    )
                })?
        };
        self.refresh_atom_invocation_from_task(&mut primary_invocation);
        let primary_owner = primary_invocation
            .owners
            .iter()
            .next()
            .cloned()
            .ok_or_else(|| "primary invocation has no owner".to_string())?;
        let task_id = primary_invocation.task_id();

        let result = match action_kind {
            "accept" | "continue_observing" | "escalate_human" | "bail" => {
                serde_json::json!({
                    "status": "recorded",
                    "action": action_kind,
                    "mutated_primary": false,
                })
            }
            "steer_primary" => {
                match &primary_invocation.handle {
                    AtomHandle::Profile { .. } => {}
                    _ => {
                        return Err(
                            "error.incompatible_action: steer_primary requires a profile-backed primary"
                                .into(),
                        );
                    }
                }
                if !matches!(
                    primary_invocation.status,
                    InvocationStatus::Running
                        | InvocationStatus::Succeeded
                        | InvocationStatus::Failed
                        | InvocationStatus::TimedOut
                ) {
                    return Err(
                        "error.incompatible_action: primary invocation is not resumable".into(),
                    );
                }
                let prompt = action
                    .get("prompt")
                    .or_else(|| action.get("message"))
                    .and_then(serde_json::Value::as_str)
                    .filter(|value| !value.trim().is_empty())
                    .ok_or_else(|| {
                        "steer_primary requires action.prompt or action.message".to_string()
                    })?;
                let resume = self
                    .atom_resume_value(AtomResumeParams {
                        invocation_id: attachment.primary_invocation_id.clone(),
                        prompt: prompt.to_string(),
                        owner: Some(primary_owner),
                    })
                    .await?;
                serde_json::json!({
                    "status": "steered",
                    "action": action_kind,
                    "mutated_primary": true,
                    "resume": resume,
                })
            }
            "cancel_and_retry" => {
                if let Some(max_attempts) = action.get("max_attempts").and_then(|v| v.as_u64())
                    && attachment.attempt >= max_attempts
                {
                    return Err("error.retry_budget_exhausted: max_attempts reached".into());
                }
                let task_id = task_id.ok_or_else(|| {
                    "error.incompatible_action: cancel_and_retry requires a task-backed primary"
                        .to_string()
                })?;
                let task = {
                    let task_store = self.state.task_store.read();
                    task_store.get(&task_id).ok_or_else(|| {
                        format!("error.not_found: primary task '{task_id}' not found")
                    })?
                };
                orchestration::cancel_task(&task, &self.state.task_store, &self.state.store_dir)?;
                let replacement = if let Some(args) = action.get("args").cloned() {
                    let retry = Box::pin(self.atom_invoke_value(
                        AtomInvokeParams {
                            atom: primary_invocation.atom_ref.clone(),
                            args,
                            project_dir: None,
                            owner: Some(primary_owner),
                            parent_invocation_id: None,
                            runtime: None,
                            supervision_override: None,
                            suppress_auto_supervision: true,
                        },
                        None,
                    ))
                    .await?;
                    self.link_replacement_attempt(&attachment, attachment.attempt + 1, &retry);
                    Some(retry)
                } else {
                    None
                };
                serde_json::json!({
                    "status": "cancelled",
                    "action": action_kind,
                    "mutated_primary": true,
                    "task_id": task_id,
                    "replacement": replacement,
                })
            }
            "replace_primary" => {
                if let Some(max_attempts) = action.get("max_attempts").and_then(|v| v.as_u64())
                    && attachment.attempt >= max_attempts
                {
                    return Err("error.retry_budget_exhausted: max_attempts reached".into());
                }
                let replacement_atom = action
                    .get("atom_ref")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or(primary_invocation.atom_ref.as_str())
                    .to_string();
                let replacement_args = action.get("args").cloned().unwrap_or_else(|| {
                    serde_json::json!({
                        "prompt": action
                            .get("prompt")
                            .and_then(serde_json::Value::as_str)
                            .unwrap_or("")
                    })
                });
                if let Some(task_id) = task_id.as_deref() {
                    let task = {
                        let task_store = self.state.task_store.read();
                        task_store.get(task_id)
                    };
                    if let Some(task) = task {
                        let is_running = {
                            let inner = task.inner.lock();
                            matches!(inner.status, orchestration::TaskStatus::Running)
                        };
                        if is_running {
                            orchestration::cancel_task(
                                &task,
                                &self.state.task_store,
                                &self.state.store_dir,
                            )?;
                        }
                    }
                }
                let replacement = Box::pin(self.atom_invoke_value(
                    AtomInvokeParams {
                        atom: replacement_atom,
                        args: replacement_args,
                        project_dir: None,
                        owner: Some(primary_owner),
                        parent_invocation_id: None,
                        runtime: None,
                        supervision_override: None,
                        suppress_auto_supervision: true,
                    },
                    None,
                ))
                .await?;
                self.link_replacement_attempt(&attachment, attachment.attempt + 1, &replacement);
                serde_json::json!({
                    "status": "replaced",
                    "action": action_kind,
                    "mutated_primary": true,
                    "replacement": replacement,
                })
            }
            other => {
                return Err(format!(
                    "error.invalid_action: unsupported supervision action '{other}'"
                ));
            }
        };

        Ok(serde_json::json!({
            "supervision_run_id": attachment.supervision_run_id,
            "primary_invocation_id": attachment.primary_invocation_id,
            "attempt": attachment.attempt,
            "decision": action,
            "result": result,
        }))
    }

    fn link_replacement_attempt(
        &self,
        prior: &orchestration::atoms::invocation::SupervisionAttachment,
        attempt: u64,
        replacement: &serde_json::Value,
    ) {
        let Some(primary_invocation_id) = replacement
            .get("invocation_id")
            .and_then(serde_json::Value::as_str)
        else {
            return;
        };
        let Some(primary_task_id) = replacement
            .get("task_id")
            .and_then(serde_json::Value::as_str)
        else {
            return;
        };
        self.state.atom_invocation_store.write().insert_attachment(
            orchestration::atoms::invocation::SupervisionAttachment {
                supervision_run_id: prior.supervision_run_id.clone(),
                primary_invocation_id: primary_invocation_id.to_string(),
                primary_task_id: primary_task_id.to_string(),
                classifier_invocation_id: None,
                advisor_invocation_id: prior.advisor_invocation_id.clone(),
                attempt,
            },
        );
    }
}
