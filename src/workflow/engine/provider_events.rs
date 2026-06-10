use anyhow::{Result, anyhow, bail};
use serde_json::{Value, json};

use crate::transcripts::adapters::TranscriptAdapterRegistry;
use crate::transcripts::cursor_store::TranscriptCursorStore;
use crate::transcripts::types::{NormalizedTranscriptEvent, TranscriptCursor};
use crate::workflow::wait::ProviderEventWait;

use super::WorkflowRunner;
use crate::orchestration as orch;

impl WorkflowRunner<'_> {
    pub(super) async fn run_provider_event_wait_node(
        &mut self,
        node_id: &str,
        spec: &ProviderEventWait,
        timeout: Option<std::time::Duration>,
    ) -> Result<()> {
        let task_id = self
            .actor_tasks
            .get(&spec.actor)
            .cloned()
            .or_else(|| self.find_actor_task_id(&spec.actor))
            .ok_or_else(|| anyhow!("provider_event wait actor '{}' has no task", spec.actor))?;
        let task = self
            .server
            .state
            .task_store
            .read()
            .get(&task_id)
            .ok_or_else(|| anyhow!("provider_event wait task '{task_id}' not found"))?;
        let deadline = timeout.map(|d| std::time::Instant::now() + d);
        let mut consecutive_errors = 0usize;

        loop {
            if self.cancel_token.is_cancelled() {
                self.log_event(
                    "provider_event_cancelled",
                    json!({"node": node_id, "actor": spec.actor, "task_id": task_id}),
                );
                bail!("arc cancelled");
            }
            if deadline.is_some_and(|d| std::time::Instant::now() >= d) {
                self.log_event(
                    "provider_event_timeout",
                    json!({"node": node_id, "actor": spec.actor, "task_id": task_id}),
                );
                self.record_output(
                    node_id,
                    json!({"name": "__timeout__", "actor": spec.actor, "taskId": task_id})
                        .to_string(),
                );
                self.arc_note(
                    "surprise",
                    &format!(
                        "Provider-event wait '{node_id}' timed out after {:?}",
                        timeout
                    ),
                );
                return Ok(());
            }

            match self.read_provider_event_batch(&task) {
                Ok((events, cursor)) => {
                    consecutive_errors = 0;
                    if let Some(ref cursor) = cursor {
                        {
                            let mut inner = task.inner.lock();
                            inner.transcript_cursor = Some(cursor.clone());
                        }
                        crate::orchestration::request_persist(
                            &self.server.state.task_store,
                            &self.server.state.store_dir,
                        );
                    }
                    if let Some(event) = events
                        .iter()
                        .find(|event| provider_event_matches(spec, event))
                    {
                        let payload = provider_event_payload(&task_id, &spec.actor, event);
                        self.log_event(
                            "provider_event_resolved",
                            json!({
                                "node": node_id,
                                "actor": spec.actor,
                                "task_id": task_id,
                                "event": payload,
                            }),
                        );
                        self.record_output(node_id, payload.to_string());
                        self.arc_note(
                            "done",
                            &format!(
                                "Provider-event wait '{node_id}' resolved for actor '{}'",
                                spec.actor
                            ),
                        );
                        return Ok(());
                    }
                }
                Err(err) => {
                    consecutive_errors += 1;
                    let err_msg = err.to_string();
                    self.log_event(
                        "provider_event_read_error",
                        json!({
                            "node": node_id,
                            "actor": spec.actor,
                            "task_id": task_id,
                            "attempt": consecutive_errors,
                            "error": err_msg,
                        }),
                    );
                    if consecutive_errors >= 3 {
                        self.arc_note(
                            "blocked",
                            &format!(
                                "provider_event read failed after {consecutive_errors} retries: {err_msg}"
                            ),
                        );
                        bail!(
                            "provider_event read failed after {consecutive_errors} retries: {err_msg}"
                        );
                    }
                }
            }

            tokio::time::sleep(provider_event_retry_delay(deadline)).await;
        }
    }

    fn find_actor_task_id(&self, actor: &str) -> Option<String> {
        self.ctx.actor_results.values().find_map(|result| {
            (result.get("actor").and_then(|v| v.as_str()) == Some(actor))
                .then(|| {
                    result
                        .get("taskId")
                        .and_then(|v| v.as_str())
                        .map(str::to_string)
                })
                .flatten()
        })
    }

    fn read_provider_event_batch(
        &self,
        task: &std::sync::Arc<orch::Task>,
    ) -> Result<(Vec<NormalizedTranscriptEvent>, Option<TranscriptCursor>)> {
        let (provider, session_id, location, cursor) = {
            let inner = task.inner.lock();
            (
                inner.provider,
                inner.session_id.clone(),
                inner.transcript_location.clone(),
                inner.transcript_cursor.clone(),
            )
        };
        if session_id.is_empty() || session_id == "pending" {
            bail!("provider {provider} has no resolved session id yet");
        }
        let config = self.server.state.idx.read().reindex_config();
        let registry = TranscriptAdapterRegistry::from_reindex_config(&config);
        let adapter = registry
            .adapter(provider)
            .ok_or_else(|| anyhow!("no transcript adapter registered for provider {provider}"))?;
        let location = match location {
            Some(location) => location,
            None => adapter
                .locate(&session_id)
                .map_err(|err| anyhow!("locate {provider}/{session_id}: {err}"))?
                .ok_or_else(|| anyhow!("no transcript location for {provider}/{session_id}"))?,
        };
        {
            let mut inner = task.inner.lock();
            if inner.transcript_location.is_none() {
                inner.transcript_location = Some(location.clone());
            }
        }
        let mut cursor_store = TranscriptCursorStore::load(
            TranscriptCursorStore::default_path_for_provider(provider.as_str()),
        )
        .unwrap_or_else(|_| TranscriptCursorStore::default_for_provider(provider.as_str()));
        let durable_cursor = cursor
            .clone()
            .or_else(|| cursor_store.get(&session_id, &location).cloned());
        let batch = adapter
            .read_since(&location, durable_cursor.as_ref())
            .map_err(|err| anyhow!("read {provider}/{session_id}: {err}"))?;
        if let Some(ref cursor) = batch.cursor {
            cursor_store.set(&session_id, &location, cursor.clone());
            let _ = cursor_store.save();
        }
        Ok((batch.events, batch.cursor))
    }
}

fn provider_event_matches(spec: &ProviderEventWait, event: &NormalizedTranscriptEvent) -> bool {
    if let Some(kind) = spec.kind.as_deref()
        && transcript_event_kind_name(event) != kind
    {
        return false;
    }
    if let Some(tool) = spec.tool.as_deref() {
        let event_tool = event.tool_call.as_ref().map(|call| call.name.as_str());
        if event_tool != Some(tool) {
            return false;
        }
    }
    if let Some(needle) = spec.contains.as_deref()
        && !event.content.contains(needle)
    {
        return false;
    }
    true
}

fn transcript_event_kind_name(event: &NormalizedTranscriptEvent) -> &'static str {
    match event.kind {
        crate::transcripts::types::TranscriptEventKind::Message => "message",
        crate::transcripts::types::TranscriptEventKind::Thinking => "thinking",
        crate::transcripts::types::TranscriptEventKind::ToolUse => "tool_use",
        crate::transcripts::types::TranscriptEventKind::ToolResult => "tool_result",
        crate::transcripts::types::TranscriptEventKind::Developer => "developer",
    }
}

fn provider_event_payload(task_id: &str, actor: &str, event: &NormalizedTranscriptEvent) -> Value {
    json!({
        "taskId": task_id,
        "actor": actor,
        "provider": event.source,
        "sessionId": event.session_id,
        "kind": transcript_event_kind_name(event),
        "role": format!("{:?}", event.role).to_lowercase(),
        "content": event.content,
        "tool": event.tool_call.as_ref().map(|call| call.name.clone()),
        "cursor": event.raw.entity_id.clone().or_else(|| event.jsonl_entity_id()),
        "path": event.raw.path,
    })
}

fn provider_event_retry_delay(deadline: Option<std::time::Instant>) -> std::time::Duration {
    let default = std::time::Duration::from_secs(20);
    let Some(deadline) = deadline else {
        return default;
    };
    let remaining = deadline.saturating_duration_since(std::time::Instant::now());
    if remaining.is_zero() {
        std::time::Duration::from_millis(1)
    } else {
        remaining.min(default)
    }
}
