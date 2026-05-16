//! `csharp_organize_usings` — Roslyn LSP source.organizeImports code action.
//!
//! Mirrors `rust_organize_imports`. RX-V3 fail-closed when the LSP
//! session manager is absent.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use lsp_types::{
    CodeActionContext, CodeActionKind, CodeActionOrCommand, CodeActionParams, Position, Range,
    TextDocumentIdentifier, TextDocumentItem, request::CodeActionRequest,
};
use reqwest::Url;

use crate::projects::Language;
use crate::refactor::{
    FileEdit, PlanContext, RefactorPlanParams, SemanticStatus, ValidationStep,
    csharp::empty_plan, rust,
};

pub fn plan_organize_usings(p: &RefactorPlanParams, ctx: &PlanContext) -> Result<String> {
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

    let manager = ctx.lsp.as_ref().ok_or_else(|| {
        anyhow!(
            "error.lsp_unavailable: csharp_organize_usings requires the LSP session manager (RX-V3)"
        )
    })?;

    let file_edits = roslyn_organize_usings(manager, &project_dir, &source_path)
        .map_err(|e| anyhow!("error.lsp_unavailable: {e}"))?;

    if file_edits.is_empty() {
        // Empty edit set is benign — the file is already organized.
        // Return an empty plan rather than an error so the atom can
        // continue without rolling back.
    }

    let validations: Vec<ValidationStep> = file_edits
        .iter()
        .map(|edit| ValidationStep::TreeSitterNoErrors {
            path: edit.path.clone(),
            byte_range: None,
        })
        .collect();

    let mut plan = empty_plan(
        "csharp_organize_usings",
        format!("Roslyn organize usings in {}", path_string(&source_path)),
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

fn roslyn_organize_usings(
    manager: &crate::lsp::LspSessionManager,
    project_dir: &Path,
    source_path: &Path,
) -> Result<Vec<FileEdit>> {
    let source_uri = Url::from_file_path(source_path)
        .map_err(|_| anyhow!("failed to convert {} to file URL", source_path.display()))?;
    let source = fs::read_to_string(source_path)
        .with_context(|| format!("reading {}", source_path.display()))?;
    let end_position = rust::byte_to_lsp_position(&source, source.len());
    let code_action_params = CodeActionParams {
        text_document: TextDocumentIdentifier { uri: source_uri.clone() },
        range: Range {
            start: Position { line: 0, character: 0 },
            end: end_position,
        },
        context: CodeActionContext {
            diagnostics: vec![],
            only: Some(vec![CodeActionKind::SOURCE_ORGANIZE_IMPORTS]),
            trigger_kind: None,
        },
        work_done_progress_params: Default::default(),
        partial_result_params: Default::default(),
    };

    let response = manager.with_session(project_dir, Language::Csharp, |mut client| {
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
    })?;

    let actions = response.unwrap_or_default();
    let mut edits = Vec::new();
    for entry in actions {
        if let CodeActionOrCommand::CodeAction(action) = entry
            && action.kind.as_ref() == Some(&CodeActionKind::SOURCE_ORGANIZE_IMPORTS)
            && let Some(workspace_edit) = action.edit
        {
            edits.extend(rust::workspace_edit_to_file_edits(workspace_edit)?);
        }
    }
    if edits.is_empty() {
        bail!(
            "Roslyn LSP returned no source.organizeImports code actions for {}",
            source_path.display()
        );
    }
    Ok(edits)
}
