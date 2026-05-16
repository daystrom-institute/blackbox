//! `csharp_primary_ctor_migrate` — multi-arg constructor → primary ctor.
//!
//! C# 12 primary constructors for classes; supported earlier for records.
//! Applies when a class has exactly one constructor whose body is
//! parameter-assignment-only (each statement is `this.X = X;` /
//! `X = X;` / `_x = x;` style). Multiple constructors, ctor with
//! arbitrary logic, base-call chaining beyond pure forwarding all
//! refuse cleanly.
//!
//! v1 is **syntax_only**: the rewrite is mechanical and the operator
//! reviews the resulting class manually. The full `lsp_verified`
//! flavor (cross-file callsite scan to verify the migration didn't
//! break extension methods that take the class as a typed receiver)
//! waits for Phase 2.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use sha2::{Digest, Sha256};

use super::lex::{
    find_matching_close_brace, is_ident_char, is_word_boundary, match_keyword, read_ident,
    skip_lex_atom, skip_whitespace,
};
use crate::refactor::{
    FileEdit, RefactorPlanParams, SemanticStatus, TextEdit, ValidationStep,
    csharp::empty_plan,
};

pub fn plan_primary_ctor_migrate(p: &RefactorPlanParams) -> Result<String> {
    let source_path = resolve_path(p.project_dir.as_deref(), &p.source)?;
    refuse_generated_file(&source_path)?;
    let class_name = p
        .item_names
        .as_deref()
        .and_then(|names| names.first())
        .map(String::as_str)
        .ok_or_else(|| {
            anyhow!("item_names[0] (target class) is required for csharp_primary_ctor_migrate")
        })?;

    let source = fs::read_to_string(&source_path)
        .with_context(|| format!("reading {}", source_path.display()))?;

    let class = locate_class(&source, class_name)?;
    let ctors = find_constructors(&source, &class, class_name)?;
    if ctors.is_empty() {
        bail!(
            "error.no_constructor: class `{class_name}` has no explicit constructor; primary-ctor migration is only meaningful for classes with one"
        );
    }
    if ctors.len() > 1 {
        let names: Vec<String> = ctors
            .iter()
            .map(|c| format!("({})", c.param_signature))
            .collect();
    bail!(
            "error.multiple_constructors: class `{class_name}` has {} constructors {}; primary-ctor migration requires exactly one assignment-only ctor",
            ctors.len(),
            names.join(", ")
        );
    }
    let ctor = &ctors[0];
    if !ctor.assignment_only {
        bail!(
            "error.constructor_logic_present: class `{class_name}` ctor body contains non-assignment statements; primary-ctor migration requires assignment-only ctors"
        );
    }
    if let Some(ref base_call) = ctor.base_chain {
        if !base_call.pure_forward {
            bail!(
                "error.base_chain_with_logic: class `{class_name}` ctor chains base()/this() with non-forwarding arguments; primary-ctor migration refuses"
            );
        }
    }

    // Build the rewrite:
    //   1. Insert `(param1, param2, ...)` after the class name (and
    //      base/interface clause if present — primary ctor goes
    //      between class name and `: BaseClass(...)`).
    //   2. Delete the ctor declaration including its body.
    //   3. Remove field declarations whose only assignment is from
    //      the ctor (operator-driven; v1 keeps the fields and the
    //      `this.X = X` lines disappear by virtue of ctor removal,
    //      which is operator-reviewable).
    let mut edits = Vec::new();
    // 1. Insert primary-ctor param list immediately after the class
    //    name (and generic-args list, if any). This sits before any
    //    `:` inheritance clause or `{` body start, so the result is
    //    `class Foo(...) : Bar { ... }` or `class Foo(...) { ... }`.
    let insert_pos = class.name_end;
    let param_list = format!("({})", ctor.param_signature);
    edits.push(TextEdit {
        byte_start: insert_pos,
        byte_end: insert_pos,
        replacement: param_list,
    });
    // 2. Remove ctor declaration.
    edits.push(TextEdit {
        byte_start: ctor.head_start,
        byte_end: ctor.body_end + 1,
        replacement: String::new(),
    });

    let mut hasher = Sha256::new();
    hasher.update(source.as_bytes());
    let sha = format!("{:x}", hasher.finalize());

    let mut plan = empty_plan(
        "csharp_primary_ctor_migrate",
        format!(
            "migrate `{class_name}` to primary constructor in {}",
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
    if lower.contains("/generated/")
        || lower.ends_with(".g.cs")
        || lower.ends_with(".designer.cs")
    {
        bail!(
            "error.generated_file_refusal: `{}`",
            path.display()
        );
    }
    Ok(())
}

#[derive(Debug)]
struct ClassLoc {
    /// Byte position immediately after the class-name identifier
    /// (and after any `<T, U>` generic param list). Primary-ctor
    /// parameter list inserts here.
    name_end: usize,
    body_start: usize,
    body_end: usize,
    /// If the class has `: BaseClass, IFoo` clause, this is the
    /// byte range of the leading `:`. Unused for the insertion site
    /// today — the primary-ctor params attach to the class name
    /// regardless of whether an inheritance clause is present —
    /// but kept for future use when we need to rewrite the chain.
    #[allow(dead_code)]
    inheritance_position: Option<InhPos>,
}

#[derive(Debug)]
struct InhPos {
    start: usize,
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
                // Walk forward looking for `:` (inheritance) or `{` (body).
                let mut j = name_end;
                // Skip generic param list `<T, U>` if present.
                if bytes.get(j) == Some(&b'<') {
                    let mut depth = 1i32;
                    j += 1;
                    while j < bytes.len() && depth > 0 {
                        match bytes[j] {
                            b'<' => depth += 1,
                            b'>' => depth -= 1,
                            _ => {}
                        }
                        j += 1;
                    }
                }
                let mut inh: Option<InhPos> = None;
                while j < bytes.len() {
                    if let Some(next) = skip_lex_atom(bytes, j) {
                        j = next;
                        continue;
                    }
                    match bytes[j] {
                        b':' if inh.is_none() => {
                            inh = Some(InhPos { start: j });
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
                let body_end = find_matching_close_brace(bytes, body_start)
                    .ok_or_else(|| anyhow!("error.unbalanced_class_braces"))?;
                let name_end_after_generics = j;
                // name_end_after_generics is the position right after
                // `>` (if generics) or just after the class name; but
                // we want immediately after the identifier or generic
                // list, before any whitespace. Walk back over trailing
                // whitespace until we hit the `>` or the identifier.
                let mut cursor = name_end_after_generics;
                // We over-walked to `:` or `{` — back up to the last
                // non-whitespace byte that's still part of the name
                // (or generic args).
                // Walk back from cursor while previous byte is whitespace
                // or part of the inheritance clause we haven't crossed.
                // Simplest correct approach: just use `name_end_id`,
                // the position right after the identifier or generics.
                // Compute that by re-scanning from `name_start`.
                let mut id_end = name_start + parsed_name.len();
                if bytes.get(id_end) == Some(&b'<') {
                    let mut depth = 1i32;
                    id_end += 1;
                    while id_end < bytes.len() && depth > 0 {
                        match bytes[id_end] {
                            b'<' => depth += 1,
                            b'>' => depth -= 1,
                            _ => {}
                        }
                        id_end += 1;
                    }
                }
                let _ = cursor;
                return Ok(ClassLoc {
                    name_end: id_end,
                    body_start,
                    body_end,
                    inheritance_position: inh,
                });
            }
            i = after_class;
            continue;
        }
        i += 1;
    }
    bail!("error.class_not_found: `{class_name}` not found as a class declaration")
}

#[derive(Debug)]
struct Ctor {
    head_start: usize,
    body_end: usize,
    param_signature: String,
    assignment_only: bool,
    base_chain: Option<BaseCall>,
}

#[derive(Debug)]
struct BaseCall {
    pure_forward: bool,
}

fn find_constructors(source: &str, class: &ClassLoc, class_name: &str) -> Result<Vec<Ctor>> {
    let bytes = source.as_bytes();
    let body = &bytes[class.body_start + 1..class.body_end];
    let region_offset = class.body_start + 1;
    let mut out = Vec::new();
    let mut i = 0usize;
    while i < body.len() {
        if let Some(next) = skip_lex_atom(body, i) {
            i = next;
            continue;
        }
        if !is_word_boundary(body, i) {
            i += 1;
            continue;
        }
        let (token, end) = read_ident(body, i);
        if token == class_name {
            let after_ws = skip_whitespace(body, end);
            if body.get(after_ws) != Some(&b'(') {
                i = end.max(i + 1);
                continue;
            }
            // It's a ctor (or a method with same name; same-name methods
            // are forbidden by C# so we can trust the structure). Find
            // the closing `)`.
            let param_open = after_ws;
            let mut paren_depth = 1i32;
            let mut k = param_open + 1;
            while k < body.len() && paren_depth > 0 {
                match body[k] {
                    b'(' => paren_depth += 1,
                    b')' => paren_depth -= 1,
                    _ => {}
                }
                k += 1;
            }
            let param_close = k - 1; // points at `)`
            let param_text = std::str::from_utf8(&body[param_open + 1..param_close])
                .unwrap_or("")
                .trim()
                .to_string();

            // Look for `: base(...)` or `: this(...)` after the closing `)`.
            let mut m = skip_whitespace(body, k);
            let mut base_chain: Option<BaseCall> = None;
            if body.get(m) == Some(&b':') {
                m += 1;
                m = skip_whitespace(body, m);
                let (called, called_end) = read_ident(body, m);
                if called == "base" || called == "this" {
                    m = skip_whitespace(body, called_end);
                    if body.get(m) == Some(&b'(') {
                        let mut depth = 1i32;
                        let mut n = m + 1;
                        while n < body.len() && depth > 0 {
                            match body[n] {
                                b'(' => depth += 1,
                                b')' => depth -= 1,
                                _ => {}
                            }
                            n += 1;
                        }
                        let chain_args = std::str::from_utf8(&body[m + 1..n - 1])
                            .unwrap_or("")
                            .trim();
                        let param_names = parse_param_names(&param_text);
                        let pure_forward = chain_args_match_param_names(chain_args, &param_names);
                        base_chain = Some(BaseCall { pure_forward });
                        m = n;
                    }
                }
            }
            // Find ctor body `{ ... }`.
            let body_open = skip_whitespace(body, m);
            if body.get(body_open) != Some(&b'{') {
                i = body_open.max(i + 1);
                continue;
            }
            let body_close = find_matching_close_brace(body, body_open)
                .ok_or_else(|| anyhow!("error.unbalanced_ctor_braces"))?;
            let ctor_body_text = std::str::from_utf8(&body[body_open + 1..body_close])
                .unwrap_or("");
            let assignment_only = is_assignment_only(ctor_body_text);
            let head_start = find_ctor_head_start(body, i);
            out.push(Ctor {
                head_start: region_offset + head_start,
                body_end: region_offset + body_close,
                param_signature: param_text,
                assignment_only,
                base_chain,
            });
            i = body_close + 1;
            continue;
        }
        i = end.max(i + 1);
    }
    Ok(out)
}

fn find_ctor_head_start(body: &[u8], ctor_name_pos: usize) -> usize {
    if ctor_name_pos == 0 {
        return 0;
    }
    let mut i = ctor_name_pos;
    while i > 0 {
        let b = body[i - 1];
        if b == b';' || b == b'{' || b == b'}' || b == b']' {
            let mut start = i;
            while start < body.len() && body[start].is_ascii_whitespace() {
                start += 1;
            }
            return start;
        }
        i -= 1;
    }
    let mut start = 0;
    while start < body.len() && body[start].is_ascii_whitespace() {
        start += 1;
    }
    start
}

fn parse_param_names(param_text: &str) -> Vec<String> {
    param_text
        .split(',')
        .filter_map(|s| {
            let s = s.trim();
            if s.is_empty() {
                return None;
            }
            // Each `<modifiers...> Type name` — take the last whitespace-separated token.
            let last = s.split_whitespace().last()?;
            // Strip default-value `name = expr` shape.
            let name = last.split('=').next()?.trim();
            Some(name.to_string())
        })
        .collect()
}

fn chain_args_match_param_names(args_text: &str, names: &[String]) -> bool {
    let provided: Vec<String> = args_text
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    if provided.len() != names.len() {
        return false;
    }
    provided
        .iter()
        .zip(names.iter())
        .all(|(p, n)| p == n)
}

fn is_assignment_only(body_text: &str) -> bool {
    // Strip comments first (line + block, naive but adequate for v1 detection).
    let mut cleaned = String::with_capacity(body_text.len());
    let bytes = body_text.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        match (bytes[i], bytes.get(i + 1).copied()) {
            (b'/', Some(b'/')) => {
                while i < bytes.len() && bytes[i] != b'\n' {
                    i += 1;
                }
            }
            (b'/', Some(b'*')) => {
                i += 2;
                while i + 1 < bytes.len()
                    && !(bytes[i] == b'*' && bytes[i + 1] == b'/')
                {
                    i += 1;
                }
                i = (i + 2).min(bytes.len());
            }
            _ => {
                cleaned.push(bytes[i] as char);
                i += 1;
            }
        }
    }
    for stmt in cleaned.split(';') {
        let s = stmt.trim();
        if s.is_empty() {
            continue;
        }
        // Accept shapes:
        //   this.X = Y
        //   _x = X
        //   X = Y
        // The LHS must be a simple identifier-or-this.ident expression
        // and the `=` must not be `==`, `+=`, etc.
        let Some(eq_pos) = find_simple_assign_op(s) else {
            return false;
        };
        let lhs = s[..eq_pos].trim();
        if !is_simple_assign_target(lhs) {
            return false;
        }
    }
    true
}

fn find_simple_assign_op(s: &str) -> Option<usize> {
    let bytes = s.as_bytes();
    for (i, &b) in bytes.iter().enumerate() {
        if b == b'=' {
            let prev = if i > 0 { bytes[i - 1] } else { b' ' };
            let next = bytes.get(i + 1).copied().unwrap_or(b' ');
            if matches!(prev, b'=' | b'!' | b'<' | b'>' | b'+' | b'-' | b'*' | b'/' | b'%' | b'&' | b'|' | b'^')
                || next == b'='
            {
                continue;
            }
            return Some(i);
        }
    }
    None
}

fn is_simple_assign_target(lhs: &str) -> bool {
    let trimmed = lhs.trim();
    if trimmed.starts_with("this.") {
        let rest = &trimmed[5..];
        return rest.chars().all(|c| is_ident_char(c as u8));
    }
    trimmed.chars().all(|c| is_ident_char(c as u8))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(path: &Path, class: &str) -> RefactorPlanParams {
        RefactorPlanParams {
            kind: "csharp_primary_ctor_migrate".to_string(),
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

    #[test]
    fn refuses_class_with_multiple_constructors() {
        let src = "public class Foo {\n  public Foo(int x) { X = x; }\n  public Foo(int x, int y) { X = x; Y = y; }\n  public int X { get; }\n  public int Y { get; }\n}";
        let dir = write(src);
        let err = plan_primary_ctor_migrate(&p(&dir.path().join("Foo.cs"), "Foo")).unwrap_err();
        assert!(err.to_string().contains("multiple_constructors"));
    }

    #[test]
    fn refuses_constructor_with_logic() {
        let src = "public class Foo {\n  public Foo(int x) { X = x; Console.WriteLine(x); }\n  public int X { get; }\n}";
        let dir = write(src);
        let err = plan_primary_ctor_migrate(&p(&dir.path().join("Foo.cs"), "Foo")).unwrap_err();
        assert!(err.to_string().contains("constructor_logic_present"));
    }

    #[test]
    fn emits_primary_ctor_for_assignment_only() {
        let src = "public class Foo {\n  public Foo(int x, string name) { X = x; this.Name = name; }\n  public int X { get; }\n  public string Name { get; }\n}";
        let dir = write(src);
        let json = plan_primary_ctor_migrate(&p(&dir.path().join("Foo.cs"), "Foo")).unwrap();
        let plan: serde_json::Value = serde_json::from_str(&json).unwrap();
        let text_edits: Vec<TextEdit> =
            serde_json::from_value(plan["edits"][0]["edits"].clone()).unwrap();
        // Should be 2 edits: insert param list + remove ctor.
        assert_eq!(text_edits.len(), 2, "{text_edits:?}");
        // Apply in reverse.
        let mut s = src.to_string();
        let mut sorted: Vec<&TextEdit> = text_edits.iter().collect();
        sorted.sort_by(|a, b| b.byte_start.cmp(&a.byte_start));
        for te in sorted {
            s.replace_range(te.byte_start..te.byte_end, &te.replacement);
        }
        assert!(s.contains("class Foo(int x, string name)"), "{s}");
        assert!(!s.contains("public Foo(int"), "{s}");
    }

    #[test]
    fn refuses_base_chain_with_non_forwarding_args() {
        let src = "public class Foo : Bar {\n  public Foo(int x) : base(x + 1) { X = x; }\n  public int X { get; }\n}";
        let dir = write(src);
        let err = plan_primary_ctor_migrate(&p(&dir.path().join("Foo.cs"), "Foo")).unwrap_err();
        assert!(err.to_string().contains("base_chain_with_logic"));
    }

    #[test]
    fn accepts_pure_forwarding_base_chain() {
        let src = "public class Foo : Bar {\n  public Foo(int x) : base(x) { X = x; }\n  public int X { get; }\n}";
        let dir = write(src);
        let json = plan_primary_ctor_migrate(&p(&dir.path().join("Foo.cs"), "Foo")).unwrap();
        let plan: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert!(!plan["edits"].as_array().unwrap().is_empty());
    }
}
