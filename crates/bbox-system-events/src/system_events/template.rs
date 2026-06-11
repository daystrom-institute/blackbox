use anyhow::{Result, bail};
use serde_json::Value;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnresolvedPolicy {
    LeaveVerbatim,
    HardError,
}

pub fn render_template_with_roots(
    template: &str,
    roots: &serde_json::Map<String, Value>,
    unresolved: UnresolvedPolicy,
) -> Result<String> {
    let mut out = String::with_capacity(template.len());
    let bytes = template.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if i + 1 < bytes.len() && bytes[i] == b'$' && bytes[i + 1] == b'{' {
            if let Some(close) = find_close_brace(template, i + 2) {
                let expr = &template[i + 2..close];
                match resolve_in_roots(expr, roots) {
                    Some(v) => out.push_str(&value_to_template_string(&v)),
                    None => match unresolved {
                        UnresolvedPolicy::LeaveVerbatim => {
                            out.push_str("${");
                            out.push_str(expr);
                            out.push('}');
                        }
                        UnresolvedPolicy::HardError => {
                            bail!("unresolved template expression: '${{{expr}}}'")
                        }
                    },
                }
                i = close + 1;
                continue;
            }
        }
        out.push(bytes[i] as char);
        i += 1;
    }
    Ok(out)
}

fn resolve_in_roots(expr: &str, roots: &serde_json::Map<String, Value>) -> Option<Value> {
    let parts: Vec<&str> = expr.split('.').collect();
    if parts.is_empty() {
        return None;
    }
    match parts[0] {
        "env" => {
            if parts.len() != 2 {
                return None;
            }
            std::env::var(parts[1]).ok().map(Value::String)
        }
        head => {
            if let Some(root) = roots.get(head) {
                return walk_value(root, &parts[1..]);
            }
            if parts.len() == 2 && parts[1] == "output" {
                if let Some(outputs) = roots.get("outputs") {
                    return walk_value(outputs, &[parts[0]]);
                }
            }
            None
        }
    }
}

pub fn walk_value(root: &Value, path: &[&str]) -> Option<Value> {
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

pub fn find_close_brace(s: &str, start: usize) -> Option<usize> {
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

pub fn value_to_template_string(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn make_roots(pairs: &[(&str, Value)]) -> serde_json::Map<String, Value> {
        let mut m = serde_json::Map::new();
        for (k, v) in pairs {
            m.insert(k.to_string(), v.clone());
        }
        m
    }

    #[test]
    fn renders_simple_substitution() {
        let roots = make_roots(&[("event", json!({"kind": "task.completed"}))]);
        let result = render_template_with_roots(
            "event is ${event.kind}",
            &roots,
            UnresolvedPolicy::HardError,
        )
        .unwrap();
        assert_eq!(result, "event is task.completed");
    }

    #[test]
    fn renders_nested_dot_walk() {
        let roots = make_roots(&[(
            "event",
            json!({"payload": {"instance": "forgejo-bro-reviewer"}}),
        )]);
        let result = render_template_with_roots(
            "${event.payload.instance}",
            &roots,
            UnresolvedPolicy::HardError,
        )
        .unwrap();
        assert_eq!(result, "forgejo-bro-reviewer");
    }

    #[test]
    fn hard_error_on_unresolved() {
        let roots = make_roots(&[("event", json!({}))]);
        let result = render_template_with_roots(
            "${event.missing.field}",
            &roots,
            UnresolvedPolicy::HardError,
        );
        assert!(result.is_err());
        assert!(
            result.unwrap_err().to_string().contains("unresolved"),
            "expected unresolved error"
        );
    }

    #[test]
    fn leave_verbatim_on_unresolved() {
        let roots = make_roots(&[("event", json!({}))]);
        let result = render_template_with_roots(
            "${event.missing.field}",
            &roots,
            UnresolvedPolicy::LeaveVerbatim,
        )
        .unwrap();
        assert_eq!(result, "${event.missing.field}");
    }

    #[test]
    fn mixed_resolved_and_verbatim() {
        let roots = make_roots(&[("event", json!({"kind": "task.started"}))]);
        let result = render_template_with_roots(
            "${event.kind} ${event.missing}",
            &roots,
            UnresolvedPolicy::LeaveVerbatim,
        )
        .unwrap();
        assert_eq!(result, "task.started ${event.missing}");
    }

    #[test]
    fn renders_env_var() {
        let _env = bbox_util::util::test_env_lock();
        // SAFETY: test-only, no concurrent access to this env var
        unsafe { std::env::set_var("BBOX_TEST_TEMPLATE_PHASE3", "hello") };
        let roots = make_roots(&[]);
        let result = render_template_with_roots(
            "${env.BBOX_TEST_TEMPLATE_PHASE3}",
            &roots,
            UnresolvedPolicy::HardError,
        )
        .unwrap();
        assert_eq!(result, "hello");
        // SAFETY: test-only cleanup
        unsafe { std::env::remove_var("BBOX_TEST_TEMPLATE_PHASE3") };
    }

    #[test]
    fn no_substitutions_passes_through() {
        let roots = make_roots(&[]);
        let result = render_template_with_roots(
            "plain text no templates",
            &roots,
            UnresolvedPolicy::HardError,
        )
        .unwrap();
        assert_eq!(result, "plain text no templates");
    }

    #[test]
    fn renders_numeric_value() {
        let roots = make_roots(&[("event", json!({"count": 42}))]);
        let result =
            render_template_with_roots("count=${event.count}", &roots, UnresolvedPolicy::HardError)
                .unwrap();
        assert_eq!(result, "count=42");
    }

    #[test]
    fn renders_array_index() {
        let roots = make_roots(&[("event", json!({"tags": ["a", "b", "c"]}))]);
        let result =
            render_template_with_roots("${event.tags.1}", &roots, UnresolvedPolicy::HardError)
                .unwrap();
        assert_eq!(result, "b");
    }
}
