//! `move_csharp_members_to_partial` — split a class body across two
//! `partial class` files.
//!
//! Inputs:
//!   - `source` = file containing the class
//!   - `target` = new file path (must not exist)
//!   - `item_names[0]` = class simple name
//!   - `item_names[1..]` = member simple names to relocate
//!
//! Output: two-file plan:
//!   1. source: insert `partial ` modifier on the class declaration,
//!      delete the named members (and their trivia).
//!   2. target: create with the namespace prelude + a sibling
//!      `partial class Foo { <moved-members> }`.
//!
//! Precondition: caller should run `csharp_partial_class_audit`
//! first (RX-V4). This kind does not verify generator-binding;
//! the audit + manifest does.
//!
//! v1 limits (`indexed_hints`):
//!   - Single-namespace source.
//!   - Members must be unique by simple name (no method overload
//!     disambiguation in v1).
//!   - Generated-file guard.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use sha2::{Digest, Sha256};

use super::lex::{
    find_matching_close_brace, is_word_boundary, match_keyword, read_ident, skip_lex_atom,
    skip_whitespace,
};
use crate::refactor::{
    FileEdit, RefactorPlanParams, SemanticStatus, TextEdit, ValidationStep, csharp::empty_plan,
};

pub fn plan_move_members_to_partial(p: &RefactorPlanParams) -> Result<String> {
    let source_path = resolve_path(p.project_dir.as_deref(), &p.source)?;
    refuse_generated_file(&source_path)?;
    let target_path = p
        .target
        .as_deref()
        .map(|t| resolve_path(p.project_dir.as_deref(), t))
        .transpose()?
        .ok_or_else(|| anyhow!("target is required for move_csharp_members_to_partial"))?;
    if source_path == target_path {
        bail!("source != target required");
    }
    let names = p
        .item_names
        .as_deref()
        .filter(|v| v.len() >= 2)
        .ok_or_else(|| anyhow!("item_names must contain class + at least one member name"))?;
    let class_name = names[0].as_str();
    let member_names: Vec<&str> = names[1..].iter().map(String::as_str).collect();

    if target_path.exists() {
        let existing = fs::read_to_string(&target_path).unwrap_or_default();
        if !existing.trim().is_empty() {
            bail!(
                "error.target_exists: `{}` is non-empty",
                target_path.display()
            );
        }
    }

    let source = fs::read_to_string(&source_path)
        .with_context(|| format!("reading {}", source_path.display()))?;

    let class = locate_class(&source, class_name)?;
    if !class.is_partial {
        // The plan inserts the `partial` modifier as part of its
        // edit set, so the source side becomes partial after apply.
    }

    let mut member_ranges = Vec::new();
    for name in &member_names {
        let range = locate_member(&source, &class, name)?;
        member_ranges.push((name.to_string(), range));
    }
    if member_ranges.is_empty() {
        bail!("no member ranges resolved");
    }

    // Build the moved-member text block (preserve original indentation).
    let mut moved_block = String::new();
    for (_name, range) in &member_ranges {
        moved_block.push_str(&source[range.start..range.end]);
        if !moved_block.ends_with('\n') {
            moved_block.push('\n');
        }
        moved_block.push('\n');
    }

    // Source-side edits: insert `partial ` if not present + delete
    // each member range. Apply in reverse to preserve byte ranges.
    let mut source_edits: Vec<TextEdit> = Vec::new();
    if !class.is_partial {
        source_edits.push(TextEdit {
            byte_start: class.class_keyword_start,
            byte_end: class.class_keyword_start,
            replacement: "partial ".to_string(),
        });
    }
    for (_name, range) in &member_ranges {
        // Expand to include trailing newline.
        let bytes = source.as_bytes();
        let mut end = range.end;
        while end < bytes.len() && (bytes[end] == b' ' || bytes[end] == b'\t') {
            end += 1;
        }
        if end < bytes.len() && bytes[end] == b'\n' {
            end += 1;
        }
        source_edits.push(TextEdit {
            byte_start: range.start,
            byte_end: end,
            replacement: String::new(),
        });
    }

    let prelude = extract_prelude(&source, class.namespace_body_start)?;
    let target_text = render_target(class_name, &prelude, &moved_block);
    let target_sha = format!("{:x}", {
        let mut h = Sha256::new();
        h.update("".as_bytes());
        h.finalize()
    });
    let target_edit = FileEdit {
        path: path_string(&target_path),
        original_sha256: target_sha,
        edits: vec![TextEdit {
            byte_start: 0,
            byte_end: 0,
            replacement: target_text,
        }],
        new_text: None,
    };
    let mut hasher = Sha256::new();
    hasher.update(source.as_bytes());
    let source_sha = format!("{:x}", hasher.finalize());
    let source_edit = FileEdit {
        path: path_string(&source_path),
        original_sha256: source_sha,
        edits: source_edits,
        new_text: None,
    };

    let mut plan = empty_plan(
        "move_csharp_members_to_partial",
        format!(
            "move {} member(s) from `{class_name}` to partial sibling file",
            member_names.len()
        ),
        SemanticStatus::IndexedHints,
    );
    plan.validations.push(ValidationStep::TreeSitterNoErrors {
        path: path_string(&source_path),
        byte_range: None,
    });
    plan.validations.push(ValidationStep::TreeSitterNoErrors {
        path: path_string(&target_path),
        byte_range: None,
    });
    plan.edits = vec![source_edit, target_edit];
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

fn refuse_generated_file(path: &Path) -> Result<()> {
    let lower = path.to_str().unwrap_or("").to_ascii_lowercase();
    if lower.contains("/generated/") || lower.ends_with(".g.cs") || lower.ends_with(".designer.cs")
    {
        bail!("error.generated_file_refusal");
    }
    Ok(())
}

#[derive(Debug)]
struct ClassLoc {
    class_keyword_start: usize,
    body_start: usize,
    body_end: usize,
    is_partial: bool,
    namespace_body_start: Option<usize>,
}

fn locate_class(source: &str, class_name: &str) -> Result<ClassLoc> {
    let bytes = source.as_bytes();
    let mut namespace_body_start = None;
    let mut i = 0usize;
    // First pass: namespace block-scoped opener.
    while i < bytes.len() {
        if let Some(next) = skip_lex_atom(bytes, i) {
            i = next;
            continue;
        }
        if !is_word_boundary(bytes, i) {
            i += 1;
            continue;
        }
        if let Some(after_ns) = match_keyword(bytes, i, b"namespace") {
            let mut j = after_ns;
            while j < bytes.len() && bytes[j] != b';' && bytes[j] != b'{' {
                j += 1;
            }
            if j < bytes.len() && bytes[j] == b'{' && namespace_body_start.is_none() {
                namespace_body_start = Some(j);
            }
            i = j + 1;
            continue;
        }
        i += 1;
    }

    // Second pass: class declaration.
    i = 0;
    while i < bytes.len() {
        if let Some(next) = skip_lex_atom(bytes, i) {
            i = next;
            continue;
        }
        if !is_word_boundary(bytes, i) {
            i += 1;
            continue;
        }
        if let Some(after_class) = match_keyword(bytes, i, b"class") {
            let name_start = skip_whitespace(bytes, after_class);
            let (parsed, _) = read_ident(bytes, name_start);
            if parsed == class_name {
                let max_back = i.saturating_sub(256);
                let prefix = std::str::from_utf8(&bytes[max_back..i]).unwrap_or("");
                let is_partial = prefix.split_whitespace().any(|tok| tok == "partial");
                let mut j = name_start + parsed.len();
                while j < bytes.len() && bytes[j] != b'{' {
                    j += 1;
                }
                let body_start = j;
                let body_end = find_matching_close_brace(bytes, body_start)
                    .ok_or_else(|| anyhow!("unbalanced class braces"))?;
                return Ok(ClassLoc {
                    class_keyword_start: i,
                    body_start,
                    body_end,
                    is_partial,
                    namespace_body_start,
                });
            }
            i = after_class;
            continue;
        }
        i += 1;
    }
    bail!("error.class_not_found: `{class_name}`")
}

#[derive(Debug)]
struct MemberRange {
    start: usize,
    end: usize,
}

fn locate_member(source: &str, class: &ClassLoc, member_name: &str) -> Result<MemberRange> {
    let bytes = source.as_bytes();
    let body_start = class.body_start + 1;
    let body_end = class.body_end;
    let mut i = body_start;
    while i < body_end {
        if let Some(next) = skip_lex_atom(bytes, i) {
            i = next;
            continue;
        }
        if !is_word_boundary(bytes, i) {
            i += 1;
            continue;
        }
        let (token, end) = read_ident(bytes, i);
        if token == member_name {
            let after = skip_whitespace(bytes, end);
            // Method: `(`. Property: `{`. Field: `;` or `=`.
            match bytes.get(after) {
                Some(b'(') => {
                    // Walk to matching `)` then to `{` or `;`.
                    let mut depth = 1i32;
                    let mut k = after + 1;
                    while k < body_end && depth > 0 {
                        match bytes[k] {
                            b'(' => depth += 1,
                            b')' => depth -= 1,
                            _ => {}
                        }
                        k += 1;
                    }
                    // Skip `where T : ...` if present.
                    let after_params = skip_whitespace(bytes, k);
                    let body_or_semi = after_params;
                    let end_byte = match bytes.get(body_or_semi) {
                        Some(b'{') => find_matching_close_brace(bytes, body_or_semi)
                            .map(|c| c + 1)
                            .unwrap_or(body_end),
                        Some(b';') => body_or_semi + 1,
                        Some(b'=') => {
                            // Expression-bodied: `=> expr;`
                            let mut m = body_or_semi;
                            while m < body_end && bytes[m] != b';' {
                                m += 1;
                            }
                            (m + 1).min(body_end)
                        }
                        _ => return Err(anyhow!("error.member_body_ambiguous: {member_name}")),
                    };
                    let head_start = walk_member_head_start(bytes, i, body_start);
                    return Ok(MemberRange {
                        start: head_start,
                        end: end_byte,
                    });
                }
                Some(b'{') => {
                    let close = find_matching_close_brace(bytes, after)
                        .ok_or_else(|| anyhow!("unbalanced property braces for {member_name}"))?;
                    let head_start = walk_member_head_start(bytes, i, body_start);
                    return Ok(MemberRange {
                        start: head_start,
                        end: close + 1,
                    });
                }
                Some(b';') | Some(b'=') => {
                    let mut m = after;
                    while m < body_end && bytes[m] != b';' {
                        m += 1;
                    }
                    let head_start = walk_member_head_start(bytes, i, body_start);
                    return Ok(MemberRange {
                        start: head_start,
                        end: (m + 1).min(body_end),
                    });
                }
                _ => {
                    i = end.max(i + 1);
                    continue;
                }
            }
        }
        i = end.max(i + 1);
    }
    bail!("error.member_not_found: `{member_name}` in class body")
}

fn walk_member_head_start(bytes: &[u8], member_name_pos: usize, body_start: usize) -> usize {
    if member_name_pos <= body_start {
        return body_start;
    }
    let mut i = member_name_pos;
    while i > body_start {
        let b = bytes[i - 1];
        if b == b';' || b == b'{' || b == b'}' {
            let mut start = i;
            while start < bytes.len() && bytes[start].is_ascii_whitespace() {
                start += 1;
            }
            return start;
        }
        i -= 1;
    }
    body_start
}

#[derive(Debug)]
struct Prelude {
    usings_text: String,
    namespace_form: Option<NamespaceForm>,
}

#[derive(Debug)]
enum NamespaceForm {
    FileScoped(String),
    BlockScoped(String),
}

fn extract_prelude(source: &str, ns_body_start: Option<usize>) -> Result<Prelude> {
    let bytes = source.as_bytes();
    let mut usings = String::new();
    let mut i = 0usize;
    while i < bytes.len() {
        let line_start = i;
        while i < bytes.len() && bytes[i] != b'\n' {
            i += 1;
        }
        let line_end = i;
        if i < bytes.len() {
            i += 1;
        }
        let line = std::str::from_utf8(&bytes[line_start..line_end]).unwrap_or("");
        let trimmed = line.trim_start();
        if trimmed.is_empty() {
            usings.push_str(line);
            usings.push('\n');
            continue;
        }
        if trimmed.starts_with("using ") {
            usings.push_str(line);
            usings.push('\n');
            continue;
        }
        break;
    }
    let ns_form = locate_namespace(source, ns_body_start);
    Ok(Prelude {
        usings_text: usings,
        namespace_form: ns_form,
    })
}

fn locate_namespace(source: &str, _ns_body_start: Option<usize>) -> Option<NamespaceForm> {
    let bytes = source.as_bytes();
    let mut i = 0usize;
    while i < bytes.len() {
        if let Some(next) = skip_lex_atom(bytes, i) {
            i = next;
            continue;
        }
        if !is_word_boundary(bytes, i) {
            i += 1;
            continue;
        }
        if let Some(after_ns) = match_keyword(bytes, i, b"namespace") {
            let mut j = after_ns;
            while j < bytes.len() && bytes[j] != b';' && bytes[j] != b'{' {
                j += 1;
            }
            if j >= bytes.len() {
                return None;
            }
            let decl = std::str::from_utf8(&bytes[i..=j]).unwrap_or("").to_string();
            return Some(if bytes[j] == b';' {
                NamespaceForm::FileScoped(decl)
            } else {
                NamespaceForm::BlockScoped(decl)
            });
        }
        i += 1;
    }
    None
}

fn render_target(class_name: &str, prelude: &Prelude, moved_block: &str) -> String {
    let mut out = String::new();
    if !prelude.usings_text.is_empty() {
        out.push_str(&prelude.usings_text);
        if !prelude.usings_text.ends_with('\n') {
            out.push('\n');
        }
    }
    match &prelude.namespace_form {
        Some(NamespaceForm::FileScoped(decl)) => {
            out.push_str(decl);
            out.push('\n');
            out.push('\n');
            out.push_str(&format!("public partial class {class_name}\n{{\n"));
            out.push_str(moved_block);
            out.push_str("}\n");
        }
        Some(NamespaceForm::BlockScoped(decl)) => {
            out.push_str(decl);
            out.push('\n');
            out.push_str(&format!("    public partial class {class_name}\n    {{\n"));
            for line in moved_block.lines() {
                out.push_str("    ");
                out.push_str(line);
                out.push('\n');
            }
            out.push_str("    }\n}\n");
        }
        None => {
            out.push_str(&format!("public partial class {class_name}\n{{\n"));
            out.push_str(moved_block);
            out.push_str("}\n");
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(src: &Path, tgt: &Path, names: &[&str]) -> RefactorPlanParams {
        RefactorPlanParams {
            kind: "move_csharp_members_to_partial".to_string(),
            source: src.to_string_lossy().to_string(),
            target: Some(tgt.to_string_lossy().to_string()),
            item_names: Some(names.iter().map(|s| s.to_string()).collect()),
            ..Default::default()
        }
    }

    #[test]
    fn moves_named_method_to_partial_file() {
        let src = "namespace Foo;\n\npublic class Bar {\n    public int Keep() => 1;\n    public int Move() => 2;\n}\n";
        let dir = tempfile::tempdir().unwrap();
        let s = dir.path().join("Bar.cs");
        let t = dir.path().join("Bar.Moved.cs");
        std::fs::write(&s, src).unwrap();
        let json = plan_move_members_to_partial(&p(&s, &t, &["Bar", "Move"])).unwrap();
        let plan: serde_json::Value = serde_json::from_str(&json).unwrap();
        let edits = plan["edits"].as_array().unwrap();
        assert_eq!(edits.len(), 2);
        // Source side: partial inserted + Move() deleted.
        let source_edit_text: Vec<TextEdit> =
            serde_json::from_value(edits[0]["edits"].clone()).unwrap();
        let mut s_after = src.to_string();
        let mut sorted: Vec<&TextEdit> = source_edit_text.iter().collect();
        sorted.sort_by_key(|b| std::cmp::Reverse(b.byte_start));
        for e in sorted {
            s_after.replace_range(e.byte_start..e.byte_end, &e.replacement);
        }
        assert!(s_after.contains("public partial class Bar"), "{s_after}");
        assert!(!s_after.contains("public int Move()"), "{s_after}");
        assert!(s_after.contains("public int Keep()"), "{s_after}");
        // Target side: contains the moved method.
        let target_text: Vec<TextEdit> = serde_json::from_value(edits[1]["edits"].clone()).unwrap();
        let target_body = &target_text[0].replacement;
        assert!(target_body.contains("namespace Foo;"), "{target_body}");
        assert!(
            target_body.contains("public partial class Bar"),
            "{target_body}"
        );
        assert!(
            target_body.contains("public int Move() => 2"),
            "{target_body}"
        );
    }

    #[test]
    fn refuses_target_collision() {
        let src = "namespace Foo; public class Bar { public int X => 1; }\n";
        let dir = tempfile::tempdir().unwrap();
        let s = dir.path().join("Bar.cs");
        let t = dir.path().join("BarX.cs");
        std::fs::write(&s, src).unwrap();
        std::fs::write(&t, "existing").unwrap();
        let err = plan_move_members_to_partial(&p(&s, &t, &["Bar", "X"])).unwrap_err();
        assert!(err.to_string().contains("target_exists"));
    }
}
