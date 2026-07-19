//! `rust.fixRound` — classify rustc/clippy JSON diagnostics into edit proposals.
//!
//! The loop engine of the compile-fix loop (design
//! refactor-tools/rust/rust-isolate-surface.md §2.2): a cell runs
//! `build.gate("cargo check --message-format=json")`, feeds the diagnostics
//! array here, and merges the returned `changes` via `edits.merge`. Leftovers
//! (borrow-checker, trait-bound, type-mismatch errors) are surfaced, not
//! retried; the loop hard-caps at ~5 rounds (recipe discipline, not a
//! binding concern).
//!
//! Port of the v1 `rust_compile_fix::plan_compile_fix` classifier, re-shaped
//! to the binding's `{changes, findings, leftovers}` output. The v1 returned
//! a serialized `RefactorPlan` JSON string and lived behind a `pub(crate)`
//! module; re-porting here keeps the bbox-refactor API surface unchanged and
//! yields the edits-algebra shape natively. The line-level decode reuses
//! `bbox_refactor::parse_rustc_json_output` (the public parser).
//!
//! Provenance (design §8.1): only edits whose span AND replacement come
//! verbatim from a rustc/clippy `MachineApplicable` `suggested_replacement`
//! are recorded at the `compiler_suggested` tier. Classifier-synthesized
//! proposals (add-use insertion, visibility-bump) floor at `syntax_only` —
//! they are planner guesses informed by compiler output, not compiler bytes.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use bbox_refactor::sha256_hex;
use bro_tools::{Tool, ToolAnnotations, ToolCx, ToolResult};
use serde::Deserialize;
use serde_json::{Value, json};

use super::super::code_facts::Span;
use super::super::ledger::{AuthorityTier, ProvenanceLedger};

/// One change plus the tier its producer earned (lineage).
#[derive(Debug, Clone)]
struct FixChange {
    span: Span,
    new_text: String,
    tier: AuthorityTier,
}

/// `rust.fixRound` — the compile-fix loop engine.
pub struct RustFixRound(pub Arc<ProvenanceLedger>);

#[derive(Deserialize)]
struct FixRoundInput {
    /// The diagnostics array as `build.gate` emits it (each entry carries
    /// `file`, `line`, `column`, `code`, `message`, and `suggestions[]`).
    /// Mutually exclusive with `rustc_json`.
    #[serde(default, rename = "diagnostics")]
    diagnostics: Option<Value>,
    /// A raw rustc/cargo JSON-lines string; parsed with
    /// `bbox_refactor::parse_rustc_json_output`. Mutually exclusive with
    /// `diagnostics`.
    #[serde(default, rename = "rustcJson", alias = "rustc_json")]
    rustc_json: Option<String>,
    /// Optional file path substrings; diagnostics whose primary file does
    /// not match any are skipped.
    #[serde(default, rename = "restrictToFiles", alias = "restrict_to_files")]
    restrict_to_files: Option<Vec<String>>,
}

#[async_trait]
impl Tool for RustFixRound {
    fn name(&self) -> &str {
        "rust.fixRound"
    }
    fn description(&self) -> &str {
        "Classify rustc/clippy JSON diagnostics (the shape build.gate emits for cargo --message-format=json) into edit proposals + explicit leftovers. MachineApplicable suggested_replacement edits record at compiler_suggested; add-use/visibility-bump classifier proposals floor at syntax_only. Never writes; feed changes to edits.merge."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "diagnostics": {
                    "type": "array",
                    "description": "The diagnostics array from build.gate (each entry: file?, line?, column?, code?, message, suggestions[])."
                },
                "rustcJson": {
                    "type": "string",
                    "description": "A raw rustc/cargo --message-format=json JSON-lines string; parsed host-side. Mutually exclusive with diagnostics."
                },
                "restrictToFiles": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Optional file path substrings; diagnostics whose primary file does not match any are skipped."
                }
            },
            "description": "Pass exactly one of diagnostics or rustcJson."
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
    async fn call(&self, input: Value, cx: &ToolCx) -> ToolResult {
        let params: FixRoundInput = match serde_json::from_value(input) {
            Ok(p) => p,
            Err(e) => return err(format!("rust.fixRound: bad input: {e}")),
        };

        // Normalize the two input modes into the canonical diagnostic shape
        // (the same shape build.gate emits). A raw rustc_json string is
        // decoded then re-projected so both modes share one classifier.
        let diagnostics_val: Vec<Value> = match (params.diagnostics, params.rustc_json) {
            (Some(d), None) => match d.as_array() {
                Some(arr) => arr.clone(),
                None => return err("rust.fixRound: diagnostics must be an array".to_string()),
            },
            (None, Some(json_text)) => {
                let rustc_diags = bbox_refactor::parse_rustc_json_output(json_text.as_bytes());
                rustc_diags.into_iter().map(rustc_diag_to_build_json).collect()
            }
            (Some(_), Some(_)) => {
                return err(
                    "rust.fixRound: pass exactly one of diagnostics or rustcJson, not both"
                        .to_string(),
                );
            }
            (None, None) => {
                return err(
                    "rust.fixRound: pass one of diagnostics (from build.gate) or rustcJson"
                        .to_string(),
                );
            }
        };

        if diagnostics_val.is_empty() {
            return err("rust.fixRound: no diagnostics to classify".to_string());
        }

        let restrict = params.restrict_to_files;
        let root = cx.root.clone();
        let ledger = Arc::clone(&self.0);
        bro_tools::tool::call_blocking(move || {
            classify_and_build(&root, &diagnostics_val, restrict.as_deref(), &ledger)
        })
        .await
    }
}

/// Project a `bbox_refactor::RustcDiagnostic` into the same JSON shape
/// `build.gate` emits, so the classifier has one input form.
fn rustc_diag_to_build_json(diag: bbox_refactor::RustcDiagnostic) -> Value {
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
            (
                s.get("file_name").and_then(|v| v.as_str()).map(String::from),
                s.get("line_start").and_then(|v| v.as_u64()),
                s.get("column_start").and_then(|v| v.as_u64()),
            )
        })
        .unwrap_or((None, None, None));
    let suggestions = collect_suggestions_json(&diag.spans, &diag.children);
    json!({
        "file": file,
        "line": line,
        "column": column,
        "severity": diag.level,
        "code": diag.code,
        "message": diag.message,
        "suggestions": suggestions,
    })
}

/// Flatten suggestion spans (own + children) into the summary JSON shape.
fn collect_suggestions_json(
    spans: &[Value],
    children: &[Value],
) -> Vec<Value> {
    let mut out = Vec::new();
    let push = |out: &mut Vec<Value>, span: &Value, message: &str| {
        let replacement = match span.get("suggested_replacement").and_then(|v| v.as_str()) {
            Some(r) => r,
            None => return,
        };
        out.push(json!({
            "message": message,
            "applicability": span.get("suggestion_applicability").and_then(|v| v.as_str()).unwrap_or(""),
            "replacement": replacement,
            "file": span.get("file_name").and_then(|v| v.as_str()),
            "line": span.get("line_start").and_then(|v| v.as_u64()),
            "column": span.get("column_start").and_then(|v| v.as_u64()),
            "byte_start": span.get("byte_start").and_then(|v| v.as_u64()),
            "byte_end": span.get("byte_end").and_then(|v| v.as_u64()),
        }));
    };
    for span in spans {
        let label = span.get("label").and_then(|v| v.as_str()).unwrap_or("");
        push(&mut out, span, label);
    }
    for child in children {
        let message = child.get("message").and_then(|v| v.as_str()).unwrap_or("");
        if let Some(child_spans) = child.get("spans").and_then(|v| v.as_array()) {
            for span in child_spans {
                push(&mut out, span, message);
            }
        }
    }
    out
}

/// The classifier: pure function of the diagnostics JSON + root path. Reads
/// files only to mint content shas for the Spans it builds.
#[allow(clippy::disallowed_methods)]
fn classify_and_build(
    root: &Path,
    diagnostics: &[Value],
    restrict: Option<&[String]>,
    ledger: &ProvenanceLedger,
) -> ToolResult {
    let mut changes: Vec<FixChange> = Vec::new();
    let mut findings: Vec<Value> = Vec::new();
    let mut leftovers: Vec<Value> = Vec::new();
    // Cache file reads (bytes + sha) across diagnostics touching the same file.
    let mut file_cache: BTreeMap<PathBuf, Option<(String, Vec<u8>)>> = BTreeMap::new();

    for diag in diagnostics {
        let code = diag.get("code").and_then(|v| v.as_str()).unwrap_or("");
        let message = diag.get("message").and_then(|v| v.as_str()).unwrap_or("");
        let file = diag.get("file").and_then(|v| v.as_str()).unwrap_or("");
        let suggestions = diag.get("suggestions").and_then(|v| v.as_array());

        // Optional file filter (matches v1 restrict_to_files semantics).
        let skip = restrict
            .map(|filters| !file.is_empty() && !filters.iter().any(|f| file.contains(f.as_str())))
            .unwrap_or(false);
        if skip {
            continue;
        }

        // 1. MachineApplicable verbatim suggestions -> compiler_suggested edits.
        //    This is the general mechanism (design §2.2/§8.1); the v1 E0061
        //    special-case is subsumed by it.
        for sug in suggestions.cloned().unwrap_or_default() {
            let applicability = sug
                .get("applicability")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            if applicability != "MachineApplicable" {
                continue;
            }
            let replacement = match sug.get("replacement").and_then(|v| v.as_str()) {
                Some(r) => r.to_string(),
                None => continue,
            };
            let sug_file = sug
                .get("file")
                .and_then(|v| v.as_str())
                .unwrap_or(file)
                .to_string();
            let (Some(byte_start), Some(byte_end)) = (
                sug.get("byte_start").and_then(|v| v.as_u64()).map(|v| v as usize),
                sug.get("byte_end").and_then(|v| v.as_u64()).map(|v| v as usize),
            ) else {
                // No anchored byte range (file not under root, or un-anchored):
                // cannot build a hash-anchored Span. Surface as a finding so
                // the cell knows the suggestion existed but was not actionable.
                findings.push(json!({
                    "finding": "machine_applicable_unanchored",
                    "code": code,
                    "file": sug_file,
                    "message": message,
                    "replacement": replacement,
                    "resolution_hint": "re-run build.gate with anchor_spans:true, or the file is outside the session root"
                }));
                continue;
            };
            match build_span(root, &sug_file, byte_start, byte_end, &mut file_cache) {
                Ok(span) => changes.push(FixChange {
                    span,
                    new_text: replacement,
                    tier: AuthorityTier::CompilerSuggested,
                }),
                Err(reason) => findings.push(json!({
                    "finding": "machine_applicable_unreadable",
                    "code": code,
                    "file": sug_file,
                    "message": reason,
                })),
            }
        }

        // 2. Code-specific classifier synthesis (floors at syntax_only).
        match code {
            "E0432" | "E0433" | "E0599" => {
                // Unresolved import/path or no-method: synthesize an add-use
                // insertion from a MachineApplicable suggestion whose
                // replacement looks like a `use ...;` line.
                if let Some(decl) = first_use_decl_suggestion(suggestions, message) {
                    if let Some(sug_file) = diag.get("file").and_then(|v| v.as_str()) {
                        match synthesize_add_use(root, sug_file, &decl, &mut file_cache) {
                            Ok(Some(change)) => changes.push(change),
                            Ok(None) => findings.push(json!({
                                "finding": "use_decl_already_present",
                                "code": code,
                                "declaration": decl,
                            })),
                            Err(reason) => findings.push(json!({
                                "finding": "use_decl_synthesis_failed",
                                "code": code,
                                "message": reason,
                            })),
                        }
                    }
                } else if code == "E0432" || code == "E0433" {
                    leftovers.push(json!({
                        "code": code,
                        "message": format!("{message} (no suggested_replacement)"),
                    }));
                } else {
                    // E0599 with no suggestion: leftover (v1 behavior).
                    leftovers.push(json!({ "code": code, "message": message }));
                }
            }
            "E0603" | "E0624" | "E0616" => {
                // Private item: synthesize a `pub(crate)` visibility bump at
                // the suggestion span; ALWAYS also a leftover because the
                // operator must review whether widening is the right call.
                if let Some(change) = synthesize_visibility_bump(diag, suggestions, root, &mut file_cache)
                {
                    changes.push(change);
                }
                leftovers.push(json!({
                    "code": code,
                    "message": format!("{message} - visibility rewrite proposed (operator review required)"),
                }));
            }
            "E0277" | "E0382" | "E0502" | "E0308" => {
                // Trait bound / borrow-checker / type mismatch: always leftover.
                leftovers.push(json!({ "code": code, "message": message }));
            }
            "" => {
                leftovers.push(json!({ "code": "unknown", "message": message }));
            }
            other => {
                // Unknown code: leftover, but a MachineApplicable suggestion
                // (if any) was already promoted to an edit in step 1.
                leftovers.push(json!({ "code": other, "message": message }));
            }
        }
    }

    // Split changes by tier for ledger recording + the result summary.
    let mut compiler_changes: Vec<(&Span, &str)> = Vec::new();
    let mut compiler_suggested_count = 0usize;
    let mut syntax_only_count = 0usize;
    for c in &changes {
        match c.tier {
            AuthorityTier::CompilerSuggested => {
                compiler_suggested_count += 1;
                compiler_changes.push((&c.span, c.new_text.as_str()));
            }
            AuthorityTier::SyntaxOnly => syntax_only_count += 1,
            AuthorityTier::LspVerified => {}
        }
    }

    // Record ONLY the compiler_suggested changes at their tier. Syntax-only
    // changes are not ledgered (edits.merge floors unrecognized material at
    // syntax_only by default, which is exactly their tier).
    let issuance = if compiler_changes.is_empty() {
        None
    } else {
        Some(ledger.record_changes(
            "rust.fixRound",
            AuthorityTier::CompilerSuggested,
            compiler_changes,
        ))
    };

    // Result provenance: the strongest tier any change reached (for the
    // summary field). Leftover-only rounds report syntax_only (the floor).
    let provenance = if compiler_suggested_count > 0 {
        AuthorityTier::CompilerSuggested.as_str()
    } else {
        AuthorityTier::SyntaxOnly.as_str()
    };

    let leftover_count = leftovers.len();
    let change_count = changes.len();
    let title = format!(
        "rust.fixRound: {change_count} proposed edit(s), {leftover_count} leftover(s)"
    );

    ToolResult::Json(json!({
        "title": title,
        "changes": changes.iter().map(|c| json!({
            "span": c.span,
            "new_text": c.new_text,
        })).collect::<Vec<_>>(),
        "findings": findings,
        "leftovers": leftovers,
        "issuance": issuance,
        "compiler_suggested": compiler_suggested_count,
        "syntax_only": syntax_only_count,
        "leftover_count": leftover_count,
        "provenance": provenance,
    }))
}

/// Resolve `file` under `root`, read it, and build a hash-anchored `Span`
/// for the given byte range. Returns `Err(message)` when the file is
/// unreadable or the byte range is out of bounds; both become findings.
#[allow(clippy::disallowed_methods)]
fn build_span(
    root: &Path,
    file: &str,
    byte_start: usize,
    byte_end: usize,
    cache: &mut BTreeMap<PathBuf, Option<(String, Vec<u8>)>>,
) -> Result<Span, String> {
    let resolved = match bro_tools::workspace::resolve_in_root(root, file) {
        Ok(p) => p,
        Err(e) => return Err(format!("resolve {file}: {e}")),
    };
    let entry = cache.entry(resolved.clone()).or_insert_with(|| {
        let bytes = std::fs::read(&resolved).ok()?;
        let sha = sha256_hex(&bytes);
        Some((sha, bytes))
    });
    let (sha, bytes) = entry.as_ref().ok_or_else(|| format!("unreadable: {file}"))?;
    if byte_end > bytes.len() || byte_start > byte_end {
        return Err(format!(
            "byte range {byte_start}..{byte_end} out of bounds for {file} ({} bytes)",
            bytes.len()
        ));
    }
    Ok(Span {
        file: file.to_string(),
        byte_start,
        byte_end,
        content_sha256: sha.clone(),
    })
}

/// Find the first MachineApplicable suggestion whose replacement parses as a
/// `use ...;` declaration, returning the normalized use path. Mirrors v1
/// `extract_suggested_replacement` + `normalize_use_path`.
fn first_use_decl_suggestion(suggestions: Option<&Vec<Value>>, _diag_message: &str) -> Option<String> {
    let suggestions = suggestions?;
    for sug in suggestions {
        let applicability = sug
            .get("applicability")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        if applicability != "MachineApplicable" {
            continue;
        }
        let replacement = match sug.get("replacement").and_then(|v| v.as_str()) {
            Some(r) => r.trim().to_string(),
            None => continue,
        };
        if replacement.starts_with("use ") || replacement.contains("::") {
            return Some(normalize_use_path(&replacement));
        }
    }
    None
}

/// Strip `use ` prefix and `;` suffix to get a bare use path (v1 parity).
fn normalize_use_path(replacement: &str) -> String {
    let s = replacement.trim();
    let s = s.strip_prefix("use ").unwrap_or(s);
    let s = s.strip_suffix(';').unwrap_or(s);
    s.trim().to_string()
}

/// Synthesize an add-use insertion edit. Inserts `use <path>;` at the
/// canonical use-decl insertion point (after the last existing `use ...;`
/// line, else at the top after leading line doc/comments). Returns
/// `Ok(None)` when the declaration is already present (v1 parity: idempotent).
#[allow(clippy::disallowed_methods)]
fn synthesize_add_use(
    root: &Path,
    file: &str,
    use_path: &str,
    cache: &mut BTreeMap<PathBuf, Option<(String, Vec<u8>)>>,
) -> Result<Option<FixChange>, String> {
    if !is_plausible_use_path(use_path) {
        return Err(format!("implausible use path: `{use_path}`"));
    }
    let declaration = format!("use {use_path};");
    let resolved = match bro_tools::workspace::resolve_in_root(root, file) {
        Ok(p) => p,
        Err(e) => return Err(format!("resolve {file}: {e}")),
    };
    let entry = cache
        .entry(resolved.clone())
        .or_insert_with(|| {
            let bytes = std::fs::read(&resolved).ok()?;
            Some((sha256_hex(&bytes), bytes))
        });
    let (sha, bytes) = entry.as_ref().ok_or_else(|| format!("unreadable: {file}"))?;
    let source = std::str::from_utf8(bytes).map_err(|e| format!("utf8: {e}"))?;

    // Already present? Idempotent no-op (v1 parity).
    if source.lines().any(|line| line.trim() == declaration) {
        return Ok(None);
    }

    let insert_at = use_decl_insert_byte(source);
    // Match the surrounding newline context so the inserted line lands on
    // its own line regardless of where the insertion point sits.
    let replacement = if source[insert_at..].starts_with('\n') {
        format!("\n{declaration}")
    } else if insert_at == source.len() || source[..insert_at].ends_with('\n') {
        format!("{declaration}\n")
    } else {
        format!("\n{declaration}\n")
    };

    Ok(Some(FixChange {
        span: Span {
            file: file.to_string(),
            byte_start: insert_at,
            byte_end: insert_at,
            content_sha256: sha.clone(),
        },
        new_text: replacement,
        tier: AuthorityTier::SyntaxOnly,
    }))
}

/// Reject use paths that are obviously not rust paths (contain whitespace,
/// are empty, or start with a digit). A deliberately loose check: the
/// classifier is a planner guess, and edits.apply's parse validation is the
/// real gate.
fn is_plausible_use_path(path: &str) -> bool {
    let trimmed = path.trim();
    if trimmed.is_empty() {
        return false;
    }
    if trimmed.chars().any(|c| c.is_whitespace()) {
        return false;
    }
    if trimmed
        .chars()
        .next()
        .map(|c| c.is_ascii_digit())
        .unwrap_or(true)
    {
        return false;
    }
    true
}

/// Find the byte offset at which to insert a new `use` declaration: just
/// after the last existing `use ...;` line, else just after leading
/// inner-doc (`//!`) / outer-doc (`//`) / attribute (`#![...]`) lines, else 0.
fn use_decl_insert_byte(source: &str) -> usize {
    let mut last_use_end: Option<usize> = None;
    for line in source.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("use ") && trimmed.ends_with(';') {
            // byte offset of the end of this line (exclusive of its newline).
            last_use_end = Some(line_end_byte(source, line.as_ptr() as usize, trimmed.len()));
        }
    }
    if let Some(end) = last_use_end {
        return end;
    }
    // No existing use decls: skip leading module-level doc/attribute lines.
    let mut insert_at = 0usize;
    for line in source.lines() {
        let trimmed = line.trim_start();
        if trimmed.starts_with("//!")
            || trimmed.starts_with("//")
            || trimmed.starts_with("#![")
            || trimmed.starts_with("#[")
            || trimmed.is_empty()
        {
            insert_at = line_end_byte(source, line.as_ptr() as usize, trimmed.len());
            continue;
        }
        break;
    }
    insert_at
}

/// Compute the byte offset immediately after `line`'s content (including its
/// trailing newline if present). `line_ptr` is the line's start address inside
/// `source`; used to find the line's absolute byte offset safely.
fn line_end_byte(source: &str, line_ptr: usize, line_len: usize) -> usize {
    // The lines() iterator yields &str slices that are subslices of source,
    // so the pointer offset is valid; compute the start byte from the
    // difference and add the line length + an optional trailing newline.
    let source_ptr = source.as_ptr() as usize;
    let start = line_ptr.saturating_sub(source_ptr);
    let mut end = start + line_len;
    if end < source.len() && source.as_bytes()[end] == b'\n' {
        end += 1;
    }
    end.min(source.len())
}

/// Synthesize a `pub(crate) ` visibility-bump edit at the first suggestion
/// span (regardless of applicability — the v1 always used `pub(crate) ` and
/// ignored the suggestion text). Floors at syntax_only.
fn synthesize_visibility_bump(
    diag: &Value,
    suggestions: Option<&Vec<Value>>,
    root: &Path,
    cache: &mut BTreeMap<PathBuf, Option<(String, Vec<u8>)>>,
) -> Option<FixChange> {
    let primary_file = diag.get("file").and_then(|v| v.as_str())?;
    for sug in suggestions? {
        let sug_file = sug
            .get("file")
            .and_then(|v| v.as_str())
            .unwrap_or(primary_file);
        let (Some(byte_start), Some(byte_end)) = (
            sug.get("byte_start").and_then(|v| v.as_u64()).map(|v| v as usize),
            sug.get("byte_end").and_then(|v| v.as_u64()).map(|v| v as usize),
        ) else {
            continue;
        };
        match build_span(root, sug_file, byte_start, byte_end, cache) {
            Ok(span) => {
                return Some(FixChange {
                    span,
                    new_text: "pub(crate) ".to_string(),
                    tier: AuthorityTier::SyntaxOnly,
                });
            }
            Err(_) => continue,
        }
    }
    None
}

fn err(msg: String) -> ToolResult {
    ToolResult::Error(msg)
}

// Silence the blocking-fs clippy gate inside this binding: span-building and
// add-use synthesis read source files from a `call_blocking` tail (never a
// tokio worker), matching build_gate.rs's sanctioned pattern.
#[allow(clippy::disallowed_methods)]
fn _fs_read_sanctioned(path: &Path) -> Option<Vec<u8>> {
    std::fs::read(path).ok()
}

pub const FIX_ROUND_CONTRACT: &str = r#"rust.fixRound - classify rustc/clippy JSON diagnostics into edit proposals + explicit leftovers.

WHAT IT DOES

Takes the diagnostics array `build.gate` emits for `cargo check --message-format=json`
(or a raw rustc JSON-lines string) and buckets each diagnostic into:

- machine-applicable replace edits: any suggestion whose applicability is
  `MachineApplicable` becomes a verbatim replace edit at the
  `compiler_suggested` provenance tier (span + replacement copied byte-for-byte
  from rustc). This is the general mechanism; the v1 E0061 special-case is
  subsumed by it.
- classifier-synthesized proposals (floor at `syntax_only`):
  - E0432 / E0433 / E0599 with a `use ...;` MachineApplicable suggestion:
    synthesize an add-use insertion at the file's use-decl insertion point.
  - E0603 / E0624 / E0616 (private item): synthesize a `pub(crate) ` bump at
    the suggestion span AND always add a leftover (operator review required).
- explicit leftovers (surfaced, not retried): E0277 (trait bound), E0382 /
  E0502 (borrow-checker), E0308 (type mismatch), unknown codes, and any
  diagnostic whose suggestion could not be anchored.

Clippy rides free: clippy emits the same JSON message format, so clippy
diagnostics classify identically (lint codes become edits/leftovers by the
same rules). This closes the G13 `rust_clippy_fix_round` gap as a mode of
the same tool.

PARAMETERS

- diagnostics: array - the `diagnostics` field of a `build.gate` result.
  Each entry: { file?, line?, column?, severity, code?, message, suggestions[] }
  where each suggestion is
  { message, applicability, replacement, file?, line?, column?, byte_start?, byte_end? }.
- rustcJson: string - a raw `cargo --message-format=json` / `rustc
  --error-format=json` stdout string; parsed host-side. Mutually exclusive
  with `diagnostics`.
- restrictToFiles: string[] - optional path substrings; diagnostics whose
  primary file does not contain any are skipped.

RESULT

- changes: { span, new_text }[] for `edits.merge` (hash-anchored Spans).
- findings: structured notes (use_decl_already_present,
  machine_applicable_unanchored, machine_applicable_unreadable,
  use_decl_synthesis_failed) - repairable without re-running discovery.
- leftovers: { code?, message }[] - the manual punch list; surfaced, not retried.
- issuance: the provenance ledger id for the compiler_suggested batch (null
  when no compiler_suggested edits were produced).
- compiler_suggested / syntax_only: per-tier edit counts.
- leftover_count: leftovers.length.
- provenance: the strongest tier any change reached (`compiler_suggested` or
  `syntax_only`).

PROVENANCE (design section 8.1)

Only edits whose span AND replacement come verbatim from a rustc/clippy
`MachineApplicable` `suggested_replacement` are recorded at `compiler_suggested`.
Classifier-synthesized proposals (add-use, visibility-bump) floor at
`syntax_only`. Tiers are authorship lineage, not outcome guarantees; the
terminal `cargo check` in every recipe is the outcome gate. Pass `changes`
to `edits.merge` UNMODIFIED to preserve the tier (rewriting a change's bytes
floors it at syntax_only).

NEVER WRITES. Feed `changes` to `edits.merge` then `edits.apply`. Re-run
`build.gate("cargo check --message-format=json")` after each apply; stop when
green or at the recipe's ~5-round cap.

NOT IDEMPOTENT over its own output in the sense that a re-call after a
successful apply sees the now-resolved diagnostics disappear from
`build.gate` - that empty-diagnostics state is the DONE signal, not a retry.
"#;

/// `rust.describe` - depth-on-demand contract for one transform (matches the
/// `java.describe` / `analysis.describe` pattern; the namespace index stays
/// a compact one-liner).
pub struct RustDescribe;

#[async_trait]
impl Tool for RustDescribe {
    fn name(&self) -> &str {
        "rust.describe"
    }
    fn description(&self) -> &str {
        "Full contract for one rust.* transform (params, findings vocabulary, recipe). The namespace index lists transforms one line each; call this before first use of a transform."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "transform": { "type": "string", "description": "Transform name, e.g. \"fixRound\"." }
            },
            "required": ["transform"]
        })
    }
    fn annotations(&self) -> ToolAnnotations {
        ToolAnnotations {
            read_only: true,
            destructive: false,
        }
    }
    fn namespace_binding(&self) -> Option<(String, String)> {
        Some(("rust".to_string(), "describe".to_string()))
    }
    async fn call(&self, input: Value, _cx: &ToolCx) -> ToolResult {
        let transform = input
            .get("transform")
            .and_then(Value::as_str)
            .unwrap_or_default();
        match transform {
            "fixRound" => ToolResult::Json(json!({ "contract": FIX_ROUND_CONTRACT })),
            other => err(format!(
                "rust.describe: unknown transform `{other}` (available: fixRound)"
            )),
        }
    }
}

#[cfg(test)]
#[allow(clippy::disallowed_methods)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn cx_in(dir: &Path) -> ToolCx {
        ToolCx {
            root: dir.to_path_buf(),
            safety: Arc::new(bro_tools::SafetyPolicy::new()),
            http: reqwest::Client::new(),
            todos: Arc::new(std::sync::Mutex::new(bro_tools::TodoList::default())),
            shell_sessions: Arc::new(std::sync::Mutex::new(bro_tools::ShellSessions::default())),
            edits: Arc::new(std::sync::Mutex::new(bro_tools::EditSink::default())),
            session_env: Arc::new(std::collections::BTreeMap::new()),
            tool_arg_defaults: Arc::new(bro_tools::ToolArgDefaults::default()),
            shell_env: Arc::new(Default::default()),
        }
    }

    fn json_of(result: ToolResult) -> Value {
        match result {
            ToolResult::Json(v) => v,
            other => panic!("expected json, got {other:?}"),
        }
    }

    fn write_file(root: &Path, rel: &str, content: &str) {
        let path = root.join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, content).unwrap();
    }

    fn diag(code: Option<&str>, message: &str, file: &str, suggestions: Vec<Value>) -> Value {
        json!({
            "file": file,
            "line": 1,
            "column": 1,
            "severity": "error",
            "code": code,
            "message": message,
            "suggestions": suggestions,
        })
    }

    fn machine_applicable_suggestion(
        file: &str,
        byte_start: usize,
        byte_end: usize,
        replacement: &str,
    ) -> Value {
        json!({
            "message": "help",
            "applicability": "MachineApplicable",
            "replacement": replacement,
            "file": file,
            "line": 1,
            "column": 1,
            "byte_start": byte_start,
            "byte_end": byte_end,
        })
    }

    async fn run(root: &Path, diagnostics: Vec<Value>) -> Value {
        let ledger = Arc::new(ProvenanceLedger::default());
        let tool = RustFixRound(ledger);
        json_of(
            tool.call(json!({ "diagnostics": diagnostics }), &cx_in(root))
                .await,
        )
    }

    #[tokio::test]
    async fn empty_diagnostics_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let ledger = Arc::new(ProvenanceLedger::default());
        let tool = RustFixRound(ledger);
        let res = tool
            .call(json!({ "diagnostics": [] }), &cx_in(&root))
            .await;
        assert!(matches!(res, ToolResult::Error(ref e) if e.contains("no diagnostics")));
    }

    #[tokio::test]
    async fn both_input_modes_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let ledger = Arc::new(ProvenanceLedger::default());
        let tool = RustFixRound(ledger);
        let res = tool
            .call(
                json!({
                    "diagnostics": [],
                    "rustcJson": "{\"reason\":\"compiler-message\"}"
                }),
                &cx_in(&root),
            )
            .await;
        assert!(matches!(res, ToolResult::Error(ref e) if e.contains("exactly one")));
    }

    #[tokio::test]
    #[allow(clippy::disallowed_methods)]
    async fn machine_applicable_suggestion_becomes_compiler_suggested_edit() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        // 16 bytes: "let x: u32 = -1;\n" (note: -1 is 2 bytes at 13..15)
        write_file(&root, "src/lib.rs", "let x: u32 = -1;\n");
        let d = diag(
            Some("E0308"),
            "mismatched types: expected `u32`, found `i32`",
            "src/lib.rs",
            vec![machine_applicable_suggestion("src/lib.rs", 13, 15, "1u32")],
        );
        let out = run(&root, vec![d]).await;
        // E0308 is a type-mismatch leftover too, but the MachineApplicable
        // suggestion still promotes to a compiler_suggested edit.
        assert_eq!(out["compiler_suggested"], 1);
        assert_eq!(out["syntax_only"], 0);
        assert_eq!(out["provenance"], "compiler_suggested");
        assert!(out["issuance"].as_str().is_some());
        let change = &out["changes"][0];
        assert_eq!(change["span"]["byte_start"], 13);
        assert_eq!(change["span"]["byte_end"], 15);
        assert_eq!(change["new_text"], "1u32");
        assert!(!change["span"]["content_sha256"].as_str().unwrap().is_empty());
        // The E0308 is also surfaced as a leftover (trait/borrow/type-mismatch bucket).
        assert_eq!(out["leftover_count"], 1);
    }

    #[tokio::test]
    #[allow(clippy::disallowed_methods)]
    async fn e0432_with_use_suggestion_synthesizes_add_use_at_syntax_only() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        write_file(&root, "src/lib.rs", "fn main() {}\n");
        let d = diag(
            Some("E0432"),
            "unresolved import `std::collections::HashMap`",
            "src/lib.rs",
            vec![machine_applicable_suggestion(
                "src/lib.rs",
                0,
                0,
                "use std::collections::HashMap;",
            )],
        );
        let out = run(&root, vec![d]).await;
        // The suggestion itself is also promoted to a compiler_suggested
        // replace edit at 0..0 (an insertion). Plus the classifier
        // synthesizes an add-use at the insertion point (syntax_only).
        assert!(out["compiler_suggested"].as_u64().unwrap() >= 1);
        assert_eq!(out["syntax_only"], 1);
        // No leftover for a cleanly resolved import.
        assert_eq!(out["leftover_count"], 0);
        // The add-use change carries the full declaration.
        let has_use_decl = out["changes"]
            .as_array()
            .unwrap()
            .iter()
            .any(|c| c["new_text"]
                .as_str()
                .unwrap()
                .contains("use std::collections::HashMap;"));
        assert!(has_use_decl, "expected an add-use change: {out}");
    }

    #[tokio::test]
    #[allow(clippy::disallowed_methods)]
    async fn e0603_proposes_pub_crate_bump_and_leftover_review_note() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        write_file(&root, "src/lib.rs", "fn private_fn() {}\n");
        let d = diag(
            Some("E0603"),
            "function `private_fn` is private",
            "src/lib.rs",
            vec![json!({
                "message": "make pub",
                "applicability": "MaybeIncorrect",
                "replacement": "pub ",
                "file": "src/lib.rs",
                "byte_start": 0,
                "byte_end": 0,
                "line": 1,
                "column": 1,
            })],
        );
        let out = run(&root, vec![d]).await;
        // MaybeIncorrect suggestion is NOT promoted to compiler_suggested.
        assert_eq!(out["compiler_suggested"], 0);
        // The visibility bump is synthesized at syntax_only.
        assert_eq!(out["syntax_only"], 1);
        let bump = &out["changes"][0];
        assert_eq!(bump["new_text"], "pub(crate) ");
        // Operator-review leftover is always present for visibility.
        assert!(
            out["leftovers"]
                .as_array()
                .unwrap()
                .iter()
                .any(|l| l["message"]
                    .as_str()
                    .unwrap()
                    .contains("operator review required")),
            "{out}"
        );
    }

    #[tokio::test]
    async fn borrow_checker_and_trait_bound_are_pure_leftovers() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let d1 = diag(Some("E0382"), "use of moved value: `x`", "src/lib.rs", vec![]);
        let d2 = diag(Some("E0277"), "the trait bound is not satisfied", "src/lib.rs", vec![]);
        let out = run(&root, vec![d1, d2]).await;
        assert_eq!(out["changes"].as_array().unwrap().len(), 0);
        assert_eq!(out["leftover_count"], 2);
        assert_eq!(out["provenance"], "syntax_only");
        assert!(out["issuance"].as_str().is_none());
    }

    #[tokio::test]
    #[allow(clippy::disallowed_methods)]
    async fn raw_rustc_json_input_classifies_like_diagnostics_array() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        write_file(&root, "src/lib.rs", "let x: u32 = -1;\n");
        let abs = root.join("src/lib.rs");
        let abs_str = abs.to_string_lossy().to_string();
        // rustc JSON uses absolute paths and the cargo span shape.
        let json_text = format!(
            r#"{{"reason":"compiler-message","message":{{"code":{{"code":"E0308"}},"level":"error","message":"mismatched types","spans":[{{"file_name":"{abs_str}","byte_start":13,"byte_end":15,"line_start":1,"column_start":14,"is_primary":true}}],"children":[{{"message":"help","spans":[{{"file_name":"{abs_str}","byte_start":13,"byte_end":15,"line_start":1,"column_start":14,"is_primary":true,"suggested_replacement":"1u32","suggestion_applicability":"MachineApplicable"}}]}}]}}}}"#
        );
        let ledger = Arc::new(ProvenanceLedger::default());
        let tool = RustFixRound(ledger);
        let out = json_of(
            tool.call(json!({ "rustcJson": json_text }), &cx_in(&root))
                .await,
        );
        // The absolute path is NOT under the session root as a relative path,
        // so build_span resolves it via resolve_in_root (which accepts abs
        // paths under root). The edit lands at compiler_suggested.
        assert_eq!(out["compiler_suggested"], 1);
        assert_eq!(out["changes"][0]["new_text"], "1u32");
    }

    #[tokio::test]
    #[allow(clippy::disallowed_methods)]
    async fn out_of_bounds_byte_range_becomes_finding_not_edit() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        write_file(&root, "src/lib.rs", "fn main() {}\n"); // 13 bytes
        let d = diag(
            Some("E0308"),
            "mismatched types",
            "src/lib.rs",
            vec![machine_applicable_suggestion("src/lib.rs", 900, 999, "x")],
        );
        let out = run(&root, vec![d]).await;
        assert_eq!(out["compiler_suggested"], 0);
        assert!(
            out["findings"]
                .as_array()
                .unwrap()
                .iter()
                .any(|f| f["finding"] == "machine_applicable_unreadable"
                    || f["finding"] == "machine_applicable_unanchored"),
            "{out}"
        );
    }

    #[tokio::test]
    async fn restrict_to_files_skips_non_matching_diagnostics() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let d = diag(Some("E0382"), "moved value", "other.rs", vec![]);
        let ledger = Arc::new(ProvenanceLedger::default());
        let tool = RustFixRound(ledger);
        let out = json_of(
            tool.call(
                json!({
                    "diagnostics": [d],
                    "restrictToFiles": ["src/"]
                }),
                &cx_in(&root),
            )
            .await,
        );
        assert_eq!(out["leftover_count"], 0);
        assert_eq!(out["changes"].as_array().unwrap().len(), 0);
    }

    #[tokio::test]
    async fn unknown_code_goes_to_leftover_but_suggestion_still_promotes() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        write_file(&root, "src/lib.rs", "let x: u32 = -1;\n");
        let d = diag(
            Some("E9999"),
            "some future error",
            "src/lib.rs",
            vec![machine_applicable_suggestion("src/lib.rs", 13, 15, "1u32")],
        );
        let out = run(&root, vec![d]).await;
        assert_eq!(out["compiler_suggested"], 1);
        assert_eq!(out["leftover_count"], 1);
    }

    #[tokio::test]
    async fn ledger_recognizes_compiler_suggested_changes_round_trip() {
        // After rust.fixRound records at compiler_suggested, the same span +
        // new_text must be recognizable (the edits.merge contract).
        let ledger = Arc::new(ProvenanceLedger::default());
        let span = Span {
            file: "a.rs".to_string(),
            byte_start: 0,
            byte_end: 2,
            content_sha256: "abc".to_string(),
        };
        let issuance = ledger.record_changes(
            "rust.fixRound",
            AuthorityTier::CompilerSuggested,
            std::iter::once((&span, "xy")),
        );
        assert!(issuance.starts_with("led-"));
        assert_eq!(
            ledger.recognize(&span, "xy"),
            Some(AuthorityTier::CompilerSuggested)
        );
        assert_eq!(
            ledger.issuance_of(&span, "xy"),
            Some((issuance, "rust.fixRound"))
        );
    }

    #[test]
    fn use_decl_insert_byte_after_existing_use() {
        let src = "//! doc\nuse foo::Bar;\n\nfn main() {}\n";
        let off = use_decl_insert_byte(src);
        // Insert after the `use foo::Bar;\n` line.
        // "//! doc\n" = 8 bytes; "use foo::Bar;" = 13 chars (8..21) + "\n" at 21 -> 22.
        assert_eq!(off, 22);
    }

    #[test]
    fn use_decl_insert_byte_at_top_when_no_uses() {
        let src = "//! doc\n// comment\nfn main() {}\n";
        let off = use_decl_insert_byte(src);
        // After leading doc lines: "//! doc\n" (8) + "// comment\n" (8..18 + \n -> 19) = 19.
        assert_eq!(off, 19);
    }

    #[test]
    fn normalize_use_path_strips_use_prefix_and_semicolon() {
        assert_eq!(normalize_use_path("use std::collections::HashMap;"), "std::collections::HashMap");
        assert_eq!(normalize_use_path("crate::utils::helper;"), "crate::utils::helper");
        assert_eq!(normalize_use_path(" foo::bar "), "foo::bar");
    }

    #[tokio::test]
    async fn describe_returns_fixround_contract_and_rejects_unknown() {
        let dir = tempfile::tempdir().unwrap();
        let cx = cx_in(dir.path());
        let out = json_of(
            RustDescribe
                .call(json!({ "transform": "fixRound" }), &cx)
                .await,
        );
        assert!(
            out["contract"]
                .as_str()
                .unwrap()
                .contains("compiler_suggested"),
            "{out}"
        );
        let unknown = RustDescribe
            .call(json!({ "transform": "bogus" }), &cx)
            .await;
        assert!(matches!(unknown, ToolResult::Error(ref e) if e.contains("unknown transform")));
    }
}
