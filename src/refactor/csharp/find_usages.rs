//! `find_csharp_usages` — Roslyn LSP textDocument/references analysis.
//!
//! Analysis-only. Given a `source` file + position (item_names[0]
//! identifies the symbol), enumerates references across the workspace
//! the Roslyn LSP has loaded.
//!
//! RX-V3 fail-closed: missing LSP returns `error.lsp_unavailable`.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow};
use lsp_types::{
    ReferenceContext, ReferenceParams, TextDocumentIdentifier, TextDocumentItem,
    TextDocumentPositionParams, request::References,
};
use reqwest::Url;
use serde::{Deserialize, Serialize};

use crate::lsp::convert;
use crate::projects::Language;
use crate::refactor::{PlanContext, RefactorPlanParams, csharp::lsp_rename_helpers};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UsageSite {
    pub path: String,
    pub line: u32,
    pub character: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FindUsagesReport {
    pub kind: String,
    pub source: String,
    pub symbol: String,
    pub usage_count: usize,
    pub usages: Vec<UsageSite>,
    /// Set when the LSP refused to find the symbol at the supplied
    /// position. Distinguishes "no usages" from "couldn't resolve the
    /// symbol".
    pub symbol_resolved: bool,
}

pub fn plan_find_csharp_usages(p: &RefactorPlanParams, ctx: &PlanContext) -> Result<String> {
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

    let symbol = p
        .item_names
        .as_deref()
        .and_then(|names| names.first())
        .map(String::as_str)
        .or(p.old_text.as_deref())
        .ok_or_else(|| anyhow!("item_names[0] or old_text is required for find_csharp_usages"))?;

    let source = fs::read_to_string(&source_path)
        .with_context(|| format!("reading {}", source_path.display()))?;
    let position_byte = lsp_rename_helpers::find_first_identifier_byte(&source, symbol)
        .ok_or_else(|| anyhow!("symbol `{symbol}` not found in {}", source_path.display()))?;
    let position = convert::byte_to_lsp_position(&source, position_byte);

    let manager = ctx.lsp.as_ref().ok_or_else(|| {
        anyhow!(
            "error.lsp_unavailable: find_csharp_usages requires the LSP session manager (RX-V3)"
        )
    })?;

    let source_uri = Url::from_file_path(&source_path)
        .map_err(|_| anyhow!("failed to convert {} to file URL", source_path.display()))?;
    let params = ReferenceParams {
        text_document_position: TextDocumentPositionParams {
            text_document: TextDocumentIdentifier {
                uri: source_uri.clone(),
            },
            position,
        },
        context: ReferenceContext {
            include_declaration: true,
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
            let id = client.send_request::<References>(&params)?;
            client.read_response::<References>(id)
        })
        .map_err(|e| anyhow!("error.lsp_unavailable: {e}"))?;

    let (usages, resolved) = match response {
        Some(locations) => {
            let usages: Vec<UsageSite> = locations
                .into_iter()
                .map(|loc| UsageSite {
                    path: loc
                        .uri
                        .to_file_path()
                        .map(|p| p.to_string_lossy().to_string())
                        .unwrap_or_else(|_| loc.uri.to_string()),
                    line: loc.range.start.line,
                    character: loc.range.start.character,
                })
                .collect();
            (usages, true)
        }
        None => (Vec::new(), false),
    };

    let report = FindUsagesReport {
        kind: "find_csharp_usages".to_string(),
        source: path_string(&source_path),
        symbol: symbol.to_string(),
        usage_count: usages.len(),
        usages,
        symbol_resolved: resolved,
    };

    Ok(serde_json::to_string_pretty(&report)?)
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
