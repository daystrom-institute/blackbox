use super::OpEffect;
use anyhow::{Result, anyhow};
use serde_json::{Map, Value, json};

pub(super) fn exec_parse_json(args: &Value, into_var: Option<&str>) -> Result<OpEffect> {
    let into = into_var.ok_or_else(|| anyhow!("ParseJson requires into_var on the HookOp spec"))?;
    let from = args
        .get("from")
        .ok_or_else(|| anyhow!("ParseJson requires args.from (string or value)"))?;
    let repair_missing_closing_delimiters = args
        .get("repair_missing_closing_delimiters")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let parsed = match from {
        Value::String(s) => parse_json_string(s, repair_missing_closing_delimiters)?,
        other => other.clone(),
    };
    Ok(OpEffect::SetVar {
        key: into.to_string(),
        value: parsed,
    })
}

fn parse_json_string(s: &str, repair_missing_closing_delimiters: bool) -> Result<Value> {
    let trimmed = s.trim();
    if trimmed.is_empty() {
        return Ok(Value::Null);
    }
    let stripped = strip_code_fence(trimmed).unwrap_or(trimmed.to_string());
    match serde_json::from_str(&stripped) {
        Ok(value) => Ok(value),
        Err(first_err) => {
            let mut last_err = first_err.to_string();
            if repair_missing_closing_delimiters
                && let Some(repaired) = append_missing_json_closers(&stripped)
            {
                match serde_json::from_str(&repaired) {
                    Ok(value) => return Ok(value),
                    Err(err) => last_err = err.to_string(),
                }
            }
            let candidates = crate::tools::bro_helpers::extract_json_candidates(trimmed);
            for candidate in candidates {
                match serde_json::from_str(&candidate) {
                    Ok(value) => return Ok(value),
                    Err(err) => {
                        last_err = err.to_string();
                        if repair_missing_closing_delimiters
                            && let Some(repaired) = append_missing_json_closers(&candidate)
                        {
                            match serde_json::from_str(&repaired) {
                                Ok(value) => return Ok(value),
                                Err(repair_err) => {
                                    last_err = repair_err.to_string();
                                }
                            }
                        }
                    }
                }
            }
            Err(anyhow!(
                "ParseJson: input did not parse as JSON: {last_err}"
            ))
        }
    }
}

pub(super) fn coerce_json_value(value: &Value) -> Value {
    match value {
        Value::String(s) => {
            let trimmed = s.trim();
            if trimmed.starts_with('{') || trimmed.starts_with('[') {
                serde_json::from_str(trimmed).unwrap_or_else(|_| value.clone())
            } else {
                value.clone()
            }
        }
        other => other.clone(),
    }
}

pub(super) fn ensure_objectish_json(value: Value) -> Result<Value> {
    match value {
        Value::Object(_) => Ok(value),
        Value::String(s) if s.trim().is_empty() => Ok(json!({})),
        Value::String(s) => Ok(json!({ "summary": s })),
        other => Ok(json!({ "value": other })),
    }
}

pub(super) fn normalize_array_field(
    request_args: &mut Map<String, Value>,
    defaults: &Map<String, Value>,
    key: &str,
) {
    let current = request_args
        .remove(key)
        .or_else(|| defaults.get(key).cloned())
        .unwrap_or_else(|| Value::Array(Vec::new()));
    let normalized = match coerce_json_value(&current) {
        Value::Array(values) => Value::Array(values),
        Value::String(s) if s.trim().is_empty() => Value::Array(Vec::new()),
        Value::String(s) => Value::Array(vec![Value::String(s)]),
        other => Value::Array(vec![other]),
    };
    request_args.insert(key.to_string(), normalized);
}

fn append_missing_json_closers(s: &str) -> Option<String> {
    let mut stack = Vec::new();
    let mut in_string = false;
    let mut escaped = false;

    for ch in s.chars() {
        if in_string {
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == '"' {
                in_string = false;
            }
            continue;
        }

        match ch {
            '"' => in_string = true,
            '{' => stack.push('}'),
            '[' => stack.push(']'),
            '}' | ']' if stack.pop() != Some(ch) => {
                return None;
            }
            _ => {}
        }
    }

    if in_string || stack.is_empty() {
        return None;
    }

    let mut repaired = s.trim_end().to_string();
    for ch in stack.into_iter().rev() {
        repaired.push(ch);
    }
    Some(repaired)
}

fn strip_code_fence(s: &str) -> Option<String> {
    let lines: Vec<&str> = s.lines().collect();
    if lines.is_empty() {
        return None;
    }
    let first = lines[0].trim();
    let opens_fence = first == "```json" || first == "```JSON" || first == "```";
    if opens_fence {
        let last_idx = lines.iter().rposition(|l| l.trim() == "```")?;
        if last_idx == 0 {
            return None;
        }
        return Some(lines[1..last_idx].join("\n"));
    }
    let opener_idx = lines.iter().position(|l| {
        let t = l.trim();
        t == "```json" || t == "```JSON" || t == "```"
    })?;
    let closer_idx = lines[opener_idx + 1..]
        .iter()
        .position(|l| l.trim() == "```")?
        + opener_idx
        + 1;
    if closer_idx <= opener_idx + 1 {
        return None;
    }
    Some(lines[opener_idx + 1..closer_idx].join("\n"))
}
