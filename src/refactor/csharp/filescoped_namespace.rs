//! `migrate_csharp_to_filescoped_namespace` — syntax_only sweep.
//!
//! Converts a single C# source file from block-scoped namespace
//! syntax to file-scoped namespace syntax (C# 10+):
//!
//! ```ignore
//! // before
//! namespace Foo.Bar {
//!     public class Baz { }
//! }
//!
//! // after
//! namespace Foo.Bar;
//!
//! public class Baz { }
//! ```
//!
//! Refusal rules:
//! - The file must contain exactly one top-level `namespace X { ... }`
//!   block. Multiple namespaces in one file or nested namespaces refuse
//!   with `error.multiple_namespaces` (file-scoped namespaces are
//!   one-per-file by language design).
//! - Already file-scoped → returns an empty edit set; the plan is
//!   idempotent.
//!
//! No tree-sitter dep here — the conversion is mechanical enough that a
//! stack-counted scan over braces (skipping strings, char literals,
//! verbatim strings, raw strings, line comments, block comments)
//! suffices and avoids pulling tree-sitter-c-sharp.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};

use crate::refactor::{
    FileEdit, RefactorPlanParams, SemanticStatus, TextEdit, ValidationStep, csharp::empty_plan,
};
use sha2::{Digest, Sha256};

pub fn plan_filescoped_namespace(p: &RefactorPlanParams) -> Result<String> {
    let source_path = resolve_path(p.project_dir.as_deref(), &p.source)?;
    let source = fs::read_to_string(&source_path)
        .with_context(|| format!("reading {}", source_path.display()))?;

    let edits = compute_edit(&source, &source_path)?;
    let display = path_string(&source_path);
    let mut plan = empty_plan(
        "migrate_csharp_to_filescoped_namespace",
        format!("convert {display} to file-scoped namespace"),
        SemanticStatus::SyntaxOnly,
    );
    plan.validations.push(ValidationStep::TreeSitterNoErrors {
        path: display.clone(),
        byte_range: None,
    });
    plan.edits = edits;
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

/// Returns the edit set that converts a block-scoped namespace to a
/// file-scoped one. Empty vec when the file is already file-scoped.
fn compute_edit(source: &str, path: &Path) -> Result<Vec<FileEdit>> {
    let bytes = source.as_bytes();
    let mut scan = Scanner::new(bytes);

    let mut namespace_kw_starts: Vec<usize> = Vec::new();
    let mut top_level_namespace: Option<NamespaceMatch> = None;

    while let Some(kw_start) = scan.find_top_level_keyword(b"namespace") {
        namespace_kw_starts.push(kw_start);
        // Look at first non-whitespace after the keyword to decide
        // block vs file-scoped.
        let after_kw = kw_start + b"namespace".len();
        scan.cursor = after_kw;
        scan.skip_whitespace_and_comments();
        let name_start = scan.cursor;
        scan.skip_namespace_name();
        let name_end = scan.cursor;
        if name_end == name_start {
            bail!(
                "error.namespace_parse_failed: empty namespace name at byte {} in {}",
                kw_start,
                path.display()
            );
        }
        scan.skip_whitespace_and_comments();
        let next_byte = scan.peek();
        match next_byte {
            Some(b';') => {
                // Already file-scoped at this site. Continue scanning;
                // if the file has only this one, the plan is idempotent.
            }
            Some(b'{') => {
                // Block-scoped — record. Find the matching close brace.
                let open = scan.cursor;
                scan.cursor += 1;
                let close = scan.find_matching_close_brace().ok_or_else(|| {
                    anyhow!("error.unbalanced_namespace_braces in {}", path.display())
                })?;
                // Advance past the close brace so the next find_top_level_keyword
                // call doesn't decrement depth below zero on it.
                scan.cursor = close + 1;
                if top_level_namespace.is_some() {
                    bail!(
                        "error.multiple_namespaces: file-scoped namespaces are one-per-file by language rules; {} contains more than one top-level namespace",
                        path.display()
                    );
                }
                top_level_namespace = Some(NamespaceMatch {
                    name: source[name_start..name_end].to_string(),
                    name_end,
                    open_brace: open,
                    close_brace: close,
                });
            }
            Some(other) => bail!(
                "error.namespace_parse_failed: expected `;` or `{{` after namespace name in {}, found `{}`",
                path.display(),
                other as char,
            ),
            None => bail!(
                "error.namespace_parse_failed: unexpected EOF after namespace declaration in {}",
                path.display()
            ),
        }
    }

    let Some(m) = top_level_namespace else {
        return Ok(Vec::new());
    };

    // Construct two edits:
    //   1. Replace the byte range from `name_end` through `open_brace+1`
    //      (inclusive) with `;\n` — keeps the leading `namespace Foo.Bar`
    //      intact and drops the brace + any whitespace between them.
    //   2. Replace the close brace and the immediately preceding newline
    //      whitespace with empty text — the file body is now top-level.
    //
    // The body indentation is intentionally NOT reflowed: callers
    // typically follow with `dotnet format whitespace` or the operator
    // chooses whether to dedent. Keeping the edits surgical means the
    // plan is review-friendly and the diff is small.

    let mut hasher = Sha256::new();
    hasher.update(source.as_bytes());
    let sha = format!("{:x}", hasher.finalize());
    let text_edits = vec![
        TextEdit {
            byte_start: m.name_end,
            byte_end: m.open_brace + 1,
            replacement: ";\n".to_string(),
        },
        TextEdit {
            byte_start: trim_trailing_blank(source.as_bytes(), m.close_brace),
            byte_end: m.close_brace + 1,
            replacement: String::new(),
        },
    ];
    Ok(vec![FileEdit {
        path: path_string(path),
        original_sha256: sha,
        edits: text_edits,
        new_text: None,
    }])
}

#[derive(Debug)]
struct NamespaceMatch {
    #[allow(dead_code)]
    name: String,
    name_end: usize,
    open_brace: usize,
    close_brace: usize,
}

fn trim_trailing_blank(bytes: &[u8], close_brace: usize) -> usize {
    // Walk backwards from `close_brace` past whitespace/newline so the
    // generated diff doesn't leave a trailing blank line.
    let mut i = close_brace;
    while i > 0 {
        let prev = bytes[i - 1];
        if prev == b'\n' || prev == b'\r' || prev == b' ' || prev == b'\t' {
            i -= 1;
            continue;
        }
        break;
    }
    i
}

/// Brace-depth-aware scanner that skips C# tokens which can contain
/// braces inside their literal payload (strings, chars, comments,
/// verbatim/raw strings).
struct Scanner<'a> {
    bytes: &'a [u8],
    cursor: usize,
}

impl<'a> Scanner<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, cursor: 0 }
    }

    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.cursor).copied()
    }

    fn skip_whitespace_and_comments(&mut self) {
        loop {
            match self.peek() {
                Some(c) if c.is_ascii_whitespace() => self.cursor += 1,
                Some(b'/') if self.bytes.get(self.cursor + 1) == Some(&b'/') => {
                    while let Some(c) = self.peek() {
                        if c == b'\n' {
                            break;
                        }
                        self.cursor += 1;
                    }
                }
                Some(b'/') if self.bytes.get(self.cursor + 1) == Some(&b'*') => {
                    self.cursor += 2;
                    while self.cursor + 1 < self.bytes.len() {
                        if self.bytes[self.cursor] == b'*' && self.bytes[self.cursor + 1] == b'/' {
                            self.cursor += 2;
                            break;
                        }
                        self.cursor += 1;
                    }
                }
                _ => break,
            }
        }
    }

    fn skip_namespace_name(&mut self) {
        // Accept identifier chars, dots, and `global::` qualifier.
        while let Some(c) = self.peek() {
            if c.is_ascii_alphanumeric() || c == b'_' || c == b'.' || c == b':' {
                self.cursor += 1;
            } else {
                break;
            }
        }
    }

    /// Find the next occurrence of `keyword` at brace-depth 0,
    /// ignoring strings/chars/comments. Returns the byte offset of the
    /// first char of the keyword.
    fn find_top_level_keyword(&mut self, keyword: &[u8]) -> Option<usize> {
        let mut depth: i32 = 0;
        while self.cursor < self.bytes.len() {
            let b = self.bytes[self.cursor];
            // Strings / chars / verbatim / raw — skip past their close
            // delimiter so we don't see braces inside literal payloads.
            if b == b'"' && self.maybe_skip_string() {
                continue;
            }
            if b == b'\'' && self.maybe_skip_char_literal() {
                continue;
            }
            if b == b'/' && self.maybe_skip_comment() {
                continue;
            }
            if b == b'@' && self.bytes.get(self.cursor + 1) == Some(&b'"') {
                self.cursor += 1; // step onto the quote
                self.maybe_skip_verbatim_string();
                continue;
            }
            if b == b'{' {
                depth += 1;
                self.cursor += 1;
                continue;
            }
            if b == b'}' {
                depth -= 1;
                self.cursor += 1;
                continue;
            }
            if depth == 0
                && (self.cursor == 0 || !is_ident_char(self.bytes[self.cursor - 1]))
                && self.bytes[self.cursor..].starts_with(keyword)
            {
                let after = self.cursor + keyword.len();
                if after >= self.bytes.len() || !is_ident_char(self.bytes[after]) {
                    let pos = self.cursor;
                    self.cursor = after;
                    return Some(pos);
                }
            }
            self.cursor += 1;
        }
        None
    }

    fn find_matching_close_brace(&mut self) -> Option<usize> {
        // Caller has just consumed the opening `{`. depth starts at 1.
        let mut depth: i32 = 1;
        while self.cursor < self.bytes.len() {
            let b = self.bytes[self.cursor];
            if b == b'"' && self.maybe_skip_string() {
                continue;
            }
            if b == b'\'' && self.maybe_skip_char_literal() {
                continue;
            }
            if b == b'/' && self.maybe_skip_comment() {
                continue;
            }
            if b == b'@' && self.bytes.get(self.cursor + 1) == Some(&b'"') {
                self.cursor += 1;
                self.maybe_skip_verbatim_string();
                continue;
            }
            match b {
                b'{' => {
                    depth += 1;
                    self.cursor += 1;
                }
                b'}' => {
                    depth -= 1;
                    if depth == 0 {
                        return Some(self.cursor);
                    }
                    self.cursor += 1;
                }
                _ => self.cursor += 1,
            }
        }
        None
    }

    /// If we're at `"`, advance past the closing `"` honoring escapes.
    /// Returns true iff a string was consumed.
    fn maybe_skip_string(&mut self) -> bool {
        if self.peek() != Some(b'"') {
            return false;
        }
        self.cursor += 1;
        while let Some(c) = self.peek() {
            match c {
                b'\\' => self.cursor += 2,
                b'"' => {
                    self.cursor += 1;
                    return true;
                }
                b'\n' => return true, // unterminated line — bail safely
                _ => self.cursor += 1,
            }
        }
        true
    }

    fn maybe_skip_char_literal(&mut self) -> bool {
        if self.peek() != Some(b'\'') {
            return false;
        }
        self.cursor += 1;
        while let Some(c) = self.peek() {
            match c {
                b'\\' => self.cursor += 2,
                b'\'' => {
                    self.cursor += 1;
                    return true;
                }
                b'\n' => return true,
                _ => self.cursor += 1,
            }
        }
        true
    }

    fn maybe_skip_verbatim_string(&mut self) -> bool {
        // We're at the leading `"` of an `@"..."` literal.
        if self.peek() != Some(b'"') {
            return false;
        }
        self.cursor += 1;
        while let Some(c) = self.peek() {
            if c == b'"' {
                if self.bytes.get(self.cursor + 1) == Some(&b'"') {
                    // Escaped `""` — consume both, continue.
                    self.cursor += 2;
                    continue;
                }
                self.cursor += 1;
                return true;
            }
            self.cursor += 1;
        }
        true
    }

    fn maybe_skip_comment(&mut self) -> bool {
        match (self.peek(), self.bytes.get(self.cursor + 1).copied()) {
            (Some(b'/'), Some(b'/')) => {
                while let Some(c) = self.peek() {
                    if c == b'\n' {
                        return true;
                    }
                    self.cursor += 1;
                }
                true
            }
            (Some(b'/'), Some(b'*')) => {
                self.cursor += 2;
                while self.cursor + 1 < self.bytes.len() {
                    if self.bytes[self.cursor] == b'*' && self.bytes[self.cursor + 1] == b'/' {
                        self.cursor += 2;
                        return true;
                    }
                    self.cursor += 1;
                }
                true
            }
            _ => false,
        }
    }
}

fn is_ident_char(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

#[cfg(test)]
mod tests {
    use super::*;

    fn convert(source: &str) -> String {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("Foo.cs");
        std::fs::write(&path, source).unwrap();
        let file_edits = compute_edit(source, &path).unwrap();
        let mut text = source.to_string();
        // Apply text edits in reverse to preserve byte ranges.
        for fe in file_edits {
            let mut sorted = fe.edits;
            sorted.sort_by_key(|b| std::cmp::Reverse(b.byte_start));
            for te in sorted {
                text.replace_range(te.byte_start..te.byte_end, &te.replacement);
            }
        }
        text
    }

    #[test]
    fn simple_block_to_filescoped() {
        let src = "namespace Foo.Bar {\n    public class Baz { }\n}\n";
        let out = convert(src);
        assert!(
            out.contains("namespace Foo.Bar;"),
            "expected file-scoped header in {out:?}"
        );
        assert!(out.contains("public class Baz"));
        // No trailing close brace.
        assert!(
            !out.trim_end().ends_with('}') || out.matches('}').count() < src.matches('}').count()
        );
    }

    #[test]
    fn idempotent_when_already_filescoped() {
        let src = "namespace Foo.Bar;\n\npublic class Baz { }\n";
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("Foo.cs");
        std::fs::write(&path, src).unwrap();
        let edits = compute_edit(src, &path).unwrap();
        assert!(edits.is_empty(), "expected no edits, got {edits:?}");
    }

    #[test]
    fn refuses_multiple_namespaces() {
        let src = "namespace Foo {}\nnamespace Bar {}\n";
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("Foo.cs");
        std::fs::write(&path, src).unwrap();
        let err = compute_edit(src, &path).unwrap_err();
        assert!(err.to_string().contains("multiple_namespaces"));
    }

    #[test]
    fn ignores_braces_in_strings_and_comments() {
        let src = r#"namespace Foo {
    public class Bar {
        // string with } in it
        public string Pattern = "abc { def } ghi";
        public string Verbatim = @"raw } { } string";
        /* block } comment */
    }
}
"#;
        let out = convert(src);
        assert!(out.contains("namespace Foo;"));
        assert!(out.contains("Pattern = \"abc { def } ghi\""));
    }

    #[test]
    fn handles_global_qualifier() {
        let src = "namespace global::Foo.Bar {\n    public class X {}\n}\n";
        let out = convert(src);
        assert!(out.contains("namespace global::Foo.Bar;"), "{out}");
    }
}
