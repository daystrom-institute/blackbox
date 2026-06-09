//! `csharp_public_api_guard` — analysis-only public-API surface enumeration.
//!
//! Walks every `.cs` file under `project_dir` (or the single `source`
//! file when supplied) and enumerates declarations carrying a
//! `public` access modifier: types, methods, properties, fields,
//! events. The report is the operator's input for an upcoming
//! public-API-change decision; the kind itself never emits edits.
//!
//! When a `PublicAPI.Shipped.txt` / `PublicAPI.Unshipped.txt`
//! analyzer manifest is present next to a csproj, the report also
//! lists the manifest contents (verbatim) so the operator can
//! compare declared-vs-actual.
//!
//! v1 implementation is `syntax_only` — Phase 2 sidecar's
//! `Compilation.GetSymbolsWithName` + accessibility-walk produces
//! the full `lsp_verified` flavor.
//!
//! No operator-authority flag (analysis-only).

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use walkdir::WalkDir;

use super::lex::{is_ident_char, is_word_boundary, read_ident, skip_lex_atom};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PublicSymbol {
    pub path: String,
    pub line: u32,
    pub kind: String,
    pub name: String,
    pub modifiers: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PublicApiManifest {
    pub path: String,
    pub kind: String, // "shipped" | "unshipped"
    pub lines: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PublicApiReport {
    pub kind: String,
    pub scope: String,
    pub symbols: Vec<PublicSymbol>,
    pub manifests: Vec<PublicApiManifest>,
    pub symbol_count: usize,
    pub file_count: usize,
}

pub fn plan_public_api_guard(p: &crate::RefactorPlanParams) -> Result<String> {
    let (scope_root, single_file) = resolve_scope(p)?;
    if !scope_root.exists() {
        bail!("error.scope_not_found: `{}`", scope_root.display());
    }
    let mut symbols = Vec::new();
    let mut files_scanned = 0usize;
    if let Some(ref file) = single_file {
        let source =
            fs::read_to_string(file).with_context(|| format!("reading {}", file.display()))?;
        extract_public_symbols(&source, file, &mut symbols);
        files_scanned = 1;
    } else {
        for entry in WalkDir::new(&scope_root)
            .into_iter()
            .filter_entry(|e| !is_skipped_dir(e.path()))
            .filter_map(Result::ok)
        {
            let path = entry.path();
            if !is_csharp_source(path) {
                continue;
            }
            let source =
                fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
            extract_public_symbols(&source, path, &mut symbols);
            files_scanned += 1;
        }
    }
    let manifests = if single_file.is_none() {
        load_public_api_manifests(&scope_root)?
    } else {
        Vec::new()
    };
    let report = PublicApiReport {
        kind: "csharp_public_api_guard".to_string(),
        scope: scope_root.to_string_lossy().to_string(),
        symbol_count: symbols.len(),
        symbols,
        manifests,
        file_count: files_scanned,
    };
    Ok(serde_json::to_string_pretty(&report)?)
}

fn resolve_scope(p: &crate::RefactorPlanParams) -> Result<(PathBuf, Option<PathBuf>)> {
    // Per the design doc this kind accepts either a project_dir
    // (whole-project surface) or a single source file (file-scoped).
    let single_file = if !p.source.is_empty() && p.source != "." {
        let candidate = PathBuf::from(&p.source);
        let resolved = if candidate.is_absolute() {
            candidate
        } else {
            let base = p
                .project_dir
                .as_deref()
                .map(PathBuf::from)
                .unwrap_or_else(|| std::env::current_dir().expect("cwd"));
            base.join(candidate)
        };
        if resolved.is_file() {
            Some(resolved)
        } else {
            None
        }
    } else {
        None
    };
    let scope_root = if let Some(ref f) = single_file {
        f.clone()
    } else if let Some(dir) = p.project_dir.as_deref() {
        PathBuf::from(dir)
    } else {
        std::env::current_dir().context("getting current directory")?
    };
    Ok((scope_root, single_file))
}

fn is_skipped_dir(path: &Path) -> bool {
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
    matches!(
        name,
        "bin" | "obj" | ".git" | ".worktrees" | "node_modules" | "target"
    )
}

fn is_csharp_source(path: &Path) -> bool {
    let Some(ext) = path.extension().and_then(|e| e.to_str()) else {
        return false;
    };
    if ext != "cs" {
        return false;
    }
    let name_lower = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    !(name_lower.ends_with(".g.cs") || name_lower.ends_with(".designer.cs"))
}

fn load_public_api_manifests(root: &Path) -> Result<Vec<PublicApiManifest>> {
    let mut out = Vec::new();
    for entry in WalkDir::new(root)
        .into_iter()
        .filter_entry(|e| !is_skipped_dir(e.path()))
        .filter_map(Result::ok)
    {
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        let kind = match name {
            "PublicAPI.Shipped.txt" => "shipped",
            "PublicAPI.Unshipped.txt" => "unshipped",
            _ => continue,
        };
        let text = fs::read_to_string(path).unwrap_or_default();
        let lines: Vec<String> = text.lines().map(|s| s.to_string()).collect();
        out.push(PublicApiManifest {
            path: path.to_string_lossy().to_string(),
            kind: kind.to_string(),
            lines,
        });
    }
    Ok(out)
}

fn extract_public_symbols(source: &str, path: &Path, out: &mut Vec<PublicSymbol>) {
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
        let (token, end) = read_ident(bytes, i);
        if token == "public" {
            // We have a public declaration. Walk forward, collecting
            // modifier tokens until we hit a type keyword (class /
            // record / struct / interface / enum / delegate) or a
            // type-name token (for fields/methods/properties).
            let (modifiers, type_kind, type_name, line) = classify_public_decl(bytes, end, source);
            if let Some(kind) = type_kind {
                out.push(PublicSymbol {
                    path: path.to_string_lossy().to_string(),
                    line,
                    kind: kind.to_string(),
                    name: type_name,
                    modifiers,
                });
            }
            i = end;
            continue;
        }
        i = end.max(i + 1);
    }
}

fn classify_public_decl(
    bytes: &[u8],
    after_public: usize,
    source: &str,
) -> (Vec<String>, Option<&'static str>, String, u32) {
    let mut modifiers = vec!["public".to_string()];
    let mut pos = after_public;
    let line = byte_offset_to_line(source, pos);
    let type_kw_set: &[(&str, &str)] = &[
        ("class", "class"),
        ("record", "record"),
        ("struct", "struct"),
        ("interface", "interface"),
        ("enum", "enum"),
        ("delegate", "delegate"),
    ];
    let other_modifiers = [
        "static", "sealed", "abstract", "partial", "readonly", "virtual", "override", "async",
        "extern", "unsafe", "new", "ref",
    ];
    loop {
        let pos_after_ws = skip_ws_keep_newline(bytes, pos);
        if pos_after_ws >= bytes.len() {
            return (modifiers, None, String::new(), line);
        }
        let (token, end) = read_ident(bytes, pos_after_ws);
        if token.is_empty() {
            return (modifiers, None, String::new(), line);
        }
        if let Some(&(_, kind)) = type_kw_set.iter().find(|(k, _)| k == &token.as_str()) {
            // Type declaration — next token is the type name.
            let name_start = skip_ws_keep_newline(bytes, end);
            let (name, _name_end) = read_ident(bytes, name_start);
            return (modifiers, Some(kind), name, line);
        }
        if other_modifiers.contains(&token.as_str()) {
            modifiers.push(token);
            pos = end;
            continue;
        }
        // Token is a TYPE (return type or field type). Following
        // tokens: optionally a generic, then the member name, then
        // `(` (method), `{` (property), `;` (field/event), `=` (field).
        let kind = guess_member_kind(bytes, end);
        // The member name follows the type. We need to walk past
        // optional generic args and pointer markers.
        let (name, _) = walk_member_name(bytes, end);
        return (modifiers, Some(kind), name, line);
    }
}

fn walk_member_name(bytes: &[u8], mut from: usize) -> (String, usize) {
    // Skip generic args on the return type: `IList<T>`.
    from = skip_ws_keep_newline(bytes, from);
    if bytes.get(from) == Some(&b'<') {
        let mut depth = 1i32;
        from += 1;
        while from < bytes.len() && depth > 0 {
            match bytes[from] {
                b'<' => depth += 1,
                b'>' => depth -= 1,
                _ => {}
            }
            from += 1;
        }
    }
    // Skip nullable `?` and array `[]`.
    while from < bytes.len() && (bytes[from] == b'?' || bytes[from] == b'[' || bytes[from] == b']')
    {
        from += 1;
    }
    from = skip_ws_keep_newline(bytes, from);
    // Some declarations use `out`/`ref`/`in`/`this`/explicit interface
    // implementations like `IFoo.Bar()` — for v1 we just read the
    // identifier that comes next and call it the name.
    let (name, end) = read_ident(bytes, from);
    (name, end)
}

fn guess_member_kind(bytes: &[u8], after_type: usize) -> &'static str {
    // Look forward for `(`, `{`, `;`, `=>`, `=`.
    let mut i = after_type;
    while i < bytes.len() {
        if let Some(next) = skip_lex_atom(bytes, i) {
            i = next;
            continue;
        }
        match bytes[i] {
            b'(' => return "method",
            b'{' => return "property",
            b';' => return "field",
            b'=' => {
                if bytes.get(i + 1) == Some(&b'>') {
                    return "method"; // expression-bodied
                }
                return "field";
            }
            _ => i += 1,
        }
    }
    "member"
}

fn skip_ws_keep_newline(bytes: &[u8], from: usize) -> usize {
    let mut i = from;
    while i < bytes.len() && bytes[i].is_ascii_whitespace() {
        i += 1;
    }
    i
}

fn byte_offset_to_line(source: &str, offset: usize) -> u32 {
    let mut line = 1u32;
    for (i, b) in source.as_bytes().iter().enumerate() {
        if i >= offset {
            break;
        }
        if *b == b'\n' {
            line += 1;
        }
    }
    line
}

fn _unused() {
    // Marker so is_ident_char import isn't dropped when refactoring.
    let _ = is_ident_char(b'a');
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(project_dir: Option<&Path>, source: &str) -> crate::RefactorPlanParams {
        crate::RefactorPlanParams {
            kind: "csharp_public_api_guard".to_string(),
            source: source.to_string(),
            project_dir: project_dir.map(|p| p.to_string_lossy().to_string()),
            ..Default::default()
        }
    }

    #[test]
    fn enumerates_public_types_and_methods() {
        let src = r#"namespace Foo;

public class Service {
    public int Count { get; }
    public void DoWork() {}
    public string Name;
    private int _hidden;
}

public record Dto(int Value);

internal class Hidden {}
"#;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("Foo.cs");
        std::fs::write(&path, src).unwrap();
        let json = plan_public_api_guard(&p(None, path.to_str().unwrap())).unwrap();
        let report: PublicApiReport = serde_json::from_str(&json).unwrap();
        let names: Vec<&str> = report.symbols.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"Service"), "{names:?}");
        assert!(names.contains(&"Count"), "{names:?}");
        assert!(names.contains(&"DoWork"), "{names:?}");
        assert!(names.contains(&"Name"), "{names:?}");
        assert!(names.contains(&"Dto"), "{names:?}");
        assert!(!names.contains(&"_hidden"), "{names:?}");
        assert!(!names.contains(&"Hidden"), "{names:?}");
        let kinds: Vec<&str> = report.symbols.iter().map(|s| s.kind.as_str()).collect();
        assert!(kinds.contains(&"class"), "{kinds:?}");
        assert!(kinds.contains(&"record"), "{kinds:?}");
        assert!(kinds.contains(&"property"), "{kinds:?}");
        assert!(kinds.contains(&"method"), "{kinds:?}");
        assert!(kinds.contains(&"field"), "{kinds:?}");
    }

    #[test]
    fn scopes_to_single_file_when_source_is_a_file() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("A.cs"), "public class A {}").unwrap();
        std::fs::write(dir.path().join("B.cs"), "public class B {}").unwrap();
        let json = plan_public_api_guard(&p(
            Some(dir.path()),
            dir.path().join("A.cs").to_str().unwrap(),
        ))
        .unwrap();
        let report: PublicApiReport = serde_json::from_str(&json).unwrap();
        let names: Vec<&str> = report.symbols.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"A"));
        assert!(!names.contains(&"B"));
    }

    #[test]
    fn loads_publicapi_manifest_when_present() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("Foo.cs"), "public class Foo {}").unwrap();
        std::fs::write(
            dir.path().join("PublicAPI.Shipped.txt"),
            "Foo\nFoo.Foo() -> void\n",
        )
        .unwrap();
        let json = plan_public_api_guard(&p(Some(dir.path()), ".")).unwrap();
        let report: PublicApiReport = serde_json::from_str(&json).unwrap();
        assert_eq!(report.manifests.len(), 1);
        assert_eq!(report.manifests[0].kind, "shipped");
        assert!(report.manifests[0].lines.iter().any(|l| l == "Foo"));
    }
}
