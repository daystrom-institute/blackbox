use std::sync::Arc;
use std::time::Duration;

use anyhow::{Result, anyhow, bail};
use serde_json::{Map, Value, json};
use tokio::sync::Notify;

use crate::workflow::context::SignalRef;
use crate::workflow::wait::{PendingWait, WaitSpec, canonicalize_correlation, matches_correlation};

use super::WorkflowRunner;

/// Remaining duration until an RFC 3339 deadline; zero when the
/// deadline already passed, None when the string does not parse.
fn remaining_until(deadline_iso: &str) -> Option<Duration> {
    let deadline = chrono::DateTime::parse_from_rfc3339(deadline_iso).ok()?;
    let remaining = deadline.signed_duration_since(chrono::Utc::now());
    Some(remaining.to_std().unwrap_or(Duration::ZERO))
}

/// Absolute RFC 3339 deadline `timeout` from now. Out-of-range spans
/// (WaitSpec allows absurdly large ones) clamp to ten years, which is
/// indistinguishable from "no deadline" for any real arc.
fn deadline_from_now(timeout: Duration) -> String {
    let span = chrono::Duration::from_std(timeout).unwrap_or_else(|_| chrono::Duration::days(3650));
    (chrono::Utc::now() + span).to_rfc3339()
}

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

        // Effective timeout: a fresh entry opens the spec's full window
        // and stamps its absolute deadline into the checkpoint; a
        // rehydration re-entry resumes the REMAINING window from that
        // stamped deadline (zero if it passed while the daemon was
        // down), so restarts can never extend a finite wait.
        let (timeout, waiting_deadline) = match self.resume_wait_deadline.take() {
            Some(deadline) => match remaining_until(&deadline) {
                Some(remaining) => (Some(remaining), Some(deadline)),
                None => {
                    tracing::warn!(
                        "arc {} wait '{node_id}': unparseable checkpoint deadline '{deadline}'; restarting the full window",
                        self.ctx.meta.arc_id
                    );
                    (spec.timeout, spec.timeout.map(deadline_from_now))
                }
            },
            None => (spec.timeout, spec.timeout.map(deadline_from_now)),
        };

        // Durable park, written BEFORE the registrations become visible
        // to the signal router: a daemon restart from any point in this
        // node rehydrates the arc by re-entering it (on_enter skipped),
        // re-deriving the same correlations from the restored context,
        // and re-running the ledger catch-up below against everything
        // that arrived while down. Ordering also keeps the write out of
        // the registration-to-park window, which stays as narrow as it
        // was before checkpointing existed.
        self.write_checkpoint_with_deadline(
            crate::workflow::arc_store::ArcCheckpointStatus::Waiting,
            node_id,
            waiting_deadline,
        )
        .await;

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
                    .list_events(Some(512), Some(signal), None, None)
            else {
                continue;
            };
            // list_events returns newest-first; consume the OLDEST
            // unconsumed match so a backlog drains in arrival order
            // instead of starving old events behind new ones.
            if let Some(event) = events.into_iter().rev().find(|event| {
                // Arc-targeted events (admission duplicate conversions)
                // are consumable ONLY by the arc they were queued for;
                // untargeted events match as before.
                let targeted_elsewhere = event
                    .correlation
                    .get("_target_arc")
                    .and_then(|v| v.as_str())
                    .is_some_and(|t| t != arc.as_str());
                !targeted_elsewhere
                    && !self.ctx.signal_event_consumed(&event.id)
                    && matches_correlation(correlation, &event.correlation)
            }) && let Some((resolved, notify, _, _)) =
                self.server.wait_store().take_exact(arc, wait_id)
            {
                // Router-persisted idle signals carry the caller's raw
                // payload; deliver that payload (not the event envelope)
                // so templates see the same shape as a live delivery.
                // Other producers' events keep the envelope form the
                // bridge has always delivered.
                let payload = if event.producer == "signal.router" {
                    event.payload.clone()
                } else {
                    serde_json::to_value(&event).unwrap_or_else(|e| {
                        json!({
                            "event_id": event.id,
                            "kind": signal,
                            "serialization_error": e.to_string(),
                        })
                    })
                };
                // First-writer-wins: the live bridge may have resolved
                // the group between registration and this catch-up.
                {
                    let mut slot = resolved.lock();
                    if slot.is_none() {
                        *slot = Some(SignalRef {
                            name: signal.clone(),
                            payload,
                            correlation: event.correlation,
                            received_at: crate::util::now_iso(),
                            source_event_id: Some(event.id),
                        });
                    }
                }
                // Drop sibling registrations so a racing signal cannot
                // consume one and contend for the already-filled slot.
                self.server.wait_store().cancel_node_group(arc, node_id);
                notify.notify_one();
                break;
            }
        }

        let cancel_token = self.cancel_token.clone();
        enum WaitOutcome {
            Resolved,
            Cancelled,
            TimedOut,
        }
        // `biased` makes arm order the tiebreak when several arms are
        // ready at once, which matters for a rehydrated wait whose
        // deadline expired during the outage: a catch-up resolution
        // (notify already signalled) must win over the zero-duration
        // timeout deterministically, and cancellation must win over
        // timing out.
        let outcome = match timeout {
            Some(d) => tokio::select! {
                biased;
                _ = notify.notified() => WaitOutcome::Resolved,
                _ = cancel_token.cancelled() => WaitOutcome::Cancelled,
                _ = tokio::time::sleep(d) => WaitOutcome::TimedOut,
            },
            None => tokio::select! {
                biased;
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
                source_event_id: None,
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
        // Ledger- and bridge-resolved signals carry the persisted
        // event's id; remember it so a loop revisit or a restart
        // re-registration can't consume the same event twice. The
        // payload["id"] fallback covers pre-source_event_id envelope
        // payloads still delivered for non-router producers. Live
        // direct signals have no persisted event and record nothing.
        let consumed_event_id = sig.source_event_id.clone().or_else(|| {
            sig.payload
                .get("id")
                .and_then(|v| v.as_str())
                .map(str::to_string)
        });
        if let Some(event_id) = consumed_event_id.as_deref() {
            self.ctx.record_consumed_signal_event(event_id);
        }
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
        // Durable resolution record BEFORE on_exit hooks / gate run: the
        // context now carries the signal and its consumed event id, and
        // the status flips to Running so a crash between here and the
        // next boundary rehydrates as a LOUD interrupted arc instead of
        // silently re-consuming the event and re-running post-wait
        // side effects.
        self.write_checkpoint(
            crate::workflow::arc_store::ArcCheckpointStatus::Running,
            node_id,
        )
        .await;
        self.arc_note(
            "done",
            &format!("Wait '{node_id}' resolved by signal '{}'", sig.name),
        );
        Ok(())
    }
}
