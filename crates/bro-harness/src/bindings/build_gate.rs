//! `build.*` - structured build gate execution for refactor cells.
//!
//! `build.gate` runs the supplied command through the existing shell tool path,
//! then reduces stdout/stderr into bounded diagnostics. It returns no raw logs:
//! the isolate only sees the compact fact model.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use async_trait::async_trait;
use bro_code_mode::ToolNamespaceDescription;
use bro_tools::{Tool, ToolCx, ToolResult};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::code_facts::Span;

const DEFAULT_TIMEOUT_MS: u64 = 600_000;
const DEFAULT_MAX_DIAGNOSTICS: usize = 100;
const STATUS_LINE_CAP: usize = 20;
const SHELL_CAPTURE_TOKENS: usize = 2_100_000;

#[derive(Deserialize)]
struct BuildGateInput {
    command: String,
    #[serde(default)]
    cwd: Option<String>,
    #[serde(default, rename = "timeout_ms", alias = "timeoutMs")]
    timeout_ms: Option<u64>,
    #[serde(default, rename = "max_diagnostics", alias = "maxDiagnostics")]
    max_diagnostics: Option<usize>,
    #[serde(default, rename = "anchor_spans", alias = "anchorSpans")]
    anchor_spans: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
enum BuildTool {
    Javac,
    Gradle,
    Generic,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
enum DiagnosticSeverity {
    Error,
    Warning,
}

#[derive(Debug, Clone, Serialize)]
struct BuildDiagnostic {
    #[serde(skip_serializing_if = "Option::is_none")]
    file: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    line: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    column: Option<usize>,
    severity: DiagnosticSeverity,
    message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    symbol: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    span: Option<Span>,
}

#[derive(Debug, Clone, Serialize)]
struct BuildCounts {
    errors: usize,
    warnings: usize,
}

#[derive(Debug, Clone)]
struct ParsedBuildOutput {
    tool: BuildTool,
    diagnostics: Vec<BuildDiagnostic>,
    counts: BuildCounts,
    truncated: bool,
    status_lines: Vec<String>,
}

pub struct BuildGate;

#[async_trait]
impl Tool for BuildGate {
    fn name(&self) -> &str {
        "build.gate"
    }

    fn description(&self) -> &str {
        "Run a compile/test gate command in the session root and return bounded structured diagnostics. Detects javac, Gradle-wrapped javac, and generic nonzero output. Uses the same shell execution path as shell_run but never returns raw logs."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "command": { "type": "string", "description": "Shell command line to execute through bash -lc." },
                "cwd": { "type": "string", "description": "Working directory relative to the session root. Defaults to root." },
                "timeout_ms": { "type": "number", "description": "Hard timeout in milliseconds. Default 600000." },
                "max_diagnostics": { "type": "number", "description": "Maximum diagnostics returned. Default 100." },
                "anchor_spans": { "type": "boolean", "description": "When true, attach hash-anchored line spans for diagnostics whose files exist under the session root." }
            },
            "required": ["command"]
        })
    }

    fn namespace_binding(&self) -> Option<(String, String)> {
        Some(("build".to_string(), "gate".to_string()))
    }

    async fn call(&self, input: Value, cx: &ToolCx) -> ToolResult {
        let args: BuildGateInput = match serde_json::from_value(input) {
            Ok(args) => args,
            Err(e) => return ToolResult::Error(format!("build.gate: bad input: {e}")),
        };
        if args.command.trim().is_empty() {
            return ToolResult::Error("build.gate: command must not be empty".to_string());
        }

        let timeout_ms = args.timeout_ms.unwrap_or(DEFAULT_TIMEOUT_MS);
        let max_diagnostics = args
            .max_diagnostics
            .unwrap_or(DEFAULT_MAX_DIAGNOSTICS)
            .max(1);
        let cwd_arg = args.cwd.as_deref().unwrap_or(".");
        let cwd_abs = match bro_tools::workspace::resolve_in_root(&cx.root, cwd_arg) {
            Ok(path) => path,
            Err(e) => return ToolResult::Error(format!("build.gate: {e}")),
        };

        let started = Instant::now();
        let shell = bro_tools::ShellRun;
        let command = args.command.clone();
        let shell_result = shell
            .call(
                json!({
                    "command": command,
                    "cwd": args.cwd.unwrap_or_else(|| ".".to_string()),
                    "timeout_ms": timeout_ms,
                    "yield_time_ms": 0,
                    "max_output_tokens": SHELL_CAPTURE_TOKENS,
                    "close_stdin": true
                }),
                cx,
            )
            .await;
        let duration_ms = started.elapsed().as_millis() as u64;

        let shell_json = match shell_result {
            ToolResult::Json(value) => value,
            ToolResult::Error(e) => return ToolResult::Error(format!("build.gate: {e}")),
            ToolResult::Text(t) => {
                return ToolResult::Error(format!("build.gate: unexpected shell result: {t}"));
            }
        };
        let exit_code = shell_json
            .get("exit_code")
            .and_then(Value::as_i64)
            .unwrap_or(-1);
        let timed_out = shell_json
            .get("timed_out")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let stdout = shell_json
            .get("stdout")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let stderr = shell_json
            .get("stderr")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let combined = combine_streams(stdout, stderr);

        let mut parsed = parse_build_output(
            &args.command,
            &combined,
            exit_code,
            timed_out,
            max_diagnostics,
        );
        if args.anchor_spans {
            anchor_diagnostics(&cx.root, &cwd_abs, &mut parsed.diagnostics);
        }

        ToolResult::Json(json!({
            "ok": exit_code == 0 && !timed_out,
            "exit_code": exit_code,
            "tool": parsed.tool,
            "diagnostics": parsed.diagnostics,
            "counts": parsed.counts,
            "truncated": parsed.truncated,
            "status_lines": parsed.status_lines,
            "duration_ms": duration_ms
        }))
    }
}

fn combine_streams(stdout: &str, stderr: &str) -> String {
    match (stdout.is_empty(), stderr.is_empty()) {
        (true, true) => String::new(),
        (false, true) => stdout.to_string(),
        (true, false) => stderr.to_string(),
        (false, false) => format!("{stdout}\n{stderr}"),
    }
}

fn parse_build_output(
    command: &str,
    output: &str,
    exit_code: i64,
    timed_out: bool,
    max_diagnostics: usize,
) -> ParsedBuildOutput {
    let mut status_lines = collect_status_lines(output);
    if timed_out {
        push_status_line(
            &mut status_lines,
            format!("command timed out with exit_code {exit_code}"),
        );
    }

    let mut diagnostics = parse_javac_diagnostics(output);
    let has_javac_diagnostics = !diagnostics.is_empty();
    if diagnostics.is_empty() && (exit_code != 0 || timed_out) {
        diagnostics = parse_generic_diagnostics(output);
    }

    let counts = BuildCounts {
        errors: diagnostics
            .iter()
            .filter(|d| d.severity == DiagnosticSeverity::Error)
            .count(),
        warnings: diagnostics
            .iter()
            .filter(|d| d.severity == DiagnosticSeverity::Warning)
            .count(),
    };
    let output_truncated = output.contains("[... ") && output.contains("truncated]");
    let diagnostics_truncated = diagnostics.len() > max_diagnostics;
    diagnostics.truncate(max_diagnostics);

    ParsedBuildOutput {
        tool: detect_tool(command, output, has_javac_diagnostics, &status_lines),
        diagnostics,
        counts,
        truncated: output_truncated || diagnostics_truncated,
        status_lines,
    }
}

fn parse_javac_diagnostics(output: &str) -> Vec<BuildDiagnostic> {
    let lines: Vec<&str> = output.lines().collect();
    let mut diagnostics = Vec::new();
    let mut i = 0usize;
    while i < lines.len() {
        let Some((file, line, severity, message)) = parse_javac_header(lines[i]) else {
            i += 1;
            continue;
        };

        let mut diagnostic = BuildDiagnostic {
            file: Some(file),
            line: Some(line),
            column: None,
            severity,
            message,
            symbol: None,
            span: None,
        };

        i += 1;
        while i < lines.len() && parse_javac_header(lines[i]).is_none() {
            let trimmed = lines[i].trim();
            if let Some(symbol) = trimmed.strip_prefix("symbol:") {
                let symbol = symbol.trim();
                if !symbol.is_empty() {
                    diagnostic.symbol = Some(symbol.to_string());
                }
            }
            i += 1;
        }
        diagnostics.push(diagnostic);
    }
    diagnostics
}

fn parse_javac_header(line: &str) -> Option<(String, usize, DiagnosticSeverity, String)> {
    for (marker, severity) in [
        (": error: ", DiagnosticSeverity::Error),
        (": warning: ", DiagnosticSeverity::Warning),
    ] {
        let Some(marker_idx) = line.find(marker) else {
            continue;
        };
        let before = &line[..marker_idx];
        let line_sep = before.rfind(':')?;
        let line_number = before[line_sep + 1..].parse::<usize>().ok()?;
        let file = before[..line_sep].trim();
        if file.is_empty() {
            return None;
        }
        let message = line[marker_idx + marker.len()..].trim().to_string();
        return Some((file.to_string(), line_number, severity, message));
    }
    None
}

fn parse_generic_diagnostics(output: &str) -> Vec<BuildDiagnostic> {
    let mut diagnostics: Vec<BuildDiagnostic> = output
        .lines()
        .filter_map(|line| {
            let trimmed = line.trim();
            if trimmed.is_empty() || !is_error_looking(trimmed) {
                return None;
            }
            Some(BuildDiagnostic {
                file: None,
                line: None,
                column: None,
                severity: DiagnosticSeverity::Error,
                message: trimmed.to_string(),
                symbol: None,
                span: None,
            })
        })
        .collect();
    if diagnostics.is_empty() {
        diagnostics.push(BuildDiagnostic {
            file: None,
            line: None,
            column: None,
            severity: DiagnosticSeverity::Error,
            message: "command exited without recognized diagnostics".to_string(),
            symbol: None,
            span: None,
        });
    }
    diagnostics
}

fn is_error_looking(line: &str) -> bool {
    let lower = line.to_ascii_lowercase();
    ["error", "failed", "failure", "exception", "fatal"]
        .iter()
        .any(|needle| lower.contains(needle))
}

fn collect_status_lines(output: &str) -> Vec<String> {
    let mut lines = Vec::new();
    for line in output.lines() {
        let trimmed = line.trim();
        let keep = trimmed.starts_with("BUILD ")
            || trimmed.starts_with("FAILURE: Build failed")
            || (trimmed.starts_with("> Task ") && trimmed.contains("FAILED"));
        if keep {
            push_status_line(&mut lines, trimmed.to_string());
        }
    }
    lines
}

fn push_status_line(lines: &mut Vec<String>, line: String) {
    if lines.len() >= STATUS_LINE_CAP || lines.iter().any(|existing| existing == &line) {
        return;
    }
    lines.push(line);
}

fn detect_tool(
    command: &str,
    output: &str,
    has_structured_diagnostics: bool,
    status_lines: &[String],
) -> BuildTool {
    let command_lower = command.to_ascii_lowercase();
    if command_lower.contains("gradle")
        || command_lower.contains("gradlew")
        || output.contains("BUILD SUCCESSFUL")
        || output.contains("BUILD FAILED")
        || status_lines
            .iter()
            .any(|line| line.trim_start().starts_with("> Task "))
    {
        return BuildTool::Gradle;
    }
    if has_structured_diagnostics || command_mentions_javac(&command_lower) {
        return BuildTool::Javac;
    }
    BuildTool::Generic
}

fn command_mentions_javac(command_lower: &str) -> bool {
    command_lower
        .split(|c: char| !c.is_ascii_alphanumeric() && c != '_' && c != '-')
        .any(|part| part == "javac")
}

fn anchor_diagnostics(root: &Path, cwd: &Path, diagnostics: &mut [BuildDiagnostic]) {
    let mut cache: BTreeMap<PathBuf, Option<(String, Vec<u8>)>> = BTreeMap::new();
    for diagnostic in diagnostics.iter_mut() {
        let (Some(file), Some(line)) = (diagnostic.file.clone(), diagnostic.line) else {
            continue;
        };
        let resolved = resolve_diagnostic_path(cwd, &file);
        let display_file = workspace_relative(root, &resolved).unwrap_or_else(|| file.clone());
        diagnostic.file = Some(display_file);

        let entry = cache.entry(resolved.clone()).or_insert_with(|| {
            if !is_under_existing_root(root, &resolved) {
                return None;
            }
            let bytes = std::fs::read(&resolved).ok()?;
            let sha = bbox_refactor::sha256_hex(&bytes);
            Some((sha, bytes))
        });
        let Some((sha, bytes)) = entry.as_ref() else {
            continue;
        };
        let Some((byte_start, byte_end)) = line_byte_range(bytes, line) else {
            continue;
        };
        diagnostic.span = Some(Span {
            file: diagnostic.file.clone().unwrap_or(file),
            byte_start,
            byte_end,
            content_sha256: sha.clone(),
        });
    }
}

fn resolve_diagnostic_path(cwd: &Path, file: &str) -> PathBuf {
    let path = PathBuf::from(file);
    if path.is_absolute() {
        path
    } else {
        cwd.join(path)
    }
}

fn workspace_relative(root: &Path, path: &Path) -> Option<String> {
    let root = root.canonicalize().ok()?;
    let path = path
        .canonicalize()
        .unwrap_or_else(|_| normalize_without_canonical(path));
    path.strip_prefix(root).ok().map(path_to_slash)
}

fn is_under_existing_root(root: &Path, path: &Path) -> bool {
    match (root.canonicalize(), path.canonicalize()) {
        (Ok(root), Ok(path)) => path.starts_with(root),
        _ => false,
    }
}

fn normalize_without_canonical(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        out.push(component.as_os_str());
    }
    out
}

fn path_to_slash(path: &Path) -> String {
    path.components()
        .map(|component| component.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/")
}

fn line_byte_range(bytes: &[u8], target_line: usize) -> Option<(usize, usize)> {
    if target_line == 0 {
        return None;
    }
    let mut line = 1usize;
    let mut start = 0usize;
    for (idx, byte) in bytes.iter().enumerate() {
        if line == target_line && *byte == b'\n' {
            return Some((start, idx));
        }
        if *byte == b'\n' {
            line += 1;
            start = idx + 1;
        }
    }
    (line == target_line).then_some((start, bytes.len()))
}

/// The `build.*` binding set.
pub fn tools() -> Vec<Arc<dyn Tool>> {
    vec![Arc::new(BuildGate) as Arc<dyn Tool>]
}

/// Hand-authored namespace documentation + TS declarations.
pub fn namespace_description() -> ToolNamespaceDescription {
    ToolNamespaceDescription {
        name: "build".to_string(),
        description: "Structured build/test gate runner for refactor recipes. `build.gate` executes one supplied shell command through the harness shell path, parses javac and Gradle-wrapped javac output into bounded diagnostics, and returns no raw logs. Use it after applying edits when you need compile/test feedback inside a cell; keep commands narrow and set `anchor_spans: true` only when line Spans are needed for follow-up edits."
            .to_string(),
        declarations: r#"type BuildSpan = { file: string; byte_start: number; byte_end: number; content_sha256: string };
type BuildDiagnostic = { file?: string; line?: number; column?: number; severity: "error" | "warning"; message: string; symbol?: string; span?: BuildSpan };
type BuildGateResult = { ok: boolean; exit_code: number; tool: "javac" | "gradle" | "generic"; diagnostics: BuildDiagnostic[]; counts: { errors: number; warnings: number }; truncated: boolean; status_lines: string[]; duration_ms: number };
declare const build: {
  /** Run a bounded compile/test gate command and parse javac, Gradle-wrapped javac, or generic nonzero output into structured diagnostics. */
  gate(args: { command: string; cwd?: string; timeout_ms?: number; timeoutMs?: number; max_diagnostics?: number; maxDiagnostics?: number; anchor_spans?: boolean; anchorSpans?: boolean }): Promise<BuildGateResult>;
};"#
            .to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::sync::Mutex;

    fn cx_in(dir: &Path) -> ToolCx {
        ToolCx {
            root: dir.to_path_buf(),
            safety: Arc::new(bro_tools::SafetyPolicy::new()),
            http: reqwest::Client::new(),
            todos: Arc::new(Mutex::new(bro_tools::TodoList::default())),
            shell_sessions: Arc::new(Mutex::new(bro_tools::ShellSessions::default())),
            edits: Arc::new(Mutex::new(bro_tools::EditSink::default())),
            session_env: Arc::new(BTreeMap::new()),
            tool_arg_defaults: Arc::new(bro_tools::ToolArgDefaults::default()),
            shell_env: Arc::new(Default::default()),
        }
    }

    fn json_of(result: ToolResult) -> Value {
        match result {
            ToolResult::Json(value) => value,
            other => panic!("expected json, got {other:?}"),
        }
    }

    #[test]
    fn build_gate_parses_canned_javac_symbol_location_output() {
        let output = r#"Broken.java:3: error: cannot find symbol
        missing();
        ^
  symbol:   method missing()
  location: class Broken
1 error
"#;
        let parsed = parse_build_output("javac Broken.java", output, 1, false, 100);
        assert_eq!(parsed.tool, BuildTool::Javac);
        assert_eq!(parsed.counts.errors, 1);
        assert_eq!(parsed.counts.warnings, 0);
        assert!(!parsed.truncated);
        assert_eq!(parsed.diagnostics.len(), 1);
        let diag = &parsed.diagnostics[0];
        assert_eq!(diag.file.as_deref(), Some("Broken.java"));
        assert_eq!(diag.line, Some(3));
        assert_eq!(diag.severity, DiagnosticSeverity::Error);
        assert!(diag.message.contains("cannot find symbol"));
        assert_eq!(diag.symbol.as_deref(), Some("method missing()"));
    }

    #[test]
    fn build_gate_parses_gradle_wrapped_javac_status_lines() {
        let output = r#"> Task :compileJava FAILED
/tmp/work/src/main/java/com/acme/Broken.java:7: error: cannot find symbol
        missing();
        ^
  symbol:   method missing()
  location: class Broken

FAILURE: Build failed with an exception.
BUILD FAILED in 1s
"#;
        let parsed = parse_build_output("./gradlew compileJava", output, 1, false, 100);
        assert_eq!(parsed.tool, BuildTool::Gradle);
        assert_eq!(parsed.counts.errors, 1);
        assert!(
            parsed
                .status_lines
                .iter()
                .any(|line| line == "> Task :compileJava FAILED")
        );
        assert!(
            parsed
                .status_lines
                .iter()
                .any(|line| line == "BUILD FAILED in 1s")
        );
    }

    #[test]
    fn build_gate_generic_fallback_on_unrecognized_nonzero_output() {
        let output = "link step\nfatal: could not resolve target\nsee logs\n";
        let parsed = parse_build_output("make all", output, 2, false, 100);
        assert_eq!(parsed.tool, BuildTool::Generic);
        assert_eq!(parsed.counts.errors, 1);
        assert_eq!(parsed.diagnostics.len(), 1);
        assert_eq!(
            parsed.diagnostics[0].message,
            "fatal: could not resolve target"
        );
    }

    #[test]
    fn build_gate_truncates_at_max_diagnostics() {
        let mut output = String::new();
        for idx in 1..=3 {
            output.push_str(&format!("Broken.java:{idx}: error: failure {idx}\n"));
        }
        let parsed = parse_build_output("javac Broken.java", &output, 1, false, 2);
        assert_eq!(parsed.counts.errors, 3);
        assert_eq!(parsed.diagnostics.len(), 2);
        assert!(parsed.truncated);
    }

    #[tokio::test]
    async fn build_gate_live_javac_broken_then_fixed() {
        if std::process::Command::new("javac")
            .arg("-version")
            .output()
            .is_err()
        {
            eprintln!("skipping live javac build.gate test because javac is unavailable");
            return;
        }

        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        std::fs::write(
            root.join("Broken.java"),
            "class Broken {\n    void run() {\n        missing();\n    }\n}\n",
        )
        .unwrap();
        let gate = BuildGate;
        let broken = json_of(
            gate.call(
                json!({
                    "command": "javac Broken.java",
                    "anchor_spans": true
                }),
                &cx_in(&root),
            )
            .await,
        );
        assert_eq!(broken["ok"], false);
        assert_eq!(broken["tool"], "javac");
        assert_eq!(broken["counts"]["errors"], 1);
        let diagnostic = &broken["diagnostics"][0];
        assert_eq!(diagnostic["file"], "Broken.java");
        assert_eq!(diagnostic["line"], 3);
        assert!(
            diagnostic["message"]
                .as_str()
                .unwrap()
                .contains("cannot find symbol")
        );
        assert_eq!(diagnostic["span"]["file"], "Broken.java");

        std::fs::write(
            root.join("Broken.java"),
            "class Broken {\n    void run() {\n    }\n}\n",
        )
        .unwrap();
        let fixed = json_of(
            gate.call(json!({ "command": "javac Broken.java" }), &cx_in(&root))
                .await,
        );
        assert_eq!(fixed["ok"], true);
        assert_eq!(fixed["exit_code"], 0);
        assert_eq!(fixed["diagnostics"].as_array().unwrap().len(), 0);
    }
}
