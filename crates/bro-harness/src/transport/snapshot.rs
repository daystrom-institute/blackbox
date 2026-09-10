//! Structural validation at the durable-session boundary. Provider-owned
//! extensions remain opaque, but required replay fields cannot silently vanish.

use anyhow::{Context, Result, bail, ensure};
use serde_json::Value;

/// Accept native snapshots emitted by current transports and the legacy bare
/// Responses input array. This validates structure, not tool-pair completeness:
/// interrupted histories can legitimately contain calls awaiting normalization.
pub(crate) fn validate_snapshot(transport: &str, snapshot: &Value) -> Result<()> {
    let items = match transport {
        "anthropic" | "openai-chat" => snapshot
            .as_array()
            .context("native message snapshot must be an array")?,
        "openai-responses" if snapshot.is_array() => snapshot.as_array().unwrap(),
        "openai-responses" => {
            let object = snapshot
                .as_object()
                .context("Responses snapshot must be an object or legacy array")?;
            if let Some(hash) = object.get("ambient_hash") {
                ensure!(
                    hash.is_null() || hash.as_u64().is_some(),
                    "Responses ambient_hash must be null or an unsigned integer"
                );
            }
            object
                .get("input")
                .and_then(Value::as_array)
                .context("Responses snapshot requires an input array")?
        }
        _ => bail!("unsupported snapshot transport: {transport}"),
    };
    for (index, item) in items.iter().enumerate() {
        ensure!(item.is_object(), "snapshot item {index} must be an object");
        match transport {
            "anthropic" => validate_anthropic_message(item),
            "openai-chat" => validate_chat_message(item),
            "openai-responses" => validate_responses_item(item),
            _ => unreachable!(),
        }
        .with_context(|| format!("invalid {transport} snapshot item {index}"))?;
    }
    Ok(())
}

fn string<'a>(value: &'a Value, key: &str) -> Result<&'a str> {
    value
        .get(key)
        .and_then(Value::as_str)
        .with_context(|| format!("{key} must be a string"))
}

fn nonempty<'a>(value: &'a Value, key: &str) -> Result<&'a str> {
    let text = string(value, key)?;
    ensure!(!text.trim().is_empty(), "{key} must not be empty");
    Ok(text)
}

fn optional_string(value: &Value, key: &str) -> Result<()> {
    if let Some(field) = value.get(key) {
        ensure!(
            field.is_null() || field.is_string(),
            "{key} must be null or a string"
        );
    }
    Ok(())
}

fn role<'a>(message: &'a Value, allowed: &[&str]) -> Result<&'a str> {
    let role = string(message, "role")?;
    ensure!(allowed.contains(&role), "invalid message role");
    Ok(role)
}

fn validate_anthropic_message(message: &Value) -> Result<()> {
    role(message, &["user", "assistant"])?;
    let content = message
        .get("content")
        .context("message content is missing")?;
    if content.is_string() {
        return Ok(());
    }
    let blocks = content
        .as_array()
        .context("Anthropic content must be a string or block array")?;
    for (index, block) in blocks.iter().enumerate() {
        validate_anthropic_block(block)
            .with_context(|| format!("invalid content block {index}"))?;
    }
    Ok(())
}

fn validate_anthropic_block(block: &Value) -> Result<()> {
    ensure!(block.is_object(), "content block must be an object");
    match nonempty(block, "type")? {
        "text" => {
            string(block, "text")?;
        }
        "thinking" => {
            string(block, "thinking")?;
            optional_string(block, "signature")?;
        }
        "redacted_thinking" => {
            string(block, "data")?;
        }
        "tool_use" | "server_tool_use" => {
            nonempty(block, "id")?;
            nonempty(block, "name")?;
            ensure!(
                block.get("input").is_some_and(Value::is_object),
                "tool input must be an object"
            );
        }
        kind if kind.ends_with("tool_result") => {
            nonempty(block, "tool_use_id")?;
            let content = block
                .get("content")
                .context("tool result content is missing")?;
            ensure!(
                content.is_string() || content.is_array() || content.is_object(),
                "tool result content must be text or structured content"
            );
            if let Some(is_error) = block.get("is_error") {
                ensure!(
                    is_error.is_boolean(),
                    "tool result is_error must be a boolean"
                );
            }
        }
        // Server-produced blocks are stored verbatim by the transport. Preserve
        // vendor extensions rather than guessing a schema and dropping them.
        _ => {}
    }
    Ok(())
}

fn validate_chat_message(message: &Value) -> Result<()> {
    let role = role(
        message,
        &["user", "assistant", "system", "developer", "tool"],
    )?;
    match message.get("content") {
        Some(Value::String(_)) => {}
        Some(Value::Array(parts)) => validate_content_parts(parts)?,
        None | Some(Value::Null) if role == "assistant" => {}
        _ => bail!(
            "Chat message content must be text or a part array; only assistant content can be absent/null"
        ),
    }
    if role == "tool" {
        nonempty(message, "tool_call_id")?;
    }
    if let Some(calls) = message.get("tool_calls").filter(|value| !value.is_null()) {
        ensure!(
            role == "assistant",
            "only assistant messages can contain tool_calls"
        );
        for (index, call) in calls
            .as_array()
            .context("tool_calls must be an array")?
            .iter()
            .enumerate()
        {
            (|| -> Result<()> {
                ensure!(call.is_object(), "tool call must be an object");
                nonempty(call, "id")?;
                ensure!(
                    string(call, "type")? == "function",
                    "unsupported Chat tool call type"
                );
                let function = call
                    .get("function")
                    .filter(|value| value.is_object())
                    .context("function must be an object")?;
                nonempty(function, "name")?;
                super::parse_tool_arguments(string(function, "arguments")?)?;
                Ok(())
            })()
            .with_context(|| format!("invalid tool call {index}"))?;
        }
    }
    Ok(())
}

fn validate_content_parts(parts: &[Value]) -> Result<()> {
    for (index, part) in parts.iter().enumerate() {
        ensure!(part.is_object(), "content part {index} must be an object");
        match nonempty(part, "type")? {
            "text" | "input_text" | "output_text" | "summary_text" => {
                string(part, "text")?;
            }
            "refusal" => {
                string(part, "refusal")?;
            }
            // Multimodal and vendor content parts are provider-owned and pass
            // through unchanged. Their outer typed-object shape is required.
            _ => {}
        }
    }
    Ok(())
}

fn validate_responses_item(item: &Value) -> Result<()> {
    // Remote compaction may retain easy-input messages without an explicit type.
    let kind = match item.get("type") {
        None if item.get("role").is_some() => "message",
        _ => nonempty(item, "type")?,
    };
    match kind {
        "message" => {
            role(item, &["user", "assistant", "system", "developer"])?;
            match item.get("content") {
                Some(Value::String(_)) => {}
                Some(Value::Array(parts)) => validate_content_parts(parts)?,
                _ => bail!("Responses message content must be text or a part array"),
            }
        }
        "function_call" => {
            nonempty(item, "call_id")?;
            nonempty(item, "name")?;
            super::parse_tool_arguments(string(item, "arguments")?)?;
        }
        "custom_tool_call" => {
            nonempty(item, "call_id")?;
            nonempty(item, "name")?;
            string(item, "input")?;
        }
        "function_call_output" | "custom_tool_call_output" => {
            nonempty(item, "call_id")?;
            match item.get("output") {
                Some(Value::String(_)) => {}
                Some(Value::Array(parts)) => validate_content_parts(parts)?,
                _ => bail!("tool output must be text or a content array"),
            }
        }
        "reasoning" => {
            optional_string(item, "encrypted_content")?;
            for key in ["summary", "content"] {
                if let Some(parts) = item.get(key).filter(|value| !value.is_null()) {
                    validate_content_parts(
                        parts
                            .as_array()
                            .with_context(|| format!("reasoning {key} must be an array"))?,
                    )?;
                }
            }
        }
        "compaction" | "compaction_summary" => {
            nonempty(item, "encrypted_content")?;
        }
        // Native server tools and future opaque items must survive local resume.
        // We deliberately do not reconstruct or authorize them as client calls.
        _ => {}
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn snapshots_accept_native_legacy_and_current_containers_without_modification() {
        let anthropic = json!([
            {"role":"user","content":"legacy text"},
            {"role":"assistant","content":[
                {"type":"thinking","thinking":"reasoning","signature":"signed"},
                {"type":"server_tool_use","id":"native-1","name":"web_search","input":{"query":"synthetic"}},
                {"type":"web_search_tool_result","tool_use_id":"native-1","content":[]},
                {"type":"vendor_extension","opaque":{"retain":true}},
                {"type":"tool_use","id":"call-1","name":"read_file","input":{"path":"sample.txt"}}
            ]}
        ]);
        let chat = json!([
            {"role":"user","content":"fixture"},
            {"role":"assistant","content":null,"tool_calls":[{"id":"call-1","type":"function","function":{"name":"read_file","arguments":"{}"}}]},
            {"role":"tool","tool_call_id":"call-1","content":"fixture output"}
        ]);
        let responses = json!([
            {"role":"user","content":"legacy easy input"},
            {"type":"message","role":"assistant","content":[{"type":"output_text","text":"fixture"}]},
            {"type":"reasoning","encrypted_content":"encrypted","summary":[]},
            {"type":"compaction_summary","encrypted_content":"summary"},
            {"type":"web_search_call","id":"native-1","status":"completed","action":{"type":"search","query":"synthetic"}},
            {"type":"function_call","call_id":"call-1","name":"read_file","arguments":"{}"},
            {"type":"function_call_output","call_id":"call-1","output":"contents"},
            {"type":"custom_tool_call","call_id":"call-2","name":"exec","input":"text(1)"},
            {"type":"custom_tool_call_output","call_id":"call-2","output":"1"}
        ]);
        for (transport, snapshot) in [
            ("anthropic", anthropic),
            ("openai-chat", chat),
            ("openai-responses", responses.clone()),
            (
                "openai-responses",
                json!({"input":responses,"ambient_hash":null}),
            ),
            (
                "openai-responses",
                json!({"input":[],"ambient_hash":42,"future_metadata":true}),
            ),
            ("openai-responses", json!({"input":[]})),
        ] {
            let before = snapshot.clone();
            validate_snapshot(transport, &snapshot).unwrap();
            assert_eq!(snapshot, before);
        }
        for transport in ["anthropic", "openai-chat", "openai-responses"] {
            validate_snapshot(transport, &json!([])).unwrap();
        }
    }

    #[test]
    fn snapshots_reject_wrong_container_missing_content_and_invalid_metadata() {
        for (transport, snapshot) in [
            ("unknown", json!([])),
            ("anthropic", Value::Null),
            ("anthropic", json!({"messages":[]})),
            ("openai-chat", json!({"messages":[]})),
            ("openai-responses", json!({})),
            ("openai-responses", json!({"input":null})),
            ("openai-responses", json!({"input":[],"ambient_hash":"42"})),
            ("openai-responses", json!({"input":[],"ambient_hash":-1})),
            ("anthropic", json!([null])),
            ("openai-chat", json!(["message"])),
            ("anthropic", json!([{"role":"assistant"}])),
            ("anthropic", json!([{"role":"unknown","content":[]}])),
            ("openai-chat", json!([{"role":"user","content":null}])),
            ("openai-chat", json!([{"role":"tool","content":"result"}])),
            (
                "openai-responses",
                json!([{"type":"message","role":"user"}]),
            ),
            ("openai-responses", json!([{}])),
            (
                "openai-responses",
                json!([{"type":"message","role":"user","content":[{"type":"input_text"}]}]),
            ),
            ("openai-responses", json!([{"type":"compaction_summary"}])),
        ] {
            assert!(
                validate_snapshot(transport, &snapshot).is_err(),
                "accepted {transport}: {snapshot}"
            );
        }
    }

    #[test]
    fn snapshots_reject_corrupted_calls_without_reinterpreting_their_arguments() {
        for raw in ["", "{", "null", "[]", "1"] {
            let call = json!({"type":"function_call","call_id":"call-1","name":"file_write","arguments":raw});
            assert!(validate_snapshot("openai-responses", &json!([call])).is_err());
            let call = json!({"id":"call-1","type":"function","function":{"name":"file_write","arguments":raw}});
            assert!(
                validate_snapshot(
                    "openai-chat",
                    &json!([{"role":"assistant","content":null,"tool_calls":[call]}])
                )
                .is_err()
            );
        }
        for input in [Value::Null, json!([]), json!(1)] {
            assert!(
                validate_snapshot(
                    "anthropic",
                    &json!([{"role":"assistant","content":[{
                        "type":"tool_use","id":"call-1","name":"file_write","input":input
                    }]}])
                )
                .is_err()
            );
        }
        assert!(
            validate_snapshot(
                "openai-responses",
                &json!([{
                    "type":"custom_tool_call","call_id":"call-1","name":"exec","input":null
                }])
            )
            .is_err()
        );
    }
}
