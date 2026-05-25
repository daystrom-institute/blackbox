//! `csharp_lsp_move_item` — Roslyn LSP-backed move via code action.
//!
//! Mirrors `rust_ra_move_item_to_module` but speaks to
//! `Microsoft.CodeAnalysis.LanguageServer`. Roslyn's LSP exposes
//! several refactoring code actions; we ask for `refactor.move`
//! and apply the returned `WorkspaceEdit`.
//!
//! Capability probing: Microsoft.CodeAnalysis.LanguageServer does
//! not guarantee `refactor.move` is advertised on every version. If
//! the server returns no `refactor.move` action for the supplied
//! range, the plan fails closed with
//! `error.lsp_code_action_unavailable` per the design doc Phase 1
//! decision; the operator falls back to `move_csharp_type_to_file`
//! for the syntax-only path.
//!
//! Inputs:
//!   - source = file containing the item
//!   - target = destination file
//!   - item_names[0] = simple name of the type to move
//!
//! RX-V3 fail-closed: missing LSP session manager → error.lsp_unavailable.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use lsp_types::{
    CodeActionContext, CodeActionKind, CodeActionOrCommand, CodeActionParams, Position, Range,
    TextDocumentIdentifier, TextDocumentItem, request::CodeActionRequest,
};
use reqwest::Url;

use super::lex::{is_word_boundary, match_keyword, read_ident, skip_lex_atom, skip_whitespace};
use crate::projects::Language;
use crate::lsp::convert;
use crate::refactor::{
    PlanContext, RefactorPlanParams, SemanticStatus, ValidationStep, csharp::empty_plan,
};

pub fn plan_lsp_move_item(p: &RefactorPlanParams, ctx: &PlanContext) -> Result<String> {
    let source_path = resolve_path(p.project_dir.as_deref(), &p.source)?;
    let target_path = p
        .target
        .as_deref()
        .map(|t| resolve_path(p.project_dir.as_deref(), t))
        .transpose()?
        .ok_or_else(|| anyhow!("target is required for csharp_lsp_move_item"))?;
    if source_path == target_path {
        bail!("csharp_lsp_move_item requires source != target");
    }
    let type_name = p
        .item_names
        .as_deref()
        .and_then(|names| names.first())
        .map(String::as_str)
        .ok_or_else(|| {
            anyhow!("item_names[0] (target type) is required for csharp_lsp_move_item")
        })?;

    let project_dir = p
        .project_dir
        .as_deref()
        .map(|d| resolve_path(None, d))
        .transpose()?
        .unwrap_or_else(|| {
            crate::entity_ref::git_root_for_path(&source_path)
                .unwrap_or_else(|| source_path.parent().unwrap_or(Path::new(".")).to_path_buf())
        });

    let source = fs::read_to_string(&source_path)
        .with_context(|| format!("reading {}", source_path.display()))?;

    let type_byte = locate_type_decl(&source, type_name)?;
    let type_position = convert::byte_to_lsp_position(&source, type_byte);

    let manager = ctx.lsp.as_ref().ok_or_else(|| {
        anyhow!(
            "error.lsp_unavailable: csharp_lsp_move_item requires the LSP session manager (RX-V3)"
        )
    })?;

    let source_uri = Url::from_file_path(&source_path)
        .map_err(|_| anyhow!("failed to convert {} to file URL", source_path.display()))?;
    let range = Range {
        start: type_position,
        end: type_position,
    };
    let only_kinds = vec![
        CodeActionKind::new("refactor.move"),
        CodeActionKind::new("refactor.move.file"),
    ];
    let code_action_params = CodeActionParams {
        text_document: TextDocumentIdentifier {
            uri: source_uri.clone(),
        },
        range,
        context: CodeActionContext {
            diagnostics: vec![],
            only: Some(only_kinds.clone()),
            trigger_kind: None,
        },
        work_done_progress_params: Default::default(),
        partial_result_params: Default::default(),
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
            client.wait_for_diagnostics(source_uri.as_str(), std::time::Duration::from_secs(60));
            let id = client.send_request::<CodeActionRequest>(&code_action_params)?;
            client.read_response::<CodeActionRequest>(id)
        })
        .map_err(|e| anyhow!("error.lsp_unavailable: {e}"))?;

    let actions = response.unwrap_or_default();
    let mut found_workspace_edit = None;
    for entry in actions {
        if let CodeActionOrCommand::CodeAction(action) = entry {
            // Match anything under the refactor.move family. Some
            // server versions tag as `refactor.move`, others as
            // `refactor.move.file` or even just `refactor`.
            let kind_match = action
                .kind
                .as_ref()
                .map(|k| {
                    let s = k.as_str();
                    s.starts_with("refactor.move") || s.starts_with("refactor.move.file")
                })
                .unwrap_or(false);
            if !kind_match {
                continue;
            }
            // Some versions name the action like "Move type to TargetFile.cs"
            // — prefer the action whose title mentions the target file
            // basename, so we don't accept a generic "move to namespace"
            // refactor that doesn't actually emit a file write.
            let target_basename = target_path
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("");
            let title_match = action
                .title
                .to_ascii_lowercase()
                .contains(&target_basename.to_ascii_lowercase());
            if let Some(workspace_edit) = action.edit {
                if title_match || found_workspace_edit.is_none() {
                    found_workspace_edit = Some(workspace_edit);
                    if title_match {
                        break;
                    }
                }
            }
        }
    }

    let workspace_edit = found_workspace_edit.ok_or_else(|| {
        anyhow!(
            "error.lsp_code_action_unavailable: Roslyn LSP did not advertise a `refactor.move` code action for `{type_name}` in {}; fall back to move_csharp_type_to_file for the syntax-only path",
            source_path.display()
        )
    })?;

    let file_edits = convert::workspace_edit_to_file_edits(workspace_edit)?;
    if file_edits.is_empty() {
        bail!(
            "error.empty_workspace_edit: Roslyn LSP returned a `refactor.move` action with no edits"
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
        "csharp_lsp_move_item",
        format!(
            "Roslyn refactor.move `{type_name}` from {} to {}",
            path_string(&source_path),
            path_string(&target_path)
        ),
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

fn path_string(path: &Path) -> String {
    path.to_str().unwrap_or("").to_string()
}

/// Walk the source for the type-declaration position (the first byte
/// of the type-name identifier). The LSP uses this position to scope
/// the code action.
fn locate_type_decl(source: &str, type_name: &str) -> Result<usize> {
    let bytes = source.as_bytes();
    let keywords = [
        b"class".as_ref(),
        b"record",
        b"struct",
        b"interface",
        b"enum",
        b"delegate",
    ];
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
        for kw in keywords {
            if let Some(after_kw) = match_keyword(bytes, i, kw) {
                let name_start = skip_whitespace(bytes, after_kw);
                let (parsed, _name_end) = read_ident(bytes, name_start);
                if parsed == type_name {
                    return Ok(name_start);
                }
                i = after_kw;
                break;
            }
        }
        i += 1;
    }
    let _ = Position {
        line: 0,
        character: 0,
    };
    bail!("error.type_not_found: `{type_name}` not found as a top-level type")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn locate_type_decl_finds_class() {
        let src = "namespace Foo;\n\npublic class Service {\n    public int X { get; }\n}\n";
        let pos = locate_type_decl(src, "Service").unwrap();
        assert_eq!(&src[pos..pos + 7], "Service");
    }

    #[test]
    fn locate_type_decl_returns_err_on_missing() {
        let src = "public class Foo {}";
        let err = locate_type_decl(src, "Bar").unwrap_err();
        assert!(err.to_string().contains("type_not_found"));
    }
}
