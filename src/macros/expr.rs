//! Bounded expression DSL for macro probe/refusal predicates and string
//! interpolation.
//!
//! # Safety contract
//!
//! The grammar is **deliberately small** so project macros stay human-writable
//! and reviewable without a parser or VM:
//!
//! - No arithmetic.
//! - No loops.
//! - No user-defined functions.
//! - No arbitrary boolean programs beyond `all`/`any`/`not` over bounded
//!   predicate lists.
//! - `${path}` interpolation only expands scalar JSON values (string, bool,
//!   null, number); arrays/objects are refused.
//!
//! Any structural extension that introduces a new evaluation form **must**
//! be added as a new `Predicate` variant here and reviewed for the above
//! invariants before merging.

use std::collections::HashMap;
use std::fmt;

use rmcp::schemars;
use serde::{Deserialize, Serialize};
use serde_json::Value;

// ---------------------------------------------------------------------------
// Context
// ---------------------------------------------------------------------------

/// Evaluation context threaded through [`eval`] and [`interpolate`].
///
/// `inputs` comes directly from [`super::model::MacroInvocation::inputs`].
/// `probes` is a map of named probe results collected at plan time.
///
/// ### Path resolution
///
/// A dotted path like `"inputs.service_name"` resolves the key
/// `"service_name"` from `inputs`. A path like `"binding.exists"` resolves
/// `"exists"` from the probe result named `"binding"`. Deeper nesting
/// (e.g. `"probe.nested.field"`) walks into JSON objects/arrays. Integer
/// segments index into arrays.
#[derive(Debug, Clone, Default)]
pub struct Context {
    pub inputs: serde_json::Map<String, Value>,
    pub probes: HashMap<String, Value>,
}

impl Context {
    /// Resolve a dotted path into the context, returning a reference to the
    /// `Value` at that location, or `None` if any segment is missing.
    pub fn resolve(&self, path: &str) -> Option<&Value> {
        let mut parts = path.split('.');
        let head = parts.next()?;

        if head == "inputs" {
            let key = parts.next()?;
            let mut current = self.inputs.get(key)?;
            for segment in parts {
                current = walk_one(current, segment)?;
            }
            Some(current)
        } else {
            // head is a probe name
            let mut current = self.probes.get(head)?;
            for segment in parts {
                current = walk_one(current, segment)?;
            }
            Some(current)
        }
    }
}

/// Walk one path segment into a JSON value (object key or array index).
fn walk_one<'a>(val: &'a Value, segment: &str) -> Option<&'a Value> {
    match val {
        Value::Object(map) => map.get(segment),
        Value::Array(arr) => {
            let idx: usize = segment.parse().ok()?;
            arr.get(idx)
        }
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Predicate enum
// ---------------------------------------------------------------------------

/// A bounded logical predicate over a [`Context`].
///
/// Variants form a closed, human-writable grammar with no evaluation escape
/// hatches. See the module-level doc for the full safety contract.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Predicate {
    /// True when the path resolves to any value (including `null`).
    Exists { path: String },
    /// True when the path resolves to a value that equals `value`.
    Eq { path: String, value: Value },
    /// True when the path resolves to a value that appears in `list`.
    In { path: String, list: Vec<Value> },
    /// True when **all** sub-predicates are true. Empty list is vacuously true.
    All { predicates: Vec<Predicate> },
    /// True when **any** sub-predicate is true. Empty list is vacuously false.
    Any { predicates: Vec<Predicate> },
    /// True when the inner predicate is false.
    Not { predicate: Box<Predicate> },
}

// ---------------------------------------------------------------------------
// eval
// ---------------------------------------------------------------------------

/// Evaluate a [`Predicate`] against a [`Context`].
///
/// Evaluation is pure (no I/O, no side effects) and terminates because
/// `All`/`Any`/`Not` only recurse into statically-bounded predicate lists.
pub fn eval(predicate: &Predicate, ctx: &Context) -> bool {
    match predicate {
        Predicate::Exists { path } => ctx.resolve(path).is_some(),

        Predicate::Eq { path, value } => ctx.resolve(path).is_some_and(|v| v == value),

        Predicate::In { path, list } => ctx
            .resolve(path)
            .is_some_and(|v| list.iter().any(|item| item == v)),

        Predicate::All { predicates } => predicates.iter().all(|p| eval(p, ctx)),

        Predicate::Any { predicates } => predicates.iter().any(|p| eval(p, ctx)),

        Predicate::Not { predicate } => !eval(predicate, ctx),
    }
}

// ---------------------------------------------------------------------------
// Interpolation
// ---------------------------------------------------------------------------

/// Error returned by [`interpolate`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InterpolateError {
    /// A `${...}` placeholder was opened but not closed before the end of the
    /// template string.
    UnclosedBrace,
    /// A `${path}` placeholder referenced a path that did not resolve in the
    /// context.
    MissingPath(String),
    /// A `${path}` placeholder resolved to an array or object value, which
    /// cannot be serialised as a plain string scalar.
    NonScalar(String),
}

impl fmt::Display for InterpolateError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            InterpolateError::UnclosedBrace => write!(f, "unclosed '${{...}}' placeholder"),
            InterpolateError::MissingPath(p) => write!(f, "path not found in context: '{p}'"),
            InterpolateError::NonScalar(p) => {
                write!(f, "path '{p}' resolved to a non-scalar (array/object)")
            }
        }
    }
}

impl std::error::Error for InterpolateError {}

/// Expand `${path.to.value}` placeholders in `template` using `ctx`.
///
/// Only scalar JSON values (string, bool, null, number) are accepted.
/// Resolving to an array or object returns [`InterpolateError::NonScalar`].
/// A missing path returns [`InterpolateError::MissingPath`].
pub fn interpolate(template: &str, ctx: &Context) -> Result<String, InterpolateError> {
    let mut result = String::with_capacity(template.len());
    let mut chars = template.chars().peekable();

    while let Some(c) = chars.next() {
        if c == '$' && chars.peek() == Some(&'{') {
            chars.next(); // consume '{'
            let mut path = String::new();
            loop {
                match chars.next() {
                    Some('}') => break,
                    Some(ch) => path.push(ch),
                    None => return Err(InterpolateError::UnclosedBrace),
                }
            }
            let val = ctx
                .resolve(&path)
                .ok_or_else(|| InterpolateError::MissingPath(path.clone()))?;
            match val {
                Value::String(s) => result.push_str(s),
                Value::Bool(b) => result.push_str(if *b { "true" } else { "false" }),
                Value::Null => result.push_str("null"),
                Value::Number(n) => result.push_str(&n.to_string()),
                Value::Array(_) | Value::Object(_) => {
                    return Err(InterpolateError::NonScalar(path));
                }
            }
        } else {
            result.push(c);
        }
    }

    Ok(result)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};
    use std::collections::HashMap;

    fn ctx_with_inputs(inputs: serde_json::Map<String, Value>) -> Context {
        Context {
            inputs,
            probes: HashMap::new(),
        }
    }

    fn make_ctx() -> Context {
        let mut inputs = serde_json::Map::new();
        inputs.insert("service_name".into(), json!("PaymentService"));
        inputs.insert("count".into(), json!(3));
        inputs.insert("enabled".into(), json!(true));
        inputs.insert("tag".into(), json!(null));
        inputs.insert("nested".into(), json!({"a": {"b": "deep_value"}}));

        let mut probes = HashMap::new();
        probes.insert("binding".into(), json!({"exists": true, "kind": "guice"}));
        probes.insert("type_count".into(), json!(5));

        Context { inputs, probes }
    }

    // -----------------------------------------------------------------------
    // Context resolution
    // -----------------------------------------------------------------------

    #[test]
    fn resolve_inputs_top_level() {
        let ctx = make_ctx();
        assert_eq!(
            ctx.resolve("inputs.service_name"),
            Some(&json!("PaymentService"))
        );
    }

    #[test]
    fn resolve_inputs_nested() {
        let ctx = make_ctx();
        assert_eq!(ctx.resolve("inputs.nested.a.b"), Some(&json!("deep_value")));
    }

    #[test]
    fn resolve_probe_top_level() {
        let ctx = make_ctx();
        assert_eq!(ctx.resolve("type_count"), Some(&json!(5)));
    }

    #[test]
    fn resolve_probe_nested() {
        let ctx = make_ctx();
        assert_eq!(ctx.resolve("binding.kind"), Some(&json!("guice")));
    }

    #[test]
    fn resolve_missing_returns_none() {
        let ctx = make_ctx();
        assert_eq!(ctx.resolve("inputs.does_not_exist"), None);
        assert_eq!(ctx.resolve("nonexistent_probe"), None);
        assert_eq!(ctx.resolve("binding.missing_key"), None);
    }

    #[test]
    fn resolve_inputs_alone_returns_none() {
        // "inputs" without a key has nothing to point at
        let ctx = make_ctx();
        assert_eq!(ctx.resolve("inputs"), None);
    }

    // -----------------------------------------------------------------------
    // Predicate::Exists
    // -----------------------------------------------------------------------

    #[test]
    fn exists_true_when_present() {
        let ctx = make_ctx();
        assert!(eval(
            &Predicate::Exists {
                path: "inputs.service_name".into()
            },
            &ctx
        ));
    }

    #[test]
    fn exists_true_for_null_value() {
        let ctx = make_ctx();
        // null is still a value — exists should be true
        assert!(eval(
            &Predicate::Exists {
                path: "inputs.tag".into()
            },
            &ctx
        ));
    }

    #[test]
    fn exists_false_when_missing() {
        let ctx = make_ctx();
        assert!(!eval(
            &Predicate::Exists {
                path: "inputs.ghost".into()
            },
            &ctx
        ));
    }

    // -----------------------------------------------------------------------
    // Predicate::Eq
    // -----------------------------------------------------------------------

    #[test]
    fn eq_true_on_match() {
        let ctx = make_ctx();
        assert!(eval(
            &Predicate::Eq {
                path: "inputs.service_name".into(),
                value: json!("PaymentService"),
            },
            &ctx
        ));
    }

    #[test]
    fn eq_false_on_mismatch() {
        let ctx = make_ctx();
        assert!(!eval(
            &Predicate::Eq {
                path: "inputs.service_name".into(),
                value: json!("OtherService"),
            },
            &ctx
        ));
    }

    #[test]
    fn eq_false_when_missing() {
        let ctx = make_ctx();
        assert!(!eval(
            &Predicate::Eq {
                path: "inputs.ghost".into(),
                value: json!("x"),
            },
            &ctx
        ));
    }

    #[test]
    fn eq_bool_probe() {
        let ctx = make_ctx();
        assert!(eval(
            &Predicate::Eq {
                path: "binding.exists".into(),
                value: json!(true),
            },
            &ctx
        ));
    }

    // -----------------------------------------------------------------------
    // Predicate::In
    // -----------------------------------------------------------------------

    #[test]
    fn in_true_when_value_in_list() {
        let ctx = make_ctx();
        assert!(eval(
            &Predicate::In {
                path: "inputs.service_name".into(),
                list: vec![json!("Foo"), json!("PaymentService"), json!("Bar")],
            },
            &ctx
        ));
    }

    #[test]
    fn in_false_when_value_not_in_list() {
        let ctx = make_ctx();
        assert!(!eval(
            &Predicate::In {
                path: "inputs.service_name".into(),
                list: vec![json!("Foo"), json!("Bar")],
            },
            &ctx
        ));
    }

    #[test]
    fn in_false_when_missing() {
        let ctx = make_ctx();
        assert!(!eval(
            &Predicate::In {
                path: "inputs.ghost".into(),
                list: vec![json!("x")],
            },
            &ctx
        ));
    }

    // -----------------------------------------------------------------------
    // Predicate::All / Any / Not
    // -----------------------------------------------------------------------

    #[test]
    fn all_empty_is_true() {
        let ctx = make_ctx();
        assert!(eval(&Predicate::All { predicates: vec![] }, &ctx));
    }

    #[test]
    fn any_empty_is_false() {
        let ctx = make_ctx();
        assert!(!eval(&Predicate::Any { predicates: vec![] }, &ctx));
    }

    #[test]
    fn all_true_when_all_hold() {
        let ctx = make_ctx();
        let p = Predicate::All {
            predicates: vec![
                Predicate::Exists {
                    path: "inputs.service_name".into(),
                },
                Predicate::Eq {
                    path: "inputs.enabled".into(),
                    value: json!(true),
                },
            ],
        };
        assert!(eval(&p, &ctx));
    }

    #[test]
    fn all_false_when_one_fails() {
        let ctx = make_ctx();
        let p = Predicate::All {
            predicates: vec![
                Predicate::Exists {
                    path: "inputs.service_name".into(),
                },
                Predicate::Exists {
                    path: "inputs.ghost".into(),
                },
            ],
        };
        assert!(!eval(&p, &ctx));
    }

    #[test]
    fn any_true_when_one_holds() {
        let ctx = make_ctx();
        let p = Predicate::Any {
            predicates: vec![
                Predicate::Exists {
                    path: "inputs.ghost".into(),
                },
                Predicate::Exists {
                    path: "inputs.service_name".into(),
                },
            ],
        };
        assert!(eval(&p, &ctx));
    }

    #[test]
    fn any_false_when_none_hold() {
        let ctx = make_ctx();
        let p = Predicate::Any {
            predicates: vec![
                Predicate::Exists {
                    path: "inputs.ghost".into(),
                },
                Predicate::Exists {
                    path: "inputs.ghost2".into(),
                },
            ],
        };
        assert!(!eval(&p, &ctx));
    }

    #[test]
    fn not_inverts() {
        let ctx = make_ctx();
        let p = Predicate::Not {
            predicate: Box::new(Predicate::Exists {
                path: "inputs.ghost".into(),
            }),
        };
        assert!(eval(&p, &ctx));
        let p2 = Predicate::Not {
            predicate: Box::new(Predicate::Exists {
                path: "inputs.service_name".into(),
            }),
        };
        assert!(!eval(&p2, &ctx));
    }

    #[test]
    fn nested_all_any_not() {
        let ctx = make_ctx();
        // not(all([exists(ghost), exists(service_name)])) => not(false) => true
        let p = Predicate::Not {
            predicate: Box::new(Predicate::All {
                predicates: vec![
                    Predicate::Exists {
                        path: "inputs.ghost".into(),
                    },
                    Predicate::Exists {
                        path: "inputs.service_name".into(),
                    },
                ],
            }),
        };
        assert!(eval(&p, &ctx));
    }

    // -----------------------------------------------------------------------
    // Interpolation
    // -----------------------------------------------------------------------

    #[test]
    fn interpolate_string_scalar() {
        let ctx = make_ctx();
        let out = interpolate("Hello ${inputs.service_name}!", &ctx).unwrap();
        assert_eq!(out, "Hello PaymentService!");
    }

    #[test]
    fn interpolate_bool_scalar() {
        let ctx = make_ctx();
        let out = interpolate("enabled=${inputs.enabled}", &ctx).unwrap();
        assert_eq!(out, "enabled=true");
    }

    #[test]
    fn interpolate_null_scalar() {
        let ctx = make_ctx();
        let out = interpolate("tag=${inputs.tag}", &ctx).unwrap();
        assert_eq!(out, "tag=null");
    }

    #[test]
    fn interpolate_number_scalar() {
        let ctx = make_ctx();
        let out = interpolate("count=${inputs.count}", &ctx).unwrap();
        assert_eq!(out, "count=3");
    }

    #[test]
    fn interpolate_probe_value() {
        let ctx = make_ctx();
        let out = interpolate("binding_kind=${binding.kind}", &ctx).unwrap();
        assert_eq!(out, "binding_kind=guice");
    }

    #[test]
    fn interpolate_multiple_placeholders() {
        let ctx = make_ctx();
        let out = interpolate("${inputs.service_name}:${inputs.count}", &ctx).unwrap();
        assert_eq!(out, "PaymentService:3");
    }

    #[test]
    fn interpolate_no_placeholders() {
        let ctx = make_ctx();
        let out = interpolate("no placeholders here", &ctx).unwrap();
        assert_eq!(out, "no placeholders here");
    }

    #[test]
    fn interpolate_missing_path_error() {
        let ctx = make_ctx();
        let err = interpolate("${inputs.ghost}", &ctx).unwrap_err();
        assert_eq!(err, InterpolateError::MissingPath("inputs.ghost".into()));
    }

    #[test]
    fn interpolate_non_scalar_error() {
        let ctx = make_ctx();
        // inputs.nested is an object
        let err = interpolate("${inputs.nested}", &ctx).unwrap_err();
        assert_eq!(err, InterpolateError::NonScalar("inputs.nested".into()));
    }

    #[test]
    fn interpolate_unclosed_brace_error() {
        let ctx = make_ctx();
        let err = interpolate("${inputs.service_name", &ctx).unwrap_err();
        assert_eq!(err, InterpolateError::UnclosedBrace);
    }

    // -----------------------------------------------------------------------
    // Serde round-trip for Predicate
    // -----------------------------------------------------------------------

    #[test]
    fn predicate_serde_round_trip() {
        let p = Predicate::All {
            predicates: vec![
                Predicate::Exists {
                    path: "inputs.a".into(),
                },
                Predicate::Not {
                    predicate: Box::new(Predicate::In {
                        path: "inputs.b".into(),
                        list: vec![json!("x"), json!("y")],
                    }),
                },
                Predicate::Eq {
                    path: "binding.kind".into(),
                    value: json!("guice"),
                },
            ],
        };
        let json = serde_json::to_string(&p).expect("serialize");
        let p2: Predicate = serde_json::from_str(&json).expect("deserialize");
        // Re-serialize and compare JSON strings as a proxy for structural equality
        let json2 = serde_json::to_string(&p2).expect("serialize round-trip");
        assert_eq!(json, json2);
    }

    // -----------------------------------------------------------------------
    // Grammar escape-hatch guard
    // -----------------------------------------------------------------------

    /// Verify that the Predicate enum is exhaustively handled in eval and that
    /// no variant can encode arbitrary computation: the only valid ops are
    /// exists, eq, in, all, any, not. Any future variant addition will require
    /// updating the match in eval(), which serves as a compile-time gate.
    #[test]
    fn no_escape_hatch_variants() {
        // This test exists to document intent. If a new Predicate variant is
        // added without updating eval(), the compiler will emit an
        // unreachable-pattern warning or error (with #[deny(unused_variables)]
        // in CI). Here we confirm all current variants are exercised.
        let ctx = ctx_with_inputs(serde_json::Map::new());
        let cases: &[Predicate] = &[
            Predicate::Exists {
                path: "inputs.x".into(),
            },
            Predicate::Eq {
                path: "inputs.x".into(),
                value: json!(1),
            },
            Predicate::In {
                path: "inputs.x".into(),
                list: vec![],
            },
            Predicate::All { predicates: vec![] },
            Predicate::Any { predicates: vec![] },
            Predicate::Not {
                predicate: Box::new(Predicate::Exists {
                    path: "inputs.x".into(),
                }),
            },
        ];
        // eval each without panic — correctness of values not checked here
        for p in cases {
            let _ = eval(p, &ctx);
        }
    }
}
