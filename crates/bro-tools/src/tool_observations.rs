//! Host-only argument-policy observations, separate from domain tool results.

use serde::Serialize;
use serde_json::{Value, json};
use std::collections::VecDeque;
use std::sync::Mutex;

const MAX_ENTRIES: usize = 128;
const MAX_BYTES: usize = 64 * 1024;
const MAX_ENTRY_BYTES: usize = 8 * 1024;

#[derive(Debug, Clone, Serialize)]
pub struct ToolObservation {
    /// Process-local sequence within the durable event envelope, never a provider call ID.
    pub id: u64,
    pub tool: String,
    pub instruction_generation: u64,
    pub context: Value,
}

#[derive(Debug, Default, Serialize)]
pub struct ToolObservationBatch {
    pub observations: Vec<ToolObservation>,
    pub dropped_observations: u64,
}

#[derive(Debug, Default)]
struct State {
    next_id: u64,
    entries: VecDeque<(ToolObservation, usize)>,
    bytes: usize,
    dropped: u64,
}

#[derive(Debug, Default)]
pub struct ToolObservationSink(Mutex<State>);

impl ToolObservationSink {
    pub fn record(&self, tool: &str, generation: u64, mut context: Value) {
        if context.as_object().is_some_and(|object| object.is_empty()) {
            return;
        }
        if context.to_string().len() > MAX_ENTRY_BYTES {
            context = json!({
                "values_omitted": true,
                "defaults_applied_count": context.get("defaults_applied").and_then(Value::as_object).map_or(0, |map| map.len()),
                "pin_enforced_count": context.get("pin_enforced").and_then(Value::as_object).map_or(0, |map| map.len()),
                "policy_error": context.get("policy_error").is_some(),
                "pin_conflict": context.get("pin_conflict").is_some(),
            });
        }
        let mut state = self.0.lock().expect("tool observation sink poisoned");
        state.next_id += 1;
        let observation = ToolObservation {
            id: state.next_id,
            tool: tool.into(),
            instruction_generation: generation,
            context,
        };
        let bytes = serde_json::to_vec(&observation)
            .expect("observation serializes")
            .len();
        if bytes > MAX_BYTES {
            state.dropped += 1;
            return;
        }
        while state.entries.len() >= MAX_ENTRIES || state.bytes + bytes > MAX_BYTES {
            let (_, removed) = state.entries.pop_front().expect("bounded nonempty queue");
            state.bytes -= removed;
            state.dropped += 1;
        }
        state.bytes += bytes;
        state.entries.push_back((observation, bytes));
    }

    pub fn drain(&self) -> ToolObservationBatch {
        let mut state = self.0.lock().expect("tool observation sink poisoned");
        let observations = state
            .entries
            .drain(..)
            .map(|(observation, _)| observation)
            .collect();
        state.bytes = 0;
        ToolObservationBatch {
            observations,
            dropped_observations: std::mem::take(&mut state.dropped),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn observations_are_bounded_with_disclosed_loss_and_stable_ids() {
        let sink = ToolObservationSink::default();
        for _ in 0..140 {
            sink.record("fixture", 7, json!({"defaults_applied":{"cwd":"."}}));
        }
        let batch = sink.drain();
        assert_eq!(batch.observations.len(), 128);
        assert_eq!(batch.dropped_observations, 12);
        assert_eq!(batch.observations[0].id, 13);
        assert!(sink.drain().observations.is_empty());
        sink.record(
            "fixture",
            8,
            json!({"defaults_applied":{"content":"x".repeat(20_000)}}),
        );
        let batch = sink.drain();
        assert_eq!(batch.observations[0].id, 141);
        assert_eq!(batch.observations[0].context["values_omitted"], true);
    }
}
