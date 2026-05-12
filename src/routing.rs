//! Routing primitives shared between every event inlet (webhooks
//! today, pollers tomorrow). The pipeline is the same regardless of
//! how the event arrived:
//!
//! ```text
//!   raw payload + headers
//!     → extractor (one entity)
//!     → routing packet (verdict consequent: JSON-encoded)
//!     → resolve_entity_template (substitute ${entity.X} → typed value)
//!     → RoutingVerdict::parse (typed verdict)
//!     → dispatch (start_arc / signal_arc / cancel_arc / ignore / dead_letter)
//! ```
//!
//! Inlets are responsible for steps 1–2 (they each have their own
//! signature/scheduling concerns). The shared primitives below cover
//! 3–4. Step 5 is `crate::dispatch_routed_event` in `main.rs` so it
//! can reach `BlackboxServer` + `workflow_registry` without dragging
//! the whole orchestration surface into this module.

use anyhow::{Result, anyhow};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Dispatch verdict produced by a routing packet. Routing packet
/// rule consequents are JSON-encoded objects of this shape (or the
/// shorthand string forms `"ignore"` / `"dead_letter"`).
///
/// `correlate` and `payload` (where present) may contain
/// `${entity.path}` template strings; the dispatch path runs
/// [`resolve_entity_template`] over the parsed value before matching
/// against pending waits, so the routing rule can carry typed fields
/// from the extracted entity into the verdict without the routing
/// packet author having to know predicate-AST templating tricks.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "route", rename_all = "snake_case")]
pub enum RoutingVerdict {
    /// Spawn a fresh arc against the named workflow.
    StartArc {
        workflow: String,
        #[serde(default)]
        initial_vars: serde_json::Map<String, Value>,
    },
    /// Resolve a pending Wait by signal name + correlation tuple.
    SignalArc {
        signal: String,
        #[serde(default)]
        correlate: serde_json::Map<String, Value>,
        /// Optional payload delivered to the resumed wait as
        /// `${last_signal.payload}`. When omitted, the dispatcher
        /// substitutes the full extracted entity.
        #[serde(default)]
        payload: Option<Value>,
    },
    /// Match-and-cancel running arcs by correlation.
    CancelArc {
        #[serde(default)]
        correlate: serde_json::Map<String, Value>,
    },
    /// Drop silently.
    Ignore,
    /// Drop loudly (write to bbox_inbox for forensics).
    DeadLetter {
        #[serde(default)]
        reason: Option<String>,
    },
}

impl RoutingVerdict {
    /// Parse a routing packet's consequent into a typed verdict.
    /// Accepted shapes:
    ///   1. Plain string shorthand: `"ignore"`, `"dead_letter"`.
    ///   2. JSON object discriminated by `route`. Because rule
    ///      consequents are typed `packets::Value` (scalar only), the
    ///      object form is carried as a JSON-encoded string.
    ///   3. Structured `Value::Object` (already an object) — for
    ///      tests that bypass the packet AST.
    pub fn parse(consequent: &Value) -> Result<RoutingVerdict> {
        if let Some(s) = consequent.as_str() {
            let trimmed = s.trim();
            match trimmed {
                "ignore" => return Ok(RoutingVerdict::Ignore),
                "dead_letter" => return Ok(RoutingVerdict::DeadLetter { reason: None }),
                _ => {}
            }
            if trimmed.starts_with('{') {
                let parsed: Value = serde_json::from_str(trimmed)
                    .map_err(|e| anyhow!("routing verdict JSON in string: {e}"))?;
                return serde_json::from_value(parsed)
                    .map_err(|e| anyhow!("routing verdict shape: {e}"));
            }
            return Ok(RoutingVerdict::DeadLetter {
                reason: Some(format!("unknown route shorthand '{trimmed}'")),
            });
        }
        serde_json::from_value(consequent.clone())
            .map_err(|e| anyhow!("routing verdict parse failed: {e}"))
    }
}

/// Walk `raw` and substitute every whole-string `${entity.path}`
/// reference with the typed value at that path in `entity`.
///
/// Mirrors `workflow::context::resolve_arg_value` but with one scope
/// (the extracted entity). Used by the dispatch path so a routing
/// rule's verdict consequent can carry typed correlation tuples
/// without the rule author hand-encoding ints, e.g.:
///
/// ```jsonc
/// "consequent": "{\"route\":\"signal_arc\",\"signal\":\"pr-ready\",\"correlate\":{\"pr\":\"${entity.pr_number}\"}}"
/// ```
///
/// resolves `${entity.pr_number}` to `Number(117)` so it actually
/// matches the `correlate: {"pr": 117}` registered by the wait.
///
/// Mixed-content strings (e.g. `"PR ${entity.pr_number} ready"`) are
/// rendered as text via `Value::String`. Whole-string templates
/// (the entire string is one `${...}`) preserve typing.
///
/// Unresolved references are left verbatim so misspelled paths show
/// up in the dispatched verdict rather than turning into Null.
pub fn resolve_entity_template(entity: &Value, raw: &Value) -> Value {
    match raw {
        Value::String(s) => {
            let trimmed = s.trim();
            // JSON-encoded object/array carrier: rule consequents are
            // typed as packet scalar `Value::String` so the structured
            // verdict travels as a JSON-encoded blob. Parse it first,
            // resolve the parsed tree (so leaf `${entity.X}` strings hit
            // the whole-string typing branch and preserve `Number` /
            // `Bool`), then re-encode for the caller's parse step.
            // Without this, `render_string` stringifies every leaf and
            // a typed `correlate: {pr: Number(19)}` round-trips as
            // `{pr: "19"}` — broken correlation matching.
            if (trimmed.starts_with('{') && trimmed.ends_with('}'))
                || (trimmed.starts_with('[') && trimmed.ends_with(']'))
            {
                if let Ok(parsed) = serde_json::from_str::<Value>(trimmed) {
                    let resolved = resolve_entity_template(entity, &parsed);
                    return Value::String(
                        serde_json::to_string(&resolved).unwrap_or_else(|_| s.clone()),
                    );
                }
            }
            let is_whole = trimmed.starts_with("${")
                && trimmed.ends_with('}')
                && find_close_brace(trimmed, 2) == Some(trimmed.len() - 1);
            if is_whole {
                let expr = &trimmed[2..trimmed.len() - 1];
                resolve_one(entity, expr).unwrap_or_else(|| raw.clone())
            } else {
                Value::String(render_string(entity, s))
            }
        }
        Value::Array(items) => Value::Array(
            items
                .iter()
                .map(|i| resolve_entity_template(entity, i))
                .collect(),
        ),
        Value::Object(map) => {
            let mut out = serde_json::Map::with_capacity(map.len());
            for (k, v) in map {
                out.insert(k.clone(), resolve_entity_template(entity, v));
            }
            Value::Object(out)
        }
        other => other.clone(),
    }
}

fn resolve_one(entity: &Value, expr: &str) -> Option<Value> {
    let parts: Vec<&str> = expr.split('.').collect();
    if parts.is_empty() || parts[0] != "entity" {
        return None;
    }
    walk(entity, &parts[1..])
}

fn walk(root: &Value, path: &[&str]) -> Option<Value> {
    let mut cur = root.clone();
    for seg in path {
        cur = match &cur {
            Value::Object(m) => m.get(*seg).cloned()?,
            Value::Array(a) => {
                let idx: usize = seg.parse().ok()?;
                a.get(idx).cloned()?
            }
            _ => return None,
        };
    }
    Some(cur)
}

fn render_string(entity: &Value, template: &str) -> String {
    let mut out = String::with_capacity(template.len());
    let bytes = template.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if i + 1 < bytes.len() && bytes[i] == b'$' && bytes[i + 1] == b'{' {
            if let Some(close) = find_close_brace(template, i + 2) {
                let expr = &template[i + 2..close];
                match resolve_one(entity, expr) {
                    Some(Value::String(s)) => out.push_str(&s),
                    Some(Value::Null) => {}
                    Some(other) => out.push_str(&other.to_string()),
                    None => {
                        out.push_str("${");
                        out.push_str(expr);
                        out.push('}');
                    }
                }
                i = close + 1;
                continue;
            }
        }
        out.push(bytes[i] as char);
        i += 1;
    }
    out
}

fn find_close_brace(s: &str, start: usize) -> Option<usize> {
    let bytes = s.as_bytes();
    let mut i = start;
    while i < bytes.len() {
        if bytes[i] == b'}' {
            return Some(i);
        }
        i += 1;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parse_string_shorthand() {
        assert!(matches!(
            RoutingVerdict::parse(&json!("ignore")).unwrap(),
            RoutingVerdict::Ignore
        ));
        assert!(matches!(
            RoutingVerdict::parse(&json!("dead_letter")).unwrap(),
            RoutingVerdict::DeadLetter { .. }
        ));
    }

    #[test]
    fn parse_json_string_object() {
        let v = json!(r#"{"route":"start_arc","workflow":"x"}"#);
        match RoutingVerdict::parse(&v).unwrap() {
            RoutingVerdict::StartArc { workflow, .. } => assert_eq!(workflow, "x"),
            _ => panic!("expected StartArc"),
        }
    }

    #[test]
    fn parse_signal_arc_with_payload() {
        let v = json!({
            "route": "signal_arc",
            "signal": "pr-feedback",
            "correlate": {"pr": 42},
            "payload": {"review": {"body": "lgtm"}}
        });
        match RoutingVerdict::parse(&v).unwrap() {
            RoutingVerdict::SignalArc {
                signal,
                correlate,
                payload,
            } => {
                assert_eq!(signal, "pr-feedback");
                assert_eq!(correlate.get("pr"), Some(&json!(42)));
                assert_eq!(payload, Some(json!({"review": {"body": "lgtm"}})));
            }
            _ => panic!("expected SignalArc"),
        }
    }

    #[test]
    fn entity_template_whole_string_preserves_typing() {
        let entity = json!({"pr_number": 117, "owner": "alice"});
        // Single ${entity.pr_number} resolves to typed Number, not "117".
        let raw = json!("${entity.pr_number}");
        let resolved = resolve_entity_template(&entity, &raw);
        assert_eq!(resolved, json!(117));
    }

    #[test]
    fn entity_template_object_substitutes_typed() {
        let entity = json!({"pr_number": 117, "owner": "alice"});
        let raw = json!({
            "route": "signal_arc",
            "signal": "pr-ready",
            "correlate": {"pr": "${entity.pr_number}", "owner": "${entity.owner}"}
        });
        let resolved = resolve_entity_template(&entity, &raw);
        assert_eq!(
            resolved,
            json!({
                "route": "signal_arc",
                "signal": "pr-ready",
                "correlate": {"pr": 117, "owner": "alice"}
            })
        );
    }

    #[test]
    fn entity_template_json_encoded_string_preserves_typing() {
        // Rule consequents arrive as `Value::String("{...}")` because the
        // packet's typed scalar can't carry an Object. The resolver must
        // detect that, parse the inner JSON, walk it, and re-encode — so
        // leaf `${entity.X}` strings hit the whole-string typing branch
        // instead of being stringified by `render_string`. This pins the
        // bug fix for the keystone webhook → wait correlation path.
        let entity = json!({"pr_number": 19});
        let raw = json!(
            r#"{"route":"signal_arc","signal":"pr-ready","correlate":{"pr":"${entity.pr_number}"}}"#
        );
        let resolved = resolve_entity_template(&entity, &raw);
        // Result is still a JSON-encoded string (caller's parse step
        // re-decodes it), but the embedded `pr` value is a JSON number,
        // not a JSON string.
        let resolved_str = resolved.as_str().expect("string carrier preserved");
        let parsed: Value = serde_json::from_str(resolved_str).unwrap();
        assert_eq!(
            parsed,
            json!({
                "route": "signal_arc",
                "signal": "pr-ready",
                "correlate": {"pr": 19}
            })
        );
    }

    #[test]
    fn entity_template_mixed_string_renders_text() {
        let entity = json!({"pr_number": 117});
        let raw = json!("PR #${entity.pr_number} is ready");
        let resolved = resolve_entity_template(&entity, &raw);
        assert_eq!(resolved, json!("PR #117 is ready"));
    }

    #[test]
    fn entity_template_unresolved_left_verbatim() {
        let entity = json!({"x": 1});
        let raw = json!("${entity.missing.path}");
        let resolved = resolve_entity_template(&entity, &raw);
        // Whole-string unresolved → original value preserved verbatim.
        assert_eq!(resolved, json!("${entity.missing.path}"));
    }

    #[test]
    fn entity_template_non_entity_head_left_verbatim() {
        // Only `entity.X` is in scope here — `vars.X` etc are not.
        let entity = json!({"x": 1});
        let raw = json!("${vars.x}");
        let resolved = resolve_entity_template(&entity, &raw);
        assert_eq!(resolved, json!("${vars.x}"));
    }

    #[test]
    fn parse_then_resolve_yields_typed_correlation() {
        let consequent = json!(
            r#"{"route":"signal_arc","signal":"pr-ready","correlate":{"pr":"${entity.pr_number}"}}"#
        );
        let entity = json!({"pr_number": 117});
        // Step 1: parse the static shape.
        let verdict = RoutingVerdict::parse(&consequent).unwrap();
        // Step 2: resolve the embedded ${entity.X} references against
        // the entity. To do that we serialize back, resolve, and
        // reparse — same path the dispatcher uses.
        let serialized = serde_json::to_value(&verdict).unwrap();
        let resolved = resolve_entity_template(&entity, &serialized);
        let final_verdict = RoutingVerdict::parse(&resolved).unwrap();
        match final_verdict {
            RoutingVerdict::SignalArc { correlate, .. } => {
                assert_eq!(correlate.get("pr"), Some(&json!(117)))
            }
            _ => panic!("expected SignalArc"),
        }
    }
}
