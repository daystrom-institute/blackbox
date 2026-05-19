use anyhow::{Result, anyhow, bail};
use serde_json::{Map, Value, json};

use super::OpEffect;
use crate::workflow::context::ArcContext;

pub(super) fn exec_find_first(args: &Value, into_var: Option<&str>) -> Result<OpEffect> {
    let into = into_var.ok_or_else(|| anyhow!("FindFirst requires into_var on the HookOp spec"))?;
    let arr = args
        .get("from")
        .ok_or_else(|| anyhow!("FindFirst requires args.from (array)"))?;
    let where_obj = args
        .get("where")
        .and_then(|v| v.as_object())
        .ok_or_else(|| anyhow!("FindFirst requires args.where (object of field->value)"))?;
    let items: &[Value] = match arr {
        Value::Array(a) => a.as_slice(),
        Value::Null => &[],
        other => bail!("FindFirst args.from must be array or null, got {other:?}"),
    };
    let mut found: Value = Value::Null;
    'outer: for item in items {
        for (k, expected) in where_obj {
            // Walk dotted path inside the element.
            let actual = walk_dotted(item, k);
            if actual.as_ref() != Some(expected) {
                continue 'outer;
            }
        }
        found = item.clone();
        break;
    }
    Ok(OpEffect::SetVar {
        key: into.to_string(),
        value: found,
    })
}

fn walk_dotted(root: &Value, path: &str) -> Option<Value> {
    let mut cur = root.clone();
    for seg in path.split('.') {
        cur = match &cur {
            Value::Object(m) => m.get(seg).cloned()?,
            Value::Array(a) => {
                let idx: usize = seg.parse().ok()?;
                a.get(idx).cloned()?
            }
            _ => return None,
        };
    }
    Some(cur)
}

pub(super) fn exec_set_var(args: &Value) -> Result<OpEffect> {
    // Two arg shapes accepted:
    //   { "key": "name", "value": <any> }
    //   { "name": <any>, "other_name": <any>, ... }   (bulk form)
    if let Some(obj) = args.as_object() {
        if let (Some(Value::String(k)), Some(v)) = (obj.get("key"), obj.get("value")) {
            return Ok(OpEffect::SetVar {
                key: k.clone(),
                value: v.clone(),
            });
        }
        // Bulk form: every key is a var name. We can only return one
        // effect here; the runner treats SetVar as a single mutation,
        // so bulk form is implemented by emitting multiple effects via
        // the special `Bulk` shape - keep it simple, require `{key,
        // value}` for now.
        if obj.len() == 1 {
            let (k, v) = obj.iter().next().unwrap();
            return Ok(OpEffect::SetVar {
                key: k.clone(),
                value: v.clone(),
            });
        }
    }
    bail!("SetVar args must be {{key,value}} or a single-entry object, got: {args}")
}

pub(super) fn exec_default_var(args: &Value, ctx: &ArcContext) -> Result<OpEffect> {
    let key = args
        .get("key")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("DefaultVar requires args.key (string)"))?;
    let value = args
        .get("value")
        .ok_or_else(|| anyhow!("DefaultVar requires args.value"))?;
    if ctx.vars.get(key).is_some_and(|v| !v.is_null()) {
        return Ok(OpEffect::None);
    }
    Ok(OpEffect::SetVar {
        key: key.to_string(),
        value: value.clone(),
    })
}

pub(super) fn exec_inc_var(args: &Value, ctx: &ArcContext) -> Result<OpEffect> {
    let key = args
        .get("key")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("IncVar requires args.key (string)"))?;
    let by = args.get("by").and_then(|v| v.as_i64()).unwrap_or(1);
    let current = ctx.vars.get(key).and_then(|v| v.as_i64()).unwrap_or(0);
    Ok(OpEffect::SetVar {
        key: key.to_string(),
        value: json!(current + by),
    })
}

pub(super) fn exec_append_var(args: &Value, ctx: &ArcContext) -> Result<OpEffect> {
    let key = args
        .get("key")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("AppendVar requires args.key (string)"))?;
    let value = args
        .get("value")
        .ok_or_else(|| anyhow!("AppendVar requires args.value"))?;
    let mut arr = match ctx.vars.get(key).cloned() {
        Some(Value::Array(a)) => a,
        Some(Value::Null) | None => Vec::new(),
        Some(other) => {
            bail!("AppendVar: vars[{key}] is {other:?}, not an array");
        }
    };
    arr.push(value.clone());
    Ok(OpEffect::SetVar {
        key: key.to_string(),
        value: Value::Array(arr),
    })
}

pub(super) fn exec_merge_var(args: &Value, ctx: &ArcContext) -> Result<OpEffect> {
    let key = args
        .get("key")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("MergeVar requires args.key (string)"))?;
    let value = args
        .get("value")
        .and_then(|v| v.as_object())
        .ok_or_else(|| anyhow!("MergeVar requires args.value (object)"))?;
    let mut merged = match ctx.vars.get(key).cloned() {
        Some(Value::Object(m)) => m,
        Some(Value::Null) | None => Map::new(),
        Some(other) => bail!("MergeVar: vars[{key}] is {other:?}, not an object"),
    };
    for (k, v) in value {
        merged.insert(k.clone(), v.clone());
    }
    Ok(OpEffect::SetVar {
        key: key.to_string(),
        value: Value::Object(merged),
    })
}

pub(super) fn exec_set_meta(args: &Value) -> Result<OpEffect> {
    // Mutable meta keys: `worktree`, `project_dir`. Other meta fields
    // are arc-intrinsic and immutable.
    let key = args
        .get("key")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("SetMeta requires args.key"))?;
    let value = args.get("value").cloned().unwrap_or(Value::Null);
    match key {
        "worktree" => {
            let v = match value {
                Value::String(s) => Some(s),
                Value::Null => None,
                other => bail!("SetMeta worktree must be string or null, got {other:?}"),
            };
            Ok(OpEffect::SetWorktree(v))
        }
        "project_dir" => {
            let v = match value {
                Value::String(s) if s.is_empty() => None,
                Value::String(s) => Some(s),
                Value::Null => None,
                other => bail!("SetMeta project_dir must be string or null, got {other:?}"),
            };
            Ok(OpEffect::SetProjectDir(v))
        }
        other => bail!("SetMeta: unsupported key '{other}' (mutable keys: worktree, project_dir)"),
    }
}

/// Pick the first element of an array into `vars[into_var]`.
///
/// Reads the array from:
/// - `args.from` if it is already an array value (e.g. a rendered template),
/// - `vars[args.array]` if `args.array` is a simple var name,
/// - `vars[args.array.path...]` via dotted-path walk when the var is a nested
///   object (e.g. `"array": "candidate_pairs.candidates"`).
///
/// Writes `Value::Null` when the array is absent or empty.
pub(super) fn exec_pick_first(
    args: &Value,
    into_var: Option<&str>,
    ctx: &ArcContext,
) -> Result<OpEffect> {
    let into = into_var.ok_or_else(|| anyhow!("PickFirst requires into_var"))?;
    let resolved: Option<Value> = if let Some(from) = args.get("from") {
        Some(from.clone())
    } else if let Some(path) = args.get("array").and_then(Value::as_str) {
        // Walk dotted path inside vars.
        let mut cur = Value::Object(ctx.vars.clone());
        for seg in path.split('.') {
            cur = match cur {
                Value::Object(m) => m.get(seg).cloned().unwrap_or(Value::Null),
                Value::Array(a) => {
                    if let Ok(i) = seg.parse::<usize>() {
                        a.get(i).cloned().unwrap_or(Value::Null)
                    } else {
                        Value::Null
                    }
                }
                _ => Value::Null,
            };
        }
        Some(cur)
    } else {
        None
    };
    let first = resolved
        .as_ref()
        .and_then(Value::as_array)
        .and_then(|a| a.first())
        .cloned()
        .unwrap_or(Value::Null);
    Ok(OpEffect::SetVar {
        key: into.to_string(),
        value: first,
    })
}
