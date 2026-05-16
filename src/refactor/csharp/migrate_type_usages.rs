//! `migrate_csharp_type_usages` — cross-project type rename.
//!
//! v1 implementation walks every `.cs` file under `project_dir` and
//! finds identifier-bounded occurrences of `old_text`, generating
//! per-file `FileEdit`s that replace each with `new_text`. Skips
//! generated files (`*.g.cs`, `*.Designer.cs`, `Generated/`),
//! `bin/`, `obj/`, `.worktrees/`. Identifier boundaries prevent
//! substring matches inside other identifiers (e.g. `Old` won't
//! match `Older`).
//!
//! Refusal cases:
//!   - `old_text == new_text`
//!   - Invalid C# identifier in either parameter
//!   - No matches found in the project
//!
//! Limits:
//!   - Simple-name match only. A class `Foo` defined in two
//!     namespaces (`A.Foo` and `B.Foo`) gets indiscriminately
//!     renamed. The full lsp_verified flavor with cross-namespace
//!     resolution waits for Phase 2 sidecar.
//!   - String literals and comments are scanned: matches inside
//!     `"old"` or `// old` are skipped via the shared lex helpers.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use sha2::{Digest, Sha256};
use walkdir::WalkDir;

use super::lex::{is_ident_char, skip_lex_atom};
use crate::refactor::{
    FileEdit, RefactorPlanParams, SemanticStatus, TextEdit, ValidationStep, csharp::empty_plan,
};

pub fn plan_migrate_type_usages(p: &RefactorPlanParams) -> Result<String> {
    let project_dir = p
        .project_dir
        .as_deref()
        .ok_or_else(|| anyhow!("project_dir is required for migrate_csharp_type_usages"))?;
    let project_root = PathBuf::from(project_dir);
    if !project_root.is_dir() {
        bail!(
            "error.project_dir_not_a_directory: `{}`",
            project_root.display()
        );
    }
    let old_text = p
        .old_text
        .as_deref()
        .ok_or_else(|| anyhow!("old_text is required for migrate_csharp_type_usages"))?;
    let new_text = p
        .new_text
        .as_deref()
        .ok_or_else(|| anyhow!("new_text is required for migrate_csharp_type_usages"))?;
    validate_simple_identifier(old_text, "old_text")?;
    validate_simple_identifier(new_text, "new_text")?;
    if old_text == new_text {
        bail!("migrate_csharp_type_usages requires different old_text and new_text");
    }

    let mut file_edits = Vec::new();
    let mut total_matches = 0usize;
    for entry in WalkDir::new(&project_root)
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
        let text_edits = find_identifier_occurrences(&source, old_text, new_text);
        if text_edits.is_empty() {
            continue;
        }
        total_matches += text_edits.len();
        let mut hasher = Sha256::new();
        hasher.update(source.as_bytes());
        let sha = format!("{:x}", hasher.finalize());
        file_edits.push(FileEdit {
            path: path.to_string_lossy().to_string(),
            original_sha256: sha,
            edits: text_edits,
            new_text: None,
        });
    }

    if file_edits.is_empty() {
        bail!(
            "error.no_matches: no identifier-bounded occurrences of `{old_text}` under {}",
            project_root.display()
        );
    }

    let validations: Vec<ValidationStep> = file_edits
        .iter()
        .map(|edit| ValidationStep::TreeSitterNoErrors {
            path: edit.path.clone(),
            byte_range: None,
        })
        .collect();

    let mut plan = empty_plan(
        "migrate_csharp_type_usages",
        format!(
            "rename `{old_text}` → `{new_text}` across {} ({total_matches} occurrence(s) in {} file(s))",
            project_root.display(),
            file_edits.len()
        ),
        SemanticStatus::SyntaxOnly,
    );
    plan.validations = validations;
    plan.edits = file_edits;
    Ok(serde_json::to_string_pretty(&plan)?)
}

fn validate_simple_identifier(name: &str, field: &str) -> Result<()> {
    if name.is_empty() {
        bail!("error.invalid_csharp_identifier: `{field}` is empty");
    }
    let body = name.strip_prefix('@').unwrap_or(name);
    let mut chars = body.chars();
    let first = chars.next().unwrap();
    if !(first.is_ascii_alphabetic() || first == '_') {
        bail!(
            "error.invalid_csharp_identifier: `{field}=\"{name}\"` must start with letter or underscore"
        );
    }
    for c in chars {
        if !(c.is_ascii_alphanumeric() || c == '_') {
            bail!(
                "error.invalid_csharp_identifier: `{field}=\"{name}\"` contains invalid character `{c}`"
            );
        }
    }
    Ok(())
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

fn find_identifier_occurrences(source: &str, old: &str, new: &str) -> Vec<TextEdit> {
    let bytes = source.as_bytes();
    let needle = old.as_bytes();
    let mut edits = Vec::new();
    let mut i = 0usize;
    while i < bytes.len() {
        if let Some(next) = skip_lex_atom(bytes, i) {
            i = next;
            continue;
        }
        if i + needle.len() > bytes.len() {
            break;
        }
        if &bytes[i..i + needle.len()] == needle {
            let before_ok = i == 0 || !is_ident_char(bytes[i - 1]);
            let after_ok =
                i + needle.len() == bytes.len() || !is_ident_char(bytes[i + needle.len()]);
            if before_ok && after_ok {
                edits.push(TextEdit {
                    byte_start: i,
                    byte_end: i + needle.len(),
                    replacement: new.to_string(),
                });
                i += needle.len();
                continue;
            }
        }
        i += 1;
    }
    edits
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(project_dir: &Path, old: &str, new: &str) -> RefactorPlanParams {
        RefactorPlanParams {
            kind: "migrate_csharp_type_usages".to_string(),
            source: ".".to_string(),
            project_dir: Some(project_dir.to_string_lossy().to_string()),
            old_text: Some(old.to_string()),
            new_text: Some(new.to_string()),
            ..Default::default()
        }
    }

    #[test]
    fn refuses_identical_names() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("Foo.cs"), "class Foo {}").unwrap();
        let err = plan_migrate_type_usages(&p(dir.path(), "Foo", "Foo")).unwrap_err();
        assert!(err.to_string().contains("different old_text and new_text"));
    }

    #[test]
    fn refuses_when_no_matches() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("Bar.cs"), "class Bar {}").unwrap();
        let err = plan_migrate_type_usages(&p(dir.path(), "Foo", "Foo2")).unwrap_err();
        assert!(err.to_string().contains("no_matches"));
    }

    #[test]
    fn renames_identifier_bounded_only() {
        let src = r#"public class Foo {
    public Foo() {}
    public string Older = "Foo";
    // Foo in comment
    public string Pattern = "abc Foo def";
}
"#;
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("Foo.cs"), src).unwrap();
        let json = plan_migrate_type_usages(&p(dir.path(), "Foo", "Bar")).unwrap();
        let plan: serde_json::Value = serde_json::from_str(&json).unwrap();
        let text_edits: Vec<TextEdit> =
            serde_json::from_value(plan["edits"][0]["edits"].clone()).unwrap();
        // Two valid hits: `class Foo` and `public Foo()`.
        // `Older` (substring), `"Foo"` (string), `// Foo` (comment),
        // `"abc Foo def"` (string) all skipped.
        assert_eq!(text_edits.len(), 2, "{text_edits:?}");
    }

    #[test]
    fn skips_generated_and_bin_dirs() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("Foo.cs"), "class Foo {}").unwrap();
        std::fs::write(dir.path().join("Foo.g.cs"), "class Foo {}").unwrap();
        std::fs::create_dir_all(dir.path().join("bin")).unwrap();
        std::fs::write(dir.path().join("bin").join("Foo.cs"), "class Foo {}").unwrap();
        let json = plan_migrate_type_usages(&p(dir.path(), "Foo", "Bar")).unwrap();
        let plan: serde_json::Value = serde_json::from_str(&json).unwrap();
        let files = plan["edits"].as_array().unwrap();
        assert_eq!(files.len(), 1, "{plan:?}");
        let path = files[0]["path"].as_str().unwrap();
        assert!(path.ends_with("Foo.cs") && !path.contains(".g.cs"));
    }
}
