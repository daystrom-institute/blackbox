//! Compaction only installs completed, structurally valid replacements. Request
//! fitting operates on a copy so failed attempts retain the full source history.

use anyhow::{Context, Result, bail, ensure};
use futures_util::StreamExt;
use serde_json::{Value, json};

use crate::transport::{CompactionParams, TurnOpts};

pub(super) async fn collect_summary(
    response: reqwest::Response,
    usage: &mut crate::transport::Usage,
) -> Result<String> {
    let mut stream = response.bytes_stream();
    let mut pending = Vec::new();
    let mut text = String::new();
    let mut received = 0usize;
    loop {
        let chunk = tokio::time::timeout(super::super::http::stream_idle_timeout(), stream.next())
            .await
            .context("responses compaction stream idle timeout")?;
        let Some(chunk) = chunk else {
            bail!("responses compaction stream closed before response.completed");
        };
        let chunk = chunk.context("read responses compact chunk")?;
        received = received.saturating_add(chunk.len());
        ensure!(
            received <= super::COMPACTION_BYTE_BUDGET,
            "inline compaction stream exceeded byte budget"
        );
        pending.extend_from_slice(&chunk);
        while let Some(end) = pending.iter().position(|byte| *byte == b'\n') {
            let line: Vec<_> = pending.drain(..=end).collect();
            let line = std::str::from_utf8(&line).context("invalid compaction SSE UTF-8")?;
            let Some(data) = line.trim().strip_prefix("data:") else {
                continue;
            };
            let data = data.trim();
            if data.is_empty() {
                continue;
            }
            ensure!(
                data != "[DONE]",
                "compaction ended before response.completed"
            );
            let event: Value = serde_json::from_str(data).context("invalid compaction SSE JSON")?;
            let kind = event["type"]
                .as_str()
                .context("compaction event missing type")?;
            if matches!(
                kind,
                "response.completed" | "response.failed" | "response.incomplete" | "error"
            ) {
                usage.add(&crate::transport::openai_responses_stream::terminal_usage(
                    &event,
                ));
            }
            match kind {
                "response.output_text.delta" => {
                    text.push_str(event["delta"].as_str().context("invalid summary delta")?);
                }
                "response.completed" => {
                    ensure!(
                        event["response"].is_object(),
                        "compaction completion missing response object"
                    );
                    ensure!(
                        event["response"]["status"].is_null()
                            || event["response"]["status"] == "completed",
                        "compaction completion has unsuccessful status"
                    );
                    ensure!(
                        event["response"]["error"].is_null(),
                        "compaction completion contains error"
                    );
                    return Ok(text);
                }
                "response.failed" | "response.incomplete" | "error" => {
                    bail!("compaction did not complete successfully: {event}");
                }
                _ => {}
            }
        }
    }
}

/// Codex's `RETAINED_MESSAGE_TOKEN_BUDGET`: how much verbatim user context
/// survives a server-side compaction, newest first.
pub(super) const RETAINED_MESSAGE_TOKEN_BUDGET: u64 = 64_000;

/// Exactly one encrypted compaction item from a v2 compaction stream. Both
/// native aliases are accepted; the item is spliced verbatim, never rewritten.
pub(super) fn validate_v2_output(output: &[Value]) -> Result<(Value, String)> {
    let summaries: Vec<&Value> = output
        .iter()
        .filter(|item| {
            matches!(
                item["type"].as_str(),
                Some("compaction" | "compaction_summary")
            )
        })
        .collect();
    ensure!(
        summaries.len() == 1,
        "remote compaction expected exactly one compaction output item, got {} from {} output items",
        summaries.len(),
        output.len()
    );
    let item = summaries[0];
    let summary = item["encrypted_content"]
        .as_str()
        .filter(|text| !text.trim().is_empty())
        .context("compaction summary encrypted_content must be nonempty")?
        .to_owned();
    Ok((item.clone(), summary))
}

/// Charge only the text content of a retained user message, never its
/// serialized JSON: counting the envelope (and any base64 image payload
/// inside it) at full length wrongly evicted messages whose model-visible
/// text fits the budget. Non-text content blocks pass through retention
/// uncharged, matching codex's text-only retention accounting; degenerate
/// no-content shapes keep the old whole-JSON charge so they still count.
fn user_message_tokens(item: &Value) -> u64 {
    match item.get("content") {
        Some(Value::String(text)) => crate::context::budget::text_tokens(text),
        Some(Value::Array(blocks)) => blocks
            .iter()
            .map(|block| {
                block["text"]
                    .as_str()
                    .map(crate::context::budget::text_tokens)
                    .unwrap_or(0)
            })
            .fold(0u64, u64::saturating_add),
        _ => crate::context::budget::text_tokens(&item.to_string()),
    }
    .max(1)
}

/// Codex's v2 retention shape: only user messages survive, newest first
/// within `budget_tokens`. Text is charged by content, not serialized JSON.
/// The boundary message (the oldest one that does not fit whole) is truncated
/// to the remaining budget instead of dropped, so the newest user context
/// keeps a prefix and ordering stays intact; everything older is dropped. The
/// truncated boundary item is a client-rebuilt copy of the source message
/// (its text is cut and marked), never a rewritten source: the session
/// transcript keeps the original. Non-message items carrying `role:"user"`
/// are not retained. Assistant turns and tool traffic are covered by the
/// encrypted summary, and the ambient developer manifest is re-injected by
/// the next turn.
pub(super) fn retain_for_v2(input: &[Value], budget_tokens: u64) -> Vec<Value> {
    let mut remaining = budget_tokens;
    let mut kept = Vec::new();
    for item in input.iter().rev() {
        let is_user_message =
            item["role"] == "user" && item["type"].as_str().is_none_or(|kind| kind == "message");
        if !is_user_message {
            continue;
        }
        let tokens = user_message_tokens(item);
        if tokens <= remaining {
            remaining -= tokens;
            kept.push(item.clone());
            continue;
        }
        if remaining > 0
            && let Some(truncated) = truncate_user_message_to_budget(item, remaining)
        {
            kept.push(truncated);
        }
        // The boundary was reached (or could not be truncated at all): older
        // messages can no longer fit.
        break;
    }
    kept.reverse();
    kept
}

/// Marker appended to a boundary message cut to fit the retention budget. Its
/// bytes are charged inside the budget so the truncated copy never exceeds
/// the remaining tokens.
const RETENTION_TRUNCATION_MARKER: &str = "… [retention truncated]";

/// Cut `text` to at most `budget_tokens` (four bytes per token) at a char
/// boundary, charging the truncation marker inside the budget. When the
/// budget is too small for the marker, a valid UTF-8 prefix without the
/// marker is kept instead of dropping the text entirely. `None` only when no
/// nonempty prefix fits.
fn truncate_text_to_token_budget(text: &str, budget_tokens: u64) -> Option<String> {
    let byte_budget = usize::try_from(budget_tokens.saturating_mul(4)).ok()?;
    if text.len() <= byte_budget {
        return Some(text.to_owned());
    }
    let prefix_at = |limit: usize| {
        text.char_indices()
            .map(|(index, _)| index)
            .take_while(|index| *index <= limit)
            .last()
            .unwrap_or(0)
    };
    if let Some(content_budget) = byte_budget.checked_sub(RETENTION_TRUNCATION_MARKER.len()) {
        let cut = prefix_at(content_budget);
        if cut > 0 {
            let mut truncated = String::with_capacity(cut + RETENTION_TRUNCATION_MARKER.len());
            truncated.push_str(&text[..cut]);
            truncated.push_str(RETENTION_TRUNCATION_MARKER);
            debug_assert!(truncated.len() <= byte_budget);
            return Some(truncated);
        }
    }
    // The marker does not fit: keep the largest nonempty valid UTF-8 prefix.
    let cut = prefix_at(byte_budget.saturating_sub(1));
    (cut > 0).then(|| text[..cut].to_owned())
}

/// Rebuild a user message whose whole form exceeds `budget_tokens` as a copy
/// truncated to exactly that budget. Text blocks are cut in order (later
/// blocks drop once the budget is spent; a budget too small to cut a block
/// keeps the blocks already retained), non-text content passes through
/// untouched, and a shape with nothing left to keep yields `None` so the
/// caller drops it instead of keeping a silent rewrite.
fn truncate_user_message_to_budget(item: &Value, budget_tokens: u64) -> Option<Value> {
    let mut truncated = item.clone();
    let content = truncated.get_mut("content")?;
    match content {
        Value::String(text) => {
            let cut = truncate_text_to_token_budget(text, budget_tokens)?;
            if cut.len() == text.len() {
                // Nothing to cut: the item fits whole; keep it verbatim.
                return Some(truncated);
            }
            *text = cut;
        }
        Value::Array(blocks) => {
            let mut remaining = budget_tokens;
            let mut rewritten = Vec::with_capacity(blocks.len());
            for block in blocks {
                let is_text = block["type"]
                    .as_str()
                    .is_some_and(|kind| matches!(kind, "input_text" | "output_text" | "text"))
                    || (block.get("type").is_none() && block.get("text").is_some());
                if !is_text {
                    rewritten.push(block.clone());
                    continue;
                }
                let Some(text) = block["text"].as_str() else {
                    // Untruncatable shape: keep what is retained so far.
                    break;
                };
                let tokens = crate::context::budget::text_tokens(text);
                if tokens <= remaining {
                    remaining -= tokens;
                    rewritten.push(block.clone());
                    continue;
                }
                if remaining == 0 {
                    // Budget spent earlier in this message: drop later blocks.
                    continue;
                }
                let Some(cut) = truncate_text_to_token_budget(text, remaining) else {
                    // Too small to cut even a prefix: keep the blocks already
                    // retained and stop; do not discard them with the message.
                    break;
                };
                let mut block = block.clone();
                block["text"] = Value::String(cut);
                rewritten.push(block);
                remaining = 0;
            }
            if rewritten.is_empty() {
                return None;
            }
            *content = Value::Array(rewritten);
        }
        _ => return None,
    }
    Some(truncated)
}

const SUMMARY_SYSTEM: &str = "You summarize coding-agent conversations precisely and completely.";

/// Replacement payload for a tool output trimmed from a remote v2 compaction
/// request, mirroring codex's `CONTEXT_WINDOW_TRUNCATED_OUTPUT_MESSAGE`.
const REMOTE_TRIMMED_OUTPUT: &str = "Output exceeded the available model context and was truncated";

/// The token limit a remote v2 compaction request must fit. The operator's
/// compaction config window (when `BRO_HARNESS_COMPACTION_CONFIG` is in
/// force) wins, then the catalog's usable window (`effective_context_window_percent`,
/// codex default 95, of the target window), then the built-in table's window.
/// `None` leaves the request provider-validated, mirroring the loop's window
/// resolution. The caller must build the policy under the same environment
/// this resolution runs in.
pub(super) fn remote_compaction_limit(
    limits: Option<&crate::transport::ModelLimits>,
    policy: &crate::compaction::CompactionPolicy,
    model: &str,
) -> Option<u64> {
    if crate::transport::session_var("BRO_HARNESS_COMPACTION_CONFIG").is_some() {
        return policy.context_window(model);
    }
    limits
        .map(|limits| limits.usable_context_window())
        .flatten()
        .or_else(|| policy.context_window(model))
}

/// Fit a remote (`compact_remote_v2`) compaction request into `limit`
/// estimated model-visible tokens. History is estimated through the shared
/// per-item accounting (`context::budget::item_tokens`), which discounts
/// encrypted reasoning and compaction payloads, so an encrypted-heavy
/// history the loop considers inside the window fits here too; raw JSON
/// size is never the verdict. Trimming mirrors codex's
/// `trim_function_call_history_to_fit_context_window`: only trailing output
/// groups are rewritten, newest first, with the synthetic trailing trigger
/// skipped, the pass stopping as soon as the running estimate fits (so a
/// single sufficient replacement leaves older trailing outputs
/// byte-identical) and at the first item that is not a tool output. Outputs
/// behind later user or assistant messages stay verbatim.
/// Protected content that still overflows is an error rather than a silent
/// loss. `limit` is the already-resolved usable window: no further output
/// reservation is subtracted, because the usable percentage already reserves
/// that headroom (unlike `inline_request`, which reserves the summarizer's
/// output tokens against a full window). The body is a fitted copy; the
/// caller's source history is untouched whether this succeeds or fails.
pub(super) fn fit_remote_input(mut body: Value, limit: Option<u64>) -> Result<Value> {
    let Some(limit) = limit else {
        return Ok(body);
    };
    let initial = crate::context::budget::request_tokens(&body);
    if initial <= limit {
        return Ok(body);
    }
    let items = body
        .get_mut("input")
        .and_then(Value::as_array_mut)
        .context("remote compaction request requires input array")?;
    let mut rewritten = 0u64;
    let mut running = initial;
    for item in items.iter_mut().rev() {
        if running <= limit {
            // One replacement was enough: keep the remaining trailing
            // outputs byte-identical rather than trimming eagerly.
            break;
        }
        if item["type"].as_str() == Some("compaction_trigger") {
            // The synthetic trailing trigger is not history; skip, do not trim.
            continue;
        }
        if !matches!(
            item["type"].as_str(),
            Some("function_call_output" | "custom_tool_call_output")
        ) {
            // First non-output item ends the trailing output group: history
            // behind it is protected context, not trimmable output.
            break;
        }
        let before = crate::context::budget::item_tokens(item);
        let mut replacement = item.clone();
        replacement["output"] = Value::String(REMOTE_TRIMMED_OUTPUT.to_owned());
        let after = crate::context::budget::item_tokens(&replacement);
        if after >= before {
            // Already minimal; older trailing outputs may still help.
            continue;
        }
        *item = replacement;
        rewritten += 1;
        running = running.saturating_sub(before).saturating_add(after);
    }
    // Recompute from the full body instead of trusting the running estimate:
    // the shared estimator stays the single authority for the verdict.
    let final_tokens = crate::context::budget::request_tokens(&body);
    tracing::debug!(
        limit,
        estimated_tokens_before = initial,
        estimated_tokens_after = final_tokens,
        rewritten_outputs = rewritten,
        "fitted remote compaction request to the usable context window"
    );
    ensure!(
        final_tokens <= limit,
        "remote compaction request still exceeds the usable context after trimming {rewritten} trailing tool outputs; protected history requires ~{final_tokens} tokens, limit is {limit}"
    );
    Ok(body)
}

/// Fit the API-key summarizer before any HTTP request. First honor the existing
/// per-output rendering cap. If the rendered request still exceeds the window,
/// fit a structured copy so only tool outputs can be replaced. A final check
/// includes the exact rendered transcript, directive and output reservation.
pub(super) fn inline_request(
    input: &[Value],
    params: CompactionParams,
    instruction: &str,
    opts: &TurnOpts,
    window: Option<u64>,
) -> Result<Value> {
    let render = |input: &[Value]| {
        let transcript =
            super::responses_common::render_responses_transcript(input, params.tool_render_cap);
        json!({
            "model": opts.model,
            "input": [{"type":"message", "role":"user", "content":[{
                "type":"input_text", "text":format!("{transcript}\n\n---\n{instruction}")
            }]}],
            "instructions": SUMMARY_SYSTEM,
            "max_output_tokens": params.summary_max_tokens,
            "stream": true,
            "store": false,
        })
    };
    let body = render(input);
    if let Ok(fitted) = fit_input(body, window) {
        return Ok(fitted);
    }
    let mut prefix = input.to_vec();
    for item in &mut prefix {
        if matches!(
            item["type"].as_str(),
            Some("function_call_output" | "custom_tool_call_output")
        ) {
            // Match the plaintext renderer exactly, including its treatment of
            // non-text outputs, without modifying the persisted native items.
            item["output"] = Value::String(super::super::truncate(
                item["output"].as_str().unwrap_or(""),
                params.tool_render_cap,
            ));
        }
    }
    let fitted = fit_input(
        json!({
            "model": opts.model,
            "input": prefix,
            "instructions": format!("{SUMMARY_SYSTEM}\n\n---\n{instruction}"),
            "max_output_tokens": params.summary_max_tokens,
            "stream": true,
            "store": false,
        }),
        window,
    )?;
    fit_input(
        render(
            fitted["input"]
                .as_array()
                .context("fitted prefix missing input")?,
        ),
        window,
    )
}

const OMITTED_OUTPUT: &str = "[Tool output omitted to fit the compaction request context window. The original output remains in the session transcript.]";

/// Match the harness's approximate four-bytes-per-token accounting, including
/// instructions, tool schemas and framing, with room for the summary. This is a
/// local estimate, not a provider tokenizer or an endpoint-capacity guarantee.
/// Unknown model windows remain provider-validated for compatibility.
pub(super) fn fit_input(mut body: Value, window: Option<u64>) -> Result<Value> {
    let Some(window) = window else {
        return Ok(body);
    };
    let output_reservation = body["max_output_tokens"].as_u64().unwrap_or_default();
    let budget = window.saturating_sub(8192.min(window / 4).max(output_reservation));
    let mut bytes = serde_json::to_vec(&body)?.len() as u64;
    let byte_budget = budget.saturating_mul(4);
    if bytes <= byte_budget {
        return Ok(body);
    }
    let items = body
        .get_mut("input")
        .and_then(Value::as_array_mut)
        .context("compaction request requires input array")?;
    for item in items.iter_mut().rev() {
        if !matches!(
            item["type"].as_str(),
            Some("function_call_output" | "custom_tool_call_output")
        ) {
            continue;
        }
        let old_bytes = serde_json::to_vec(&item["output"])?.len() as u64;
        let replacement = Value::String(OMITTED_OUTPUT.to_owned());
        let new_bytes = serde_json::to_vec(&replacement)?.len() as u64;
        if new_bytes >= old_bytes {
            continue;
        }
        item["output"] = replacement;
        bytes = bytes.saturating_sub(old_bytes - new_bytes);
        if bytes <= byte_budget {
            return Ok(body);
        }
    }
    bail!(
        "compaction request still exceeds estimated context budget after fitting tool outputs; preserved user messages, instructions, schemas or other protected history require {bytes} bytes, budget is {byte_budget} bytes"
    )
}

#[cfg(test)]
#[path = "openai_responses_compaction_tests.rs"]
mod tests;
