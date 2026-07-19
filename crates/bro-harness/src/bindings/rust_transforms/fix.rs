//! `rust.fixRound` - classify rustc/clippy diagnostics into edit proposals.
//!
//! The loop engine of the rust compile-fix surface
//! (design/refactor-tools/rust/rust-isolate-surface.md §2.2, §3.1). It
//! accepts the `build.gate` diagnostics shape verbatim OR raw rustc
//! `--message-format=json` / `--error-format=json` lines, and ports the v1
//! `rust_compile_fix_round` classifier:
//!
//!   - verbatim `MachineApplicable` `suggested_replacement` edits become
//!     `{changes}` entries recorded at `compiler_suggested` provenance;
//!   - add-use / visibility-bump classifier proposals are synthesized at
//!     `syntax_only` (planner guesses informed by compiler output, not
//!     compiler bytes);
//!   - borrow-checker / trait-bound / unknown errors become explicit
//!     `leftovers` (never retried blindly).
//!
//! NEVER writes. Output is `{changes, findings, leftovers}` for
//! `edits.merge`; the cell then `edits.apply` and re-runs `build.gate` to
//! reach the diagnostics-settling fixed point (cap ~5 rounds).

use std::sync::Arc;

use async_trait::async_trait;
use bro_tools::{Tool, ToolAnnotations, ToolCx, ToolResult};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::bindings::code_facts::Span;
use crate::bindings::ledger::{AuthorityTier, ProvenanceLedger};

/// `rust.fixRound` tool. Holds the session-shared provenance ledger so
/// verbatim compiler suggestions enter the lineage at `compiler_suggested`
/// and survive `edits.merge` content-digest recognition.
pub struct RustFixRound(pub Arc<ProvenanceLedger>);

#[derive(Deserialize)]
struct FixRoundInput {
    /// Diagnostics from `build.gate` (the `diagnostics[]` array verbatim).
    /// Each entry may carry `code`, `file`, `message`, and `suggestions[]`.
    #[serde(default)]
    diagnostics: Vec<Value>,
    /// Optional raw `cargo --message-format=json` / `rustc --error-format=json`
    /// stdout. When present, these are parsed and appended to `diagnostics`.
    #[serde(default)]
    raw_json: Option<String>,
    /// Optional list of file path substrings; diagnostics whose primary
    /// span file does not match any are skipped (mirrors v1
    /// `restrict_to_files`).
    #[serde(default, rename = "restrict_to_files", alias = "restrictToFiles")]
    restrict_to_files: Option<Vec<String>>,
}

/// One synthesized edit proposal ready for `edits.merge`.
#[derive(Debug, Clone, serde::Serialize)]
struct ChangeProposal {
    span: Span,
    new_text: String,
    /// Why this change was proposed; carries the provenance tier so the cell
    /// (and tests) can see which changes are compiler-authored vs
    /// planner-guessed without re-deriving from the ledger.
    provenance: &'static str,
    /// The diagnostic code that motivated the proposal (E0308, clippy::*, ...).
    #[serde(skip_serializing_if = "Option::is_none")]
    code: Option<String>,
}

/// A leftover the cell must address by hand (borrow-checker, trait bound,
/// or an error with no actionable suggestion).
#[derive(Debug, Clone, serde::Serialize)]
struct Leftover {
    message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    code: Option<String>,
    /// Why this was not auto-fixed.
    reason: String,
}

impl RustFixRound {
    fn classify(&self, diagnostics: &[Value], restrict: Option<&[String]>) -> ToolResult {
        if diagnostics.is_empty() {
            return ToolResult::Error(
                "rust.fixRound: no diagnostics to classify (pass build.gate diagnostics[] or raw_json)"
                    .to_string(),
            );
        }
        let mut changes: Vec<ChangeProposal> = Vec::new();
        let mut leftovers: Vec<Leftover> = Vec::new();
        let mut findings: Vec<Value> = Vec::new();
        // Per-file `use` proposals so we can deduplicate add-use across
        // diagnostics that flag the same missing import (v1 behavior).
        let mut seen_use: std::collections::BTreeSet<(String, String)> =
            std::collections::BTreeSet::new();

        for diag in diagnostics {
            let code = diag
                .get("code")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());
            let message = diag
                .get("message")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let suggestions = diag.get("suggestions").and_then(|v| v.as_array());

            // restrict_to_files: skip diagnostics whose file is not in scope.
            if let Some(files) = restrict {
                let file = diag
                    .get("file")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default();
                if !file.is_empty()
                    && !files.iter().any(|f| file.contains(f.as_str()))
                {
                    continue;
                }
            }

            // 1. Verbatim MachineApplicable suggestions -> compiler_suggested
            //    changes. MaybeIncorrect / HasPlaceholders suggestions are
            //    surfaced as findings (review-before-apply), not applied.
            if let Some(arr) = suggestions {
                let mut machine_applicable_found = false;
                for s in arr {
                    let applicability = s
                        .get("applicability")
                        .and_then(|v| v.as_str())
                        .unwrap_or("");
                    if applicability != "MachineApplicable" {
                        continue;
                    }
                    let Some(proposal) = suggestion_to_change(s, code.as_deref()) else {
                        continue;
                    };
                    machine_applicable_found = true;
                    // Record at compiler_suggested: edits.merge recognizes the
                    // change by content digest and the lineage carries the
                    // tier through to apply.
                    self.0.record_changes(
                        "rust.fixRound",
                        AuthorityTier::CompilerSuggested,
                        [(&proposal.span, proposal.new_text.as_str())],
                    );
                    changes.push(proposal);
                }
                if machine_applicable_found {
                    continue;
                }
            }

            // 2. Classifier-synthesized proposals keyed on the diagnostic code.
            match code.as_deref() {
                Some("E0432") | Some("E0433") | Some("E0282") => {
                    // Unresolved import/path: the compiler's
                    // suggested_replacement often carries the `use` path
                    // verbatim (and build.gate mined it into a suggestion).
                    // Try to synthesize an add-use proposal from the
                    // suggestion replacement text; otherwise leftover.
                    if let Some(proposal) =
                        synthesize_add_use(suggestions, &mut seen_use, code.as_deref())
                    {
                        changes.push(proposal);
                    } else {
                        leftovers.push(Leftover {
                            message,
                            code,
                            reason: "no machine-applicable suggestion; add the import by hand"
                                .to_string(),
                        });
                    }
                }
                Some("E0599") => {
                    // No method named X: if the owning trait is in scope a
                    // missing `use` is the likely fix. Same add-use synthesis.
                    if let Some(proposal) =
                        synthesize_add_use(suggestions, &mut seen_use, code.as_deref())
                    {
                        changes.push(proposal);
                    } else {
                        leftovers.push(Leftover {
                            message,
                            code,
                            reason: "method not found; check the trait is in scope or implement it"
                                .to_string(),
                        });
                    }
                }
                Some("E0603") | Some("E0624") | Some("E0616") => {
                    // Private item access: the classifier can propose a
                    // visibility bump, but it is a planner guess and requires
                    // operator review. Surface as a finding + leftover.
                    if let Some(proposal) = synthesize_visibility_bump(suggestions, code.as_deref())
                    {
                        findings.push(json!({
                            "finding": "visibility_bump_proposed",
                            "detail": "proposed pub(crate) rewrite; operator review required",
                            "code": code,
                            "message": message,
                        }));
                        changes.push(proposal);
                    }
                    leftovers.push(Leftover {
                        message,
                        code,
                        reason: "visibility rewrite proposed; operator review required".to_string(),
                    });
                }
                Some("E0277") | Some("E0382") | Some("E0502") | Some("E0507") | Some("E0596") => {
                    // Trait bound not satisfied / borrow-checker / move errors:
                    // always leftovers (the compiler cannot suggest a fix that
                    // is both mechanical and semantics-preserving).
                    leftovers.push(Leftover {
                        message,
                        code,
                        reason: "borrow-checker or trait-bound error; fix by hand".to_string(),
                    });
                }
                Some("E0308") => {
                    // Type mismatch: if build.gate mined a MachineApplicable
                    // suggestion it was handled above. Otherwise leftover.
                    leftovers.push(Leftover {
                        message,
                        code,
                        reason: "type mismatch with no machine-applicable suggestion".to_string(),
                    });
                }
                _ => {
                    // Unknown or empty code: leftover so the cell surfaces it.
                    leftovers.push(Leftover {
                        message,
                        code: code.clone(),
                        reason: "uncategorized diagnostic; no mechanical fix".to_string(),
                    });
                }
            }
        }

        let change_count = changes.len();
        let leftover_count = leftovers.len();
        ToolResult::Json(json!({
            "changes": changes,
            "findings": findings,
            "leftovers": leftovers,
            "counts": {
                "changes": change_count,
                "leftovers": leftover_count,
            },
            "issuance": "rust.fixRound",
        }))
    }
}

/// Convert a build.gate `BuildSuggestion` JSON value into a `ChangeProposal`
/// with a hash-anchored Span. Requires the suggestion to carry an anchored
/// `span` (build.gate produced one when the file was under the session
/// root); otherwise the cell must re-derive a Span via code.read before
/// merging.
fn suggestion_to_change(suggestion: &Value, code: Option<&str>) -> Option<ChangeProposal> {
    let span_json = suggestion.get("span")?;
    let span: Span = serde_json::from_value(span_json.clone()).ok()?;
    let new_text = suggestion
        .get("replacement")
        .and_then(|v| v.as_str())?
        .to_string();
    Some(ChangeProposal {
        span,
        new_text,
        provenance: AuthorityTier::CompilerSuggested.as_str(),
        code: code.map(|s| s.to_string()),
    })
}

/// Synthesize an add-use proposal from a suggestion whose replacement is a
/// `use <path>;` string. The suggestion's byte range points at the import
/// site; we re-anchor it as an insertion at the byte position where the `use`
/// decl should land (after the last existing `use`/`mod`). Returns None when
/// no usable suggestion is present.
///
/// NOTE: this is a planner guess (syntax_only). It does not read the file to
/// find the real insert position or check for an existing decl; the cell is
/// expected to use rust.moduleWiring for idempotent add-use. When the
/// suggestion carries a hash-anchored span, we still emit it so the cell can
/// choose; provenance stays syntax_only because the REPLACEMENT text came
/// from the compiler but the INSERT POSITION is inferred.
fn synthesize_add_use(
    suggestions: Option<&Vec<Value>>,
    seen: &mut std::collections::BTreeSet<(String, String)>,
    code: Option<&str>,
) -> Option<ChangeProposal> {
    let arr = suggestions?;
    for s in arr {
        let replacement = match s.get("replacement").and_then(|v| v.as_str()) {
            Some(r) => r,
            None => continue,
        };
        let span_json = match s.get("span") {
            Some(span) => span,
            None => continue,
        };
        let span: Span = match serde_json::from_value(span_json.clone()) {
            Ok(sp) => sp,
            Err(_) => continue,
        };
        // Only treat replacements that look like a use decl as add-use
        // candidates; other MachineApplicable suggestions were handled above.
        let trimmed = replacement.trim();
        if !trimmed.starts_with("use ") || !trimmed.ends_with(';') {
            continue;
        }
        if !seen.insert((span.file.clone(), trimmed.to_string())) {
            continue;
        }
        return Some(ChangeProposal {
            span,
            new_text: replacement.to_string(),
            provenance: AuthorityTier::SyntaxOnly.as_str(),
            code: code.map(|s| s.to_string()),
        });
    }
    None
}

/// Synthesize a visibility-bump proposal (`pub(crate) `) from a suggestion
/// span. Floors at syntax_only: the replacement is planner-authored, not
/// compiler-authored.
fn synthesize_visibility_bump(
    suggestions: Option<&Vec<Value>>,
    code: Option<&str>,
) -> Option<ChangeProposal> {
    let arr = suggestions?;
    for s in arr {
        let span_json = match s.get("span") {
            Some(span) => span,
            None => continue,
        };
        let span: Span = match serde_json::from_value(span_json.clone()) {
            Ok(sp) => sp,
            Err(_) => continue,
        };
        // The suggestion span covers the item whose visibility must change;
        // zero-length spans point at an insertion position (prefix `pub`).
        let new_text = "pub(crate) ".to_string();
        return Some(ChangeProposal {
            span,
            new_text,
            provenance: AuthorityTier::SyntaxOnly.as_str(),
            code: code.map(|c| c.to_string()),
        });
    }
    None
}

#[async_trait]
impl Tool for RustFixRound {
    fn name(&self) -> &str {
        "rust.fixRound"
    }

    fn description(&self) -> &str {
        "Classify rustc/clippy diagnostics (build.gate output shape, or raw cargo --message-format=json lines) into edit proposals + explicit leftovers. Verbatim MachineApplicable compiler suggestions become compiler_suggested changes; add-use/visibility-bump classifier proposals are synthesized at syntax_only; borrow-checker/trait-bound errors become leftovers. NEVER writes: feed {changes} into edits.merge."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "diagnostics": {
                    "type": "array",
                    "description": "Diagnostics array from build.gate (the diagnostics[] field verbatim). Each entry may carry code, file, message, suggestions[].",
                    "items": { "type": "object" }
                },
                "raw_json": {
                    "type": "string",
                    "description": "Optional raw cargo --message-format=json / rustc --error-format=json stdout; parsed and appended to diagnostics."
                },
                "restrict_to_files": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Optional file path substrings; diagnostics whose file does not match any are skipped."
                }
            }
        })
    }

    fn annotations(&self) -> ToolAnnotations {
        ToolAnnotations {
            read_only: true,
            destructive: false,
        }
    }

    fn namespace_binding(&self) -> Option<(String, String)> {
        Some(("rust".to_string(), "fixRound".to_string()))
    }

    async fn call(&self, input: Value, _cx: &ToolCx) -> ToolResult {
        let args: FixRoundInput = match serde_json::from_value(input) {
            Ok(args) => args,
            Err(e) => return ToolResult::Error(format!("rust.fixRound: bad input: {e}")),
        };

        let mut diagnostics = args.diagnostics;
        // Parse raw_json lines into the build.gate diagnostic shape so the
        // classifier sees a uniform input. Reuse the bbox-refactor parser for
        // the line decode, then normalize each RustcDiagnostic into the same
        // {code, message, suggestions} shape build.gate emits.
        if let Some(raw) = args.raw_json.as_deref() {
            let parsed = parse_raw_json_to_gate_shape(raw);
            diagnostics.extend(parsed);
        }

        if diagnostics.is_empty() {
            return ToolResult::Error(
                "rust.fixRound: no diagnostics to classify (pass build.gate diagnostics[] or raw_json)"
                    .to_string(),
            );
        }

        self.classify(&diagnostics, args.restrict_to_files.as_deref())
    }
}

/// Parse raw `cargo --message-format=json` / `rustc --error-format=json`
/// stdout into the same `{code, message, file, suggestions[]}` shape
/// `build.gate` emits, so the classifier sees one uniform input.
fn parse_raw_json_to_gate_shape(raw: &str) -> Vec<Value> {
    let parsed = bbox_refactor::parse_rustc_json_output(raw.as_bytes());
    parsed
        .into_iter()
        .map(|d| {
            // Mine suggestions from spans + children, mirroring build.gate.
            let mut suggestions = Vec::new();
            mine_raw_suggestions(&d.spans, &mut suggestions);
            for child in &d.children {
                let child_spans = child.get("spans").and_then(|v| v.as_array());
                if let Some(arr) = child_spans {
                    mine_raw_suggestions(arr, &mut suggestions);
                }
            }
            let file = d
                .spans
                .iter()
                .find(|s| {
                    s.get("is_primary")
                        .and_then(|v| v.as_bool())
                        .unwrap_or(false)
                })
                .or_else(|| d.spans.first())
                .and_then(|s| s.get("file_name"))
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());
            json!({
                "code": d.code,
                "message": d.message,
                "file": file,
                "suggestions": suggestions,
            })
        })
        .collect()
}

fn mine_raw_suggestions(spans: &[Value], out: &mut Vec<Value>) {
    for span in spans {
        let applicability = span
            .get("suggestion_applicability")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        if !matches!(
            applicability,
            "MachineApplicable" | "MaybeIncorrect" | "HasPlaceholders"
        ) {
            continue;
        }
        let Some(file) = span.get("file_name").and_then(|v| v.as_str()) else {
            continue;
        };
        let Some(replacement) = span.get("suggested_replacement").and_then(|v| v.as_str()) else {
            continue;
        };
        let byte_start = span.get("byte_start").and_then(|v| v.as_u64());
        let byte_end = span.get("byte_end").and_then(|v| v.as_u64());
        let (Some(byte_start), Some(byte_end)) = (byte_start, byte_end) else {
            continue;
        };
        out.push(json!({
            "file": file,
            "byte_start": byte_start,
            "byte_end": byte_end,
            "replacement": replacement,
            "applicability": applicability,
        }));
    }
}

#[cfg(test)]
#[allow(clippy::disallowed_methods)]
mod tests {
    use super::*;

    fn ledger() -> Arc<ProvenanceLedger> {
        Arc::new(ProvenanceLedger::default())
    }

    fn fix_round() -> RustFixRound {
        RustFixRound(ledger())
    }

    fn result_json(r: ToolResult) -> Value {
        match r {
            ToolResult::Json(v) => v,
            other => panic!("expected json, got {other:?}"),
        }
    }

    fn machine_applicable_suggestion(file: &str, start: usize, end: usize, replacement: &str) -> Value {
        let sha = bbox_refactor::sha256_hex(b"fixture");
        json!({
            "file": file,
            "byte_start": start,
            "byte_end": end,
            "replacement": replacement,
            "applicability": "MachineApplicable",
            "span": {
                "file": file,
                "byte_start": start,
                "byte_end": end,
                "content_sha256": sha,
            }
        })
    }

    fn diag_with_suggestion(code: &str, message: &str, suggestion: Value) -> Value {
        json!({
            "code": code,
            "message": message,
            "file": "src/lib.rs",
            "suggestions": [suggestion],
        })
    }

    #[test]
    fn empty_diagnostics_is_an_error() {
        let r = fix_round().classify(&[], None);
        match r {
            ToolResult::Error(e) => assert!(e.contains("no diagnostics")),
            other => panic!("expected error, got {other:?}"),
        }
    }

    #[test]
    fn machine_applicable_suggestion_becomes_compiler_suggested_change() {
        let lr = ledger();
        let tool = RustFixRound(Arc::clone(&lr));
        let suggestion = machine_applicable_suggestion("src/lib.rs", 10, 14, "\"\"");
        let diag = diag_with_suggestion("E0308", "mismatched types", suggestion);
        let result = result_json(tool.classify(std::slice::from_ref(&diag), None));

        let changes = result["changes"].as_array().unwrap();
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0]["provenance"], "compiler_suggested");
        assert_eq!(changes[0]["new_text"], "\"\"");
        assert_eq!(changes[0]["span"]["byte_start"], 10);
        assert_eq!(changes[0]["code"], "E0308");
        // Leftovers empty: the machine-applicable suggestion resolved it.
        assert!(result["leftovers"].as_array().unwrap().is_empty());

        // The ledger recorded the change at compiler_suggested tier.
        let span_json = &changes[0]["span"];
        let span: Span = serde_json::from_value(span_json.clone()).unwrap();
        assert_eq!(
            lr.recognize(&span, "\"\""),
            Some(AuthorityTier::CompilerSuggested)
        );
    }

    #[test]
    fn borrow_checker_error_is_a_leftover() {
        let tool = fix_round();
        let diag = json!({
            "code": "E0502",
            "message": "cannot borrow as mutable",
            "file": "src/lib.rs",
            "suggestions": [],
        });
        let result = result_json(tool.classify(std::slice::from_ref(&diag), None));
        assert!(result["changes"].as_array().unwrap().is_empty());
        let leftovers = result["leftovers"].as_array().unwrap();
        assert_eq!(leftovers.len(), 1);
        assert!(leftovers[0]["reason"].as_str().unwrap().contains("borrow"));
    }

    #[test]
    fn trait_bound_error_is_a_leftover() {
        let tool = fix_round();
        let diag = json!({
            "code": "E0277",
            "message": "the trait bound is not satisfied",
            "file": "src/lib.rs",
            "suggestions": [],
        });
        let result = result_json(tool.classify(std::slice::from_ref(&diag), None));
        assert!(result["changes"].as_array().unwrap().is_empty());
        assert_eq!(result["leftovers"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn machine_applicable_takes_precedence_over_add_use_synthesis() {
        // An E0432 whose suggestion replacement is a `use` decl: the verbatim
        // MachineApplicable suggestion fires first (compiler_suggested),
        // so the add-use synthesis arm (syntax_only) is never reached.
        let sha = bbox_refactor::sha256_hex(b"fixture");
        let suggestion = json!({
            "file": "src/lib.rs",
            "byte_start": 0,
            "byte_end": 0,
            "replacement": "use std::collections::HashMap;",
            "applicability": "MachineApplicable",
            "span": {
                "file": "src/lib.rs",
                "byte_start": 0,
                "byte_end": 0,
                "content_sha256": sha,
            }
        });
        let diag = diag_with_suggestion("E0432", "unresolved import", suggestion);
        let result = result_json(fix_round().classify(std::slice::from_ref(&diag), None));
        let changes = result["changes"].as_array().unwrap();
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0]["provenance"], "compiler_suggested");
    }

    #[test]
    fn visibility_bump_surfaces_finding_and_leftover() {
        let tool = fix_round();
        let sha = bbox_refactor::sha256_hex(b"fixture");
        let suggestion = json!({
            "file": "src/lib.rs",
            "byte_start": 0,
            "byte_end": 0,
            "replacement": "",
            "applicability": "MaybeIncorrect",
            "span": {
                "file": "src/lib.rs",
                "byte_start": 0,
                "byte_end": 0,
                "content_sha256": sha,
            }
        });
        let diag = json!({
            "code": "E0603",
            "message": "function is private",
            "file": "src/lib.rs",
            "suggestions": [suggestion],
        });
        let result = result_json(tool.classify(std::slice::from_ref(&diag), None));
        // Visibility bump is a syntax_only proposal + a finding + a leftover.
        let changes = result["changes"].as_array().unwrap();
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0]["provenance"], "syntax_only");
        assert_eq!(changes[0]["new_text"], "pub(crate) ");
        assert!(!result["findings"].as_array().unwrap().is_empty());
        assert_eq!(result["leftovers"].as_array().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn raw_json_is_parsed_into_diagnostics() {
        let tool = fix_round();
        let raw = r#"{"reason":"compiler-message","message":{"level":"error","code":{"code":"E0502"},"message":"borrow error","spans":[],"children":[]}}"#;
        let input = json!({ "raw_json": raw });
        let cx = ToolCx {
            root: std::path::Path::new("/tmp").to_path_buf(),
            safety: Arc::new(bro_tools::SafetyPolicy::new()),
            http: reqwest::Client::new(),
            todos: Arc::new(std::sync::Mutex::new(bro_tools::TodoList::default())),
            shell_sessions: Arc::new(std::sync::Mutex::new(
                bro_tools::ShellSessions::default(),
            )),
            edits: Arc::new(std::sync::Mutex::new(bro_tools::EditSink::default())),
            session_env: Arc::new(std::collections::BTreeMap::new()),
            tool_arg_defaults: Arc::new(bro_tools::ToolArgDefaults::default()),
            shell_env: Arc::new(Default::default()),
        };
        let result = result_json(tool.call(input, &cx).await);
        assert!(result["changes"].as_array().unwrap().is_empty());
        assert_eq!(result["leftovers"].as_array().unwrap().len(), 1);
        assert_eq!(result["leftovers"][0]["code"], "E0502");
    }

    #[test]
    fn restrict_to_files_skips_out_of_scope_diagnostics() {
        let tool = fix_round();
        let suggestion = machine_applicable_suggestion("src/lib.rs", 0, 4, "x");
        let diag = diag_with_suggestion("E0308", "mismatch", suggestion);
        let restrict = vec!["other_file.rs".to_string()];
        let result = result_json(tool.classify(
            std::slice::from_ref(&diag),
            Some(&restrict),
        ));
        // Diagnostic file src/lib.rs does not contain other_file.rs: skipped.
        assert!(result["changes"].as_array().unwrap().is_empty());
    }

    #[test]
    fn clippy_diagnostic_classifies_like_rustc() {
        // Clippy emits the same JSON shape; clippy::* codes fall through to
        // the default leftover branch (unknown code).
        let tool = fix_round();
        let diag = json!({
            "code": "clippy::needless_return",
            "message": "unneeded return statement",
            "file": "src/lib.rs",
            "suggestions": [],
        });
        let result = result_json(tool.classify(std::slice::from_ref(&diag), None));
        assert!(result["changes"].as_array().unwrap().is_empty());
        assert_eq!(result["leftovers"].as_array().unwrap().len(), 1);
        assert_eq!(result["leftovers"][0]["code"], "clippy::needless_return");
    }
}
