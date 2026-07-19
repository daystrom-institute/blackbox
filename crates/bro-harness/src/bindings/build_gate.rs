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
    Rustc,
    Generic,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
enum DiagnosticSeverity {
    Error,
    Warning,
    Note,
    Help,
}

impl DiagnosticSeverity {
    fn from_rustc_level(level: &str) -> Option<Self> {
        match level {
            "error" => Some(DiagnosticSeverity::Error),
            "warning" => Some(DiagnosticSeverity::Warning),
            "note" => Some(DiagnosticSeverity::Note),
            "help" => Some(DiagnosticSeverity::Help),
            _ => None,
        }
    }
}

/// One machine-applicable suggestion surfaced from a rustc/clippy diagnostic.
/// Byte spans are 1-based line / 1-based column from the cargo JSON message;
/// `byte_start`/`byte_end` are populated by `anchor_spans` when the file
/// resolves under the session root, mirroring the diagnostic's own span.
#[derive(Debug, Clone, Serialize)]
struct BuildSuggestion {
    message: String,
    /// `MachineApplicable` / `MaybeIncorrect` / ... verbatim from rustc.
    applicability: String,
    /// Suggested replacement text (may be empty for a pure deletion).
    replacement: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    file: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    line: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    column: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    byte_start: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    byte_end: Option<usize>,
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
    code: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    symbol: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    span: Option<Span>,
    /// Machine-applicable suggestions (rustc/clippy JSON only). Bounded by
    /// `max_diagnostics` alongside the parent diagnostic.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    suggestions: Vec<BuildSuggestion>,
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
        "Run a compile/test gate command in the session root and return bounded structured diagnostics. Detects javac, Gradle-wrapped javac, cargo/rustc --message-format=json (compiler-message entries with machine-applicable suggestions), and generic nonzero output. Uses the same shell execution path as shell_run but never returns raw logs."
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
                    "command": command.clone(),
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
        // Parsing is pure, but span anchoring reads files; run the tail on
        // the blocking pool so no fs I/O lands on a tokio worker (I2).
        let root = cx.root.clone();
        let anchor_spans = args.anchor_spans;
        bro_tools::tool::call_blocking(move || {
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

            let mut parsed =
                parse_build_output(&command, &combined, exit_code, timed_out, max_diagnostics);
            if anchor_spans {
                anchor_diagnostics(&root, &cwd_abs, &mut parsed.diagnostics);
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
        })
        .await
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

    // rustc/cargo --message-format=json is content-detected (JSON-lines with
    // "reason":"compiler-message") and flag-detected (--message-format=json).
    // It takes priority over the javac/generic parsers: the lines are JSON,
    // not javac headers, so the javac parser would find nothing and generic
    // would produce unusable one-line-per-JSON-blob diagnostics.
    let rustc_flags = command_mentions_rustc_json(command);
    let rustc_content = output_has_compiler_message(output);
    let mut diagnostics = if rustc_flags || rustc_content {
        parse_rustc_json_diagnostics(output)
    } else {
        Vec::new()
    };
    let has_rustc_diagnostics = !diagnostics.is_empty();

    if diagnostics.is_empty() {
        diagnostics = parse_javac_diagnostics(output);
    }
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
        tool: detect_tool(
            command,
            output,
            has_javac_diagnostics,
            has_rustc_diagnostics,
            &status_lines,
        ),
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
            code: None,
            symbol: None,
            span: None,
            suggestions: Vec::new(),
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
                code: None,
                symbol: None,
                span: None,
                suggestions: Vec::new(),
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
            code: None,
            symbol: None,
            span: None,
            suggestions: Vec::new(),
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
    has_javac_diagnostics: bool,
    has_rustc_diagnostics: bool,
    status_lines: &[String],
) -> BuildTool {
    let command_lower = command.to_ascii_lowercase();
    // Rustc/cargo JSON takes priority: once we have compiler-message lines,
    // the command is a cargo/rustc JSON gate regardless of wrapper wording.
    if has_rustc_diagnostics || command_mentions_rustc_json(command) {
        return BuildTool::Rustc;
    }
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
    if has_javac_diagnostics || command_mentions_javac(&command_lower) {
        return BuildTool::Javac;
    }
    BuildTool::Generic
}

fn command_mentions_javac(command_lower: &str) -> bool {
    command_lower
        .split(|c: char| !c.is_ascii_alphanumeric() && c != '_' && c != '-')
        .any(|part| part == "javac")
}

/// Flag-based detection: does the command ask for cargo/rustc JSON output?
/// Matches `--message-format=json` (cargo) and `--error-format=json` (rustc
/// direct), including `=json,rendered` suffixes, when the command also
/// invokes `cargo`/`rustc`/`x`.
fn command_mentions_rustc_json(command: &str) -> bool {
    let lower = command.to_ascii_lowercase();
    let mentions_rustc = lower.split_whitespace().any(|tok| {
        tok == "cargo" || tok == "rustc" || tok.ends_with("/cargo") || tok.ends_with("/rustc")
    });
    if !mentions_rustc {
        return false;
    }
    lower.contains("--message-format=json") || lower.contains("--error-format=json")
}

/// Content-based detection: is the output a JSON-lines stream containing at
/// least one `"reason":"compiler-message"` entry? This catches a cargo JSON
/// run whose command wrapper obscured the flag (e.g. `make check` invoking
/// cargo internally).
fn output_has_compiler_message(output: &str) -> bool {
    output.lines().any(|line| {
        let trimmed = line.trim();
        if !trimmed.starts_with('{') {
            return false;
        }
        trimmed.contains("\"reason\"") && trimmed.contains("\"compiler-message\"")
    })
}

/// Parse `cargo --message-format=json` / `rustc --error-format=json` output
/// into bounded diagnostics. Each `compiler-message` entry becomes one
/// `BuildDiagnostic` carrying its code, primary span (file/line/column), and
/// any machine-applicable suggestions (with byte ranges from the cargo span).
/// Non-`compiler-message` lines (build-script output, artifact messages) are
/// silently dropped. Reuses `bbox_refactor::parse_rustc_json_output` for the
/// line-level decode so the parser stays the single source of truth.
fn parse_rustc_json_diagnostics(output: &str) -> Vec<BuildDiagnostic> {
    let diags = bbox_refactor::parse_rustc_json_output(output.as_bytes());
    diags.into_iter().map(rustc_diag_to_build).collect()
}

fn rustc_diag_to_build(diag: bbox_refactor::RustcDiagnostic) -> BuildDiagnostic {
    let severity = DiagnosticSeverity::from_rustc_level(&diag.level)
        .unwrap_or(DiagnosticSeverity::Error);

    let primary = diag
        .spans
        .iter()
        .find(|s| {
            s.get("is_primary")
                .and_then(|v| v.as_bool())
                .unwrap_or(false)
        })
        .or_else(|| diag.spans.first());

    let (file, line, column) = primary
        .map(|s| {
            let file = s
                .get("file_name")
                .and_then(|v| v.as_str())
                .map(|f| f.to_string());
            let line = s
                .get("line_start")
                .and_then(|v| v.as_u64())
                .map(|v| v as usize);
            let column = s
                .get("column_start")
                .and_then(|v| v.as_u64())
                .map(|v| v as usize);
            (file, line, column)
        })
        .unwrap_or((None, None, None));

    let suggestions = collect_rustc_suggestions(&diag.spans, &diag.children);

    BuildDiagnostic {
        file,
        line,
        column,
        severity,
        message: diag.message,
        code: diag.code,
        symbol: None,
        span: None,
        suggestions,
    }
}

/// Flatten the suggestion spans from the diagnostic's own spans and its
/// children (help/note sub-diagnostics carry the `suggested_replacement`).
/// Each carries its cargo byte range so `anchor_spans` can hash-anchor it.
fn collect_rustc_suggestions(
    spans: &[serde_json::Value],
    children: &[serde_json::Value],
) -> Vec<BuildSuggestion> {
    let mut out = Vec::new();
    let push_span = |out: &mut Vec<BuildSuggestion>, span: &serde_json::Value, message: &str| {
        let applicability = span
            .get("suggestion_applicability")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        // Only surface spans that actually carry a replacement; bare spans
        // without one are not actionable suggestions.
        let replacement = match span.get("suggested_replacement").and_then(|v| v.as_str()) {
            Some(r) => r.to_string(),
            None => return,
        };
        let file = span
            .get("file_name")
            .and_then(|v| v.as_str())
            .map(|f| f.to_string());
        let line = span
            .get("line_start")
            .and_then(|v| v.as_u64())
            .map(|v| v as usize);
        let column = span
            .get("column_start")
            .and_then(|v| v.as_u64())
            .map(|v| v as usize);
        let byte_start = span
            .get("byte_start")
            .and_then(|v| v.as_u64())
            .map(|v| v as usize);
        let byte_end = span
            .get("byte_end")
            .and_then(|v| v.as_u64())
            .map(|v| v as usize);
        out.push(BuildSuggestion {
            message: message.to_string(),
            applicability,
            replacement,
            file,
            line,
            column,
            byte_start,
            byte_end,
        });
    };

    for span in spans {
        let label = span
            .get("label")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        push_span(&mut out, span, &label);
    }
    for child in children {
        let message = child
            .get("message")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        if let Some(child_spans) = child.get("spans").and_then(|v| v.as_array()) {
            for span in child_spans {
                push_span(&mut out, span, &message);
            }
        }
    }
    out
}

// Sync fs read is sanctioned here: anchor_diagnostics only runs inside the
// call_blocking tail of build.gate, never on a tokio worker (concurrency-model
// section 5).
#[allow(clippy::disallowed_methods)]
fn anchor_diagnostics(root: &Path, cwd: &Path, diagnostics: &mut [BuildDiagnostic]) {
    let mut cache: BTreeMap<PathBuf, Option<(String, Vec<u8>)>> = BTreeMap::new();
    for diagnostic in diagnostics.iter_mut() {
        let (Some(file), Some(line)) = (diagnostic.file.clone(), diagnostic.line) else {
            // Even without a primary span, anchor any suggestions that
            // carry their own file (a child suggestion can outlive a
            // span-less parent diagnostic).
            anchor_suggestions(root, cwd, &mut cache, &mut diagnostic.suggestions);
            continue;
        };
        let resolved = resolve_diagnostic_path(cwd, &file);
        let display_file = workspace_relative(root, &resolved).unwrap_or_else(|| file.clone());
        diagnostic.file = Some(display_file.clone());

        let entry = cache.entry(resolved.clone()).or_insert_with(|| {
            if !is_under_existing_root(root, &resolved) {
                return None;
            }
            let bytes = std::fs::read(&resolved).ok()?;
            let sha = bbox_refactor::sha256_hex(&bytes);
            Some((sha, bytes))
        });
        let Some((sha, bytes)) = entry.as_ref() else {
            anchor_suggestions(root, cwd, &mut cache, &mut diagnostic.suggestions);
            continue;
        };
        let Some((byte_start, byte_end)) = line_byte_range(bytes, line) else {
            anchor_suggestions(root, cwd, &mut cache, &mut diagnostic.suggestions);
            continue;
        };
        diagnostic.span = Some(Span {
            file: diagnostic.file.clone().unwrap_or(file),
            byte_start,
            byte_end,
            content_sha256: sha.clone(),
        });
        anchor_suggestions(root, cwd, &mut cache, &mut diagnostic.suggestions);
    }
}

/// Rewrite suggestion file paths to workspace-relative and validate their
/// cargo-supplied byte ranges against the on-disk file, dropping ranges
/// that fall outside the file bounds. Suggestions keep their byte offsets
/// (rustc byte ranges are the source of truth, not line numbers); the
/// anchor pass only path-relativizes and bounds-checks.
#[allow(clippy::disallowed_methods)]
fn anchor_suggestions(
    root: &Path,
    cwd: &Path,
    cache: &mut BTreeMap<PathBuf, Option<(String, Vec<u8>)>>,
    suggestions: &mut [BuildSuggestion],
) {
    for sug in suggestions.iter_mut() {
        let Some(file) = sug.file.clone() else {
            continue;
        };
        let resolved = resolve_diagnostic_path(cwd, &file);
        let display_file = workspace_relative(root, &resolved).unwrap_or_else(|| file.clone());
        sug.file = Some(display_file.clone());
        let (Some(byte_start), Some(byte_end)) = (sug.byte_start, sug.byte_end) else {
            continue;
        };
        let entry = cache.entry(resolved.clone()).or_insert_with(|| {
            if !is_under_existing_root(root, &resolved) {
                return None;
            }
            let bytes = std::fs::read(&resolved).ok()?;
            let sha = bbox_refactor::sha256_hex(&bytes);
            Some((sha, bytes))
        });
        let Some((sha, bytes)) = entry.as_ref() else {
            // File not under root / unreadable: drop the byte ranges so the
            // consumer doesn't act on an un-anchored offset.
            sug.byte_start = None;
            sug.byte_end = None;
            continue;
        };
        if byte_end > bytes.len() || byte_start > byte_end {
            sug.byte_start = None;
            sug.byte_end = None;
            continue;
        }
        // Keep the byte ranges; the content sha is implicit (the consumer
        // re-derives it from the file when minting a Span). We do not embed
        // a full Span here because suggestions are a flat summary, not edit
        // addresses; rust.fixRound builds the Span from these fields.
        let _ = sha;
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
        description: "Structured build/test gate runner for refactor recipes. `build.gate` executes one supplied shell command through the harness shell path, parses javac, Gradle-wrapped javac, cargo/rustc --message-format=json (compiler-message entries with machine-applicable suggestions), or generic nonzero output into bounded diagnostics, and returns no raw logs. Use it after applying edits when you need compile/test feedback inside a cell; keep commands narrow and set `anchor_spans: true` only when line Spans are needed for follow-up edits."
            .to_string(),
        declarations: r#"type BuildSpan = { file: string; byte_start: number; byte_end: number; content_sha256: string };
type BuildSuggestion = { message: string; applicability: string; replacement: string; file?: string; line?: number; column?: number; byte_start?: number; byte_end?: number };
type BuildDiagnostic = { file?: string; line?: number; column?: number; severity: "error" | "warning" | "note" | "help"; message: string; code?: string; symbol?: string; span?: BuildSpan; suggestions: BuildSuggestion[] };
type BuildGateResult = { ok: boolean; exit_code: number; tool: "javac" | "gradle" | "rustc" | "generic"; diagnostics: BuildDiagnostic[]; counts: { errors: number; warnings: number }; truncated: boolean; status_lines: string[]; duration_ms: number };
declare const build: {
  /** Run a bounded compile/test gate command and parse javac, Gradle-wrapped javac, cargo/rustc JSON, or generic nonzero output into structured diagnostics. */
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

    // ---- rustc / cargo --message-format=json parsing ----

    #[test]
    fn build_gate_parses_cargo_json_compiler_message_into_structured_diagnostic() {
        // One E0308 with a primary span, plus a MachineApplicable child suggestion.
        let line = r#"{"reason":"compiler-message","message":{"code":{"code":"E0308"},"level":"error","message":"mismatched types: expected `u32`, found `i32`","spans":[{"file_name":"src/lib.rs","byte_start":10,"byte_end":12,"line_start":2,"column_start":5,"is_primary":true,"label":"expected `u32`"}],"children":[{"message":"change the type","spans":[{"file_name":"src/lib.rs","byte_start":10,"byte_end":12,"line_start":2,"column_start":5,"is_primary":true,"label":"","suggested_replacement":"0u32","suggestion_applicability":"MachineApplicable"}]}]}}"#;
        let parsed = parse_build_output(
            "cargo check --message-format=json",
            line,
            1,
            false,
            100,
        );
        assert_eq!(parsed.tool, BuildTool::Rustc);
        assert_eq!(parsed.counts.errors, 1);
        assert_eq!(parsed.diagnostics.len(), 1);
        let diag = &parsed.diagnostics[0];
        assert_eq!(diag.file.as_deref(), Some("src/lib.rs"));
        assert_eq!(diag.line, Some(2));
        assert_eq!(diag.column, Some(5));
        assert_eq!(diag.severity, DiagnosticSeverity::Error);
        assert_eq!(diag.code.as_deref(), Some("E0308"));
        assert!(diag.message.contains("mismatched types"));
        assert_eq!(diag.suggestions.len(), 1);
        let sug = &diag.suggestions[0];
        assert_eq!(sug.applicability, "MachineApplicable");
        assert_eq!(sug.replacement, "0u32");
        assert_eq!(sug.byte_start, Some(10));
        assert_eq!(sug.byte_end, Some(12));
    }

    #[test]
    fn build_gate_detects_rustc_by_content_when_flag_absent() {
        // A wrapper command (e.g. `make check`) that emits cargo JSON
        // internally is still detected as Rustc by content.
        let line = r#"{"reason":"compiler-message","message":{"code":null,"level":"warning","message":"unused variable","spans":[],"children":[]}}"#;
        let parsed = parse_build_output("make check", line, 0, false, 100);
        assert_eq!(parsed.tool, BuildTool::Rustc);
        assert_eq!(parsed.counts.warnings, 1);
    }

    #[test]
    fn build_gate_drops_non_compiler_message_json_lines() {
        // cargo --message-format=json emits many line kinds (build-script,
        // compiler-artifact, ...); only compiler-message becomes a diag.
        let output = concat!(
            r#"{"reason":"build-script-executed","package_id":"foo"}"#,
            "\n",
            r#"{"reason":"compiler-message","message":{"code":{"code":"E0432"},"level":"error","message":"unresolved import","spans":[{"file_name":"src/lib.rs","byte_start":0,"byte_end":4,"line_start":1,"column_start":1,"is_primary":true}],"children":[]}}"#,
            "\n",
            r#"{"reason":"compiler-artifact","package_id":"foo","target":{"name":"foo","kind":["lib"]}}"#,
        );
        let parsed = parse_build_output("cargo build --message-format=json", output, 101, false, 100);
        assert_eq!(parsed.tool, BuildTool::Rustc);
        assert_eq!(parsed.diagnostics.len(), 1);
        assert_eq!(parsed.diagnostics[0].code.as_deref(), Some("E0432"));
    }

    #[test]
    fn build_gate_rustc_truncates_at_max_diagnostics() {
        let mut output = String::new();
        for _ in 0..3 {
            output.push_str(
                r#"{"reason":"compiler-message","message":{"code":{"code":"E0308"},"level":"error","message":"mismatch","spans":[],"children":[]}}"#,
            );
            output.push('\n');
        }
        let parsed = parse_build_output("cargo check --message-format=json", &output, 1, false, 2);
        assert_eq!(parsed.counts.errors, 3);
        assert_eq!(parsed.diagnostics.len(), 2);
        assert!(parsed.truncated);
    }

    #[test]
    #[allow(clippy::disallowed_methods)]
    fn build_gate_rustc_anchor_spans_relativizes_and_bounds_checks_suggestions() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        std::fs::create_dir_all(root.join("src")).unwrap();
        // 20 bytes: "let x: u32 = -1;\n" is 17 chars + newline = 18; pad to be safe.
        std::fs::write(root.join("src/lib.rs"), "let x: u32 = -1;\n").unwrap();
        let abs = root.join("src/lib.rs");
        let abs_str = abs.to_string_lossy().to_string();
        // byte_start/byte_end 13..15 points at "-1" inside the file (in bounds).
        let line = format!(
            r#"{{"reason":"compiler-message","message":{{"code":{{"code":"E0308"}},"level":"error","message":"mismatched types","spans":[{{"file_name":"{abs_str}","byte_start":13,"byte_end":15,"line_start":1,"column_start":14,"is_primary":true}}],"children":[{{"message":"use a u32 literal","spans":[{{"file_name":"{abs_str}","byte_start":13,"byte_end":15,"line_start":1,"column_start":14,"is_primary":true,"suggested_replacement":"1u32","suggestion_applicability":"MachineApplicable"}}]}}]}}}}"#
        );
        let mut parsed = parse_build_output("cargo check --message-format=json", &line, 1, false, 100);
        anchor_diagnostics(&root, &root, &mut parsed.diagnostics);
        let diag = &parsed.diagnostics[0];
        // File relativized under the session root.
        assert_eq!(diag.file.as_deref(), Some("src/lib.rs"));
        // Primary span anchored with a content sha.
        let span = diag.span.as_ref().expect("primary span anchored");
        assert_eq!(span.file, "src/lib.rs");
        assert!(!span.content_sha256.is_empty());
        // Suggestion file relativized, byte ranges kept (in bounds).
        assert_eq!(diag.suggestions.len(), 1);
        let sug = &diag.suggestions[0];
        assert_eq!(sug.file.as_deref(), Some("src/lib.rs"));
        assert_eq!(sug.byte_start, Some(13));
        assert_eq!(sug.byte_end, Some(15));
    }

    #[test]
    #[allow(clippy::disallowed_methods)]
    fn build_gate_rustc_anchor_drops_out_of_bounds_suggestion_byte_ranges() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        std::fs::write(root.join("lib.rs"), "fn main() {}\n").unwrap(); // 13 bytes
        let abs = root.join("lib.rs");
        let abs_str = abs.to_string_lossy().to_string();
        // byte_end 999 is out of bounds; anchoring must drop the ranges.
        let line = format!(
            r#"{{"reason":"compiler-message","message":{{"code":{{"code":"E0308"}},"level":"error","message":"mismatch","spans":[{{"file_name":"{abs_str}","byte_start":0,"byte_end":2,"line_start":1,"column_start":1,"is_primary":true}}],"children":[{{"message":"help","spans":[{{"file_name":"{abs_str}","byte_start":900,"byte_end":999,"line_start":1,"column_start":1,"is_primary":true,"suggested_replacement":"x","suggestion_applicability":"MachineApplicable"}}]}}]}}}}"#
        );
        let mut parsed = parse_build_output("cargo check --message-format=json", &line, 1, false, 100);
        anchor_diagnostics(&root, &root, &mut parsed.diagnostics);
        let sug = &parsed.diagnostics[0].suggestions[0];
        assert_eq!(sug.byte_start, None);
        assert_eq!(sug.byte_end, None);
    }

    #[test]
    fn build_gate_javac_unchanged_when_no_rustc_signals() {
        // Regression: the new rustc detection must not swallow javac output.
        let output =
            "Broken.java:3: error: cannot find symbol\n  symbol:   method missing()\n1 error\n";
        let parsed = parse_build_output("javac Broken.java", output, 1, false, 100);
        assert_eq!(parsed.tool, BuildTool::Javac);
        assert_eq!(parsed.counts.errors, 1);
    }
}
