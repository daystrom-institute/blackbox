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
    /// The fallback `instructions` text the Responses builder sends when base
    /// and stable are both empty; charged so the Lite estimate matches the
    /// request-only developer prefix message that actually ships.
    const DEFAULT_INSTRUCTIONS: &str =
        "You are a helpful coding assistant operating non-interactively.";

    /// Include the native history, current instructions and actual activated
    /// schemas. Counting serialized bytes avoids building another large JSON
    /// string; the four-byte heuristic is approximate, not a tokenizer promise.
    ///
    /// A prepared Responses Lite snapshot (`responses_lite: true`) carries the
    /// tool definitions as history items, so activated schemas are excluded
    /// here (charging both would double-count every request); the request-only
    /// base+stable developer prefix message is overhead and stays charged. In
    /// ordinary mode, history `additional_tools` items never ride the wire
    /// (the builder filters them per request), so their cost is excluded too.
    pub(crate) fn new(snapshot: &Value, tools: &[ToolSpec], opts: &TurnOpts) -> Self {
        let lite = snapshot.get("responses_lite").and_then(Value::as_bool) == Some(true);
        let history = snapshot.get("input").unwrap_or(snapshot);
        let history_tokens = if lite {
            history_tokens(history)
        } else {
            match history.as_array() {
                Some(items) => items
                    .iter()
                    .filter(|item| item["type"].as_str() != Some("additional_tools"))
                    .map(item_tokens)
                    .fold(0u64, u64::saturating_add),
                None => history_tokens(history),
            }
        };
        let mut overhead_tokens = 64u64;
        let base = opts.base_instructions.as_ref().and_then(|base| base.text());
        let stable = opts.system.stable_text();
        let mut sections = [
            base,
            stable,
            // Responses materializes ambient context into input before this
            // estimate. Other transports carry it in their system parameter.
            if snapshot.get("ambient_hash").is_some() {
                None
            } else {
                opts.system.ambient_text()
            },
            opts.system.volatile_text(),
        ];
        if lite && base.is_none() && stable.is_none() {
            sections[0] = Some(Self::DEFAULT_INSTRUCTIONS);
        }
        for text in sections.into_iter().flatten() {
            overhead_tokens = overhead_tokens.saturating_add(text_tokens(text));
        }
        if !lite {
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

/// History tokens with codex's discount for encrypted payloads: reasoning and
/// compaction items carry base64 whose model-visible cost is roughly the
/// decoded bytes less a fixed envelope, not the raw JSON length. Counting the
/// raw bytes made long brodex sessions look 20 to 30 percent larger than the
/// provider measured them.
pub(crate) fn history_tokens(history: &Value) -> u64 {
    let Some(items) = history.as_array() else {
        return json_tokens(history);
    };
    items
        .iter()
        .map(item_tokens)
        .fold(0u64, u64::saturating_add)
}

/// One native history or transport item in the shared accounting: encrypted
/// reasoning and compaction payloads get codex's decoded-length discount,
/// everything else is serialized bytes over four. Transport-side request
/// fitting must use this same estimate so a history the loop considers inside
/// the window is never rejected client-side for its raw JSON size.
pub(crate) fn item_tokens(item: &Value) -> u64 {
    let encrypted = match item["type"].as_str() {
        Some("reasoning" | "compaction" | "compaction_summary") => {
            item["encrypted_content"].as_str()
        }
        _ => None,
    };
    match encrypted {
        Some(content) => encrypted_payload_tokens(content.len()),
        None => json_tokens(item),
    }
}

/// codex `estimate_reasoning_length`: base64 decodes to 3/4 of its length,
/// minus a 650-byte envelope, then the four-bytes-per-token heuristic.
fn encrypted_payload_tokens(encoded_len: usize) -> u64 {
    ((encoded_len as u64).saturating_mul(3) / 4)
        .saturating_sub(650)
        .div_ceil(4)
}

pub(crate) fn json_tokens(value: &Value) -> u64 {
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

/// Estimated model-visible tokens for a rendered transport request body:
/// native input items through the shared per-item history accounting
/// (`item_tokens`, with the encrypted-payload discount), plus instructions,
/// activated tool schemas, and a small framing allowance. The per-item
/// history accounting is exactly what the loop's occupancy projections use;
/// the overhead policy on top of it (instructions, schemas, framing) is this
/// helper's own explicit choice, so callers comparing against loop estimates
/// should count their overhead the same way. Deliberately excludes any
/// output-token reservation; callers that must reserve output space do so
/// explicitly.
pub(crate) fn request_tokens(body: &Value) -> u64 {
    let mut tokens = 16u64;
    if let Some(input) = body.get("input").and_then(Value::as_array) {
        for item in input {
            tokens = tokens.saturating_add(item_tokens(item));
        }
    }
    if let Some(instructions) = body.get("instructions").and_then(Value::as_str) {
        tokens = tokens.saturating_add(text_tokens(instructions));
    }
    if let Some(tools) = body.get("tools").and_then(Value::as_array) {
        for tool in tools {
            tokens = tokens.saturating_add(json_tokens(tool)).saturating_add(4);
        }
    }
    tokens
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
    fn encrypted_reasoning_is_discounted_like_codex() {
        let encoded = "A".repeat(4_000);
        let reasoning = json!({"type":"reasoning","id":"rs_1","encrypted_content":encoded});
        let message = json!({"type":"message","role":"user","content":[{"type":"input_text","text":"x".repeat(4_000)}]});
        // 4000 base64 chars decode to 3000 bytes, less the 650-byte envelope:
        // 2350 bytes, so 588 tokens instead of the ~1000 the raw JSON implies.
        assert_eq!(item_tokens(&reasoning), 588);
        assert!(json_tokens(&reasoning) > 1_000);
        assert_eq!(item_tokens(&message), json_tokens(&message));
        assert_eq!(encrypted_payload_tokens(100), 0);
        let history = json!([reasoning, message]);
        assert_eq!(
            history_tokens(&history),
            item_tokens(&history[0]) + item_tokens(&history[1])
        );
    }

    #[test]
    fn request_tokens_shares_the_encrypted_discount_with_history_estimates() {
        let body = json!({
            "instructions": "x".repeat(4_000),
            "tools": [{"type":"function", "name":"probe", "parameters":{"type":"object"}}],
            "input": [
                {"type":"reasoning", "encrypted_content":"A".repeat(80_000)},
                {"type":"function_call_output", "call_id":"c", "output":"o".repeat(4_000)},
                {"type":"compaction_trigger"}
            ]
        });
        let tokens = request_tokens(&body);
        // The encrypted payload is discounted to ~14.9K tokens, not the ~20K
        // its raw JSON implies; every other part is counted at full length.
        assert_eq!(
            tokens,
            16 + item_tokens(&body["input"][0])
                + item_tokens(&body["input"][1])
                + item_tokens(&body["input"][2])
                + text_tokens(&body["instructions"].as_str().unwrap())
                + json_tokens(&body["tools"][0])
                + 4
        );
        assert!(tokens * 4 < serde_json::to_vec(&body).unwrap().len());
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
    fn lite_snapshots_charge_base_prefix_once_and_never_double_charge_schemas() {
        let schema = json!({"type":"object", "properties":{"q":{"type":"string"}}});
        let definition = json!({
            "type":"function", "name":"read", "description":"x".repeat(4_000),
            "parameters": schema,
        });
        let input = json!([
            {"type":"additional_tools", "id":"at_1", "role":"developer", "tools":[definition.clone()]},
            {"type":"message", "role":"user", "content":[{"type":"input_text", "text":"task"}]},
        ]);
        let mut options = opts();
        options.base_instructions =
            Some(crate::transport::BaseInstructions::new(&"B ".repeat(4_000)));
        let tool = ToolSpec {
            name: "read".into(),
            description: "x".repeat(4_000),
            schema: schema.clone(),
            grammar: None,
        };

        let history_tokens_total = item_tokens(&input[0]) + item_tokens(&input[1]);
        // Lite: the definitions item is history; the schema is excluded from
        // overhead; the request-only base prefix message stays charged.
        let lite_snapshot = json!({"responses_lite": true, "input": input.clone()});
        let lite = RequestEstimate::new(&lite_snapshot, std::slice::from_ref(&tool), &options);
        assert_eq!(lite.history_tokens, history_tokens_total);
        let base_tokens = text_tokens(&"B ".repeat(4_000));
        assert_eq!(lite.overhead_tokens, 64 + base_tokens);
        // No double charge: the schema bytes are counted exactly once.
        assert!(lite.history_tokens >= json_tokens(&definition));
        assert!(lite.overhead_tokens < base_tokens + json_tokens(&schema));

        // Empty base and stable fall back to the default instructions text
        // the Lite builder actually ships.
        let bare = RequestEstimate::new(&lite_snapshot, &[], &opts());
        assert_eq!(
            bare.overhead_tokens,
            64 + text_tokens(RequestEstimate::DEFAULT_INSTRUCTIONS)
        );

        // Ordinary mode: Lite-only history items never ride the wire, so
        // their cost is excluded while the activated schemas are charged.
        let ordinary_snapshot = json!({"responses_lite": false, "input": input});
        let ordinary = RequestEstimate::new(&ordinary_snapshot, &[tool], &options);
        assert_eq!(
            ordinary.history_tokens,
            item_tokens(&ordinary_snapshot["input"][1])
        );
        assert!(ordinary.overhead_tokens > lite.overhead_tokens);
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
