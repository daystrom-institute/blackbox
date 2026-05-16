//! `csharp_lsp_rename` — Roslyn LSP-backed workspace rename.
//!
//! Phase 1 write smoke. Mirrors `rust_lsp_rename` (`src/refactor/rust.rs`)
//! but speaks to `Microsoft.CodeAnalysis.LanguageServer` via the
//! shared `LspSessionManager`.
//!
//! RX-V3 fail-closed: when the LSP session manager is absent or the
//! Roslyn binary is unavailable, plan refuses with `error.lsp_unavailable`
//! rather than silently downgrading.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use lsp_types::{
    RenameParams, TextDocumentIdentifier, TextDocumentItem,
    TextDocumentPositionParams, request::Rename,
};
use reqwest::Url;

use crate::projects::Language;
use crate::refactor::{
    PlanContext, RefactorPlanParams, SemanticStatus, ValidationStep,
    csharp::empty_plan, rust,
};

pub fn plan_lsp_rename(p: &RefactorPlanParams, ctx: &PlanContext) -> Result<String> {
    let source_path = resolve_path(p.project_dir.as_deref(), &p.source)?;
    let project_dir = p
        .project_dir
        .as_deref()
        .map(|dir| resolve_path(None, dir))
        .transpose()?
        .unwrap_or_else(|| {
            crate::entity_ref::git_root_for_path(&source_path)
                .unwrap_or_else(|| source_path.parent().unwrap_or(Path::new(".")).to_path_buf())
        });

    let old_name = p
        .item_names
        .as_deref()
        .and_then(|names| names.first())
        .map(String::as_str)
        .or(p.old_text.as_deref())
        .ok_or_else(|| anyhow!("item_names[0] or old_text is required for csharp_lsp_rename"))?;
    validate_csharp_identifier(old_name, "item_names[0]")?;
    let new_name = p
        .new_text
        .as_deref()
        .ok_or_else(|| anyhow!("new_text is required for csharp_lsp_rename"))?;
    validate_csharp_identifier(new_name, "new_text")?;
    if old_name == new_name {
        bail!("csharp_lsp_rename requires different old and new names");
    }

    let source = fs::read_to_string(&source_path)
        .with_context(|| format!("reading {}", source_path.display()))?;
    let position_byte = find_first_identifier_byte(&source, old_name).ok_or_else(|| {
        anyhow!(
            "old name `{old_name}` not found in {}",
            source_path.display()
        )
    })?;
    let position = rust::byte_to_lsp_position(&source, position_byte);

    let manager = ctx.lsp.as_ref().ok_or_else(|| {
        anyhow!(
            "error.lsp_unavailable: csharp_lsp_rename requires the LSP session manager (RX-V3)"
        )
    })?;

    let source_uri = Url::from_file_path(&source_path)
        .map_err(|_| anyhow!("failed to convert {} to file URL", source_path.display()))?;
    let rename_params = RenameParams {
        text_document_position: TextDocumentPositionParams {
            text_document: TextDocumentIdentifier {
                uri: source_uri.clone(),
            },
            position,
        },
        new_name: new_name.to_string(),
        work_done_progress_params: Default::default(),
    };

    let response = manager
        .with_session(&project_dir, Language::Csharp, |mut client| {
            client.send_notification::<lsp_types::notification::DidOpenTextDocument>(
                &lsp_types::DidOpenTextDocumentParams {
                    text_document: TextDocumentItem {
                        uri: source_uri.clone(),
                        language_id: "csharp".to_string(),
                        version: 0,
                        text: source.clone(),
                    },
                },
            )?;
            // Roslyn LSP indexes lazily on didOpen; drain diagnostics
            // before the rename request so symbol analysis is ready.
            client.wait_for_diagnostics(source_uri.as_str(), std::time::Duration::from_secs(60));
            let id = client.send_request::<Rename>(&rename_params)?;
            client.read_response::<Rename>(id)
        })
        .map_err(|e| anyhow!("error.lsp_unavailable: {e}"))?;

    let file_edits = if let Some(edit) = response {
        rust::workspace_edit_to_file_edits(edit)?
    } else {
        Vec::new()
    };

    if file_edits.is_empty() {
        bail!("Roslyn LSP returned no edits for rename `{old_name}` to `{new_name}`");
    }

    let validations: Vec<ValidationStep> = file_edits
        .iter()
        .map(|edit| ValidationStep::TreeSitterNoErrors {
            path: edit.path.clone(),
            byte_range: None,
        })
        .collect();

    let mut plan = empty_plan(
        "csharp_lsp_rename",
        format!("Roslyn rename {old_name} to {new_name}"),
        SemanticStatus::LspVerified,
    );
    plan.edits = file_edits;
    plan.validations = validations;
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

/// Scan `source` for the first identifier-bounded occurrence of `name`.
/// Returns the byte offset of the first character. Mirrors the
/// `rust_rename_position_byte` heuristic — the LSP server resolves the
/// actual symbol semantically; we just need a position inside the
/// identifier token.
fn find_first_identifier_byte(source: &str, name: &str) -> Option<usize> {
    let bytes = source.as_bytes();
    let needle = name.as_bytes();
    let mut i = 0usize;
    while i + needle.len() <= bytes.len() {
        if &bytes[i..i + needle.len()] == needle {
            let before_ok = i == 0 || !is_ident_char(bytes[i - 1]);
            let after_ok =
                i + needle.len() == bytes.len() || !is_ident_char(bytes[i + needle.len()]);
            if before_ok && after_ok {
                return Some(i);
            }
        }
        i += 1;
    }
    None
}

fn is_ident_char(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

/// Minimal C# identifier check — matches the conservative shape the
/// Roslyn parser would accept for the common cases (no leading digit,
/// no whitespace, ASCII-only). The `@`-escaped contextual identifier
/// form is allowed.
fn validate_csharp_identifier(name: &str, field: &str) -> Result<()> {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_identifier_offset() {
        let src = "class Foo { void Bar() {} void Baz(int Bar) {} }";
        assert_eq!(find_first_identifier_byte(src, "Bar"), Some(17));
        assert_eq!(find_first_identifier_byte(src, "Baz"), Some(31));
        assert!(find_first_identifier_byte(src, "Quux").is_none());
    }

    #[test]
    fn validates_identifiers() {
        validate_csharp_identifier("Foo", "field").unwrap();
        validate_csharp_identifier("_bar", "field").unwrap();
        validate_csharp_identifier("@class", "field").unwrap();
        assert!(validate_csharp_identifier("", "field").is_err());
        assert!(validate_csharp_identifier("1bad", "field").is_err());
        assert!(validate_csharp_identifier("has space", "field").is_err());
    }
}
