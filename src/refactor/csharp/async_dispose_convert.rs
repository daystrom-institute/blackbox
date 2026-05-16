//! `csharp_async_dispose_convert` — IDisposable → IAsyncDisposable pattern.
//!
//! Finds a class that implements `IDisposable` and inserts the
//! standard async-dispose pattern alongside, so the operator can
//! progressively migrate callers:
//!
//! ```ignore
//! // before
//! public class Foo : IDisposable {
//!     public void Dispose() { /* sync teardown */ }
//! }
//!
//! // after
//! public class Foo : IDisposable, IAsyncDisposable {
//!     public void Dispose() { /* sync teardown */ }
//!     public async ValueTask DisposeAsync() {
//!         Dispose();
//!         await Task.CompletedTask;
//!     }
//! }
//! ```
//!
//! v1 implementation is **syntax_only**. The forwarding body is
//! deliberately conservative — the operator replaces the
//! `Task.CompletedTask` line with real async cleanup. Full
//! `lsp_verified` flavor (analyze field types for `IAsyncDisposable`
//! members; auto-emit `await field.DisposeAsync()`) waits for Phase 2.
//!
//! Refusal rules:
//!   - Class already declares `IAsyncDisposable`.
//!   - Class does not declare `IDisposable`.
//!   - Generated-file guard.
//!   - Class is partial (Phase 2: needs RX-V4 partial-class audit
//!     to know which file owns the interface list).

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

pub fn plan_async_dispose_convert(p: &RefactorPlanParams) -> Result<String> {
    let source_path = resolve_path(p.project_dir.as_deref(), &p.source)?;
    refuse_generated_file(&source_path)?;
    let class_name = p
        .item_names
        .as_deref()
        .and_then(|names| names.first())
        .map(String::as_str)
        .ok_or_else(|| {
            anyhow!("item_names[0] (target class) is required for csharp_async_dispose_convert")
        })?;

    let source = fs::read_to_string(&source_path)
        .with_context(|| format!("reading {}", source_path.display()))?;

    let class = locate_class(&source, class_name)?;
    if class.is_partial {
        bail!(
            "error.partial_class_unsupported: class `{class_name}` is partial; async-dispose conversion needs the Phase 2 partial-class audit to find the interface-list file"
        );
    }

    let inheritance = class
        .inheritance_clause
        .as_deref()
        .unwrap_or("")
        .trim_start_matches(':')
        .trim();
    let parts: Vec<&str> = inheritance.split(',').map(|s| s.trim()).collect();
    let has_disposable = parts.iter().any(|p| *p == "IDisposable");
    let has_async = parts.iter().any(|p| *p == "IAsyncDisposable");
    if !has_disposable {
        bail!(
            "error.no_idisposable: class `{class_name}` does not implement IDisposable; async-dispose conversion has no anchor"
        );
    }
    if has_async {
        bail!(
            "error.already_async_disposable: class `{class_name}` already declares IAsyncDisposable"
        );
    }

    let mut edits = Vec::new();
    // 1. Append `, IAsyncDisposable` to the inheritance clause.
    edits.push(TextEdit {
        byte_start: class.inheritance_clause_end,
        byte_end: class.inheritance_clause_end,
        replacement: ", IAsyncDisposable".to_string(),
    });
    // 2. Insert the DisposeAsync method just before the class body
    //    close brace. Body uses the canonical forwarding pattern.
    let body_indent = body_indent_string(&source, class.body_start);
    let method = format!(
        "\n{indent}public async global::System.Threading.Tasks.ValueTask DisposeAsync()\n\
         {indent}{{\n\
         {indent}    Dispose();\n\
         {indent}    await global::System.Threading.Tasks.Task.CompletedTask;\n\
         {indent}}}\n",
        indent = body_indent,
    );
    edits.push(TextEdit {
        byte_start: class.body_end,
        byte_end: class.body_end,
        replacement: method,
    });

    let mut hasher = Sha256::new();
    hasher.update(source.as_bytes());
    let sha = format!("{:x}", hasher.finalize());

    let mut plan = empty_plan(
        "csharp_async_dispose_convert",
        format!(
            "add IAsyncDisposable pattern to `{class_name}` in {}",
            path_string(&source_path)
        ),
        SemanticStatus::SyntaxOnly,
    );
    plan.validations.push(ValidationStep::TreeSitterNoErrors {
        path: path_string(&source_path),
        byte_range: None,
    });
    plan.edits = vec![FileEdit {
        path: path_string(&source_path),
        original_sha256: sha,
        edits,
        new_text: None,
    }];
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
        bail!("error.generated_file_refusal: `{}`", path.display());
    }
    Ok(())
}

#[derive(Debug)]
struct ClassLoc {
    body_start: usize,
    body_end: usize,
    inheritance_clause: Option<String>,
    /// Byte position of the last character of the inheritance clause
    /// (or, when there's no clause, the position right after the
    /// class name — so we can synthesize one).
    inheritance_clause_end: usize,
    is_partial: bool,
}

fn locate_class(source: &str, class_name: &str) -> Result<ClassLoc> {
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
        if let Some(after_class) = match_keyword(bytes, i, b"class") {
            let name_start = skip_whitespace(bytes, after_class);
            let (parsed_name, name_end) = read_ident(bytes, name_start);
            if parsed_name == class_name {
                // Lookback to detect `partial`.
                let max_back = i.saturating_sub(256);
                let prefix = std::str::from_utf8(&bytes[max_back..i]).unwrap_or("");
                let is_partial = prefix.split_whitespace().any(|tok| tok == "partial");
                // Forward: find inheritance clause and body.
                let mut j = name_end;
                let mut inh_start: Option<usize> = None;
                let mut inh_end: Option<usize> = None;
                while j < bytes.len() {
                    if let Some(next) = skip_lex_atom(bytes, j) {
                        j = next;
                        continue;
                    }
                    match bytes[j] {
                        b':' if inh_start.is_none() => {
                            inh_start = Some(j);
                            j += 1;
                        }
                        b'{' => break,
                        _ => j += 1,
                    }
                }
                if j >= bytes.len() {
                    bail!("error.class_body_not_found");
                }
                let body_start = j;
                let inh_clause = inh_start.map(|s| {
                    let mut end = body_start;
                    while end > s && bytes[end - 1].is_ascii_whitespace() {
                        end -= 1;
                    }
                    inh_end = Some(end);
                    std::str::from_utf8(&bytes[s..end])
                        .unwrap_or("")
                        .to_string()
                });
                let body_end = find_matching_close_brace(bytes, body_start)
                    .ok_or_else(|| anyhow!("error.unbalanced_class_braces"))?;
                let inheritance_clause_end = inh_end.unwrap_or(name_end);
                return Ok(ClassLoc {
                    body_start,
                    body_end,
                    inheritance_clause: inh_clause,
                    inheritance_clause_end,
                    is_partial,
                });
            }
            i = after_class;
            continue;
        }
        i += 1;
    }
    bail!("error.class_not_found: `{class_name}`")
}

fn body_indent_string(source: &str, body_start: usize) -> String {
    // Find the first non-blank line inside the body, return its
    // leading indent. Fallback: 4 spaces.
    let bytes = source.as_bytes();
    let mut i = body_start + 1;
    while i < bytes.len() {
        if bytes[i] == b'\n' {
            i += 1;
            let line_start = i;
            while i < bytes.len() && (bytes[i] == b' ' || bytes[i] == b'\t') {
                i += 1;
            }
            if i < bytes.len() && bytes[i] != b'\n' && bytes[i] != b'}' {
                return std::str::from_utf8(&bytes[line_start..i])
                    .unwrap_or("    ")
                    .to_string();
            }
            continue;
        }
        i += 1;
    }
    "    ".to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(path: &Path, class: &str) -> RefactorPlanParams {
        RefactorPlanParams {
            kind: "csharp_async_dispose_convert".to_string(),
            source: path.to_string_lossy().to_string(),
            item_names: Some(vec![class.to_string()]),
            ..Default::default()
        }
    }

    fn write(src: &str) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("Foo.cs"), src).unwrap();
        dir
    }

    fn apply(src: &str, edits: &[TextEdit]) -> String {
        let mut s = src.to_string();
        let mut sorted: Vec<&TextEdit> = edits.iter().collect();
        sorted.sort_by(|a, b| b.byte_start.cmp(&a.byte_start));
        for te in sorted {
            s.replace_range(te.byte_start..te.byte_end, &te.replacement);
        }
        s
    }

    #[test]
    fn refuses_when_not_idisposable() {
        let src = "public class Foo { public void Bar() {} }";
        let dir = write(src);
        let err = plan_async_dispose_convert(&p(&dir.path().join("Foo.cs"), "Foo")).unwrap_err();
        assert!(err.to_string().contains("no_idisposable"));
    }

    #[test]
    fn refuses_when_already_iasyncdisposable() {
        let src = "public class Foo : IDisposable, IAsyncDisposable { public void Dispose() {} public ValueTask DisposeAsync() => default; }";
        let dir = write(src);
        let err = plan_async_dispose_convert(&p(&dir.path().join("Foo.cs"), "Foo")).unwrap_err();
        assert!(err.to_string().contains("already_async_disposable"));
    }

    #[test]
    fn adds_iasyncdisposable_and_dispose_async() {
        let src = "public class Foo : IDisposable {\n    public void Dispose() {\n        // cleanup\n    }\n}\n";
        let dir = write(src);
        let json = plan_async_dispose_convert(&p(&dir.path().join("Foo.cs"), "Foo")).unwrap();
        let plan: serde_json::Value = serde_json::from_str(&json).unwrap();
        let text_edits: Vec<TextEdit> =
            serde_json::from_value(plan["edits"][0]["edits"].clone()).unwrap();
        let out = apply(src, &text_edits);
        assert!(out.contains(": IDisposable, IAsyncDisposable"), "{out}");
        assert!(
            out.contains("public async global::System.Threading.Tasks.ValueTask DisposeAsync()"),
            "{out}"
        );
        assert!(out.contains("Dispose();"), "{out}");
    }

    #[test]
    fn refuses_partial_class() {
        let src = "public partial class Foo : IDisposable {\n    public void Dispose() {}\n}\n";
        let dir = write(src);
        let err = plan_async_dispose_convert(&p(&dir.path().join("Foo.cs"), "Foo")).unwrap_err();
        assert!(err.to_string().contains("partial_class_unsupported"));
    }
}
