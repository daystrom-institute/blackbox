//! Conservative next-request occupancy, separate from measured input telemetry.

use crate::transport::{ToolSpec, TurnOpts};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// The measured request and all locally appended tokens belong to the same
/// atomic history checkpoint. Legacy or invalid checkpoints fall back to a
/// full history estimate rather than interpreting restored history as empty.
#[derive(Debug, Default, Serialize, Deserialize)]
pub(crate) struct BudgetCheckpoint {
    version: u8,
    pub(crate) last_input_tokens: u64,
    pub(crate) pending_tokens: u64,
    pub(crate) overhead_tokens: u64,
}

impl BudgetCheckpoint {
    pub(crate) fn new(last_input_tokens: u64, pending_tokens: u64, overhead_tokens: u64) -> Self {
        Self {
            version: 1,
            last_input_tokens,
            pending_tokens,
            overhead_tokens,
        }
    }

    pub(crate) fn restore(value: &Value) -> Self {
        serde_json::from_value::<Self>(value.clone())
            .ok()
            .filter(|checkpoint| checkpoint.version == 1)
            .unwrap_or_default()
    }
}

pub(crate) struct RequestEstimate {
    pub(crate) history_tokens: u64,
    pub(crate) overhead_tokens: u64,
}

impl RequestEstimate {
    /// Include the native history, current instructions and actual activated
    /// schemas. Counting serialized bytes avoids building another large JSON
    /// string; the four-byte heuristic is approximate, not a tokenizer promise.
    pub(crate) fn new(snapshot: &Value, tools: &[ToolSpec], opts: &TurnOpts) -> Self {
        let history = snapshot.get("input").unwrap_or(snapshot);
        let history_tokens = json_tokens(history);
        let mut overhead_tokens = 64u64;
        for text in [
            opts.base_instructions.as_ref().and_then(|base| base.text()),
            opts.system.stable_text(),
            // Responses materializes ambient context into input before this
            // estimate. Other transports carry it in their system parameter.
            if snapshot.get("ambient_hash").is_some() {
                None
            } else {
                opts.system.ambient_text()
            },
            opts.system.volatile_text(),
        ]
        .into_iter()
        .flatten()
        {
            overhead_tokens = overhead_tokens.saturating_add(text_tokens(text));
        }
        for tool in tools {
            overhead_tokens = overhead_tokens
                .saturating_add(text_tokens(&tool.name))
                .saturating_add(text_tokens(&tool.description))
                .saturating_add(json_tokens(&tool.schema))
                .saturating_add(16);
            if let Some(grammar) = &tool.grammar {
                overhead_tokens = overhead_tokens
                    .saturating_add(text_tokens(&grammar.syntax))
                    .saturating_add(text_tokens(&grammar.definition));
            }
        }
        Self {
            history_tokens,
            overhead_tokens,
        }
    }

    pub(crate) fn projected(&self, checkpoint: &BudgetCheckpoint) -> u64 {
        let observed = checkpoint
            .last_input_tokens
            .saturating_add(checkpoint.pending_tokens)
            .saturating_add(
                self.overhead_tokens
                    .saturating_sub(checkpoint.overhead_tokens),
            );
        observed.max(self.history_tokens.saturating_add(self.overhead_tokens))
    }
}

pub(crate) fn text_tokens(text: &str) -> u64 {
    (text.len() as u64).div_ceil(4)
}

fn json_tokens(value: &Value) -> u64 {
    struct ByteCount(u64);
    impl std::io::Write for ByteCount {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0 = self.0.saturating_add(bytes.len() as u64);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut count = ByteCount(0);
    serde_json::to_writer(&mut count, value).expect("JSON values serialize");
    count.0.div_ceil(4)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transport::SystemPrompt;
    use serde_json::json;

    fn opts() -> TurnOpts {
        TurnOpts {
            model: "fixture".into(),
            max_tokens: 32_000,
            base_instructions: None,
            system: SystemPrompt::default(),
            effort: None,
            web_search: false,
            service_tier: None,
        }
    }

    #[test]
    fn projection_includes_retained_output_and_new_schema_overhead() {
        let checkpoint = BudgetCheckpoint::new(195_000, 20_000, 64);
        let tool = ToolSpec {
            name: "fixture".into(),
            description: "expanded schema ".repeat(1_000),
            schema: json!({"type":"object"}),
            grammar: None,
        };
        let estimate = RequestEstimate::new(&json!([]), &[tool], &opts());
        assert!(estimate.projected(&checkpoint) > 215_000);
        assert_eq!(
            RequestEstimate::new(&json!([]), &[], &opts()).projected(&checkpoint),
            215_000
        );
    }

    #[test]
    fn legacy_resume_estimates_existing_history_and_current_instructions() {
        let snapshot = json!({"input":[{"role":"user","content":"history ".repeat(40_000)}]});
        let mut opts = opts();
        opts.system.stable = Some("new instructions ".repeat(1_000));
        let estimate = RequestEstimate::new(&snapshot, &[], &opts);
        for saved in [
            Value::Null,
            json!({"version":99}),
            json!({"pending_tokens":"bad"}),
        ] {
            assert!(estimate.projected(&BudgetCheckpoint::restore(&saved)) > 80_000);
        }
    }

    #[test]
    fn atomic_checkpoint_retains_pending_occupancy_without_cumulative_usage() {
        let checkpoint = BudgetCheckpoint::new(120_000, 9_000, 700);
        let restored = BudgetCheckpoint::restore(&serde_json::to_value(checkpoint).unwrap());
        let estimate = RequestEstimate {
            history_tokens: 100_000,
            overhead_tokens: 800,
        };
        assert_eq!(estimate.projected(&restored), 129_100);
        assert_eq!(estimate.projected(&BudgetCheckpoint::default()), 100_800);
    }
}
