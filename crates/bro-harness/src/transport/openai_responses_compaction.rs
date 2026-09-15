//! Compaction only installs completed, structurally valid replacements. Request
//! fitting operates on a copy so failed attempts retain the full source history.

use anyhow::{Context, Result, bail, ensure};
use futures_util::StreamExt;
use serde_json::{Value, json};

use crate::transport::{CompactionParams, TurnOpts};

pub(super) async fn collect_summary(response: reqwest::Response) -> Result<String> {
    let mut stream = response.bytes_stream();
    let mut pending = Vec::new();
    let mut text = String::new();
    loop {
        let chunk = tokio::time::timeout(super::super::http::stream_idle_timeout(), stream.next())
            .await
            .context("responses compaction stream idle timeout")?;
        let Some(chunk) = chunk else {
            bail!("responses compaction stream closed before response.completed");
        };
        pending.extend_from_slice(&chunk.context("read responses compact chunk")?);
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

/// Codex's v2 retention shape: only user messages survive verbatim, newest
/// first within `budget_tokens`; a boundary message that does not fit is
/// dropped rather than split. Assistant turns and tool traffic are covered by
/// the encrypted summary, and the ambient developer manifest is re-injected by
/// the next turn. Order is preserved.
pub(super) fn retain_for_v2(input: &[Value], budget_tokens: u64) -> Vec<Value> {
    let mut remaining = budget_tokens;
    let mut kept = Vec::new();
    for item in input.iter().rev() {
        let is_user_message =
            item["role"] == "user" && item["type"].as_str().is_none_or(|kind| kind == "message");
        if !is_user_message {
            continue;
        }
        let tokens = crate::context::budget::text_tokens(&item.to_string()).max(1);
        if tokens > remaining {
            break;
        }
        remaining -= tokens;
        kept.push(item.clone());
    }
    kept.reverse();
    kept
}

const SUMMARY_SYSTEM: &str = "You summarize coding-agent conversations precisely and completely.";

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
