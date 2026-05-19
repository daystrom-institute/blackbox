use std::sync::Arc;
use std::time::Duration;

use anyhow::{Result, anyhow, bail};
use serde_json::{Map, Value, json};
use tokio::sync::Notify;

use crate::workflow::context::SignalRef;
use crate::workflow::wait::{PendingWait, WaitSpec, canonicalize_correlation, matches_correlation};

use super::WorkflowRunner;

impl WorkflowRunner<'_> {
    pub(super) async fn run_sleep_node(&mut self, node_id: &str, duration_ms: u64) -> Result<()> {
        let duration = Duration::from_millis(duration_ms);
        self.log_event(
            "sleep_started",
            json!({
                "node": node_id,
                "duration_ms": duration_ms,
            }),
        );

        tokio::select! {
            _ = tokio::time::sleep(duration) => {
                let output = json!({
                    "kind": "sleep",
                    "duration_ms": duration_ms,
                    "status": "elapsed",
                });
                self.record_output(node_id, output.to_string());
                self.log_event(
                    "sleep_elapsed",
                    json!({
                        "node": node_id,
                        "duration_ms": duration_ms,
                    }),
                );
                Ok(())
            }
            _ = self.cancel_token.cancelled() => {
                self.log_event(
                    "sleep_cancelled",
                    json!({
                        "node": node_id,
                        "duration_ms": duration_ms,
                    }),
                );
                bail!("arc cancelled")
            }
        }
    }

    /// Wait node: register pending waits in the server's WaitStore, suspend on
    /// a Notify, and resume when a matching signal arrives.
    pub(super) async fn run_wait_node(&mut self, node_id: &str, spec: &WaitSpec) -> Result<()> {
        if let Some(provider_event) = spec.provider_event.as_ref() {
            return self
                .run_provider_event_wait_node(node_id, provider_event, spec.timeout)
                .await;
        }

        self.ctx.clear_last_signal();

        let mut registered_ids: Vec<(String, String)> = Vec::new();
        let mut registered_waits: Vec<(String, String, String, Map<String, Value>)> = Vec::new();
        let resolved_slot: Arc<parking_lot::Mutex<Option<SignalRef>>> =
            Arc::new(parking_lot::Mutex::new(None));
        let notify = Arc::new(Notify::new());
        let arc_id = self.ctx.meta.arc_id.clone();

        for (idx, wait_signal) in spec.any_of.iter().enumerate() {
            let context_entity = self.ctx.flatten();
            let mut correlation = Map::new();
            for (k, sel) in &wait_signal.correlate {
                let v = sel
                    .evaluate(&context_entity)
                    .map_err(|e| anyhow!("Wait correlation eval for '{k}': {e}"))?;
                correlation.insert(k.clone(), v);
            }
            let wait_id = format!("{node_id}#{idx}");
            self.log_event(
                "wait_registered",
                json!({
                    "node": node_id,
                    "wait_id": wait_id,
                    "signal": wait_signal.signal,
                    "correlation_canonical": canonicalize_correlation(&correlation),
                }),
            );
            self.server.wait_store().register(PendingWait {
                arc_id: arc_id.clone(),
                wait_id: wait_id.clone(),
                signal: wait_signal.signal.clone(),
                correlation: correlation.clone(),
                notify: notify.clone(),
                resolved: resolved_slot.clone(),
            });
            self.emit_arc_system_event(
                crate::system_events::types::SystemEventKind::WorkflowArcWaitRegistered,
                json!({
                    "arc_id": arc_id,
                    "wait_id": wait_id,
                    "node": node_id,
                    "signal": wait_signal.signal,
                }),
            )
            .await;
            registered_ids.push((arc_id.clone(), wait_id));
            registered_waits.push((
                arc_id.clone(),
                format!("{node_id}#{idx}"),
                wait_signal.signal.clone(),
                correlation,
            ));
        }

        for (arc, wait_id, signal, correlation) in &registered_waits {
            let Ok(events) =
                self.server
                    .state
                    .system_events
                    .list_events(Some(128), Some(signal), None, None)
            else {
                continue;
            };
            if let Some(event) = events
                .into_iter()
                .find(|event| matches_correlation(correlation, &event.correlation))
                && let Some((resolved, notify, _, _)) =
                    self.server.wait_store().take_exact(arc, wait_id)
            {
                *resolved.lock() = Some(SignalRef {
                    name: signal.clone(),
                    payload: serde_json::to_value(&event).unwrap_or_else(|e| {
                        json!({
                            "event_id": event.id,
                            "kind": signal,
                            "serialization_error": e.to_string(),
                        })
                    }),
                    correlation: event.correlation,
                    received_at: crate::util::now_iso(),
                });
                notify.notify_one();
                break;
            }
        }

        let timeout = spec.timeout;
        let cancel_token = self.cancel_token.clone();
        enum WaitOutcome {
            Resolved,
            Cancelled,
            TimedOut,
        }
        let outcome = match timeout {
            Some(d) => tokio::select! {
                _ = notify.notified() => WaitOutcome::Resolved,
                _ = cancel_token.cancelled() => WaitOutcome::Cancelled,
                _ = tokio::time::sleep(d) => WaitOutcome::TimedOut,
            },
            None => tokio::select! {
                _ = notify.notified() => WaitOutcome::Resolved,
                _ = cancel_token.cancelled() => WaitOutcome::Cancelled,
            },
        };

        for (arc, wid) in &registered_ids {
            self.server.wait_store().cancel(arc, wid);
        }

        if matches!(outcome, WaitOutcome::Cancelled) {
            self.log_event(
                "wait_cancelled",
                json!({
                    "node": node_id,
                    "registered_waits": registered_ids
                        .iter()
                        .map(|(_, w)| w.clone())
                        .collect::<Vec<_>>(),
                }),
            );
            bail!("arc cancelled");
        }
        let waited = matches!(outcome, WaitOutcome::Resolved);

        if !waited {
            let sig = SignalRef {
                name: "__timeout__".into(),
                payload: json!({
                    "expired": spec.any_of.iter().map(|s| s.signal.clone()).collect::<Vec<_>>(),
                }),
                correlation: Map::new(),
                received_at: crate::util::now_iso(),
            };
            self.log_event(
                "wait_timeout",
                json!({
                    "node": node_id,
                    "expired_signals": sig.payload["expired"].clone(),
                }),
            );
            self.ctx.record_signal(sig.clone());
            self.record_output(node_id, serde_json::to_string(&sig).unwrap_or_default());
            self.arc_note(
                "surprise",
                &format!("Wait '{node_id}' timed out after {:?}", timeout),
            );
            return Ok(());
        }

        let sig = resolved_slot
            .lock()
            .clone()
            .ok_or_else(|| anyhow!("Wait '{node_id}' notified but resolved slot empty"))?;
        self.log_event(
            "wait_resolved",
            json!({
                "node": node_id,
                "signal": sig.name,
                "correlation": sig.correlation,
            }),
        );
        self.emit_arc_system_event(
            crate::system_events::types::SystemEventKind::WorkflowArcSignalReceived,
            json!({
                "arc_id": self.ctx.meta.arc_id,
                "node": node_id,
                "signal": sig.name,
            }),
        )
        .await;
        self.record_output(node_id, serde_json::to_string(&sig).unwrap_or_default());
        self.ctx.record_signal(sig.clone());
        self.arc_note(
            "done",
            &format!("Wait '{node_id}' resolved by signal '{}'", sig.name),
        );
        Ok(())
    }
}
