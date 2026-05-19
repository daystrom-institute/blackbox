use anyhow::Result;
use serde_json::{Value, json};

use super::OpEffect;
use crate::workflow::context::ArcContext;

/// Parse captured shell output from a `RunSuite` step and compute `drift_pp`.
///
/// Reads `args.from` (or `args.suite_output`) - a `{exit_code, stdout, stderr,
/// parsed}` blob captured by the preceding Shell op - and extracts:
///   1. `parsed.drift_pp`   if the script emitted a JSON summary
///   2. Exit-code heuristic: non-zero exit -> assume minor drift (5 pp)
///   3. Default 0.0          when neither signal is present
///
/// Writes `{drift_pp, suite_exit_code, raw_stdout}` into `vars[into_var]`.
pub(super) fn exec_score_eval_output(
    args: &Value,
    into_var: Option<&str>,
    ctx: &ArcContext,
) -> Result<OpEffect> {
    let into = into_var.unwrap_or("suite_score");
    let suite_output = args
        .get("from")
        .or_else(|| args.get("suite_output"))
        .or_else(|| ctx.vars.get("suite_output"))
        .cloned()
        .unwrap_or(Value::Null);

    let exit_code = suite_output
        .get("exit_code")
        .and_then(Value::as_i64)
        .unwrap_or(0);
    let stdout = suite_output
        .get("stdout")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();

    // Extract drift_pp from the parsed JSON block first, then from stdout as
    // a last-resort inline search for `"drift_pp": N`.
    let drift_pp: f64 = suite_output
        .get("parsed")
        .and_then(|p| p.get("drift_pp"))
        .and_then(Value::as_f64)
        .or_else(|| {
            // Try to parse stdout directly as JSON
            serde_json::from_str::<Value>(&stdout)
                .ok()
                .and_then(|v| v.get("drift_pp").and_then(Value::as_f64))
        })
        .unwrap_or({
            // Exit-code heuristic: non-zero -> minor drift signal
            if exit_code != 0 { 5.0 } else { 0.0 }
        });

    Ok(OpEffect::SetVar {
        key: into.to_string(),
        value: json!({
            "drift_pp": drift_pp,
            "suite_exit_code": exit_code,
            "raw_stdout": stdout,
        }),
    })
}
