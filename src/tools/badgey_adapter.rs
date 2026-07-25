use std::collections::HashMap;
use std::sync::Arc;

use crate::notes;
use crate::orchestration::{self, providers::Provider};
use crate::server::state::{BlackboxServer, SharedState};
use serde_json::{Value, json};

pub(crate) struct BadgeyAgentAdapter {
    pub(crate) state: Arc<SharedState>,
}

pub(crate) fn restore_badgey_registry_from_notes(state: &Arc<SharedState>) {
    let mut thread_badgey_ids: HashMap<String, orchestration::badgey::types::BadgeyId> =
        HashMap::new();
    let threads = state.threads.read().all().to_vec();
    for thread in threads {
        if let Some(name) = thread.name.as_deref() {
            if let Some(raw) = name.strip_prefix("badgey:") {
                if let Ok(id) = raw.parse() {
                    thread_badgey_ids.insert(thread.id.clone(), id);
                }
            }
        }
    }

    let mut latest_execs: HashMap<
        orchestration::badgey::types::BadgeyId,
        (
            String,
            String,
            orchestration::badgey::types::BadgeyScope,
            Provider,
            String,
        ),
    > = HashMap::new();
    let mut latest_dismissed: HashMap<orchestration::badgey::types::BadgeyId, String> =
        HashMap::new();
    let notes = state.notes.read().all().to_vec();
    for note in notes {
        let Some(thread_id) = note.thread_id.as_deref() else {
            continue;
        };
        let Some(id) = thread_badgey_ids.get(thread_id).cloned() else {
            continue;
        };
        let Ok(event) =
            serde_json::from_str::<orchestration::badgey::events::ThreadEvent>(&note.body)
        else {
            continue;
        };
        match event {
            orchestration::badgey::events::ThreadEvent::Exec {
                scope,
                provider,
                provider_session_id,
                ..
            } => {
                let replace = latest_execs
                    .get(&id)
                    .is_none_or(|(created_at, ..)| note.created_at > *created_at);
                if replace {
                    latest_execs.insert(
                        id,
                        (
                            note.created_at.clone(),
                            thread_id.to_string(),
                            scope,
                            provider,
                            provider_session_id,
                        ),
                    );
                }
            }
            orchestration::badgey::events::ThreadEvent::Dismiss { .. } => {
                let replace = latest_dismissed
                    .get(&id)
                    .is_none_or(|created_at| note.created_at > *created_at);
                if replace {
                    latest_dismissed.insert(id, note.created_at.clone());
                }
            }
            _ => {}
        }
    }
    for (id, (exec_at, thread_id, scope, provider, provider_session_id)) in latest_execs {
        if provider_session_id == "pending" {
            let _ = state.notes.write().create(&notes::NoteParams {
                kind: "surprise".to_string(),
                body: json!({
                    "event": "badgey_restore_skipped_unobserved_session",
                    "badgey_id": id,
                    "reason": "exec event had no observed provider session id"
                })
                .to_string(),
                task_id: None,
                session_id: None,
                project: Some(scope.project_id),
                project_id: None,
                thread_id: Some(thread_id),
                provider: Some(provider.as_str().to_string()),
                bro: Some("badgey".to_string()),
            });
            continue;
        }
        let instance = orchestration::badgey::registry::BadgeyInstance::new(
            id.clone(),
            scope,
            provider,
            provider_session_id,
            thread_id,
        );
        let _ = state.consultant_registry.register(instance);
        if latest_dismissed
            .get(&id)
            .is_some_and(|dismissed_at| *dismissed_at > exec_at)
        {
            let _ = state.consultant_registry.dismiss(&id);
        }
    }
}

pub(crate) fn recover_badgey_non_terminal_state(state: &Arc<SharedState>) {
    use orchestration::badgey::types::{ActionJournalState, ProposalState};

    if let Ok(entries) = state.consultant_journal.list_non_terminal() {
        for entry in entries {
            match entry.state.clone() {
                ActionJournalState::Seen => {
                    let _ = state.consultant_journal.transition(
                        &entry.action_id,
                        ActionJournalState::Seen,
                        ActionJournalState::Failed {
                            reason: "daemon restart before action dispatch".to_string(),
                        },
                        Some("startup recovery failed un-dispatched action".to_string()),
                    );
                }
                ActionJournalState::Dispatching { task_id } => {
                    let terminal = state.task_store.read().get(&task_id).map(|task| {
                        let inner = task.inner.lock();
                        if inner.status == orchestration::TaskStatus::Completed {
                            ActionJournalState::Completed {
                                result_ref: format!("task:{task_id}"),
                            }
                        } else if inner.status.is_terminal() {
                            ActionJournalState::Failed {
                                reason: format!("task {task_id} ended with {:?}", inner.status),
                            }
                        } else {
                            ActionJournalState::Dispatching {
                                task_id: task_id.clone(),
                            }
                        }
                    });
                    let to = terminal.unwrap_or_else(|| ActionJournalState::Failed {
                        reason: format!("dispatched task {task_id} not found after restart"),
                    });
                    if !matches!(to, ActionJournalState::Dispatching { .. }) {
                        let _ = state.consultant_journal.transition(
                            &entry.action_id,
                            entry.state,
                            to,
                            Some("startup recovery reconciled dispatched action".to_string()),
                        );
                    }
                }
                ActionJournalState::Completed { .. } | ActionJournalState::Failed { .. } => {}
            }
        }
    }

    if let Ok(proposals) = state.consultant_proposals.list_non_terminal() {
        for proposal in proposals {
            if proposal.state != ProposalState::Applying {
                continue;
            }
            let to = match proposal.applied_task_id.as_deref() {
                Some(task_id) => state.task_store.read().get(task_id).map_or_else(
                    || ProposalState::Failed,
                    |task| {
                        let status = task.inner.lock().status;
                        if status == orchestration::TaskStatus::Completed {
                            ProposalState::Applied
                        } else if status.is_terminal() {
                            ProposalState::Failed
                        } else {
                            ProposalState::Applying
                        }
                    },
                ),
                None => ProposalState::Failed,
            };
            if to != ProposalState::Applying {
                let note = if to == ProposalState::Applied {
                    "startup recovery observed applied task completion"
                } else {
                    "startup recovery failed orphaned applying proposal"
                };
                let _ = state.consultant_proposals.transition(
                    &proposal.instance_id,
                    &proposal.id,
                    ProposalState::Applying,
                    to,
                    Some(note.to_string()),
                );
            }
        }
    }
}

impl orchestration::agents::adapter::AgentDispatchAdapter for BadgeyAgentAdapter {
    fn name(&self) -> &'static str {
        "badgey"
    }

    fn dispatch(
        &self,
        _manifest: &orchestration::agents::types::AgentManifest,
        args: Value,
        ctx: orchestration::agents::adapter::DispatchContext,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<
                    Output = Result<
                        orchestration::agents::adapter::AgentDispatchResult,
                        orchestration::agents::adapter::AgentDispatchError,
                    >,
                > + Send
                + '_,
        >,
    > {
        let state = self.state.clone();
        Box::pin(async move {
            use orchestration::agents::adapter::{
                AgentDispatchError, AgentDispatchResult, DispatchDegraded,
            };
            use orchestration::agents::types::{AgentRef, AgentSession, MergedFilters};

            let server = BlackboxServer::new(state);
            let project_dir = args
                .get("project_dir")
                .and_then(Value::as_str)
                .map(String::from)
                .or(ctx.project_dir);
            let result = if let Some(badgey_id) = args.get("badgey_id").and_then(Value::as_str) {
                let prompt = args
                    .get("prompt")
                    .or_else(|| args.get("question"))
                    .and_then(Value::as_str)
                    .unwrap_or("");
                if prompt.trim().is_empty() {
                    return Err(AgentDispatchError::BadInput {
                        message: "badgey adapter resume requires args.prompt or args.question"
                            .to_string(),
                    });
                }
                server
                    .consultant_resume_internal(
                        orchestration::badgey::descriptor(),
                        badgey_id,
                        prompt,
                        None,
                    )
                    .await
                    .map_err(|message| AgentDispatchError::AdapterFailed { message })?
            } else {
                let brief = args
                    .get("brief")
                    .or_else(|| args.get("prompt"))
                    .or_else(|| args.get("question"))
                    .and_then(Value::as_str)
                    .map(String::from);
                server
                    .consultant_exec_internal(
                        orchestration::badgey::descriptor(),
                        project_dir.clone(),
                        brief,
                        ctx.bro_label_prefix.clone(),
                    )
                    .await
                    .map_err(|message| AgentDispatchError::AdapterFailed { message })?
            };
            let session_id = result
                .get("session_id")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            let provider = result
                .get("provider")
                .and_then(Value::as_str)
                .unwrap_or("unknown")
                .to_string();
            let task_id = result
                .get("task_id")
                .and_then(Value::as_str)
                .map(String::from);
            let degraded = result.get("degraded").map(|_| DispatchDegraded {
                reasons: vec!["badgey reported degraded status".to_string()],
            });
            let merged_filters = result
                .get("merged_filters")
                .and_then(|value| serde_json::from_value::<MergedFilters>(value.clone()).ok())
                .unwrap_or_default();
            Ok(AgentDispatchResult {
                session: AgentSession {
                    session_id,
                    provider,
                    project_dir,
                    agent: AgentRef {
                        name: "badgey".to_string(),
                        version: 1,
                    },
                    task_id,
                },
                resolved_brofile: Some(orchestration::badgey::descriptor().brofile_ref.to_string()),
                merged_filters,
                degraded,
            })
        })
    }
}
