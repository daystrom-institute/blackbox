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
    Cargo,
    Generic,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
enum DiagnosticSeverity {
    Error,
    Warning,
    /// rustc `note`/`help`/`ice` levels are not diagnostics proper; today
    /// they surface only as nested suggestions and never as top-level
    /// BuildDiagnostics, so this variant is reserved for future use.
    #[allow(dead_code)]
    Info,
}

/// A machine-applicable suggestion mined from a rustc/clippy diagnostic
/// span. `span` is hash-anchored only when `anchor_spans` located the file
/// under the session root; otherwise the raw byte range is still returned
/// so `rust.fixRound` can synthesize an edit against a re-derived Span.
#[derive(Debug, Clone, Serialize)]
struct BuildSuggestion {
    file: String,
    byte_start: usize,
    byte_end: usize,
    /// Verbatim `suggested_replacement` bytes from the compiler.
    replacement: String,
    /// The compiler's applicability tag (`MachineApplicable`,
    /// `MaybeIncorrect`, `HasPlaceholders`, `Unspecified`).
    applicability: String,
    /// Hash-anchored line span, present only when the file exists under the
    /// session root and `anchor_spans` was requested.
    #[serde(skip_serializing_if = "Option::is_none")]
    span: Option<Span>,
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
    /// rustc/clippy diagnostic code (e.g. `E0308`, `clippy::needless_return`).
    #[serde(skip_serializing_if = "Option::is_none")]
    code: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    span: Option<Span>,
    /// Machine-applicable (and near-applicable) suggestions mined from the
    /// diagnostic's spans/children. Populated only for cargo/rustc JSON
    /// diagnostics; javac/Gradle/generic diagnostics carry none.
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
        "Run a compile/test gate command in the session root and return bounded structured diagnostics. Detects cargo/rustc JSON (`--message-format=json`), javac, Gradle-wrapped javac, and generic nonzero output. cargo/rustc diagnostics carry the compiler code and machine-applicable suggestion spans. Uses the same shell execution path as shell_run but never returns raw logs."
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

    // cargo/rustc `--message-format=json` output is unambiguous: it is
    // newline-delimited JSON whose lines carry `"reason":"compiler-message"`.
    // When present, parse it into structured diagnostics with codes and
    // machine-applicable suggestions; this is the repair-loop input shape
    // rust.fixRound consumes. Detect by content so a wrapper script that
    // pipes cargo JSON through still classifies as cargo.
    let cargo_diagnostics = parse_cargo_json_diagnostics(output);
    let has_cargo_diagnostics = !cargo_diagnostics.is_empty();

    let mut diagnostics = if has_cargo_diagnostics {
        cargo_diagnostics
    } else {
        parse_javac_diagnostics(output)
    };
    let has_javac_diagnostics = !has_cargo_diagnostics && !diagnostics.is_empty();
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
            has_cargo_diagnostics,
            has_javac_diagnostics,
            &status_lines,
        ),
        diagnostics,
        counts,
        truncated: output_truncated || diagnostics_truncated,
        status_lines,
    }
}

/// Parse `cargo ... --message-format=json` (or raw rustc `--error-format=json`)
/// newline-delimited output into structured diagnostics.
///
/// Each `compiler-message` line carries a nested `message` object whose
/// `spans` hold file/byte/line/column and optional `suggested_replacement`
/// + `suggestion_applicability`, and whose `children` repeat the same shape
/// for help/note sub-diagnostics (where most machine-applicable suggestions
/// live). We mine suggestions from both, keeping every suggestion whose
/// applicability is actionable (`MachineApplicable`, `MaybeIncorrect`, or
/// `HasPlaceholders`); `Unspecified` is dropped as non-actionable.
///
/// Reuses `bbox_refactor::parse_rustc_json_output` for the line-level
/// decode (it already tolerates malformed/non-compiler-message lines), then
/// re-walks the raw JSON to preserve the suggestion spans the v1 classifier
/// needs verbatim.
fn parse_cargo_json_diagnostics(output: &str) -> Vec<BuildDiagnostic> {
    // Fast path: skip the per-line JSON parse entirely when the output does
    // not look like cargo JSON at all. This keeps the javac/generic path
    // zero-cost for the common case.
    if !output.contains("\"compiler-message\"") {
        return Vec::new();
    }

    let mut diagnostics = Vec::new();
    for line in output.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(val) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if val.get("reason").and_then(|r| r.as_str()) != Some("compiler-message") {
            continue;
        }
        let Some(msg) = val.get("message") else {
            continue;
        };
        let level = msg
            .get("level")
            .and_then(|v| v.as_str())
            .unwrap_or("error");
        let severity = match level {
            "warning" => DiagnosticSeverity::Warning,
            // rustc emits `error`, `ice`, `failure-note`; treat anything not
            // explicitly a warning as an error so the counts surface real
            // failures. `note`/`help` arrive as children of a real diagnostic
            // and never as a top-level compiler-message in practice.
            _ => DiagnosticSeverity::Error,
        };
        let code = msg
            .get("code")
            .and_then(|c| c.get("code"))
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        let message = msg
            .get("message")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let spans = msg.get("spans").and_then(|v| v.as_array());
        let children = msg.get("children").and_then(|v| v.as_array());

        // Resolve the primary span for file/line/column.
        let primary = spans
            .and_then(|arr| {
                arr.iter().find(|s| {
                    s.get("is_primary")
                        .and_then(|v| v.as_bool())
                        .unwrap_or(false)
                })
            })
            .or_else(|| spans.and_then(|arr| arr.first()));

        let file = primary
            .and_then(|s| s.get("file_name"))
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        let line_no = primary
            .and_then(|s| s.get("line_start"))
            .and_then(|v| v.as_u64())
            .map(|v| v as usize);
        let column_no = primary
            .and_then(|s| s.get("column_start"))
            .and_then(|v| v.as_u64())
            .map(|v| v as usize);

        // Mine suggestions from the diagnostic's own spans and its children.
        let mut suggestions = Vec::new();
        if let Some(arr) = spans {
            mine_suggestions(arr, &mut suggestions);
        }
        if let Some(arr) = children {
            for child in arr {
                if let Some(child_spans) = child.get("spans").and_then(|v| v.as_array()) {
                    mine_suggestions(child_spans, &mut suggestions);
                }
            }
        }

        diagnostics.push(BuildDiagnostic {
            file,
            line: line_no,
            column: column_no,
            severity,
            message,
            symbol: None,
            code,
            span: None,
            suggestions,
        });
    }
    diagnostics
}

/// Walk a rustc/clippy `spans` array and push every actionable suggestion
/// onto `out`. Deduplicates by `(file, byte_start, byte_end, replacement)`.
fn mine_suggestions(spans: &[Value], out: &mut Vec<BuildSuggestion>) {
    for span in spans {
        let applicability = span
            .get("suggestion_applicability")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        // `Unspecified` and missing applicability are not machine-actionable.
        if !matches!(
            applicability,
            "MachineApplicable" | "MaybeIncorrect" | "HasPlaceholders"
        ) {
            continue;
        }
        let Some(file) = span.get("file_name").and_then(|v| v.as_str()) else {
            continue;
        };
        let Some(replacement) = span.get("suggested_replacement").and_then(|v| v.as_str())
        else {
            continue;
        };
        let byte_start = span
            .get("byte_start")
            .and_then(|v| v.as_u64())
            .map(|v| v as usize);
        let byte_end = span
            .get("byte_end")
            .and_then(|v| v.as_u64())
            .map(|v| v as usize);
        let (Some(byte_start), Some(byte_end)) = (byte_start, byte_end) else {
            continue;
        };
        let suggestion = BuildSuggestion {
            file: file.to_string(),
            byte_start,
            byte_end,
            replacement: replacement.to_string(),
            applicability: applicability.to_string(),
            span: None,
        };
        if !out.iter().any(|existing| {
            existing.file == suggestion.file
                && existing.byte_start == suggestion.byte_start
                && existing.byte_end == suggestion.byte_end
                && existing.replacement == suggestion.replacement
        }) {
            out.push(suggestion);
        }
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
            code: None,
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
                symbol: None,
                code: None,
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
            symbol: None,
            code: None,
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
    has_cargo_diagnostics: bool,
    has_javac_diagnostics: bool,
    status_lines: &[String],
) -> BuildTool {
    let command_lower = command.to_ascii_lowercase();
    // cargo/rustc JSON diagnostics are content-detected and take precedence:
    // a Gradle wrapper invoking cargo, or a shell pipeline, still classifies
    // as cargo when the output is compiler-message JSON.
    if has_cargo_diagnostics || command_emits_cargo_json(&command_lower) {
        return BuildTool::Cargo;
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

/// Detect a cargo/rustc invocation that emits `--message-format=json`. The
/// content detector (`has_cargo_diagnostics`) is authoritative; this only
/// refines the `tool` field when cargo JSON is requested but the build
/// produced zero compiler-message lines (e.g. a clean check).
fn command_emits_cargo_json(command_lower: &str) -> bool {
    let mentions_cargo = command_lower
        .split(|c: char| !c.is_ascii_alphanumeric() && c != '_' && c != '-' && c != '.')
        .any(|part| part == "cargo" || part == "rustc");
    if !mentions_cargo {
        return false;
    }
    command_lower.contains("--message-format=json") || command_lower.contains("--message-format compact") || command_lower.contains("--error-format=json")
}

// Sync fs read is sanctioned here: anchor_diagnostics only runs inside the
// call_blocking tail of build.gate, never on a tokio worker (concurrency-model
// section 5).
#[allow(clippy::disallowed_methods)]
fn anchor_diagnostics(root: &Path, cwd: &Path, diagnostics: &mut [BuildDiagnostic]) {
    // Lazily-read file contents keyed by resolved path: (sha256, bytes) when
    // the file exists under the session root, None otherwise. Shared across
    // the diagnostic line-span pass and the suggestion byte-span pass so a
    // file touched by many diagnostics is read once.
    let mut cache: BTreeMap<PathBuf, Option<(String, Vec<u8>)>> = BTreeMap::new();

    let file_sha_bytes = |cache: &mut BTreeMap<PathBuf, Option<(String, Vec<u8>)>>,
                              resolved: &Path| {
        if let Some(entry) = cache.get(resolved) {
            return entry.clone();
        }
        let entry = if !is_under_existing_root(root, resolved) {
            None
        } else {
            std::fs::read(resolved)
                .ok()
                .map(|bytes| (bbox_refactor::sha256_hex(&bytes), bytes))
        };
        cache.insert(resolved.to_path_buf(), entry.clone());
        entry
    };

    for diagnostic in diagnostics.iter_mut() {
        // Diagnostic line span (javac + cargo): anchor the primary line.
        if let (Some(file), Some(line)) = (diagnostic.file.clone(), diagnostic.line) {
            let resolved = resolve_diagnostic_path(cwd, &file);
            let display_file = workspace_relative(root, &resolved).unwrap_or_else(|| file.clone());
            diagnostic.file = Some(display_file.clone());
            if let Some((sha, bytes)) = file_sha_bytes(&mut cache, &resolved) {
                if let Some((byte_start, byte_end)) = line_byte_range(&bytes, line) {
                    diagnostic.span = Some(Span {
                        file: display_file.clone(),
                        byte_start,
                        byte_end,
                        content_sha256: sha,
                    });
                }
            }
        }

        // Suggestion byte spans (cargo JSON only): the compiler already
        // gave us byte-accurate ranges against the on-disk file, so we only
        // need to resolve the path + hash-anchor. The byte range is left
        // untouched when the file is unavailable off-root; rust.fixRound
        // re-derives a Span from a fresh code.read when it needs to write.
        for suggestion in diagnostic.suggestions.iter_mut() {
            let resolved = resolve_diagnostic_path(cwd, &suggestion.file);
            let display_file =
                workspace_relative(root, &resolved).unwrap_or_else(|| suggestion.file.clone());
            suggestion.file = display_file.clone();
            if let Some((sha, _bytes)) = file_sha_bytes(&mut cache, &resolved) {
                suggestion.span = Some(Span {
                    file: display_file,
                    byte_start: suggestion.byte_start,
                    byte_end: suggestion.byte_end,
                    content_sha256: sha,
                });
            }
        }
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
        description: "Structured build/test gate runner for refactor recipes. `build.gate` executes one supplied shell command through the harness shell path, parses cargo/rustc JSON (`--message-format=json`), javac, and Gradle-wrapped javac output into bounded diagnostics, and returns no raw logs. cargo/rustc diagnostics carry the compiler code and machine-applicable suggestion spans (the repair-loop input for `rust.fixRound`). Use it after applying edits when you need compile/test feedback inside a cell; keep commands narrow and set `anchor_spans: true` only when line/byte Spans are needed for follow-up edits."
            .to_string(),
        declarations: r#"type BuildSpan = { file: string; byte_start: number; byte_end: number; content_sha256: string };
type BuildSuggestion = { file: string; byte_start: number; byte_end: number; replacement: string; applicability: "MachineApplicable" | "MaybeIncorrect" | "HasPlaceholders"; span?: BuildSpan };
type BuildDiagnostic = { file?: string; line?: number; column?: number; severity: "error" | "warning"; message: string; symbol?: string; code?: string; span?: BuildSpan; suggestions?: BuildSuggestion[] };
type BuildGateResult = { ok: boolean; exit_code: number; tool: "javac" | "gradle" | "cargo" | "generic"; diagnostics: BuildDiagnostic[]; counts: { errors: number; warnings: number }; truncated: boolean; status_lines: string[]; duration_ms: number };
declare const build: {
  /** Run a bounded compile/test gate command and parse cargo/rustc JSON, javac, Gradle-wrapped javac, or generic nonzero output into structured diagnostics. cargo/rustc diagnostics carry compiler codes and machine-applicable suggestion spans. */
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

    // ---- cargo / rustc JSON diagnostics ----

    fn compiler_message_json(level: &str, code: Option<&str>, message: &str) -> String {
        let code_json = match code {
            Some(c) => format!("{{\"code\":\"{c}\"}}"),
            None => "null".to_string(),
        };
        format!(
            r#"{{"reason":"compiler-message","message":{{"level":"{level}","code":{code_json},"message":{message:?},"spans":[],"children":[]}}}}"#
        )
    }

    fn e0308_with_machine_applicable_suggestion(file: &str) -> String {
        // A realistic E0308 (mismatched types) with a MachineApplicable
        // suggested_replacement in a child help diagnostic.
        format!(
            r#"{{"reason":"compiler-message","message":{{"level":"error","code":{{"code":"E0308"}},"message":"mismatched types","spans":[{{"file_name":"{file}","byte_start":10,"byte_end":14,"line_start":2,"column_start":5,"is_primary":true}}],"children":[{{"message":"consider using `\"\"`","spans":[{{"file_name":"{file}","byte_start":10,"byte_end":14,"line_start":2,"column_start":5,"is_primary":true,"suggested_replacement":"\"\"","suggestion_applicability":"MachineApplicable"}}]}}]}}}}"#
        )
    }

    #[test]
    fn build_gate_parses_cargo_json_compiler_messages() {
        let output = format!(
            "{}\n{}\n{}\n{}",
            compiler_message_json("error", Some("E0308"), "mismatched types"),
            compiler_message_json("error", Some("E0425"), "cannot find value"),
            compiler_message_json("warning", Some("unused_variables"), "unused variable"),
            serde_json::json!({"reason":"build-finished","success":false}).to_string()
        );
        let parsed = parse_build_output(
            "cargo check --message-format=json",
            &output,
            1,
            false,
            100,
        );
        assert_eq!(parsed.tool, BuildTool::Cargo);
        assert_eq!(parsed.counts.errors, 2);
        assert_eq!(parsed.counts.warnings, 1);
        assert_eq!(parsed.diagnostics.len(), 3);
        assert_eq!(parsed.diagnostics[0].code.as_deref(), Some("E0308"));
        assert_eq!(parsed.diagnostics[1].code.as_deref(), Some("E0425"));
        assert_eq!(
            parsed.diagnostics[2].code.as_deref(),
            Some("unused_variables")
        );
        assert_eq!(parsed.diagnostics[2].severity, DiagnosticSeverity::Warning);
    }

    #[test]
    fn build_gate_cargo_json_mines_machine_applicable_suggestions() {
        let output = e0308_with_machine_applicable_suggestion("src/lib.rs");
        let parsed = parse_build_output("cargo check --message-format=json", &output, 1, false, 100);
        assert_eq!(parsed.tool, BuildTool::Cargo);
        assert_eq!(parsed.diagnostics.len(), 1);
        let diag = &parsed.diagnostics[0];
        assert_eq!(diag.code.as_deref(), Some("E0308"));
        assert_eq!(diag.file.as_deref(), Some("src/lib.rs"));
        assert_eq!(diag.line, Some(2));
        assert_eq!(diag.column, Some(5));
        assert_eq!(diag.suggestions.len(), 1);
        let s = &diag.suggestions[0];
        assert_eq!(s.file, "src/lib.rs");
        assert_eq!(s.byte_start, 10);
        assert_eq!(s.byte_end, 14);
        assert_eq!(s.replacement, "\"\"");
        assert_eq!(s.applicability, "MachineApplicable");
        assert!(s.span.is_none(), "no anchoring without anchor_spans");
    }

    #[test]
    fn build_gate_cargo_json_skips_non_machine_applicable_suggestions() {
        // Unspecified applicability should be dropped; MaybeIncorrect kept.
        let output = format!(
            r#"{{"reason":"compiler-message","message":{{"level":"error","code":{{"code":"E0308"}},"message":"x","spans":[{{"file_name":"a.rs","byte_start":0,"byte_end":1,"is_primary":true,"suggested_replacement":"y","suggestion_applicability":"Unspecified"}}],"children":[{{"spans":[{{"file_name":"a.rs","byte_start":5,"byte_end":6,"suggested_replacement":"z","suggestion_applicability":"MaybeIncorrect"}}]}}]}}}}"#
        );
        let parsed = parse_build_output("cargo check --message-format=json", &output, 1, false, 100);
        let diag = &parsed.diagnostics[0];
        assert_eq!(diag.suggestions.len(), 1);
        assert_eq!(diag.suggestions[0].applicability, "MaybeIncorrect");
        assert_eq!(diag.suggestions[0].byte_start, 5);
    }

    #[test]
    fn build_gate_cargo_json_detected_by_content_without_command_flag() {
        // A wrapper script (e.g. `make check`) that emits cargo JSON should
        // still classify as cargo by content.
        let output = compiler_message_json("error", Some("E0308"), "boom");
        let parsed = parse_build_output("make check", &output, 1, false, 100);
        assert_eq!(parsed.tool, BuildTool::Cargo);
        assert_eq!(parsed.counts.errors, 1);
    }

    #[test]
    fn build_gate_javac_output_unaffected_when_no_cargo_json() {
        // Sanity: javac output with no compiler-message lines must NOT take
        // the cargo path.
        let output = "Broken.java:3: error: cannot find symbol\n  symbol:   method missing()\n";
        let parsed = parse_build_output("javac Broken.java", output, 1, false, 100);
        assert_eq!(parsed.tool, BuildTool::Javac);
        assert_eq!(parsed.counts.errors, 1);
        assert!(parsed.diagnostics[0].suggestions.is_empty());
        assert!(parsed.diagnostics[0].code.is_none());
    }

    #[test]
    fn build_gate_cargo_json_truncates_at_max_diagnostics() {
        let mut lines = Vec::new();
        for _ in 0..3 {
            lines.push(compiler_message_json("error", Some("E0308"), "boom"));
        }
        let parsed = parse_build_output(
            "cargo check --message-format=json",
            &lines.join("\n"),
            1,
            false,
            2,
        );
        assert_eq!(parsed.counts.errors, 3);
        assert_eq!(parsed.diagnostics.len(), 2);
        assert!(parsed.truncated);
    }

    #[test]
    fn anchor_diagnostics_anchors_cargo_suggestion_byte_spans() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let file = "src/lib.rs";
        let content = "fn main() { let x: u32 = \"oops\"; }\n";
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join(file), content).unwrap();
        let sha = bbox_refactor::sha256_hex(content.as_bytes());

        // Suggestion byte range covers the 4 bytes of `oops`-ish literal at
        // bytes 21..25 (the `"oops"` substring); use an illustrative range.
        let output = format!(
            r#"{{"reason":"compiler-message","message":{{"level":"error","code":{{"code":"E0308"}},"message":"mismatched types","spans":[{{"file_name":"{file}","byte_start":21,"byte_end":27,"line_start":1,"column_start":22,"is_primary":true}}],"children":[{{"spans":[{{"file_name":"{file}","byte_start":21,"byte_end":27,"suggested_replacement":"0","suggestion_applicability":"MachineApplicable"}}]}}]}}}}"#
        );
        let mut parsed = parse_build_output(
            "cargo check --message-format=json",
            &output,
            1,
            false,
            100,
        );
        anchor_diagnostics(&root, &root, &mut parsed.diagnostics);

        let diag = &parsed.diagnostics[0];
        assert_eq!(diag.file.as_deref(), Some(file));
        // Diagnostic line span anchored from the file bytes.
        let span = diag.span.as_ref().expect("line span anchored");
        assert_eq!(span.file, file);
        assert_eq!(span.content_sha256, sha);
        // Suggestion byte span anchored against the same hash.
        assert_eq!(diag.suggestions.len(), 1);
        let s = &diag.suggestions[0];
        assert_eq!(s.file, file);
        assert_eq!(s.byte_start, 21);
        assert_eq!(s.byte_end, 27);
        let sspan = s.span.as_ref().expect("suggestion span anchored");
        assert_eq!(sspan.file, file);
        assert_eq!(sspan.content_sha256, sha);
        assert_eq!(sspan.byte_start, 21);
        assert_eq!(sspan.byte_end, 27);
    }

    #[test]
    fn anchor_diagnostics_leaves_off_root_suggestion_spans_unanchored() {
        // A diagnostic whose file is outside the session root must not get a
        // hash-anchored span; the raw byte range stays so rust.fixRound can
        // re-derive against a fresh read.
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let output = format!(
            r#"{{"reason":"compiler-message","message":{{"level":"error","code":{{"code":"E0308"}},"message":"x","spans":[{{"file_name":"/external/a.rs","byte_start":0,"byte_end":1,"is_primary":true}}],"children":[{{"spans":[{{"file_name":"/external/a.rs","byte_start":0,"byte_end":1,"suggested_replacement":"y","suggestion_applicability":"MachineApplicable"}}]}}]}}}}"#
        );
        let mut parsed = parse_build_output(
            "cargo check --message-format=json",
            &output,
            1,
            false,
            100,
        );
        anchor_diagnostics(&root, &root, &mut parsed.diagnostics);
        let diag = &parsed.diagnostics[0];
        assert!(diag.span.is_none());
        assert!(diag.suggestions[0].span.is_none());
        // Raw byte range still present.
        assert_eq!(diag.suggestions[0].byte_start, 0);
    }
}
