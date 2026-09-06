//! Extractor AST — projects a JSON value (typically a webhook
//! payload) into a flat entity that downstream packets evaluate.
//!
//! Deliberately small. The only job is `payload → flat_entity`. No
//! transformations (regex, case folding, math) — those belong in
//! Predicate predicates or downstream nodes.
//!
//! Selectors:
//! - `JsonPath(p)`              — dotted path with array indexing
//! - `Const(v)`                 — literal value, no input dependency
//! - `Default(inner, fallback)` — `inner` if present, else `fallback`
//!                                (literal Value)
//! - `Concat(strs)`             — string concatenation of resolved
//!                                inner extractors (used for
//!                                composite keys in correlation
//!                                tuples)
//! - `Coalesce(sources)`        — first non-null result from a list
//!                                of selectors. Used when a payload
//!                                shape differs across event subtypes
//!                                (e.g. Forgejo's `pull_request_review`
//!                                puts the comment text at `.review.body`,
//!                                `pull_request_review_comment` at
//!                                `.comment.body` — Coalesce projects
//!                                both into one extracted field).

use anyhow::Result;
use rmcp::schemars;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Selector {
    /// `$.user.login` style. Leading `$.` optional.
    JsonPath { path: String },
    /// Literal value.
    Const { value: Value },
    /// Try `inner`; if it fails or resolves to null, use `fallback`.
    Default {
        inner: Box<Selector>,
        fallback: Value,
    },
    /// Concatenate multiple inner selectors as strings.
    Concat { parts: Vec<Selector> },
    /// Return the first source that resolves to a non-null value.
    /// `Value::Null` if every source is null. Composable with
    /// `Default` for a final literal fallback.
    Coalesce { sources: Vec<Selector> },
}

impl Selector {
    pub fn evaluate(&self, input: &Value) -> Result<Value> {
        match self {
            Selector::JsonPath { path } => Ok(walk_path(input, path).unwrap_or(Value::Null)),
            Selector::Const { value } => Ok(value.clone()),
            Selector::Default { inner, fallback } => {
                let v = inner.evaluate(input)?;
                Ok(if matches!(v, Value::Null) {
                    fallback.clone()
                } else {
                    v
                })
            }
            Selector::Concat { parts } => {
                let mut out = String::new();
                for p in parts {
                    let v = p.evaluate(input)?;
                    match v {
                        Value::String(s) => out.push_str(&s),
                        Value::Null => {}
                        other => out.push_str(&other.to_string()),
                    }
                }
                Ok(Value::String(out))
            }
            Selector::Coalesce { sources } => {
                for src in sources {
                    let v = src.evaluate(input)?;
                    if !matches!(v, Value::Null) {
                        return Ok(v);
                    }
                }
                Ok(Value::Null)
            }
        }
    }
}

/// Top-level Extractor: a named map of output keys to selectors.
/// Producing one flat JSON object the packet evaluator consumes.
#[derive(Debug, Clone, Serialize, Deserialize, Default, schemars::JsonSchema)]
pub struct Extractor {
    pub outputs: std::collections::HashMap<String, Selector>,
}

impl Extractor {
    pub fn extract(&self, input: &Value) -> Result<Value> {
        let mut out = Map::with_capacity(self.outputs.len());
        for (k, sel) in &self.outputs {
            out.insert(k.clone(), sel.evaluate(input)?);
        }
        Ok(Value::Object(out))
    }
}

fn walk_path(input: &Value, raw_path: &str) -> Option<Value> {
    let trimmed = raw_path.strip_prefix("$.").unwrap_or(raw_path);
    let trimmed = trimmed.strip_prefix('.').unwrap_or(trimmed);
    if trimmed.is_empty() {
        return Some(input.clone());
    }
    let mut cur = input.clone();
    for seg in trimmed.split('.') {
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn jsonpath_simple_field() {
        let s = Selector::JsonPath {
            path: "$.action".into(),
        };
        let input = json!({"action": "opened"});
        assert_eq!(s.evaluate(&input).unwrap(), json!("opened"));
    }

    #[test]
    fn jsonpath_nested() {
        let s = Selector::JsonPath {
            path: "$.issue.number".into(),
        };
        let input = json!({"issue": {"number": 42}});
        assert_eq!(s.evaluate(&input).unwrap(), json!(42));
    }

    #[test]
    fn jsonpath_array_index() {
        let s = Selector::JsonPath {
            path: "$.labels.0.name".into(),
        };
        let input = json!({"labels": [{"name": "bug"}]});
        assert_eq!(s.evaluate(&input).unwrap(), json!("bug"));
    }

    #[test]
    fn jsonpath_missing_returns_null() {
        let s = Selector::JsonPath {
            path: "$.missing.field".into(),
        };
        assert_eq!(s.evaluate(&json!({})).unwrap(), Value::Null);
    }

    #[test]
    fn default_falls_back_when_null() {
        let s = Selector::Default {
            inner: Box::new(Selector::JsonPath {
                path: "$.missing".into(),
            }),
            fallback: json!("default"),
        };
        assert_eq!(s.evaluate(&json!({})).unwrap(), json!("default"));
    }

    #[test]
    fn default_uses_inner_when_present() {
        let s = Selector::Default {
            inner: Box::new(Selector::JsonPath { path: "$.x".into() }),
            fallback: json!("default"),
        };
        assert_eq!(
            s.evaluate(&json!({"x": "actual"})).unwrap(),
            json!("actual")
        );
    }

    #[test]
    fn const_passes_through() {
        let s = Selector::Const {
            value: json!({"k": "v"}),
        };
        assert_eq!(s.evaluate(&json!(null)).unwrap(), json!({"k": "v"}));
    }

    #[test]
    fn concat_strings() {
        let s = Selector::Concat {
            parts: vec![
                Selector::Const {
                    value: json!("issue-"),
                },
                Selector::JsonPath {
                    path: "$.issue.number".into(),
                },
            ],
        };
        assert_eq!(
            s.evaluate(&json!({"issue": {"number": 42}})).unwrap(),
            json!("issue-42")
        );
    }

    #[test]
    fn coalesce_picks_first_non_null() {
        let s = Selector::Coalesce {
            sources: vec![
                Selector::JsonPath {
                    path: "$.review.body".into(),
                },
                Selector::JsonPath {
                    path: "$.comment.body".into(),
                },
                Selector::Const {
                    value: json!("(no body)"),
                },
            ],
        };
        // first source resolves -> wins
        assert_eq!(
            s.evaluate(&json!({"review": {"body": "lgtm"}, "comment": {"body": "meh"}}))
                .unwrap(),
            json!("lgtm")
        );
        // first source null -> second wins
        assert_eq!(
            s.evaluate(&json!({"comment": {"body": "inline note"}}))
                .unwrap(),
            json!("inline note")
        );
        // both null -> falls through to Const
        assert_eq!(s.evaluate(&json!({})).unwrap(), json!("(no body)"));
    }

    #[test]
    fn coalesce_returns_null_when_no_sources_resolve() {
        let s = Selector::Coalesce {
            sources: vec![
                Selector::JsonPath { path: "$.x".into() },
                Selector::JsonPath { path: "$.y".into() },
            ],
        };
        assert_eq!(s.evaluate(&json!({})).unwrap(), Value::Null);
    }

    #[test]
    fn extractor_full_projection() {
        let mut outputs = std::collections::HashMap::new();
        outputs.insert(
            "action".to_string(),
            Selector::JsonPath {
                path: "$.action".into(),
            },
        );
        outputs.insert(
            "issue_number".to_string(),
            Selector::JsonPath {
                path: "$.issue.number".into(),
            },
        );
        outputs.insert(
            "repo".to_string(),
            Selector::Concat {
                parts: vec![
                    Selector::JsonPath {
                        path: "$.repository.owner.login".into(),
                    },
                    Selector::Const { value: json!("/") },
                    Selector::JsonPath {
                        path: "$.repository.name".into(),
                    },
                ],
            },
        );
        let extractor = Extractor { outputs };
        let input = json!({
            "action": "opened",
            "issue": {"number": 42},
            "repository": {"owner": {"login": "foo"}, "name": "bar"}
        });
        let result = extractor.extract(&input).unwrap();
        assert_eq!(result["action"], json!("opened"));
        assert_eq!(result["issue_number"], json!(42));
        assert_eq!(result["repo"], json!("foo/bar"));
    }
}
