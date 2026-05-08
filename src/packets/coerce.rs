use super::{Packet, Value};

// ── Evaluator (deterministic, no LLM) ────────────────────────────

/// Augment `entity` with fields derived from `packet.rank_table` /
/// `packet.threshold_table` lookups. Pure function over the entity
/// map; does not mutate the packet.
pub(super) fn resolve_entity(
    packet: &Packet,
    entity: &serde_json::Map<String, serde_json::Value>,
) -> serde_json::Map<String, serde_json::Value> {
    let mut resolved = entity.clone();

    // rank lookup: entity[rank_lookup_key] is a name → packet.rank_table[name] → int
    if !packet.rank_table.is_empty() {
        if let Some(serde_json::Value::String(key)) = entity.get(&packet.rank_lookup_key) {
            if let Some(rank) = packet.rank_table.get(key) {
                resolved.insert(
                    format!("{}_rank", packet.rank_lookup_key),
                    serde_json::Value::Number((*rank).into()),
                );
            }
        }
    }

    if !packet.threshold_table.is_empty() {
        if let Some(serde_json::Value::String(key)) = entity.get(&packet.threshold_lookup_key) {
            if let Some(threshold) = packet.threshold_table.get(key) {
                // Convention: res_threshold (from "resource" → "res_threshold")
                let field_name = if packet.threshold_lookup_key == "resource" {
                    "res_threshold".to_string()
                } else {
                    format!("{}_threshold", packet.threshold_lookup_key)
                };
                resolved.insert(field_name, serde_json::Value::Number((*threshold).into()));
            }
        }
    }

    resolved
}

pub(super) fn entity_get(
    entity: &serde_json::Map<String, serde_json::Value>,
    field: &str,
) -> Option<Value> {
    entity_get_raw(entity, field).and_then(|v| Value::from_json(&v))
}

/// Raw JSON lookup with dotted-path support. Tries literal-key first
/// (back-compat for legacy field names containing dots), then walks
/// `head.tail.tail` against the entity, descending into nested objects
/// and array indices. Used by every entity_* helper for consistent
/// path semantics across the predicate evaluator.
pub(super) fn entity_get_raw(
    entity: &serde_json::Map<String, serde_json::Value>,
    field: &str,
) -> Option<serde_json::Value> {
    if let Some(v) = entity.get(field) {
        return Some(v.clone());
    }
    if !field.contains('.') {
        return None;
    }
    let parts: Vec<&str> = field.split('.').collect();
    let mut cur = entity.get(parts[0])?.clone();
    for seg in &parts[1..] {
        cur = match &cur {
            serde_json::Value::Object(m) => m.get(*seg)?.clone(),
            serde_json::Value::Array(a) => {
                let idx: usize = seg.parse().ok()?;
                a.get(idx)?.clone()
            }
            _ => return None,
        };
    }
    Some(cur)
}

pub(super) fn entity_int(
    entity: &serde_json::Map<String, serde_json::Value>,
    field: &str,
) -> Option<i64> {
    entity_get_raw(entity, field).and_then(|v| v.as_i64())
}

pub(super) fn entity_f64(
    entity: &serde_json::Map<String, serde_json::Value>,
    field: &str,
) -> Option<f64> {
    entity_get_raw(entity, field).and_then(|v| v.as_f64())
}

pub(super) fn entity_has(entity: &serde_json::Map<String, serde_json::Value>, field: &str) -> bool {
    // Key exists AND value is non-null. Used by `IsNonNull`. Distinct
    // from `entity_key_exists` (which counts null as present).
    match entity_get_raw(entity, field) {
        None => false,
        Some(serde_json::Value::Null) => false,
        Some(_) => true,
    }
}

pub(super) fn entity_key_exists(
    entity: &serde_json::Map<String, serde_json::Value>,
    field: &str,
) -> bool {
    if entity.contains_key(field) {
        return true;
    }
    entity_get_raw(entity, field).is_some()
}

pub(super) fn entity_is_null(
    entity: &serde_json::Map<String, serde_json::Value>,
    field: &str,
) -> bool {
    matches!(entity_get_raw(entity, field), Some(serde_json::Value::Null))
}

/// Resolve a path like `"tools[*]"` or `"vars.labels[*]"` to the
/// backing array in the entity. Returns `None` if the path doesn't end
/// in `[*]`, the field is missing, or the value isn't an array.
///
/// Dotted-path support (added when the workflow engine started passing
/// structured ArcContext entities — `vars.labels[*]`,
/// `outputs.Plan.findings[*]`, etc.). The flat single-field form
/// keeps working unchanged.
///
/// Returned as an owned Vec because the dotted-path walk has to clone
/// to descend through nested objects; a borrow would need an arena.
pub(super) fn resolve_collection(
    entity: &serde_json::Map<String, serde_json::Value>,
    path: &str,
) -> Option<Vec<serde_json::Value>> {
    let field = path.strip_suffix("[*]")?;
    let value = if field.contains('.') {
        entity_get_raw(entity, field)?
    } else {
        entity.get(field)?.clone()
    };
    match value {
        serde_json::Value::Array(a) => Some(a),
        _ => None,
    }
}

/// Absorb a provider-serialization quirk observed in E12: some MCP
/// clients (notably Codex on first-attempt) pass structured params
/// as stringified JSON rather than structured arrays/objects. This
/// helper inspects a value — if it's a `String` that starts with
/// `{` or `[` and parses as JSON, it replaces the string with the
/// parsed value in place. No-op on already-structured values.
///
/// Applied at the tool boundary so the wire shape from the agent
/// doesn't need to be pixel-perfect to succeed. Trade: an agent who
/// genuinely wants a JSON-literal string as input (very unusual in
/// this surface) sees it coerced to structure. That's the right
/// trade for an AI-facing API where the first-attempt cost of retry
/// is much higher than the near-zero cost of permissive parsing.
pub(super) fn unwrap_jsonish(v: &mut serde_json::Value) {
    if let serde_json::Value::String(s) = v {
        let trimmed = s.trim_start();
        if trimmed.starts_with('{') || trimmed.starts_with('[') {
            if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(s) {
                *v = parsed;
            }
        }
    }
}

/// Accept `packet-<8hex>`, `domain:<name>`, or bare `<8hex>`.
pub(super) fn normalize_id(id: &str) -> String {
    if id.starts_with("packet-") || id.starts_with("domain:") {
        id.to_string()
    } else {
        format!("packet-{id}")
    }
}
