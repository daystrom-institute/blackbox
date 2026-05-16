//! `move_csharp_type_to_file` — extract a single top-level type
//! into a new file.
//!
//! Inputs:
//!   - `source` = .cs containing the type
//!   - `target` = new .cs path (must not exist)
//!   - `item_names[0]` = type simple name (class / record / struct /
//!     interface / enum / delegate)
//!
//! The plan produces two edits:
//!   1. `source`: delete the type declaration block (with trivia).
//!   2. `target`: create with the namespace/using prelude from the
//!      source plus the moved type.
//!
//! v1 limits (`indexed_hints`):
//!   - Single top-level type per move. Source file may have other
//!     types; only the named one moves.
//!   - Namespace handling: copies the source's namespace block /
//!     file-scoped namespace declaration verbatim. If the source
//!     file is split across multiple namespaces, refuses.
//!   - Using directives: copies every `using` from the top of the
//!     source.
//!   - Refuses on partial classes (Phase 2 handles via
//!     partial-class audit).
//!
//! Refusal cases:
//!   - target already exists with non-empty content
//!   - source has the type marked `partial`
//!   - source's namespace topology is ambiguous

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use sha2::{Digest, Sha256};

use super::lex::{
    find_matching_close_brace, is_word_boundary, match_keyword, read_ident, skip_lex_atom,
    skip_whitespace,
};
use crate::refactor::{
    FileEdit, RefactorPlanParams, SemanticStatus, TextEdit, ValidationStep,
    csharp::empty_plan,
};

pub fn plan_move_type_to_file(p: &RefactorPlanParams) -> Result<String> {
    let source_path = resolve_path(p.project_dir.as_deref(), &p.source)?;
    refuse_generated_file(&source_path)?;
    let target_path = p
        .target
        .as_deref()
        .map(|t| resolve_path(p.project_dir.as_deref(), t))
        .transpose()?
        .ok_or_else(|| anyhow!("target is required for move_csharp_type_to_file"))?;
    if source_path == target_path {
        bail!("move_csharp_type_to_file requires source != target");
    }
    let type_name = p
        .item_names
        .as_deref()
        .and_then(|names| names.first())
        .map(String::as_str)
        .ok_or_else(|| {
            anyhow!("item_names[0] (target type) is required for move_csharp_type_to_file")
        })?;

    let source = fs::read_to_string(&source_path)
        .with_context(|| format!("reading {}", source_path.display()))?;

    if target_path.exists() {
        let existing = fs::read_to_string(&target_path).unwrap_or_default();
        if !existing.trim().is_empty() {
            bail!(
                "error.target_exists: `{}` already exists with content; refuse to overwrite",
                target_path.display()
            );
        }
    }

    let type_loc = locate_top_level_type(&source, type_name)?;
    if type_loc.is_partial {
        bail!(
            "error.partial_type_unsupported: type `{type_name}` is `partial`; move requires Phase 2 partial-class audit"
        );
    }
    let prelude = extract_prelude(&source, type_loc.namespace_body_start)?;

    let type_text = &source[type_loc.head_start..type_loc.body_end + 1];

    // Build the target file content.
    let target_content = render_target_file(&prelude, type_text);

    // Build the source-side delete edit. We also strip any trailing
    // blank line so the deletion doesn't leave a wide gap.
    let mut delete_start = type_loc.head_start;
    let bytes = source.as_bytes();
    while delete_start > 0 {
        let b = bytes[delete_start - 1];
        if b == b'\n' {
            // Take one preceding newline so the file body collapses
            // cleanly.
            delete_start -= 1;
            break;
        }
        if b == b' ' || b == b'\t' {
            delete_start -= 1;
            continue;
        }
        break;
    }
    let mut delete_end = type_loc.body_end + 1;
    while delete_end < bytes.len() && (bytes[delete_end] == b' ' || bytes[delete_end] == b'\t') {
        delete_end += 1;
    }
    if delete_end < bytes.len() && bytes[delete_end] == b'\n' {
        delete_end += 1;
    }

    let mut hasher = Sha256::new();
    hasher.update(source.as_bytes());
    let src_sha = format!("{:x}", hasher.finalize());

    let source_edit = FileEdit {
        path: path_string(&source_path),
        original_sha256: src_sha,
        edits: vec![TextEdit {
            byte_start: delete_start,
            byte_end: delete_end,
            replacement: String::new(),
        }],
        new_text: None,
    };

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
            replacement: target_content,
        }],
        new_text: None,
    };

    let mut plan = empty_plan(
        "move_csharp_type_to_file",
        format!(
            "move `{type_name}` from {} to {}",
            path_string(&source_path),
            path_string(&target_path)
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
    if lower.contains("/generated/")
        || lower.ends_with(".g.cs")
        || lower.ends_with(".designer.cs")
    {
        bail!("error.generated_file_refusal: `{}`", path.display());
    }
    Ok(())
}

#[derive(Debug)]
struct TypeLoc {
    head_start: usize,
    body_end: usize,
    is_partial: bool,
    /// Byte position of the `{` opening the enclosing namespace
    /// body, when the source uses block-scoped namespaces.
    /// `None` when file-scoped (`namespace Foo;`).
    namespace_body_start: Option<usize>,
}

const TYPE_KEYWORDS: &[&[u8]] = &[
    b"class",
    b"record",
    b"struct",
    b"interface",
    b"enum",
    b"delegate",
];

fn locate_top_level_type(source: &str, type_name: &str) -> Result<TypeLoc> {
    let bytes = source.as_bytes();
    let mut namespace_brace_open: Option<usize> = None;
    let mut found_namespace_count = 0;
    // First scan for namespace declarations (block-scoped) to set
    // up the parent body bracket; refuse on multi-namespace.
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
            found_namespace_count += 1;
            // Walk to `;` (file-scoped) or `{` (block-scoped).
            let mut j = after_ns;
            while j < bytes.len() && bytes[j] != b';' && bytes[j] != b'{' {
                j += 1;
            }
            if j < bytes.len() && bytes[j] == b'{' && namespace_brace_open.is_none() {
                namespace_brace_open = Some(j);
            }
            i = j + 1;
            continue;
        }
        i += 1;
    }
    if found_namespace_count > 1 {
        bail!(
            "error.multi_namespace_source: source contains {found_namespace_count} namespace declarations; move requires a single-namespace file"
        );
    }

    // Now scan for the type declaration. We only consider declarations
    // at the immediate top level of the file (or of the single
    // namespace block).
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
        let mut matched_kw = false;
        for kw in TYPE_KEYWORDS {
            if let Some(after_kw) = match_keyword(bytes, i, kw) {
                let name_start = skip_whitespace(bytes, after_kw);
                let (parsed, _name_end) = read_ident(bytes, name_start);
                if parsed != type_name {
                    matched_kw = true;
                    i = after_kw;
                    break;
                }
                let max_back = i.saturating_sub(256);
                let prefix = std::str::from_utf8(&bytes[max_back..i]).unwrap_or("");
                let is_partial = prefix
                    .split_whitespace()
                    .any(|tok| tok == "partial");
                let head_start = find_decl_head_start(bytes, i, max_back);
                // For delegates (single-line), the "body" is up to the `;`.
                if *kw == b"delegate" {
                    let mut j = name_start;
                    while j < bytes.len() && bytes[j] != b';' {
                        j += 1;
                    }
                    if j >= bytes.len() {
                        bail!("error.delegate_terminator_missing");
                    }
                    return Ok(TypeLoc {
                        head_start,
                        body_end: j,
                        is_partial,
                        namespace_body_start: namespace_brace_open,
                    });
                }
                // Otherwise find `{ ... }`.
                let mut j = name_start;
                while j < bytes.len() && bytes[j] != b'{' && bytes[j] != b';' {
                    j += 1;
                }
                if j >= bytes.len() || bytes[j] == b';' {
                    bail!("error.type_body_not_found");
                }
                let body_end = find_matching_close_brace(bytes, j)
                    .ok_or_else(|| anyhow!("error.unbalanced_type_braces"))?;
                return Ok(TypeLoc {
                    head_start,
                    body_end,
                    is_partial,
                    namespace_body_start: namespace_brace_open,
                });
            }
        }
        if matched_kw {
            continue;
        }
        i += 1;
    }
    bail!("error.type_not_found: `{type_name}` not found as a top-level type")
}

fn find_decl_head_start(bytes: &[u8], cursor: usize, floor: usize) -> usize {
    let mut i = cursor;
    while i > floor {
        let b = bytes[i - 1];
        if b == b'\n' || b == b';' || b == b'}' || b == b'{' {
            // Skip whitespace forward from i.
            let mut start = i;
            while start < bytes.len() && bytes[start].is_ascii_whitespace() {
                start += 1;
            }
            return start;
        }
        i -= 1;
    }
    let mut start = floor;
    while start < bytes.len() && bytes[start].is_ascii_whitespace() {
        start += 1;
    }
    start
}

#[derive(Debug)]
struct Prelude {
    /// All `using` directives, including their trailing newline.
    usings_text: String,
    /// The `namespace Foo {` opener (block-scoped) or
    /// `namespace Foo;` declaration (file-scoped), or `None` when
    /// the source has no namespace.
    namespace_decl: Option<NamespaceForm>,
}

#[derive(Debug)]
enum NamespaceForm {
    FileScoped(String),
    BlockScoped(String),
}

fn extract_prelude(source: &str, namespace_body_start: Option<usize>) -> Result<Prelude> {
    let bytes = source.as_bytes();
    let mut usings = String::new();
    // Walk lines from start, collecting `using` statements; stop on
    // the first non-using non-blank line.
    let mut i = 0usize;
    while i < bytes.len() {
        let line_start = i;
        while i < bytes.len() && bytes[i] != b'\n' {
            i += 1;
        }
        let line_end = i;
        if i < bytes.len() {
            i += 1; // consume newline
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
    let namespace_decl = locate_namespace_decl(source, namespace_body_start)?;
    Ok(Prelude {
        usings_text: usings,
        namespace_decl,
    })
}

fn locate_namespace_decl(
    source: &str,
    namespace_body_start: Option<usize>,
) -> Result<Option<NamespaceForm>> {
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
                bail!("error.namespace_not_terminated");
            }
            let decl_text = std::str::from_utf8(&bytes[i..=j]).unwrap_or("").to_string();
            let form = if bytes[j] == b';' {
                NamespaceForm::FileScoped(decl_text)
            } else {
                let _ = namespace_body_start; // present means block-scoped
                NamespaceForm::BlockScoped(decl_text)
            };
            return Ok(Some(form));
        }
        i += 1;
    }
    Ok(None)
}

fn render_target_file(prelude: &Prelude, type_text: &str) -> String {
    let mut out = String::new();
    if !prelude.usings_text.is_empty() {
        out.push_str(&prelude.usings_text);
        if !prelude.usings_text.ends_with('\n') {
            out.push('\n');
        }
    }
    match &prelude.namespace_decl {
        Some(NamespaceForm::FileScoped(decl)) => {
            out.push_str(decl);
            out.push('\n');
            out.push('\n');
            out.push_str(type_text);
            if !type_text.ends_with('\n') {
                out.push('\n');
            }
        }
        Some(NamespaceForm::BlockScoped(opener_with_brace)) => {
            // Strip the trailing `{`, replace with `;` if we're
            // producing a fresh file we may as well file-scope it.
            // Conservative: keep block-scoped to match source layout.
            out.push_str(opener_with_brace);
            out.push('\n');
            out.push_str(type_text);
            if !type_text.ends_with('\n') {
                out.push('\n');
            }
            out.push_str("}\n");
        }
        None => {
            out.push_str(type_text);
            if !type_text.ends_with('\n') {
                out.push('\n');
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(src: &Path, tgt: &Path, name: &str) -> RefactorPlanParams {
        RefactorPlanParams {
            kind: "move_csharp_type_to_file".to_string(),
            source: src.to_string_lossy().to_string(),
            target: Some(tgt.to_string_lossy().to_string()),
            item_names: Some(vec![name.to_string()]),
            ..Default::default()
        }
    }

    #[test]
    fn moves_filescoped_namespaced_class_to_new_file() {
        let src = "using System;\n\nnamespace Foo.Bar;\n\npublic class A { }\n\npublic class B { }\n";
        let dir = tempfile::tempdir().unwrap();
        let s = dir.path().join("AB.cs");
        let t = dir.path().join("A.cs");
        std::fs::write(&s, src).unwrap();
        let json = plan_move_type_to_file(&p(&s, &t, "A")).unwrap();
        let plan: serde_json::Value = serde_json::from_str(&json).unwrap();
        let edits = plan["edits"].as_array().unwrap();
        assert_eq!(edits.len(), 2);
        let source_edit = &edits[0];
        let target_edit = &edits[1];
        let mut source_after = src.to_string();
        let te: Vec<TextEdit> =
            serde_json::from_value(source_edit["edits"].clone()).unwrap();
        for e in te.iter().rev() {
            source_after.replace_range(e.byte_start..e.byte_end, &e.replacement);
        }
        assert!(!source_after.contains("public class A"), "source still has A: {source_after}");
        assert!(source_after.contains("public class B"), "source lost B: {source_after}");
        let target_text: Vec<TextEdit> =
            serde_json::from_value(target_edit["edits"].clone()).unwrap();
        let target_body = &target_text[0].replacement;
        assert!(target_body.contains("namespace Foo.Bar;"), "{target_body}");
        assert!(target_body.contains("public class A"), "{target_body}");
        assert!(target_body.contains("using System;"), "{target_body}");
    }

    #[test]
    fn refuses_when_target_already_exists() {
        let src = "namespace Foo; public class A {}\n";
        let dir = tempfile::tempdir().unwrap();
        let s = dir.path().join("Src.cs");
        let t = dir.path().join("Dst.cs");
        std::fs::write(&s, src).unwrap();
        std::fs::write(&t, "existing content\n").unwrap();
        let err = plan_move_type_to_file(&p(&s, &t, "A")).unwrap_err();
        assert!(err.to_string().contains("target_exists"));
    }

    #[test]
    fn refuses_partial_class() {
        let src = "namespace Foo; public partial class A {}\n";
        let dir = tempfile::tempdir().unwrap();
        let s = dir.path().join("Src.cs");
        let t = dir.path().join("Dst.cs");
        std::fs::write(&s, src).unwrap();
        let err = plan_move_type_to_file(&p(&s, &t, "A")).unwrap_err();
        assert!(err.to_string().contains("partial_type_unsupported"));
    }

    #[test]
    fn refuses_multi_namespace_source() {
        let src = "namespace Foo { public class A {} } namespace Bar { public class B {} }\n";
        let dir = tempfile::tempdir().unwrap();
        let s = dir.path().join("Multi.cs");
        let t = dir.path().join("A.cs");
        std::fs::write(&s, src).unwrap();
        let err = plan_move_type_to_file(&p(&s, &t, "A")).unwrap_err();
        assert!(err.to_string().contains("multi_namespace_source"));
    }
}
