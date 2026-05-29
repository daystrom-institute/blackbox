//! Minimal `jaq` (pure-Rust jq) embedding for the clipboard transform node.
//!
//! `clip_transform` parses a register's text as JSON and runs a jq program over
//! it entirely server-side — the content never enters the model context. The
//! jq language is the projection/transform vocabulary the model already knows;
//! `.body` is the trivial field-pluck, `map(.title)` the powerful case, same
//! tool. jaq is pure Rust: no shell, no IO, deterministic — strictly less
//! capable than `shell_run`, which the harness already ships.

use anyhow::{Result, anyhow, bail};
use jaq_interpret::{Ctx, FilterT, ParseCtx, RcIter, Val};
use serde_json::Value;

/// Run a jq `program` over `input`, returning the output stream (jq filters may
/// emit zero, one, or many values). Errors on parse/compile/runtime failure.
pub fn run_jq(program: &str, input: Value) -> Result<Vec<Value>> {
    let mut ctx = ParseCtx::new(Vec::new());
    ctx.insert_natives(jaq_core::core());
    ctx.insert_defs(jaq_std::std());

    let (parsed, errs) = jaq_parse::parse(program, jaq_parse::main());
    if !errs.is_empty() {
        bail!("jq parse error: {errs:?}");
    }
    let parsed = parsed.ok_or_else(|| anyhow!("jq: empty program"))?;

    let filter = ctx.compile(parsed);
    if !ctx.errs.is_empty() {
        let msg = ctx
            .errs
            .iter()
            .map(|(e, _span)| e.to_string())
            .collect::<Vec<_>>()
            .join("; ");
        bail!("jq compile error: {msg}");
    }

    let inputs = RcIter::new(core::iter::empty());
    let run_ctx = Ctx::new([], &inputs);
    let mut out = Vec::new();
    for r in filter.run((run_ctx, Val::from(input))) {
        match r {
            Ok(v) => out.push(Value::from(v)),
            Err(e) => bail!("jq runtime error: {e}"),
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn field_pluck() {
        let out = run_jq(".body", json!({"body": "hi there", "id": 1})).unwrap();
        assert_eq!(out, vec![json!("hi there")]);
    }

    #[test]
    fn map_over_array_streams() {
        let out = run_jq(
            ".items[].title",
            json!({"items": [{"title": "a"}, {"title": "b"}]}),
        )
        .unwrap();
        assert_eq!(out, vec![json!("a"), json!("b")]);
    }

    #[test]
    fn parse_error_surfaces() {
        assert!(
            run_jq(".[", json!({}))
                .unwrap_err()
                .to_string()
                .contains("jq")
        );
    }
}
