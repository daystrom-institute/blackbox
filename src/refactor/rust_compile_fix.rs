//! RX-C1: `rust_compile_fix_round` — classify rustc diagnostics into a reviewable RefactorPlan.
//!
//! The run-loop hook in `mod.rs` reads diagnostics from `RunCaptureContext` and passes them to
//! `plan_compile_fix`. The hook then calls `capture_ctx.mark_obligation_consumed` based on the
//! leftover count returned by the plan.

use std::collections::HashMap;
use std::path::Path;

use anyhow::{Result, bail};

use super::*;

/// Classify rustc diagnostics from `RunCaptureContext` into a reviewable `RefactorPlan`.
///
/// `diagnostics` come from the capture context under the ref named in
/// `p.toml_entries["diagnostics_ref"]` (default `"last"`). The run-loop hook resolves the ref
/// before calling here.
///
/// `restrict_to_files`: optional list of file path substrings from `p.toml_entries["restrict_to_files"]`.
///
/// Returns `error.bad_input: code=no_diagnostics_to_classify` when `diagnostics` is empty.
pub fn plan_compile_fix(p: &RefactorPlanParams, diagnostics: &[RustcDiagnostic]) -> Result<String> {
    if diagnostics.is_empty() {
        bail!("error.bad_input: code=no_diagnostics_to_classify");
    }

    let restrict_to_files: Option<Vec<String>> = p
        .toml_entries
        .as_ref()
        .and_then(|e| e.get("restrict_to_files"))
        .and_then(|v| serde_json::from_value(v.clone()).ok());

    let mut use_decl_proposals: Vec<(String, String)> = Vec::new(); // (file_path, use_path)
    // (file_path, suggestion_span_start, suggestion_span_end, replacement_text, diag_msg)
    let mut visibility_proposals: Vec<(String, usize, usize, String, String)> = Vec::new();
    let mut replace_proposals: Vec<(String, TextEdit)> = Vec::new(); // (file_path, edit)
    let mut leftovers: Vec<String> = Vec::new();

    for diag in diagnostics {
        // Optional file filter.
        if let Some(ref files) = restrict_to_files {
            let in_scope = diag.spans.iter().any(|span| {
                span.get("file_name")
                    .and_then(|f| f.as_str())
                    .map(|f| files.iter().any(|r| f.contains(r.as_str())))
                    .unwrap_or(false)
            });
            if !in_scope {
                continue;
            }
        }

        let code = diag.code.as_deref().unwrap_or("");

        match code {
            "E0432" | "E0433" => {
                // Unresolved import / path: propose add_rust_use_decl using suggested_replacement.
                if let Some(replacement) =
                    extract_suggested_replacement(&diag.spans, &diag.children)
                {
                    let use_path = normalize_use_path(&replacement);
                    if let Some(file) = primary_span_file(&diag.spans) {
                        match resolve_path(p.project_dir.as_deref(), &file) {
                            Ok(resolved) => {
                                use_decl_proposals.push((path_string(&resolved), use_path));
                            }
                            Err(_) => leftovers
                                .push(format!("{code}: {} (path resolution failed)", diag.message)),
                        }
                    } else {
                        leftovers.push(format!("{code}: {} (no primary span file)", diag.message));
                    }
                } else {
                    leftovers.push(format!(
                        "{code}: {} (no suggested_replacement)",
                        diag.message
                    ));
                }
            }

            "E0603" | "E0624" | "E0616" => {
                // Private item access: propose visibility rewrite to pub(crate).
                // Always notes in leftovers as operator review is required.
                let proposal = extract_visibility_proposal(
                    &diag.spans,
                    &diag.children,
                    p.project_dir.as_deref(),
                );
                match proposal {
                    Ok(Some((file, start, end, replacement))) => {
                        visibility_proposals.push((
                            file,
                            start,
                            end,
                            replacement,
                            diag.message.clone(),
                        ));
                    }
                    Ok(None) => {}
                    Err(_) => {}
                }
                // Always add leftover so operator is aware a review is required.
                leftovers.push(format!(
                    "{code}: {} — visibility rewrite proposed (operator review required)",
                    diag.message
                ));
            }

            "E0599" => {
                // No method named X: if owning trait is in project, propose add_rust_use_decl.
                if let Some(replacement) =
                    extract_suggested_replacement(&diag.spans, &diag.children)
                {
                    let use_path = normalize_use_path(&replacement);
                    if let Some(file) = primary_span_file(&diag.spans) {
                        match resolve_path(p.project_dir.as_deref(), &file) {
                            Ok(resolved) => {
                                use_decl_proposals.push((path_string(&resolved), use_path));
                            }
                            Err(_) => leftovers.push(format!("{code}: {}", diag.message)),
                        }
                    } else {
                        leftovers.push(format!("{code}: {}", diag.message));
                    }
                } else {
                    leftovers.push(format!("{code}: {}", diag.message));
                }
            }

            "E0277" | "E0382" | "E0502" | "E0308" => {
                // Trait bound / borrow-checker / type mismatch: always leftovers.
                leftovers.push(format!("{code}: {}", diag.message));
            }

            "E0061" => {
                // Wrong number of arguments: leftover UNLESS a machine-applicable suggestion
                // with a span matching the diagnostic's file is present.
                if let Some((file, edit)) =
                    machine_applicable_edit(&diag.spans, &diag.children, p.project_dir.as_deref())
                {
                    replace_proposals.push((file, edit));
                } else {
                    leftovers.push(format!("{code}: {}", diag.message));
                }
            }

            _ => {
                leftovers.push(format!(
                    "{}: {}",
                    if code.is_empty() { "unknown" } else { code },
                    diag.message
                ));
            }
        }
    }

    // Build FileEdits from proposals. All edits are accumulated per file.
    let mut file_edits: HashMap<String, FileEdit> = HashMap::new();
    let mut validations: Vec<ValidationStep> = Vec::new();

    for (file_path, use_path) in &use_decl_proposals {
        match build_use_decl_edit(file_path, use_path) {
            Ok(Some((insert_at, replacement, original_sha256))) => {
                let entry = file_edits.entry(file_path.clone()).or_insert_with(|| {
                    validations.push(ValidationStep::TreeSitterNoErrors {
                        path: file_path.clone(),
                        byte_range: None,
                    });
                    FileEdit {
                        path: file_path.clone(),
                        original_sha256,
                        edits: Vec::new(),
                        new_text: None,
                    }
                });
                // Deduplicate by insert position.
                if !entry.edits.iter().any(|e| e.byte_start == insert_at) {
                    entry.edits.push(TextEdit {
                        byte_start: insert_at,
                        byte_end: insert_at,
                        replacement,
                    });
                }
            }
            Ok(None) => {
                // Declaration already present; nothing to do.
            }
            Err(e) => {
                leftovers.push(format!(
                    "E0432/E0433: could not add use decl for `{use_path}`: {e}"
                ));
            }
        }
    }

    for (file_path, start, end, replacement, msg) in &visibility_proposals {
        match read_original_sha256(file_path) {
            Ok(original_sha256) => {
                let entry = file_edits.entry(file_path.clone()).or_insert_with(|| {
                    validations.push(ValidationStep::TreeSitterNoErrors {
                        path: file_path.clone(),
                        byte_range: None,
                    });
                    FileEdit {
                        path: file_path.clone(),
                        original_sha256,
                        edits: Vec::new(),
                        new_text: None,
                    }
                });
                if !entry.edits.iter().any(|e| e.byte_start == *start) {
                    entry.edits.push(TextEdit {
                        byte_start: *start,
                        byte_end: *end,
                        replacement: replacement.clone(),
                    });
                }
            }
            Err(e) => {
                // Couldn't read file; leftover already added above; just note the error.
                let _ = (msg, e);
            }
        }
    }

    for (file_path, text_edit) in &replace_proposals {
        match read_original_sha256(file_path) {
            Ok(original_sha256) => {
                let entry = file_edits
                    .entry(file_path.clone())
                    .or_insert_with(|| FileEdit {
                        path: file_path.clone(),
                        original_sha256,
                        edits: Vec::new(),
                        new_text: None,
                    });
                entry.edits.push(text_edit.clone());
            }
            Err(e) => {
                leftovers.push(format!("E0061: could not read {file_path}: {e}"));
            }
        }
    }

    // Sort edits within each file in reverse byte order so offsets stay valid when applying.
    for fe in file_edits.values_mut() {
        fe.edits.sort_by_key(|b| std::cmp::Reverse(b.byte_start));
    }

    let mut edits: Vec<FileEdit> = file_edits.into_values().collect();
    edits.sort_by(|a, b| a.path.cmp(&b.path)); // deterministic ordering

    let leftover_count = leftovers.len();
    let edit_count: usize = edits.iter().map(|fe| fe.edits.len()).sum();

    let plan = RefactorPlan {
        title: format!(
            "rust_compile_fix_round: {edit_count} proposed edit(s), {leftover_count} leftover(s)"
        ),
        kind: "rust_compile_fix_round".to_string(),
        semantic_status: SemanticStatus::LspVerified,
        dry_run: true,
        file_moves: Vec::new(),
        file_creates: Vec::new(),
        edits,
        validations,
        items: Vec::new(),
        leftovers,
        captured_variables: Vec::new(),
        remaining_source_accessors: Vec::new(),
        remaining_source_constant_refs: Vec::new(),
        external_calls: Vec::new(),
        inherited_dependencies: Vec::new(),
        deep_analysis: None,
        plan_status: PlanStatus::Planned,
        fixme_count: None,
    };

    // validate_plan_shape requires edits or file_moves; skip for leftover-only plans
    // (which are valid diagnostic reports with no mechanical fixes).
    if !plan.edits.is_empty() || !plan.file_moves.is_empty() {
        validate_plan_shape(&plan)?;
    }
    Ok(serde_json::to_string_pretty(&plan)?)
}

// ── helpers ──────────────────────────────────────────────────────────────────

/// Find a `suggested_replacement` in the diagnostic's own spans first, then in children's spans.
fn extract_suggested_replacement(
    spans: &[serde_json::Value],
    children: &[serde_json::Value],
) -> Option<String> {
    for span in spans {
        if let Some(r) = span.get("suggested_replacement").and_then(|v| v.as_str()) {
            return Some(r.to_string());
        }
    }
    for child in children {
        if let Some(child_spans) = child.get("spans").and_then(|v| v.as_array()) {
            for span in child_spans {
                if let Some(r) = span.get("suggested_replacement").and_then(|v| v.as_str()) {
                    return Some(r.to_string());
                }
            }
        }
    }
    None
}

/// Get the `file_name` from the primary span, falling back to the first span.
fn primary_span_file(spans: &[serde_json::Value]) -> Option<String> {
    spans
        .iter()
        .find(|s| {
            s.get("is_primary")
                .and_then(|v| v.as_bool())
                .unwrap_or(false)
        })
        .or_else(|| spans.first())
        .and_then(|s| s.get("file_name"))
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
}

/// Strip `"use "` prefix and `";"` suffix to get a bare use path.
fn normalize_use_path(replacement: &str) -> String {
    let s = replacement.trim();
    let s = s.strip_prefix("use ").unwrap_or(s);
    let s = s.strip_suffix(';').unwrap_or(s);
    s.trim().to_string()
}

/// Build the parameters for a use-declaration insertion.
/// Returns `(insert_at, replacement_text, original_sha256)` or `None` if already present.
fn build_use_decl_edit(file_path: &str, use_path: &str) -> Result<Option<(usize, String, String)>> {
    validate_rust_use_path(use_path)?;
    let declaration = format!("use {use_path};");
    let parsed = parse_rust_file(Path::new(file_path))?;
    if parsed.source.lines().any(|line| line.trim() == declaration) {
        return Ok(None);
    }
    let original_sha256 = sha256_hex(parsed.source.as_bytes());
    let items = rust_items(&parsed);
    let insert_at = items
        .iter()
        .filter(|item| item.kind == "use_declaration")
        .max_by_key(|item| item.byte_end)
        .map(|item| item.byte_end)
        .or_else(|| {
            items
                .iter()
                .filter(|item| item.kind == "mod_item")
                .max_by_key(|item| item.byte_end)
                .map(|item| item.trailing_trivia_end)
        })
        .unwrap_or_else(|| rust_module_decl_fallback_insert_byte(&parsed.source));
    let replacement = if parsed.source[insert_at..].starts_with('\n') {
        format!("\n{declaration}")
    } else if insert_at == parsed.source.len() || parsed.source[..insert_at].ends_with('\n') {
        format!("{declaration}\n")
    } else {
        format!("\n{declaration}\n")
    };
    Ok(Some((insert_at, replacement, original_sha256)))
}

/// Extract a visibility-rewrite proposal from E0603/E0624/E0616 children.
/// Looks for a suggestion span with byte range in children.
/// Returns `(resolved_file, byte_start, byte_end, replacement_text)`.
fn extract_visibility_proposal(
    diagnostic_spans: &[serde_json::Value],
    children: &[serde_json::Value],
    project_dir: Option<&str>,
) -> Result<Option<(String, usize, usize, String)>> {
    // Prefer a child span whose file matches the primary diagnostic span's file.
    let primary_file = primary_span_file(diagnostic_spans);
    for child in children {
        let Some(child_spans) = child.get("spans").and_then(|v| v.as_array()) else {
            continue;
        };
        for span in child_spans {
            let file = match span.get("file_name").and_then(|v| v.as_str()) {
                Some(f) => f,
                None => continue,
            };
            // Only use spans from a file that matches (or when no primary file is known).
            if primary_file.as_deref().is_some_and(|pf| pf != file) {
                continue;
            }
            let byte_start = match span.get("byte_start").and_then(|v| v.as_u64()) {
                Some(v) => v as usize,
                None => continue,
            };
            let byte_end = match span.get("byte_end").and_then(|v| v.as_u64()) {
                Some(v) => v as usize,
                None => continue,
            };
            // Use `pub(crate) ` as the visibility replacement regardless of suggestion text.
            let resolved = resolve_path(project_dir, file)?;
            return Ok(Some((
                path_string(&resolved),
                byte_start,
                byte_end,
                "pub(crate) ".to_string(),
            )));
        }
    }
    Ok(None)
}

/// For E0061: look for a `MachineApplicable` suggestion span in children whose file matches
/// one of the diagnostic's span files.
fn machine_applicable_edit(
    spans: &[serde_json::Value],
    children: &[serde_json::Value],
    project_dir: Option<&str>,
) -> Option<(String, TextEdit)> {
    let diag_files: Vec<&str> = spans
        .iter()
        .filter_map(|s| s.get("file_name").and_then(|v| v.as_str()))
        .collect();
    for child in children {
        let applicability = child
            .get("suggestion_applicability")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        if applicability != "MachineApplicable" {
            continue;
        }
        let Some(child_spans) = child.get("spans").and_then(|v| v.as_array()) else {
            continue;
        };
        for span in child_spans {
            let file = match span.get("file_name").and_then(|v| v.as_str()) {
                Some(f) => f,
                None => continue,
            };
            if !diag_files.contains(&file) {
                continue;
            }
            let replacement = match span.get("suggested_replacement").and_then(|v| v.as_str()) {
                Some(r) => r.to_string(),
                None => continue,
            };
            let byte_start = match span.get("byte_start").and_then(|v| v.as_u64()) {
                Some(v) => v as usize,
                None => continue,
            };
            let byte_end = match span.get("byte_end").and_then(|v| v.as_u64()) {
                Some(v) => v as usize,
                None => continue,
            };
            let resolved = resolve_path(project_dir, file).ok()?;
            return Some((
                path_string(&resolved),
                TextEdit {
                    byte_start,
                    byte_end,
                    replacement,
                },
            ));
        }
    }
    None
}

/// Read a file and compute its sha256 hex digest.
fn read_original_sha256(file_path: &str) -> Result<String> {
    let bytes =
        std::fs::read(file_path).map_err(|e| anyhow::anyhow!("failed to read {file_path}: {e}"))?;
    Ok(sha256_hex(&bytes))
}

// ── tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    /// Minimal params with an optional project dir.
    fn params(project_dir: Option<&str>) -> RefactorPlanParams {
        RefactorPlanParams {
            kind: "rust_compile_fix_round".to_string(),
            project_dir: project_dir.map(str::to_string),
            ..Default::default()
        }
    }

    /// Synthesize a `RustcDiagnostic` with a given code, message, spans, and children.
    fn diag(
        code: Option<&str>,
        message: &str,
        spans: Vec<serde_json::Value>,
        children: Vec<serde_json::Value>,
    ) -> RustcDiagnostic {
        RustcDiagnostic {
            level: "error".to_string(),
            code: code.map(str::to_string),
            message: message.to_string(),
            spans,
            children,
        }
    }

    /// A span pointing to a file.
    fn file_span(file: &str) -> serde_json::Value {
        serde_json::json!({
            "file_name": file,
            "byte_start": 0,
            "byte_end": 10,
            "is_primary": true
        })
    }

    /// A child span with a `suggested_replacement`.
    fn suggestion_child(file: &str, start: u64, end: u64, replacement: &str) -> serde_json::Value {
        serde_json::json!({
            "spans": [{
                "file_name": file,
                "byte_start": start,
                "byte_end": end,
                "is_primary": true,
                "suggested_replacement": replacement,
                "suggestion_applicability": "MachineApplicable"
            }]
        })
    }

    /// Helper: write a temp Rust source file and return its path as a string.
    fn write_rust_file(dir: &std::path::Path, name: &str, content: &str) -> String {
        let path = dir.join(name);
        std::fs::write(&path, content).unwrap();
        path.to_string_lossy().into_owned()
    }

    // ── empty input ──────────────────────────────────────────────────────────

    #[test]
    fn empty_diagnostics_returns_bad_input() {
        let text = plan_compile_fix(&params(None), &[]);
        let err = text.unwrap_err().to_string();
        assert!(
            err.contains("no_diagnostics_to_classify"),
            "unexpected error: {err}"
        );
    }

    // ── E0432 / E0433 ────────────────────────────────────────────────────────

    #[test]
    fn e0432_with_suggestion_adds_use_decl() {
        let dir = tempfile::tempdir().unwrap();
        let file = write_rust_file(dir.path(), "lib.rs", "fn foo() {}\n");

        let d = diag(
            Some("E0432"),
            "unresolved import `std::collections::HashMap`",
            vec![file_span(&file)],
            vec![suggestion_child(
                &file,
                0,
                0,
                "use std::collections::HashMap;",
            )],
        );
        let text = plan_compile_fix(&params(None), &[d]).unwrap();
        let plan: RefactorPlan = serde_json::from_str(&text).unwrap();
        assert_eq!(plan.kind, "rust_compile_fix_round");
        assert!(!plan.edits.is_empty(), "expected at least one FileEdit");
        let edit = &plan.edits[0];
        assert!(
            edit.edits[0]
                .replacement
                .contains("use std::collections::HashMap;"),
            "replacement should contain the use decl: {:?}",
            edit.edits[0].replacement
        );
        // No leftovers for a cleanly resolved import.
        assert!(
            plan.leftovers.is_empty(),
            "unexpected leftovers: {:?}",
            plan.leftovers
        );
        assert_eq!(plan.semantic_status, SemanticStatus::LspVerified);
    }

    #[test]
    fn e0432_without_suggestion_goes_to_leftovers() {
        let d = diag(
            Some("E0432"),
            "unresolved import `foo::Bar`",
            vec![],
            vec![],
        );
        let text = plan_compile_fix(&params(None), &[d]).unwrap();
        let plan: RefactorPlan = serde_json::from_str(&text).unwrap();
        assert!(plan.edits.is_empty());
        assert_eq!(plan.leftovers.len(), 1);
        assert!(plan.leftovers[0].contains("no suggested_replacement"));
    }

    #[test]
    fn e0433_with_suggestion_uses_span_data() {
        let dir = tempfile::tempdir().unwrap();
        let file = write_rust_file(dir.path(), "main.rs", "fn bar() {}\n");

        let d = diag(
            Some("E0433"),
            "unresolved path `crate::utils::helper`",
            vec![file_span(&file)],
            vec![suggestion_child(&file, 0, 0, "use crate::utils::helper;")],
        );
        let text = plan_compile_fix(&params(None), &[d]).unwrap();
        let plan: RefactorPlan = serde_json::from_str(&text).unwrap();
        assert!(!plan.edits.is_empty());
        assert!(
            plan.edits[0].edits[0]
                .replacement
                .contains("use crate::utils::helper;"),
            "expected use crate::utils::helper; in {:?}",
            plan.edits[0].edits[0].replacement
        );
    }

    // ── E0603 / E0624 / E0616 ────────────────────────────────────────────────

    #[test]
    fn e0603_proposes_visibility_rewrite_with_operator_review_note() {
        let dir = tempfile::tempdir().unwrap();
        let file = write_rust_file(dir.path(), "lib.rs", "fn private_fn() {}\n");

        let d = diag(
            Some("E0603"),
            "function `private_fn` is private",
            vec![file_span(&file)],
            vec![serde_json::json!({
                "spans": [{
                    "file_name": &file,
                    "byte_start": 0,
                    "byte_end": 0,
                    "is_primary": true,
                    "suggested_replacement": "pub ",
                    "suggestion_applicability": "MaybeIncorrect"
                }]
            })],
        );
        let text = plan_compile_fix(&params(None), &[d]).unwrap();
        let plan: RefactorPlan = serde_json::from_str(&text).unwrap();
        // Edit should be in plan.edits.
        assert!(
            !plan.edits.is_empty(),
            "expected visibility rewrite in edits"
        );
        assert_eq!(
            plan.edits[0].edits[0].replacement, "pub(crate) ",
            "should rewrite to pub(crate)"
        );
        // Leftover note must be present for operator review.
        assert!(
            plan.leftovers
                .iter()
                .any(|l| l.contains("operator review required")),
            "expected operator-review note in leftovers: {:?}",
            plan.leftovers
        );
    }

    #[test]
    fn e0624_no_span_info_goes_to_leftovers_only() {
        let d = diag(Some("E0624"), "method `secret` is private", vec![], vec![]);
        let text = plan_compile_fix(&params(None), &[d]).unwrap();
        let plan: RefactorPlan = serde_json::from_str(&text).unwrap();
        // With no span info we can't produce an edit; leftover should still note operator review.
        assert!(
            plan.leftovers
                .iter()
                .any(|l| l.contains("operator review required")),
            "expected operator-review note: {:?}",
            plan.leftovers
        );
    }

    #[test]
    fn e0616_privacy_edit_uses_pub_crate_not_suggestion_text() {
        let dir = tempfile::tempdir().unwrap();
        let file = write_rust_file(dir.path(), "lib.rs", "struct Foo { val: u32 }\n");

        let d = diag(
            Some("E0616"),
            "field `val` of struct `Foo` is private",
            vec![file_span(&file)],
            vec![serde_json::json!({
                "spans": [{
                    "file_name": &file,
                    "byte_start": 13,
                    "byte_end": 13,
                    "is_primary": true,
                    "suggested_replacement": "pub ",
                    "suggestion_applicability": "MaybeIncorrect"
                }]
            })],
        );
        let text = plan_compile_fix(&params(None), &[d]).unwrap();
        let plan: RefactorPlan = serde_json::from_str(&text).unwrap();
        // Regardless of suggestion text, replacement must be pub(crate).
        assert!(!plan.edits.is_empty());
        assert_eq!(plan.edits[0].edits[0].replacement, "pub(crate) ");
    }

    // ── E0277 / E0382 / E0502 / E0308 ── always leftovers ───────────────────

    #[test]
    fn e0277_trait_bound_goes_to_leftovers() {
        let d = diag(
            Some("E0277"),
            "the trait bound `Foo: Bar` is not satisfied",
            vec![],
            vec![],
        );
        let text = plan_compile_fix(&params(None), &[d]).unwrap();
        let plan: RefactorPlan = serde_json::from_str(&text).unwrap();
        assert!(plan.edits.is_empty(), "E0277 must never auto-repair");
        assert_eq!(plan.leftovers.len(), 1);
        assert!(plan.leftovers[0].contains("E0277"));
    }

    #[test]
    fn e0382_borrow_checker_goes_to_leftovers() {
        let d = diag(Some("E0382"), "use of moved value: `x`", vec![], vec![]);
        let text = plan_compile_fix(&params(None), &[d]).unwrap();
        let plan: RefactorPlan = serde_json::from_str(&text).unwrap();
        assert!(plan.edits.is_empty());
        assert!(plan.leftovers[0].contains("E0382"));
    }

    #[test]
    fn e0502_borrow_checker_goes_to_leftovers() {
        let d = diag(
            Some("E0502"),
            "cannot borrow `x` as mutable because it is also borrowed as immutable",
            vec![],
            vec![],
        );
        let text = plan_compile_fix(&params(None), &[d]).unwrap();
        let plan: RefactorPlan = serde_json::from_str(&text).unwrap();
        assert!(plan.edits.is_empty());
        assert!(plan.leftovers[0].contains("E0502"));
    }

    #[test]
    fn e0308_mismatched_types_goes_to_leftovers() {
        let d = diag(
            Some("E0308"),
            "mismatched types: expected `u32`, found `i32`",
            vec![],
            vec![],
        );
        let text = plan_compile_fix(&params(None), &[d]).unwrap();
        let plan: RefactorPlan = serde_json::from_str(&text).unwrap();
        assert!(plan.edits.is_empty());
        assert!(plan.leftovers[0].contains("E0308"));
    }

    // ── E0061 ────────────────────────────────────────────────────────────────

    #[test]
    fn e0061_without_machine_applicable_suggestion_goes_to_leftovers() {
        let d = diag(
            Some("E0061"),
            "this function takes 2 arguments but 1 was supplied",
            vec![],
            vec![],
        );
        let text = plan_compile_fix(&params(None), &[d]).unwrap();
        let plan: RefactorPlan = serde_json::from_str(&text).unwrap();
        assert!(plan.edits.is_empty());
        assert!(plan.leftovers[0].contains("E0061"));
    }

    #[test]
    fn e0061_with_machine_applicable_suggestion_produces_replace_text() {
        let dir = tempfile::tempdir().unwrap();
        let file = write_rust_file(
            dir.path(),
            "lib.rs",
            "fn foo(a: u32, b: u32) {} fn bar() { foo(1); }\n",
        );

        let d = diag(
            Some("E0061"),
            "this function takes 2 arguments but 1 was supplied",
            vec![file_span(&file)],
            vec![serde_json::json!({
                "suggestion_applicability": "MachineApplicable",
                "spans": [{
                    "file_name": &file,
                    "byte_start": 37,
                    "byte_end": 43,
                    "is_primary": true,
                    "suggested_replacement": "foo(1, 0)"
                }]
            })],
        );
        let text = plan_compile_fix(&params(None), &[d]).unwrap();
        let plan: RefactorPlan = serde_json::from_str(&text).unwrap();
        assert!(!plan.edits.is_empty(), "expected replace_text edit");
        assert_eq!(plan.edits[0].edits[0].replacement, "foo(1, 0)");
        assert!(plan.leftovers.is_empty());
    }

    // ── unrecognized error code ───────────────────────────────────────────────

    #[test]
    fn unrecognized_code_goes_to_leftovers() {
        let d = diag(Some("E9999"), "some unknown error", vec![], vec![]);
        let text = plan_compile_fix(&params(None), &[d]).unwrap();
        let plan: RefactorPlan = serde_json::from_str(&text).unwrap();
        assert!(plan.edits.is_empty());
        assert!(plan.leftovers[0].contains("E9999"));
    }

    #[test]
    fn null_code_goes_to_leftovers_as_unknown() {
        let d = diag(None, "some compiler note", vec![], vec![]);
        let text = plan_compile_fix(&params(None), &[d]).unwrap();
        let plan: RefactorPlan = serde_json::from_str(&text).unwrap();
        assert!(plan.leftovers[0].contains("unknown"));
    }

    // ── suggestions use span data ─────────────────────────────────────────────

    #[test]
    fn span_data_used_for_use_decl_insertion_position() {
        let dir = tempfile::tempdir().unwrap();
        // File already has a use decl so insertion should go after it.
        let file = write_rust_file(dir.path(), "lib.rs", "use std::fmt;\n\nfn foo() {}\n");
        let d = diag(
            Some("E0432"),
            "unresolved import `std::collections::HashMap`",
            vec![file_span(&file)],
            vec![suggestion_child(
                &file,
                0,
                0,
                "use std::collections::HashMap;",
            )],
        );
        let text = plan_compile_fix(&params(None), &[d]).unwrap();
        let plan: RefactorPlan = serde_json::from_str(&text).unwrap();
        assert!(!plan.edits.is_empty());
        // Insertion should be after the existing `use std::fmt;` declaration (byte > 0).
        let insert_at = plan.edits[0].edits[0].byte_start;
        assert!(
            insert_at > 0,
            "should insert after existing use decl, not at byte 0; got {insert_at}"
        );
    }

    // ── integration: obligation lifecycle via plan_compile_fix ───────────────

    /// Write a shell script that exits 1 and emits N compiler-message JSON lines with the
    /// given error code (or null code when code is None).
    fn make_compile_fix_script(
        dir: &std::path::Path,
        name: &str,
        code: Option<&str>,
        count: usize,
    ) -> std::path::PathBuf {
        let data_file = dir.join(format!("{name}_data.txt"));
        let script = dir.join(format!("{name}.sh"));
        let mut lines = Vec::new();
        for i in 0..count {
            let code_json = match code {
                Some(c) => serde_json::json!({"code": c}),
                None => serde_json::Value::Null,
            };
            lines.push(
                serde_json::json!({
                    "reason": "compiler-message",
                    "message": {
                        "level": "error",
                        "code": code_json,
                        "message": format!("error {i}"),
                        "spans": [],
                        "children": []
                    }
                })
                .to_string(),
            );
        }
        std::fs::write(&data_file, lines.join("\n")).unwrap();
        let script_body = format!("#!/bin/sh\ncat {}\nexit 1", data_file.display());
        std::fs::write(&script, script_body).unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        script
    }

    fn make_compile_fix_script_with_lines(
        dir: &std::path::Path,
        name: &str,
        lines: Vec<String>,
    ) -> std::path::PathBuf {
        let data_file = dir.join(format!("{name}_data.txt"));
        let script = dir.join(format!("{name}.sh"));
        std::fs::write(&data_file, lines.join("\n")).unwrap();
        let script_body = format!("#!/bin/sh\ncat {}\nexit 1", data_file.display());
        std::fs::write(&script, script_body).unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        script
    }

    fn project_record_for(path: &std::path::Path) -> crate::projects::ProjectRecord {
        crate::projects::ProjectRecord {
            project_id: "test-project".to_string(),
            repo_id: None,
            canonical_path: std::fs::canonicalize(path)
                .unwrap()
                .to_string_lossy()
                .into_owned(),
            registered_at: "2026-05-11T00:00:00Z".to_string(),
            is_git_repo: false,
            languages: Default::default(),
        }
    }

    fn with_state_dir(dir: &std::path::Path) -> impl Drop {
        let _lock = crate::util::test_env_lock();
        unsafe { std::env::set_var("BLACKBOX_STATE_DIR", dir) };
        struct Guard;
        impl Drop for Guard {
            fn drop(&mut self) {
                unsafe { std::env::remove_var("BLACKBOX_STATE_DIR") };
            }
        }
        Guard
    }

    /// Integration: all diagnostics are unrecognized → all leftovers → obligation LeftOver → commits.
    #[test]
    fn integration_all_leftovers_obligation_left_over_commits() {
        let dir = tempfile::tempdir().unwrap();
        let state_dir = tempfile::tempdir().unwrap();
        let _guard = with_state_dir(state_dir.path());
        // Script emits 2 diagnostics with null codes (will go to leftovers as "unknown").
        let script = make_compile_fix_script(dir.path(), "check", None, 2);

        let response = run(
            &RefactorRunParams {
                title: "compile_fix all-leftovers".into(),
                project_dir: path_string(dir.path()),
                steps: vec![
                    RefactorRunStep::Command {
                        command: path_string(&script),
                        args: Vec::new(),
                        cwd: None,
                        touches: Vec::new(),
                        required: Some(false),
                        capture: Some(CaptureSpec::RustcJson),
                        on_failure: Some(OnFailure::ContinueForRepair),
                    },
                    RefactorRunStep::Plan {
                        params: RefactorPlanParams {
                            kind: "rust_compile_fix_round".into(),
                            source: "last".into(),
                            ..Default::default()
                        },
                        optional: false,
                    },
                    RefactorRunStep::Command {
                        command: "true".into(),
                        args: Vec::new(),
                        cwd: None,
                        touches: Vec::new(),
                        required: Some(true),
                        capture: None,
                        on_failure: None,
                    },
                ],
                confirm: Some(true),
                allow_dirty_worktree: None,
                allow_unregistered_paths: Some(true),
                dispatch_origin: None,
            },
            &[project_record_for(dir.path())],
        )
        .unwrap();

        let resp: RefactorRunResponse = serde_json::from_str(&response).unwrap();
        assert_eq!(
            resp.status, "ok",
            "all-leftovers run should commit: {response}"
        );
        assert!(!resp.rolled_back);
        assert_eq!(resp.obligations.len(), 1);
        assert_eq!(
            resp.obligations[0].status, "left_over",
            "obligation should be left_over when all diagnostics → leftovers: {response}"
        );
        assert_eq!(resp.obligations[0].leftover_count, 2);
    }

    /// Integration: machine-applicable suggestions from rust_compile_fix_round
    /// are applied inside the compound run before the final validation step.
    #[test]
    fn integration_compile_fix_round_applies_repair_edits() {
        let dir = tempfile::tempdir().unwrap();
        let state_dir = tempfile::tempdir().unwrap();
        let _guard = with_state_dir(state_dir.path());
        let file = write_rust_file(dir.path(), "lib.rs", "fn main() { foo(1); }\n");
        let source = std::fs::read_to_string(&file).unwrap();
        let start = source.find("foo(1)").unwrap() as u64;
        let end = start + "foo(1)".len() as u64;
        let line = serde_json::json!({
            "reason": "compiler-message",
            "message": {
                "level": "error",
                "code": {"code": "E0061"},
                "message": "this function takes 0 arguments but 1 argument was supplied",
                "spans": [file_span(&file)],
                "children": [{
                    "suggestion_applicability": "MachineApplicable",
                    "spans": [{
                        "file_name": &file,
                        "byte_start": start,
                        "byte_end": end,
                        "is_primary": true,
                        "suggested_replacement": "foo()"
                    }]
                }]
            }
        })
        .to_string();
        let script = make_compile_fix_script_with_lines(dir.path(), "check_repair", vec![line]);

        let response = run(
            &RefactorRunParams {
                title: "compile_fix applies repairs".into(),
                project_dir: path_string(dir.path()),
                steps: vec![
                    RefactorRunStep::Command {
                        command: path_string(&script),
                        args: Vec::new(),
                        cwd: None,
                        touches: Vec::new(),
                        required: Some(false),
                        capture: Some(CaptureSpec::RustcJson),
                        on_failure: Some(OnFailure::ContinueForRepair),
                    },
                    RefactorRunStep::Plan {
                        params: RefactorPlanParams {
                            kind: "rust_compile_fix_round".into(),
                            source: "last".into(),
                            ..Default::default()
                        },
                        optional: false,
                    },
                    RefactorRunStep::Command {
                        command: "true".into(),
                        args: Vec::new(),
                        cwd: None,
                        touches: Vec::new(),
                        required: Some(true),
                        capture: None,
                        on_failure: None,
                    },
                ],
                confirm: Some(true),
                allow_dirty_worktree: None,
                allow_unregistered_paths: Some(true),
                dispatch_origin: None,
            },
            &[project_record_for(dir.path())],
        )
        .unwrap();

        let resp: RefactorRunResponse = serde_json::from_str(&response).unwrap();
        assert_eq!(resp.status, "ok", "{response}");
        assert_eq!(resp.obligations.len(), 1);
        assert_eq!(resp.obligations[0].status, "consumed");
        assert_eq!(
            std::fs::read_to_string(&file).unwrap(),
            "fn main() { foo(); }\n"
        );
        assert!(
            resp.steps.iter().any(
                |step| step.kind.as_deref() == Some("rust_compile_fix_round")
                    && step.files.iter().any(|path| path == &file)
            ),
            "repair step should report the edited file: {response}"
        );
    }

    /// Integration: no repair step → obligation stays Open → run rolls back.
    #[test]
    fn integration_no_repair_step_obligation_unresolved_rolls_back() {
        let dir = tempfile::tempdir().unwrap();
        let state_dir = tempfile::tempdir().unwrap();
        let _guard = with_state_dir(state_dir.path());
        let script = make_compile_fix_script(dir.path(), "check2", None, 1);

        let response = run(
            &RefactorRunParams {
                title: "compile_fix no repair step".into(),
                project_dir: path_string(dir.path()),
                steps: vec![
                    RefactorRunStep::Command {
                        command: path_string(&script),
                        args: Vec::new(),
                        cwd: None,
                        touches: Vec::new(),
                        required: Some(false),
                        capture: Some(CaptureSpec::RustcJson),
                        on_failure: Some(OnFailure::ContinueForRepair),
                    },
                    // No rust_compile_fix_round step — obligation stays Open.
                    RefactorRunStep::Command {
                        command: "true".into(),
                        args: Vec::new(),
                        cwd: None,
                        touches: Vec::new(),
                        required: Some(true),
                        capture: None,
                        on_failure: None,
                    },
                ],
                confirm: Some(true),
                allow_dirty_worktree: None,
                allow_unregistered_paths: Some(true),
                dispatch_origin: None,
            },
            &[project_record_for(dir.path())],
        )
        .unwrap();

        let resp: RefactorRunResponse = serde_json::from_str(&response).unwrap();
        assert_eq!(
            resp.status, "obligations_unresolved",
            "should fail with unresolved obligation: {response}"
        );
        assert!(resp.rolled_back, "should have rolled back");
        assert_eq!(resp.obligations[0].status, "open");
    }
}
