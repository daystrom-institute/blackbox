//! `unseal_csharp_class` — strangler-fig unseal of a single C# class.
//!
//! Top daystrom-mk2 pain (278 sealed-class files / 386 declarations).
//! CLAUDE.md:306–312 directive: "Prefer public class over sealed class
//! for services that may need test subclassing… When you encounter
//! sealed services: unseal them, make key methods virtual, simplify
//! constructor."
//!
//! v1 implementation is **syntax_only** — the mechanical edit (remove
//! `sealed` modifier; optionally insert `virtual` on operator-named
//! methods) does not need Roslyn. The full `lsp_verified` flavor with
//! an `inheriting_candidates` report (test types that could plausibly
//! subclass) waits for the Phase 2 sidecar. The v1 plan declares
//! `SemanticStatus::SyntaxOnly` and the design-doc-mandated
//! operator-authority flag still applies.
//!
//! Required operator-authority flag (RX-V1):
//!   `acknowledge_subclass_surface_change=true`
//!
//! Inputs:
//!   - source (file path)
//!   - item_names[0] = target class name
//!   - virtualize_methods (optional, via `item_kinds` repurposed as a
//!     CSV-string list; see plan params below)
//!
//! Refusal cases:
//!   - Missing `acknowledge_subclass_surface_change`
//!   - Class not found
//!   - Class is already non-sealed (idempotent: returns empty plan)
//!   - Generated-file guard (per Safety Rules in the design doc)

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use sha2::{Digest, Sha256};

use crate::refactor::{
    FileEdit, RefactorPlanParams, SemanticStatus, TextEdit, ValidationStep, csharp::empty_plan,
};

pub fn plan_unseal(p: &RefactorPlanParams) -> Result<String> {
    let source_path = resolve_path(p.project_dir.as_deref(), &p.source)?;
    refuse_generated_file(&source_path)?;
    let class_name = p
        .item_names
        .as_deref()
        .and_then(|names| names.first())
        .map(String::as_str)
        .ok_or_else(|| {
            anyhow!("item_names[0] (target class) is required for unseal_csharp_class")
        })?;
    validate_simple_identifier(class_name)?;

    let acknowledged = operator_flag(p, "acknowledge_subclass_surface_change");
    if !acknowledged {
        bail!(
            "error.operator_authority_required: unseal_csharp_class requires `acknowledge_subclass_surface_change=true` (RX-V1 operator-authority opt-out invariant)"
        );
    }

    let virtualize_methods = parse_virtualize_methods(p)?;

    let source = fs::read_to_string(&source_path)
        .with_context(|| format!("reading {}", source_path.display()))?;

    let class_match = locate_sealed_class(&source, class_name)?;
    let mut text_edits: Vec<TextEdit> = Vec::new();
    let mut already_unsealed = false;

    match class_match {
        ClassMatch::Sealed {
            sealed_start,
            sealed_end,
            body_start,
            body_end,
        } => {
            text_edits.push(TextEdit {
                byte_start: sealed_start,
                byte_end: sealed_end,
                replacement: String::new(),
            });
            for method_name in &virtualize_methods {
                if let Some(edit) =
                    locate_method_for_virtualize(&source, method_name, body_start, body_end)?
                {
                    text_edits.push(edit);
                }
            }
        }
        ClassMatch::AlreadyUnsealed => {
            already_unsealed = true;
        }
    }

    let mut hasher = Sha256::new();
    hasher.update(source.as_bytes());
    let sha = format!("{:x}", hasher.finalize());

    let file_edits = if text_edits.is_empty() {
        Vec::new()
    } else {
        vec![FileEdit {
            path: path_string(&source_path),
            original_sha256: sha,
            edits: text_edits,
            new_text: None,
        }]
    };

    let mut plan = empty_plan(
        "unseal_csharp_class",
        if already_unsealed {
            format!("class `{class_name}` is already non-sealed")
        } else {
            format!("unseal `{class_name}` in {}", path_string(&source_path))
        },
        SemanticStatus::SyntaxOnly,
    );
    plan.validations.push(ValidationStep::TreeSitterNoErrors {
        path: path_string(&source_path),
        byte_range: None,
    });
    plan.edits = file_edits;
    Ok(serde_json::to_string_pretty(&plan)?)
}

fn resolve_path(project_dir: Option<&str>, source: &str) -> Result<PathBuf> {
    let candidate = PathBuf::from(source);
    if candidate.is_absolute() {
        return Ok(candidate);
    }
    let base = match project_dir {
        Some(p) => PathBuf::from(p),
        None => std::env::current_dir().context("getting current directory")?,
    };
    Ok(base.join(candidate))
}

fn path_string(path: &Path) -> String {
    path.to_str().unwrap_or("").to_string()
}

fn validate_simple_identifier(name: &str) -> Result<()> {
    if name.is_empty() {
        bail!("error.invalid_csharp_identifier: empty class name");
    }
    let body = name.strip_prefix('@').unwrap_or(name);
    let mut chars = body.chars();
    let first = chars.next().unwrap();
    if !(first.is_ascii_alphabetic() || first == '_') {
        bail!("error.invalid_csharp_identifier: `{name}` must start with letter or underscore");
    }
    for c in chars {
        if !(c.is_ascii_alphanumeric() || c == '_') {
            bail!("error.invalid_csharp_identifier: `{name}` contains invalid character `{c}`");
        }
    }
    Ok(())
}

/// Read an operator-authority flag out of `toml_entries` — the same
/// channel `acknowledge_repr` / `acknowledge_public_api_change` use
/// on the Rust track. Flags are operator-explicit (RX-V1) so any
/// `false` / absent value counts as "not acknowledged."
fn operator_flag(p: &RefactorPlanParams, name: &str) -> bool {
    p.toml_entries
        .as_ref()
        .and_then(|m| m.get(name))
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
}

/// Per the design doc the v1 input shape uses `item_kinds` for the
/// virtualize-methods list (since RefactorPlanParams already carries
/// that slot and adding new typed fields requires a schema bump).
/// Each entry is a method simple-name.
fn parse_virtualize_methods(p: &RefactorPlanParams) -> Result<Vec<String>> {
    let Some(kinds) = p.item_kinds.as_deref() else {
        return Ok(Vec::new());
    };
    let mut out = Vec::with_capacity(kinds.len());
    for k in kinds {
        validate_simple_identifier(k)?;
        out.push(k.clone());
    }
    Ok(out)
}

fn refuse_generated_file(path: &Path) -> Result<()> {
    let path_str = path.to_str().unwrap_or("");
    let lower = path_str.to_ascii_lowercase();
    if lower.contains("/generated/") || lower.ends_with(".g.cs") || lower.ends_with(".designer.cs")
    {
        bail!(
            "error.generated_file_refusal: `{}` matches the generated-file guard pattern; refuse to edit (Safety Rules)",
            path_str
        );
    }
    Ok(())
}

#[derive(Debug)]
enum ClassMatch {
    Sealed {
        /// Byte offset of the `sealed` keyword start.
        sealed_start: usize,
        /// Byte offset immediately after the `sealed ` token (including
        /// trailing whitespace consumed for clean removal).
        sealed_end: usize,
        /// Byte offset of the class body's opening `{`.
        body_start: usize,
        /// Byte offset of the class body's closing `}`.
        body_end: usize,
    },
    AlreadyUnsealed,
}

/// Locate `sealed class <name> { ... }`. Returns the byte ranges
/// needed to compose the edits (remove sealed; bound body for
/// virtualize searches). The scanner walks tokens at top level only —
/// nested `sealed class` declarations inside other namespaces are
/// found via the same routine. Refuses on multiple matches with the
/// same simple name.
fn locate_sealed_class(source: &str, class_name: &str) -> Result<ClassMatch> {
    let bytes = source.as_bytes();
    let mut sealed_match: Option<(usize, usize, usize, usize)> = None;
    let mut unsealed_match_present = false;
    let mut i = 0usize;
    while i < bytes.len() {
        // Skip strings/chars/comments so braces inside literals don't
        // confuse depth tracking inside the class body bounds.
        if let Some(next) = skip_lex_atom(bytes, i) {
            i = next;
            continue;
        }
        if !is_word_boundary(bytes, i) {
            i += 1;
            continue;
        }
        if let Some(after_class) = match_keyword(bytes, i, b"class") {
            // Look back over the modifier tokens preceding `class` on
            // the same logical statement (within ~256 bytes) to find
            // `sealed`, `partial`, etc. and to find the class name
            // forward of the `class` keyword.
            let modifier_lookback = lookback_modifiers(bytes, i);
            let name_start = skip_whitespace(bytes, after_class);
            let (parsed_name, name_end) = read_ident(bytes, name_start);
            if parsed_name == class_name {
                let body_start = find_class_body_open(bytes, name_end);
                if let Some(body_start) = body_start
                    && let Some(body_end) = find_matching_close_brace(bytes, body_start)
                {
                    if let Some(sealed_span) = modifier_lookback.sealed_span {
                        if sealed_match.is_some() {
                            bail!(
                                "error.ambiguous_class_match: multiple `sealed class {class_name}` declarations in the same file"
                            );
                        }
                        sealed_match = Some((sealed_span.0, sealed_span.1, body_start, body_end));
                    } else {
                        unsealed_match_present = true;
                    }
                }
            }
            i = after_class;
            continue;
        }
        i += 1;
    }
    match sealed_match {
        Some((sealed_start, sealed_end, body_start, body_end)) => Ok(ClassMatch::Sealed {
            sealed_start,
            sealed_end,
            body_start,
            body_end,
        }),
        None if unsealed_match_present => Ok(ClassMatch::AlreadyUnsealed),
        None => bail!("error.class_not_found: `{class_name}` not found as a class declaration"),
    }
}

#[derive(Debug, Default)]
struct ModifierLookback {
    /// (start, end) byte range of the `sealed ` token + trailing
    /// whitespace, suitable for direct deletion.
    sealed_span: Option<(usize, usize)>,
}

/// Walk backwards from the position of the `class` keyword, scanning
/// over modifier tokens (`public`, `internal`, `sealed`, `partial`,
/// `static`, `abstract`, etc.) until we hit a separator. Returns the
/// `sealed` span if present.
fn lookback_modifiers(bytes: &[u8], class_kw_start: usize) -> ModifierLookback {
    let max_back = class_kw_start.saturating_sub(256);
    let region = &bytes[max_back..class_kw_start];
    let region_text = std::str::from_utf8(region).unwrap_or("");
    let region_offset = max_back;
    let mut sealed_span: Option<(usize, usize)> = None;
    // Tokens we treat as modifiers. We scan token positions in the
    // region and check each against the set.
    let modifier_set = [
        "public",
        "internal",
        "private",
        "protected",
        "sealed",
        "partial",
        "static",
        "abstract",
        "virtual",
        "override",
        "unsafe",
        "new",
    ];
    let mut pos = 0usize;
    while pos < region_text.len() {
        let bytes = region_text.as_bytes();
        if bytes[pos].is_ascii_whitespace() {
            pos += 1;
            continue;
        }
        let (token, end) = read_ident(bytes, pos);
        if token.is_empty() {
            pos += 1;
            continue;
        }
        if modifier_set.contains(&token.as_str()) {
            if token == "sealed" {
                // Include trailing whitespace so the deletion is clean
                // (otherwise we leave a double space).
                let mut e = end;
                while e < bytes.len() && bytes[e].is_ascii_whitespace() {
                    e += 1;
                }
                sealed_span = Some((region_offset + pos, region_offset + e));
            }
            pos = end;
        } else {
            // Non-modifier token in modifier position — we've gone past
            // the modifier prefix.
            break;
        }
    }
    ModifierLookback { sealed_span }
}

fn find_class_body_open(bytes: &[u8], from: usize) -> Option<usize> {
    let mut i = from;
    while i < bytes.len() {
        if let Some(next) = skip_lex_atom(bytes, i) {
            i = next;
            continue;
        }
        match bytes[i] {
            b'{' => return Some(i),
            b';' => return None, // type alias / forward decl
            _ => i += 1,
        }
    }
    None
}

/// Within `class_body_open..class_body_close`, find `<modifier>* (void|<type>) <method_name>(...)`
/// and emit an edit inserting `virtual ` if not already virtual/override.
fn locate_method_for_virtualize(
    source: &str,
    method_name: &str,
    body_start: usize,
    body_end: usize,
) -> Result<Option<TextEdit>> {
    let bytes = source.as_bytes();
    let region = &bytes[body_start + 1..body_end];
    let region_offset = body_start + 1;
    let mut i = 0usize;
    while i < region.len() {
        if let Some(next_rel) = skip_lex_atom(region, i) {
            i = next_rel;
            continue;
        }
        if !is_word_boundary(region, i) {
            i += 1;
            continue;
        }
        let (token, end) = read_ident(region, i);
        if token == method_name {
            // Confirm followed by optional generic-args then `(`:
            let after = skip_whitespace(region, end);
            let after_generics = if region.get(after) == Some(&b'<') {
                skip_balanced(region, after, b'<', b'>')
            } else {
                Some(after)
            };
            let Some(after_generics) = after_generics else {
                i += 1;
                continue;
            };
            let after_ws = skip_whitespace(region, after_generics);
            if region.get(after_ws) != Some(&b'(') {
                i += 1;
                continue;
            }
            // Walk backwards within the same line / statement to find
            // the modifiers + return type. Then check if `virtual` or
            // `override` is already there.
            let stmt_start = find_statement_start(region, i);
            let head_region = &region[stmt_start..i];
            let head_text = std::str::from_utf8(head_region).unwrap_or("");
            if head_text.contains("virtual ") || head_text.contains("override ") {
                return Ok(None); // already virtual / override — idempotent
            }
            // Find the first modifier token (public/internal/private/protected/static/etc)
            // or the start of the return type, and insert `virtual ` before the return type.
            let insert_byte = find_virtual_insert_point(region, stmt_start, i);
            return Ok(Some(TextEdit {
                byte_start: region_offset + insert_byte,
                byte_end: region_offset + insert_byte,
                replacement: "virtual ".to_string(),
            }));
        }
        i = end.max(i + 1);
    }
    Ok(None)
}

/// Walk backwards from `cursor` (which points at the method-name
/// ident) until we hit a `;`, `}`, `{`, or attribute `]`. Return the
/// next byte after that delimiter — the start of the statement
/// "head" (modifiers + return type + name).
fn find_statement_start(region: &[u8], cursor: usize) -> usize {
    if cursor == 0 {
        return 0;
    }
    let mut i = cursor;
    while i > 0 {
        let b = region[i - 1];
        if b == b';' || b == b'{' || b == b'}' || b == b']' {
            // Skip the delimiter itself plus any whitespace after it.
            let mut start = i;
            while start < region.len() && region[start].is_ascii_whitespace() {
                start += 1;
            }
            return start;
        }
        i -= 1;
    }
    // Reached start of region with no delimiter — head begins at 0.
    let mut start = 0;
    while start < region.len() && region[start].is_ascii_whitespace() {
        start += 1;
    }
    start
}

/// Determine where to insert `virtual ` within the method-head bytes
/// [stmt_start..method_name_start). Strategy: scan modifier tokens
/// from stmt_start; insert immediately after the last access modifier
/// (public/internal/private/protected). If no access modifier, insert
/// at stmt_start.
fn find_virtual_insert_point(region: &[u8], stmt_start: usize, name_start: usize) -> usize {
    let head = &region[stmt_start..name_start];
    let head_text = std::str::from_utf8(head).unwrap_or("");
    let access_set = ["public", "internal", "private", "protected"];
    let other_modifiers = [
        "static", "new", "unsafe", "extern", "async", "sealed", "partial",
    ];
    let mut pos = 0usize;
    let mut last_modifier_end_in_head: Option<usize> = None;
    while pos < head_text.len() {
        let b = head_text.as_bytes()[pos];
        if b.is_ascii_whitespace() {
            pos += 1;
            continue;
        }
        let (token, end) = read_ident(head_text.as_bytes(), pos);
        if token.is_empty() {
            break;
        }
        if access_set.contains(&token.as_str()) || other_modifiers.contains(&token.as_str()) {
            last_modifier_end_in_head = Some(end);
            pos = end;
        } else {
            // First non-modifier token = the return type. Stop here.
            break;
        }
    }
    match last_modifier_end_in_head {
        Some(rel_end) => {
            // Skip whitespace after the last modifier so the insertion
            // sits at the start of the return type.
            let bytes = region;
            let mut idx = stmt_start + rel_end;
            while idx < bytes.len() && bytes[idx].is_ascii_whitespace() {
                idx += 1;
            }
            idx
        }
        None => stmt_start,
    }
}

// Lex helpers live in `super::lex`. Imported at top of file.
use super::lex::{
    find_matching_close_brace, is_ident_char, is_word_boundary, match_keyword, read_ident,
    skip_balanced, skip_lex_atom, skip_whitespace,
};

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn p_with(source_path: &Path, class: &str, ack: bool, virt: &[&str]) -> RefactorPlanParams {
        let mut entries = BTreeMap::new();
        if ack {
            entries.insert(
                "acknowledge_subclass_surface_change".to_string(),
                serde_json::Value::Bool(true),
            );
        }
        RefactorPlanParams {
            kind: "unseal_csharp_class".to_string(),
            source: source_path.to_string_lossy().to_string(),
            item_names: Some(vec![class.to_string()]),
            item_kinds: if virt.is_empty() {
                None
            } else {
                Some(virt.iter().map(|s| s.to_string()).collect())
            },
            toml_entries: Some(entries),
            ..Default::default()
        }
    }

    fn apply(source: &str, edits: &[TextEdit]) -> String {
        let mut text = source.to_string();
        let mut sorted: Vec<&TextEdit> = edits.iter().collect();
        sorted.sort_by(|a, b| b.byte_start.cmp(&a.byte_start));
        for te in sorted {
            text.replace_range(te.byte_start..te.byte_end, &te.replacement);
        }
        text
    }

    #[test]
    fn refuses_without_acknowledge_flag() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("Foo.cs");
        std::fs::write(&path, "public sealed class Foo { }\n").unwrap();
        let p = p_with(&path, "Foo", false, &[]);
        let err = plan_unseal(&p).unwrap_err();
        assert!(err.to_string().contains("operator_authority_required"));
    }

    #[test]
    fn removes_sealed_modifier_when_acknowledged() {
        let src = "public sealed class Foo {\n    void Bar() {}\n}\n";
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("Foo.cs");
        std::fs::write(&path, src).unwrap();
        let p = p_with(&path, "Foo", true, &[]);
        let json = plan_unseal(&p).unwrap();
        let plan: serde_json::Value = serde_json::from_str(&json).unwrap();
        let edits = &plan["edits"][0]["edits"];
        let text_edits: Vec<TextEdit> = serde_json::from_value(edits.clone()).unwrap();
        let out = apply(src, &text_edits);
        assert!(out.starts_with("public class Foo {"), "{out}");
    }

    #[test]
    fn idempotent_on_non_sealed_class() {
        let src = "public class Foo { }\n";
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("Foo.cs");
        std::fs::write(&path, src).unwrap();
        let p = p_with(&path, "Foo", true, &[]);
        let json = plan_unseal(&p).unwrap();
        let plan: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert!(
            plan["edits"].as_array().unwrap().is_empty(),
            "expected no edits, got {plan:?}"
        );
    }

    #[test]
    fn virtualizes_named_method() {
        let src =
            "public sealed class Foo {\n    public void Bar() {}\n    private int Baz() => 1;\n}\n";
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("Foo.cs");
        std::fs::write(&path, src).unwrap();
        let p = p_with(&path, "Foo", true, &["Bar"]);
        let json = plan_unseal(&p).unwrap();
        let plan: serde_json::Value = serde_json::from_str(&json).unwrap();
        let text_edits: Vec<TextEdit> =
            serde_json::from_value(plan["edits"][0]["edits"].clone()).unwrap();
        let out = apply(src, &text_edits);
        assert!(out.starts_with("public class Foo {"), "{out}");
        assert!(out.contains("public virtual void Bar()"), "{out}");
        assert!(!out.contains("private virtual int Baz()"), "{out}");
    }

    #[test]
    fn skip_virtualize_when_already_virtual() {
        let src = "public sealed class Foo {\n    public virtual void Bar() {}\n}\n";
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("Foo.cs");
        std::fs::write(&path, src).unwrap();
        let p = p_with(&path, "Foo", true, &["Bar"]);
        let json = plan_unseal(&p).unwrap();
        let plan: serde_json::Value = serde_json::from_str(&json).unwrap();
        let text_edits: Vec<TextEdit> =
            serde_json::from_value(plan["edits"][0]["edits"].clone()).unwrap();
        // Only the sealed removal — no double-virtual.
        assert_eq!(text_edits.len(), 1, "{text_edits:?}");
    }

    #[test]
    fn refuses_generated_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("Foo.g.cs");
        std::fs::write(&path, "public sealed class Foo { }\n").unwrap();
        let p = p_with(&path, "Foo", true, &[]);
        let err = plan_unseal(&p).unwrap_err();
        assert!(err.to_string().contains("generated_file_refusal"));
    }

    #[test]
    fn refuses_class_not_found() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("Foo.cs");
        std::fs::write(&path, "public class Bar { }\n").unwrap();
        let p = p_with(&path, "Foo", true, &[]);
        let err = plan_unseal(&p).unwrap_err();
        assert!(err.to_string().contains("class_not_found"));
    }
}
