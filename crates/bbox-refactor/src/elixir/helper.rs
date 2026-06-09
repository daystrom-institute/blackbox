//! Rust-side client for `priv/elixir_ast_helper/` escript.
//!
//! The helper is daemon-managed: one process per registered project root,
//! recycled on project version change. v1 ships a simple one-shot invoker
//! (spawn, send, read response, terminate); v2 will keep the helper warm
//! across requests via a connection pool.
//!
//! When the helper escript is unavailable (not built yet, missing Elixir
//! toolchain, etc.), callers fall back to stderr-parsing fallbacks per the
//! design's "subprocess fallback" path in Substrate decisions.

use std::io::Write;
use std::process::{Command, Stdio};

use anyhow::{Result, anyhow};
use serde_json::Value;

#[derive(Debug, Clone)]
#[allow(dead_code)] // wired into v2 fix-round plan kinds when escript ships
pub(crate) struct HelperResponse {
    pub ok: bool,
    pub result: Option<Value>,
    pub error: Option<String>,
}

/// Locate the helper escript path. Order of preference:
///   1. `BLACKBOX_ELIXIR_AST_HELPER` env override
///   2. `priv/elixir_ast_helper/elixir_ast_helper` relative to project_dir
///   3. `$BLACKBOX_STATE_DIR/elixir_helpers/<project_id>/elixir_ast_helper`
///
/// Returns None when no helper binary is discoverable.
#[allow(dead_code)] // wired into v2 fix-round plan kinds when escript ships
pub(crate) fn locate_helper(project_dir: Option<&str>) -> Option<std::path::PathBuf> {
    if let Ok(p) = std::env::var("BLACKBOX_ELIXIR_AST_HELPER") {
        let path = std::path::PathBuf::from(p);
        if path.exists() {
            return Some(path);
        }
    }
    if let Some(dir) = project_dir {
        let local = std::path::PathBuf::from(dir).join("priv/elixir_ast_helper/elixir_ast_helper");
        if local.exists() {
            return Some(local);
        }
    }
    None
}

/// Send a one-shot JSON command. Spawns the helper, writes the request, reads
/// one response line, terminates. Use [`HelperPool`] (v2) for warm reuse.
#[allow(dead_code)] // wired into v2 fix-round plan kinds when escript ships
pub(crate) fn one_shot(
    helper_path: &std::path::Path,
    project_dir: &std::path::Path,
    cmd: &str,
    args: Value,
) -> Result<HelperResponse> {
    let mut child = Command::new(helper_path)
        .arg(project_dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| anyhow!("spawn helper: {e}"))?;

    let id = format!("call-{}", std::process::id());
    let req = serde_json::json!({"id": id, "cmd": cmd, "args": args});
    let req_line = format!("{}\n", serde_json::to_string(&req)?);
    {
        let stdin = child
            .stdin
            .as_mut()
            .ok_or_else(|| anyhow!("helper stdin unavailable"))?;
        stdin.write_all(req_line.as_bytes())?;
        stdin.flush()?;
    }

    let output = child.wait_with_output()?;
    if !output.status.success() {
        return Err(anyhow!(
            "helper exited with status {:?}; stderr: {}",
            output.status.code(),
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let first_line = stdout
        .lines()
        .next()
        .ok_or_else(|| anyhow!("helper returned no response"))?;
    let parsed: Value = serde_json::from_str(first_line)
        .map_err(|e| anyhow!("helper response not JSON: {e}: line={first_line}"))?;
    Ok(HelperResponse {
        ok: parsed.get("ok").and_then(|v| v.as_bool()).unwrap_or(false),
        result: parsed.get("result").cloned(),
        error: parsed
            .get("error")
            .and_then(|v| v.as_str())
            .map(String::from),
    })
}

/// Stderr-parsing fallback for `mix compile --warnings-as-errors`.
///
/// Format (stable across Elixir 1.14–1.17):
/// ```text
/// ** (CompileError) lib/foo.ex:42: undefined function bar/2
/// lib/foo.ex:10: warning: unused alias Foo
/// ```
///
/// We capture `<file>:<line>: <severity>?: <message>` triples.
pub(crate) fn parse_mix_compile_stderr(stderr: &str) -> Vec<MixDiagnostic> {
    let mut out = Vec::new();
    for line in stderr.lines() {
        let line = line.trim_start();
        if line.is_empty() {
            continue;
        }
        // `** (CompileError) file.ex:LINE: rest` — strip prefix first so the
        // file isn't captured as `** (CompileError) lib/bar.ex`.
        if let Some(rest) = line.strip_prefix("** (CompileError) ") {
            if let Some(diag) = try_parse_file_line(rest) {
                out.push(MixDiagnostic {
                    severity: "error".to_string(),
                    ..diag
                });
            }
            continue;
        }
        // Try `file.ex:LINE: rest` pattern.
        if let Some(diag) = try_parse_file_line(line) {
            out.push(diag);
        }
    }
    out
}

fn try_parse_file_line(line: &str) -> Option<MixDiagnostic> {
    let first_colon = line.find(':')?;
    let after_file = &line[first_colon + 1..];
    let second_colon = after_file.find(':')?;
    let line_num_str = &after_file[..second_colon];
    let line_num = line_num_str.parse::<usize>().ok()?;
    let file = line[..first_colon].to_string();
    let rest = &after_file[second_colon + 1..].trim();
    let (severity, message) = if let Some(rest2) = rest.strip_prefix("warning: ") {
        ("warning".to_string(), rest2.to_string())
    } else if let Some(rest2) = rest.strip_prefix("error: ") {
        ("error".to_string(), rest2.to_string())
    } else {
        ("info".to_string(), rest.to_string())
    };
    Some(MixDiagnostic {
        file,
        line: line_num,
        severity,
        message,
    })
}

#[derive(Debug, Clone, serde::Serialize)]
pub(crate) struct MixDiagnostic {
    pub file: String,
    pub line: usize,
    pub severity: String,
    pub message: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_basic_warning() {
        let stderr = "lib/foo.ex:42: warning: unused alias Foo\n";
        let diags = parse_mix_compile_stderr(stderr);
        assert_eq!(diags.len(), 1);
        assert_eq!(diags[0].file, "lib/foo.ex");
        assert_eq!(diags[0].line, 42);
        assert_eq!(diags[0].severity, "warning");
        assert_eq!(diags[0].message, "unused alias Foo");
    }

    #[test]
    fn parses_compile_error() {
        let stderr = "** (CompileError) lib/bar.ex:10: undefined function baz/0\n";
        let diags = parse_mix_compile_stderr(stderr);
        assert_eq!(diags.len(), 1);
        assert_eq!(diags[0].file, "lib/bar.ex");
        assert_eq!(diags[0].line, 10);
        assert_eq!(diags[0].severity, "error");
        assert_eq!(diags[0].message, "undefined function baz/0");
    }

    #[test]
    fn ignores_irrelevant_lines() {
        let stderr = "Compiling 5 files (.ex)\nGenerated app\n";
        assert_eq!(parse_mix_compile_stderr(stderr).len(), 0);
    }
}
