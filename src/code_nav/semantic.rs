//! `java_find_usages` — JDTLS LSP textDocument/references analysis.
//!
//! Resolves binding-aware references to the Java symbol at a given
//! file position. Distinct from the syntax-only `bbox_code_refs` path.
//!
//! RX-V3 fail-closed: a missing or unavailable LSP session returns
//! `error.lsp_unavailable` instead of silently downgrading to a
//! syntactic guess.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow};
use lsp_types::{
    GotoDefinitionParams, GotoDefinitionResponse, HoverContents, Location, MarkedString,
    ReferenceContext, ReferenceParams, SymbolInformation, SymbolKind, TextDocumentIdentifier,
    TextDocumentItem, TextDocumentPositionParams, WorkspaceSymbol, WorkspaceSymbolParams,
    request::{GotoImplementation, HoverRequest, References, WorkspaceSymbolRequest},
};
use reqwest::Url;
use serde::{Deserialize, Serialize};

use crate::code_nav::{
    CodeProjectRefsHint, CodeProjectRefsHintArgs, CodeRefactorHandoff, CodeRefactorStatusHint,
    CodeRefactorStatusHintArgs,
};
use crate::lsp::LspSessionManager;
use crate::projects::Language;

/// Semantic status value reported by the LSP lane.
pub const SEMANTIC_STATUS_LSP_VERIFIED: &str = "lsp_verified";

/// A single resolved usage site returned by `java_find_usages`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UsageSite {
    pub path: String,
    pub line: u32,
    pub character: u32,
    /// Handoff hint pointing the agent to `bbox_refactor_status` /
    /// `bbox_refactor_project_refs` on this specific file.
    pub handoff: CodeRefactorHandoff,
}

/// Top-level report returned by `java_find_usages`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UsagesReport {
    pub kind: String,
    /// Always `"lsp_verified"` when this report is produced; the function
    /// fails closed rather than downgrading to a syntactic approximation.
    pub semantic_status: String,
    pub source: String,
    /// `true` when JDTLS resolved the symbol at the requested position.
    /// `false` means the LSP returned an empty/null result — "no usages"
    /// vs "couldn't resolve" is ambiguous at the LSP level, so we
    /// conservatively set this to `false` only when the response is `None`.
    pub symbol_resolved: bool,
    pub usage_count: usize,
    pub usages: Vec<UsageSite>,
}

/// Resolve references to the Java symbol at `(line, column)` in
/// `source_path` via JDTLS `textDocument/references`.
///
/// Fail-closed (RX-V3): returns `Err` with an `error.lsp_unavailable`
/// message when the session manager is unavailable or JDTLS fails to
/// initialise. Never returns a syntactic approximation labelled as
/// `lsp_verified`.
///
/// `line` and `column` are **0-based** LSP coordinates.
pub(crate) fn java_find_usages(
    manager: &LspSessionManager,
    project_dir: &Path,
    source_path: &Path,
    line: u32,
    column: u32,
) -> Result<UsagesReport> {
    let source = fs::read_to_string(source_path)
        .with_context(|| format!("reading {}", source_path.display()))?;

    let source_uri = Url::from_file_path(source_path)
        .map_err(|_| anyhow!("failed to convert {} to file URL", source_path.display()))?;

    // Convert the caller-supplied 0-based (line, column) into an LSP
    // Position.  We construct it directly because byte_to_lsp_position
    // takes a byte offset; the caller already knows the LSP coordinates.
    let position = lsp_types::Position { line, character: column };

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
        .with_session(project_dir, Language::Java, |mut client| {
            // Open the file and wait for diagnostics so JDTLS has indexed
            // and type-checked it before we ask for references — same
            // pattern as jdtls_organize_imports.
            client.send_notification::<lsp_types::notification::DidOpenTextDocument>(
                &lsp_types::DidOpenTextDocumentParams {
                    text_document: TextDocumentItem {
                        uri: source_uri.clone(),
                        language_id: "java".to_string(),
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

    let project_dir_str = project_dir.to_string_lossy().to_string();
    let source_str = source_path.to_string_lossy().to_string();

    let (usages, symbol_resolved) = match response {
        Some(locations) => {
            let usages = locations
                .into_iter()
                .map(|loc| {
                    let path = loc
                        .uri
                        .to_file_path()
                        .map(|p| p.to_string_lossy().to_string())
                        .unwrap_or_else(|_| loc.uri.to_string());
                    let site_line = loc.range.start.line;
                    let site_char = loc.range.start.character;
                    let handoff = usage_site_handoff(&path, &project_dir_str);
                    UsageSite {
                        path,
                        line: site_line,
                        character: site_char,
                        handoff,
                    }
                })
                .collect();
            (usages, true)
        }
        None => (Vec::new(), false),
    };

    Ok(UsagesReport {
        kind: "java_find_usages".to_string(),
        semantic_status: SEMANTIC_STATUS_LSP_VERIFIED.to_string(),
        source: source_str,
        symbol_resolved,
        usage_count: usages.len(),
        usages,
    })
}

/// Build a minimal `CodeRefactorHandoff` for a single LSP-resolved usage
/// site.  Unlike `refactor_handoff` (which needs a live tree-sitter node),
/// this constructs the handoff from the file path alone — appropriate when
/// we have a location from the LSP but have not re-parsed the file.
fn usage_site_handoff(file: &str, project_dir: &str) -> CodeRefactorHandoff {
    CodeRefactorHandoff {
        nearest_refactor_item: None,
        refactor_status: Some(CodeRefactorStatusHint {
            tool: "bbox_refactor_status".to_string(),
            arguments: CodeRefactorStatusHintArgs {
                file: file.to_string(),
                project_dir: Some(project_dir.to_string()),
                item_names: vec![],
                item_kinds: vec![],
                limit: 50,
                include_attributes: false,
            },
        }),
        project_refs: CodeProjectRefsHint {
            tool: "bbox_refactor_project_refs".to_string(),
            arguments: CodeProjectRefsHintArgs {
                file: file.to_string(),
                project_dir: Some(project_dir.to_string()),
                query: None,
                limit: 20,
                include_excerpt: false,
            },
        },
        note: "LSP-resolved Java usage site (semantic_status=lsp_verified). Use bbox_refactor_status to inspect the enclosing item before planning edits; use bbox_refactor_project_refs for current project_file entity refs.".to_string(),
    }
}

/// Resolve a path that may be absolute or project-relative.
pub(crate) fn resolve_path_for_usages(project_dir: Option<&str>, file: &str) -> Result<PathBuf> {
    let candidate = PathBuf::from(file);
    if candidate.is_absolute() {
        return Ok(candidate);
    }
    let base = match project_dir {
        Some(p) => PathBuf::from(p),
        None => std::env::current_dir().context("getting current directory")?,
    };
    Ok(base.join(candidate))
}

/// Resolve `project_dir`: if supplied use it, otherwise walk up from
/// `source_path` to the git root, or fall back to the file's parent.
pub(crate) fn resolve_project_dir_for_usages(
    project_dir: Option<&str>,
    source_path: &Path,
) -> PathBuf {
    project_dir
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            crate::entity_ref::git_root_for_path(source_path)
                .unwrap_or_else(|| {
                    source_path
                        .parent()
                        .unwrap_or(Path::new("."))
                        .to_path_buf()
                })
        })
}

/// A single resolved implementation site returned by
/// `java_find_implementations`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImplementationSite {
    pub path: String,
    pub line: u32,
    pub character: u32,
    /// Handoff hint pointing the agent to `bbox_refactor_status` /
    /// `bbox_refactor_project_refs` on this specific file.
    pub handoff: CodeRefactorHandoff,
}

/// Top-level report returned by `java_find_implementations`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImplementationsReport {
    pub kind: String,
    pub semantic_status: String,
    pub source: String,
    /// `true` when JDTLS resolved the symbol at the requested position.
    pub symbol_resolved: bool,
    pub site_count: usize,
    pub sites: Vec<ImplementationSite>,
}

/// Top-level report returned by `java_type_at`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TypeAtReport {
    pub kind: String,
    pub semantic_status: String,
    pub source: String,
    /// `true` when JDTLS returned hover content for the position.
    pub resolved: bool,
    /// Flattened hover contents as a single string. Empty when
    /// `resolved` is `false`.
    pub contents: String,
    /// Handoff for the source file at the queried position.
    pub handoff: CodeRefactorHandoff,
}

/// A single workspace symbol returned by `java_workspace_symbols`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkspaceSymbolItem {
    /// Symbol name (e.g., class name, method name).
    pub name: String,
    /// Symbol kind as a string representation of the LSP SymbolKind.
    pub kind: String,
    /// Symbol kind as a numeric value (LSP SymbolKind enum value).
    pub kind_number: u32,
    /// File path containing this symbol.
    pub path: String,
    /// 0-based line number of the symbol's location.
    pub line: u32,
    /// 0-based character offset of the symbol's location.
    pub character: u32,
    /// Handoff hint pointing the agent to `bbox_refactor_status` /
    /// `bbox_refactor_project_refs` on this specific file.
    pub handoff: CodeRefactorHandoff,
}

/// Top-level report returned by `java_workspace_symbols`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkspaceSymbolsReport {
    pub kind: String,
    /// Always `"lsp_verified"` when this report is produced.
    pub semantic_status: String,
    /// The query string that was searched.
    pub query: String,
    /// `true` when JDTLS returned symbols for the query.
    /// `false` only when the response is `None` (no symbols found).
    pub resolved: bool,
    /// Total number of symbols returned.
    pub symbol_count: usize,
    /// List of matching workspace symbols.
    pub symbols: Vec<WorkspaceSymbolItem>,
}

/// Resolve implementations of the Java symbol at `(line, column)` in
/// `source_path` via JDTLS `textDocument/implementation`.
///
/// Fail-closed (RX-V3): returns `Err` with an `error.lsp_unavailable`
/// message when the session manager is unavailable or JDTLS fails to
/// initialise.
///
/// `line` and `column` are **0-based** LSP coordinates.
pub(crate) fn java_find_implementations(
    manager: &LspSessionManager,
    project_dir: &Path,
    source_path: &Path,
    line: u32,
    column: u32,
) -> Result<ImplementationsReport> {
    let source = fs::read_to_string(source_path)
        .with_context(|| format!("reading {}", source_path.display()))?;

    let source_uri = Url::from_file_path(source_path)
        .map_err(|_| anyhow!("failed to convert {} to file URL", source_path.display()))?;

    let position = lsp_types::Position { line, character: column };

    let params = GotoDefinitionParams {
        text_document_position_params: TextDocumentPositionParams {
            text_document: TextDocumentIdentifier {
                uri: source_uri.clone(),
            },
            position,
        },
        work_done_progress_params: Default::default(),
        partial_result_params: Default::default(),
    };

    let response = manager
        .with_session(project_dir, Language::Java, |mut client| {
            client.send_notification::<lsp_types::notification::DidOpenTextDocument>(
                &lsp_types::DidOpenTextDocumentParams {
                    text_document: TextDocumentItem {
                        uri: source_uri.clone(),
                        language_id: "java".to_string(),
                        version: 0,
                        text: source.clone(),
                    },
                },
            )?;
            client.wait_for_diagnostics(source_uri.as_str(), std::time::Duration::from_secs(60));
            let id = client.send_request::<GotoImplementation>(&params)?;
            client.read_response::<GotoImplementation>(id)
        })
        .map_err(|e| anyhow!("error.lsp_unavailable: {e}"))?;

    let project_dir_str = project_dir.to_string_lossy().to_string();
    let source_str = source_path.to_string_lossy().to_string();

    // Normalize GotoDefinitionResponse (Scalar/Array/Link) → Vec<Location>.
    let locations: Vec<Location> = match response {
        Some(GotoDefinitionResponse::Scalar(loc)) => vec![loc],
        Some(GotoDefinitionResponse::Array(locs)) => locs,
        Some(GotoDefinitionResponse::Link(links)) => links
            .into_iter()
            .map(|link| Location {
                uri: link.target_uri,
                range: link.target_range,
            })
            .collect(),
        None => Vec::new(),
    };

    let sites: Vec<ImplementationSite> = locations
        .into_iter()
        .map(|loc| {
            let path = loc
                .uri
                .to_file_path()
                .map(|p| p.to_string_lossy().to_string())
                .unwrap_or_else(|_| loc.uri.to_string());
            let site_line = loc.range.start.line;
            let site_char = loc.range.start.character;
            let handoff = usage_site_handoff(&path, &project_dir_str);
            ImplementationSite {
                path,
                line: site_line,
                character: site_char,
                handoff,
            }
        })
        .collect();

    let symbol_resolved = !sites.is_empty();

    Ok(ImplementationsReport {
        kind: "java_find_implementations".to_string(),
        semantic_status: SEMANTIC_STATUS_LSP_VERIFIED.to_string(),
        source: source_str,
        symbol_resolved,
        site_count: sites.len(),
        sites,
    })
}

/// Resolve the type/signature/documentation at a Java position via JDTLS
/// `textDocument/hover`.
///
/// Fail-closed (RX-V3): returns `Err` with an `error.lsp_unavailable`
/// message when the session manager is unavailable or JDTLS fails to
/// initialise.
///
/// `line` and `column` are **0-based** LSP coordinates.
pub(crate) fn java_type_at(
    manager: &LspSessionManager,
    project_dir: &Path,
    source_path: &Path,
    line: u32,
    column: u32,
) -> Result<TypeAtReport> {
    let source = fs::read_to_string(source_path)
        .with_context(|| format!("reading {}", source_path.display()))?;

    let source_uri = Url::from_file_path(source_path)
        .map_err(|_| anyhow!("failed to convert {} to file URL", source_path.display()))?;

    let position = lsp_types::Position { line, character: column };

    let params = lsp_types::HoverParams {
        text_document_position_params: TextDocumentPositionParams {
            text_document: TextDocumentIdentifier {
                uri: source_uri.clone(),
            },
            position,
        },
        work_done_progress_params: Default::default(),
    };

    let response = manager
        .with_session(project_dir, Language::Java, |mut client| {
            client.send_notification::<lsp_types::notification::DidOpenTextDocument>(
                &lsp_types::DidOpenTextDocumentParams {
                    text_document: TextDocumentItem {
                        uri: source_uri.clone(),
                        language_id: "java".to_string(),
                        version: 0,
                        text: source.clone(),
                    },
                },
            )?;
            client.wait_for_diagnostics(source_uri.as_str(), std::time::Duration::from_secs(60));
            let id = client.send_request::<HoverRequest>(&params)?;
            client.read_response::<HoverRequest>(id)
        })
        .map_err(|e| anyhow!("error.lsp_unavailable: {e}"))?;

    let project_dir_str = project_dir.to_string_lossy().to_string();
    let source_str = source_path.to_string_lossy().to_string();

    let (resolved, contents) = match response {
        Some(hover) => {
            let flat = flatten_hover_contents(&hover.contents);
            (true, flat)
        }
        None => (false, String::new()),
    };

    let handoff = usage_site_handoff(&source_str, &project_dir_str);

    Ok(TypeAtReport {
        kind: "java_type_at".to_string(),
        semantic_status: SEMANTIC_STATUS_LSP_VERIFIED.to_string(),
        source: source_str,
        resolved,
        contents,
        handoff,
    })
}

/// Resolve workspace symbols matching a query via JDTLS
/// `workspace/symbol`.
///
/// Fail-closed (RX-V3): returns `Err` with an `error.lsp_unavailable`
/// message when the session manager is unavailable or JDTLS fails to
/// initialise. Never returns a syntactic approximation labelled as
/// `lsp_verified`.
///
/// Unlike file-position queries (usages, implementations, type_at),
/// workspace/symbol queries the **whole indexed workspace** and does
/// NOT require didOpen — the server maintains its own workspace index.
pub(crate) fn java_workspace_symbols(
    manager: &LspSessionManager,
    project_dir: &Path,
    query: &str,
) -> Result<WorkspaceSymbolsReport> {
    let project_dir_str = project_dir.to_string_lossy().to_string();

    let params = WorkspaceSymbolParams {
        query: query.to_string(),
        work_done_progress_params: Default::default(),
        partial_result_params: Default::default(),
    };

    let response = manager
        .with_session(project_dir, Language::Java, |mut client| {
            let id = client.send_request::<WorkspaceSymbolRequest>(&params)?;
            client.read_response::<WorkspaceSymbolRequest>(id)
        })
        .map_err(|e| anyhow!("error.lsp_unavailable: {e}"))?;

    // Normalize the response: WorkspaceSymbolResponse is
    // Flat(Vec<SymbolInformation>) | Nested(Vec<WorkspaceSymbol>)
    // We flatten both into a unified list of symbol items.
    let symbols = match response {
        Some(lsp_types::WorkspaceSymbolResponse::Flat(symbols)) => {
            symbols
                .into_iter()
                .map(|sym| symbol_info_to_item(&sym, &project_dir_str))
                .collect()
        }
        Some(lsp_types::WorkspaceSymbolResponse::Nested(symbols)) => {
            flatten_workspace_symbols(symbols, &project_dir_str)
        }
        None => Vec::new(),
    };

    let resolved = !symbols.is_empty();

    Ok(WorkspaceSymbolsReport {
        kind: "java_workspace_symbols".to_string(),
        semantic_status: SEMANTIC_STATUS_LSP_VERIFIED.to_string(),
        query: query.to_string(),
        resolved,
        symbol_count: symbols.len(),
        symbols,
    })
}

/// Convert a SymbolInformation to a WorkspaceSymbolItem.
fn symbol_info_to_item(sym: &SymbolInformation, project_dir: &str) -> WorkspaceSymbolItem {
    let path = sym
        .location
        .uri
        .to_file_path()
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_else(|_| sym.location.uri.to_string());
    let (kind_str, kind_number) = symbol_kind_info(sym.kind);
    let handoff = usage_site_handoff(&path, project_dir);
    WorkspaceSymbolItem {
        name: sym.name.clone(),
        kind: kind_str,
        kind_number,
        path,
        line: sym.location.range.start.line,
        character: sym.location.range.start.character,
        handoff,
    }
}

/// Convert a WorkspaceSymbol to a WorkspaceSymbolItem.
/// WorkspaceSymbol.location is OneOf<Location, WorkspaceLocation>.
fn workspace_symbol_to_item(sym: WorkspaceSymbol, project_dir: &str) -> Option<WorkspaceSymbolItem> {
    let (kind_str, kind_number) = symbol_kind_info(sym.kind);

    let (path, line, character) = match sym.location {
        lsp_types::OneOf::Left(loc) => {
            let path = loc
                .uri
                .to_file_path()
                .map(|p| p.to_string_lossy().to_string())
                .unwrap_or_else(|_| loc.uri.to_string());
            (path, loc.range.start.line, loc.range.start.character)
        }
        lsp_types::OneOf::Right(ws_loc) => {
            // WorkspaceLocation only has uri, no range
            let path = ws_loc
                .uri
                .to_file_path()
                .map(|p| p.to_string_lossy().to_string())
                .unwrap_or_else(|_| ws_loc.uri.to_string());
            (path, 0, 0)
        }
    };

    let handoff = usage_site_handoff(&path, project_dir);
    Some(WorkspaceSymbolItem {
        name: sym.name,
        kind: kind_str,
        kind_number,
        path,
        line,
        character,
        handoff,
    })
}

/// Flatten a Vec<WorkspaceSymbol> (nested) into Vec<WorkspaceSymbolItem>.
fn flatten_workspace_symbols(symbols: Vec<WorkspaceSymbol>, project_dir: &str) -> Vec<WorkspaceSymbolItem> {
    symbols
        .into_iter()
        .filter_map(|sym| workspace_symbol_to_item(sym, project_dir))
        .collect()
}

/// Convert an LSP SymbolKind to a human-readable string and its numeric value.
/// Returns (string_representation, numeric_value).
fn symbol_kind_info(kind: SymbolKind) -> (String, u32) {
    match kind {
        SymbolKind::FILE => ("file".to_string(), 1),
        SymbolKind::MODULE => ("module".to_string(), 2),
        SymbolKind::NAMESPACE => ("namespace".to_string(), 3),
        SymbolKind::PACKAGE => ("package".to_string(), 4),
        SymbolKind::CLASS => ("class".to_string(), 5),
        SymbolKind::METHOD => ("method".to_string(), 6),
        SymbolKind::PROPERTY => ("property".to_string(), 7),
        SymbolKind::FIELD => ("field".to_string(), 8),
        SymbolKind::CONSTRUCTOR => ("constructor".to_string(), 9),
        SymbolKind::ENUM => ("enum".to_string(), 10),
        SymbolKind::INTERFACE => ("interface".to_string(), 11),
        SymbolKind::FUNCTION => ("function".to_string(), 12),
        SymbolKind::VARIABLE => ("variable".to_string(), 13),
        SymbolKind::CONSTANT => ("constant".to_string(), 14),
        SymbolKind::STRING => ("string".to_string(), 15),
        SymbolKind::NUMBER => ("number".to_string(), 16),
        SymbolKind::BOOLEAN => ("boolean".to_string(), 17),
        SymbolKind::ARRAY => ("array".to_string(), 18),
        SymbolKind::OBJECT => ("object".to_string(), 19),
        SymbolKind::KEY => ("key".to_string(), 20),
        SymbolKind::NULL => ("null".to_string(), 21),
        SymbolKind::ENUM_MEMBER => ("enum_member".to_string(), 22),
        SymbolKind::STRUCT => ("struct".to_string(), 23),
        SymbolKind::EVENT => ("event".to_string(), 24),
        SymbolKind::OPERATOR => ("operator".to_string(), 25),
        SymbolKind::TYPE_PARAMETER => ("type_parameter".to_string(), 26),
        _ => (format!("unknown_{}", symbol_kind_to_number(kind)), symbol_kind_to_number(kind)),
    }
}

/// Convert an LSP SymbolKind to its numeric discriminant value.
/// SymbolKind is a newtype wrapper, so we match all known variants.
fn symbol_kind_to_number(kind: SymbolKind) -> u32 {
    // Match all variants to extract their known values
    // The lsp_enum! macro generates const values we can match on
    match kind {
        SymbolKind::FILE => 1,
        SymbolKind::MODULE => 2,
        SymbolKind::NAMESPACE => 3,
        SymbolKind::PACKAGE => 4,
        SymbolKind::CLASS => 5,
        SymbolKind::METHOD => 6,
        SymbolKind::PROPERTY => 7,
        SymbolKind::FIELD => 8,
        SymbolKind::CONSTRUCTOR => 9,
        SymbolKind::ENUM => 10,
        SymbolKind::INTERFACE => 11,
        SymbolKind::FUNCTION => 12,
        SymbolKind::VARIABLE => 13,
        SymbolKind::CONSTANT => 14,
        SymbolKind::STRING => 15,
        SymbolKind::NUMBER => 16,
        SymbolKind::BOOLEAN => 17,
        SymbolKind::ARRAY => 18,
        SymbolKind::OBJECT => 19,
        SymbolKind::KEY => 20,
        SymbolKind::NULL => 21,
        SymbolKind::ENUM_MEMBER => 22,
        SymbolKind::STRUCT => 23,
        SymbolKind::EVENT => 24,
        SymbolKind::OPERATOR => 25,
        SymbolKind::TYPE_PARAMETER => 26,
        _ => 0, // Unknown variants get 0
    }
}

/// Flatten `HoverContents` (Scalar/Array/Markup) into a single string.
/// Each element is separated by newlines.
fn flatten_hover_contents(contents: &HoverContents) -> String {
    match contents {
        HoverContents::Scalar(ms) => marked_string_to_string(ms),
        HoverContents::Array(arr) => arr
            .iter()
            .map(marked_string_to_string)
            .collect::<Vec<_>>()
            .join("\n\n"),
        HoverContents::Markup(mc) => mc.value.clone(),
    }
}

fn marked_string_to_string(ms: &MarkedString) -> String {
    match ms {
        MarkedString::String(s) => s.clone(),
        MarkedString::LanguageString(ls) => {
            if ls.language.is_empty() {
                ls.value.clone()
            } else {
                format!("```{}\n{}\n```", ls.language, ls.value)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Serializes the live JDTLS tests. Cargo runs `#[test]` fns on parallel
    /// threads by default; each live test spawns its own `LspSessionManager`
    /// + jdtls process against the *same* fixture workspace, and 3 concurrent
    /// jdtls JVMs race on the workspace and the init timeout. Every live test
    /// acquires this lock first so only one jdtls runs at a time, making the
    /// `--ignored` suite reliable without requiring `--test-threads=1`.
    static LIVE_LSP_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// Acquire the live-LSP serialization lock, tolerating a poisoned mutex
    /// (a prior live test panicking must not cascade-fail the rest).
    fn live_lsp_guard() -> std::sync::MutexGuard<'static, ()> {
        LIVE_LSP_LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Fail-closed: when no LSP manager is configured (simulated by
    /// constructing a manager that has been shut down immediately), the
    /// function must return an `error.lsp_unavailable` error rather than
    /// silently producing a syntactic guess.
    ///
    /// This is the unit test for RX-V3 compliance on the Java usages path.
    #[test]
    fn fail_closed_when_lsp_unavailable() {
        use tempfile::tempdir;

        let dir = tempdir().unwrap();
        let source_path = dir.path().join("Foo.java");
        std::fs::write(&source_path, "public class Foo {}\n").unwrap();

        // Build a fresh manager and immediately shut it down so that
        // `with_session` returns an error (shutting_down=true).
        let manager = LspSessionManager::new();
        manager.shutdown_all();

        let err = java_find_usages(&manager, dir.path(), &source_path, 0, 7)
            .expect_err("expected error.lsp_unavailable");

        let msg = format!("{err:#}");
        assert!(
            msg.contains("error.lsp_unavailable"),
            "expected 'error.lsp_unavailable' in error message, got: {msg}"
        );
    }

    /// Verify UsagesReport serialises to JSON with the expected top-level
    /// fields so downstream agents can rely on the shape.
    #[test]
    fn usages_report_serializes() {
        let report = UsagesReport {
            kind: "java_find_usages".to_string(),
            semantic_status: SEMANTIC_STATUS_LSP_VERIFIED.to_string(),
            source: "/repo/Foo.java".to_string(),
            symbol_resolved: true,
            usage_count: 1,
            usages: vec![UsageSite {
                path: "/repo/Bar.java".to_string(),
                line: 10,
                character: 4,
                handoff: usage_site_handoff("/repo/Bar.java", "/repo"),
            }],
        };

        let json = serde_json::to_string_pretty(&report).unwrap();
        assert!(json.contains("\"lsp_verified\""));
        assert!(json.contains("\"java_find_usages\""));
        assert!(json.contains("\"symbol_resolved\": true"));
        assert!(json.contains("\"usage_count\": 1"));
        assert!(json.contains("bbox_refactor_status"));
    }

    /// Live integration test against a real JDTLS instance.
    ///
    /// Fixture: `tests/fixtures/java/Hello.java`
    ///   - `greet()` declared at 1-based line=3, col=19 → 0-based line=2, col=18
    ///   - Called at line 9 (`h.greet()`) and line 10 (`h.greet()`)
    ///   - `include_declaration=true` so the declaration itself is also returned
    ///   - Expected: >=1 resolved site, semantic_status="lsp_verified"
    ///
    /// Skipped by default (`#[ignore]`) because JDTLS has a ~60s cold start.
    ///
    /// Run with:
    ///   BLACKBOX_JDTLS_BIN=/usr/bin/jdtls cargo test --lib code_nav::semantic::tests::live_jdtls_references -- --ignored --nocapture
    #[test]
    #[ignore = "requires a live JDTLS (/usr/bin/jdtls); ~60s cold start"]
    fn live_jdtls_references() {
        let _serial = live_lsp_guard();
        let fixtures_dir =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/java");
        assert!(
            fixtures_dir.exists(),
            "Java fixture directory missing: {}",
            fixtures_dir.display()
        );

        let source_path = fixtures_dir.join("Hello.java");
        assert!(
            source_path.exists(),
            "Hello.java fixture missing: {}",
            source_path.display()
        );

        // Anchor: `greet` declaration in Hello.java.
        // 1-based: line=3, col=19
        // → 0-based LSP: line=2, col=18  (mirrors the handler's saturating_sub(1))
        let lsp_line: u32 = 3u32.saturating_sub(1); // = 2
        let lsp_col: u32 = 19u32.saturating_sub(1); // = 18

        let manager = LspSessionManager::new();
        let result = java_find_usages(&manager, &fixtures_dir, &source_path, lsp_line, lsp_col);
        match result {
            Ok(report) => {
                let json = serde_json::to_string_pretty(&report)
                    .expect("serialise UsagesReport");
                println!("--- live_jdtls_references output ---\n{json}\n---");
                assert_eq!(
                    report.semantic_status, SEMANTIC_STATUS_LSP_VERIFIED,
                    "semantic_status must be lsp_verified"
                );
                assert!(
                    report.symbol_resolved,
                    "symbol_resolved must be true for `greet`; got report: {json}"
                );
                assert!(
                    report.usage_count >= 1,
                    "expected >=1 usage site for `greet`; got {}: {json}",
                    report.usage_count
                );
            }
            Err(e) => {
                panic!("live JDTLS references failed: {e:#}");
            }
        }
        manager.shutdown_all();
    }

    // --- implementations tests ---

    /// Fail-closed (RX-V3) for java_find_implementations.
    #[test]
    fn implementations_fail_closed_when_lsp_unavailable() {
        use tempfile::tempdir;

        let dir = tempdir().unwrap();
        let source_path = dir.path().join("Foo.java");
        std::fs::write(&source_path, "public class Foo {}\n").unwrap();

        let manager = LspSessionManager::new();
        manager.shutdown_all();

        let err = java_find_implementations(&manager, dir.path(), &source_path, 0, 7)
            .expect_err("expected error.lsp_unavailable");

        let msg = format!("{err:#}");
        assert!(
            msg.contains("error.lsp_unavailable"),
            "expected 'error.lsp_unavailable' in error message, got: {msg}"
        );
    }

    /// Verify ImplementationsReport serialises correctly.
    #[test]
    fn implementations_report_serializes() {
        let report = ImplementationsReport {
            kind: "java_find_implementations".to_string(),
            semantic_status: SEMANTIC_STATUS_LSP_VERIFIED.to_string(),
            source: "/repo/Greeter.java".to_string(),
            symbol_resolved: true,
            site_count: 1,
            sites: vec![ImplementationSite {
                path: "/repo/FriendlyGreeter.java".to_string(),
                line: 3,
                character: 8,
                handoff: usage_site_handoff("/repo/FriendlyGreeter.java", "/repo"),
            }],
        };

        let json = serde_json::to_string_pretty(&report).unwrap();
        assert!(json.contains("\"lsp_verified\""));
        assert!(json.contains("\"java_find_implementations\""));
        assert!(json.contains("\"symbol_resolved\": true"));
        assert!(json.contains("\"site_count\": 1"));
        assert!(json.contains("bbox_refactor_status"));
    }

    /// Live integration test: implementations of the `Greeter` interface.
    ///
    /// Fixture: `tests/fixtures/java/Hello.java`
    ///   - `interface Greeter` at 1-based line=15, col=11 → 0-based line=14, col=10
    ///   - `FriendlyGreeter implements Greeter` at line=20
    ///   - Expected: >=1 implementation site
    ///
    /// Run with:
    ///   BLACKBOX_JDTLS_BIN=/usr/bin/jdtls cargo test --lib code_nav::semantic::tests::live_jdtls_implementations -- --ignored --nocapture
    #[test]
    #[ignore = "requires a live JDTLS (/usr/bin/jdtls); ~60s cold start"]
    fn live_jdtls_implementations() {
        let _serial = live_lsp_guard();
        let fixtures_dir =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/java");
        let source_path = fixtures_dir.join("Hello.java");

        // Anchor: `Greeter` interface type name.
        // 1-based: line=15, col=11 (the 'G' in 'Greeter')
        // → 0-based LSP: line=14, col=10
        let lsp_line: u32 = 15u32.saturating_sub(1); // = 14
        let lsp_col: u32 = 11u32.saturating_sub(1); // = 10

        let manager = LspSessionManager::new();
        let result =
            java_find_implementations(&manager, &fixtures_dir, &source_path, lsp_line, lsp_col);
        match result {
            Ok(report) => {
                let json = serde_json::to_string_pretty(&report)
                    .expect("serialise ImplementationsReport");
                println!("--- live_jdtls_implementations output ---\n{json}\n---");
                assert_eq!(
                    report.semantic_status, SEMANTIC_STATUS_LSP_VERIFIED,
                    "semantic_status must be lsp_verified"
                );
                assert!(
                    report.symbol_resolved,
                    "symbol_resolved must be true for `Greeter` interface; got report: {json}"
                );
                assert!(
                    report.site_count >= 1,
                    "expected >=1 implementation site for `Greeter`; got {}: {json}",
                    report.site_count
                );
            }
            Err(e) => {
                panic!("live JDTLS implementations failed: {e:#}");
            }
        }
        manager.shutdown_all();
    }

    // --- type_at tests ---

    /// Fail-closed (RX-V3) for java_type_at.
    #[test]
    fn type_at_fail_closed_when_lsp_unavailable() {
        use tempfile::tempdir;

        let dir = tempdir().unwrap();
        let source_path = dir.path().join("Foo.java");
        std::fs::write(&source_path, "public class Foo {}\n").unwrap();

        let manager = LspSessionManager::new();
        manager.shutdown_all();

        let err = java_type_at(&manager, dir.path(), &source_path, 0, 7)
            .expect_err("expected error.lsp_unavailable");

        let msg = format!("{err:#}");
        assert!(
            msg.contains("error.lsp_unavailable"),
            "expected 'error.lsp_unavailable' in error message, got: {msg}"
        );
    }

    /// Verify TypeAtReport serialises correctly.
    #[test]
    fn type_at_report_serializes() {
        let report = TypeAtReport {
            kind: "java_type_at".to_string(),
            semantic_status: SEMANTIC_STATUS_LSP_VERIFIED.to_string(),
            source: "/repo/Foo.java".to_string(),
            resolved: true,
            contents: "String Foo.greet()".to_string(),
            handoff: usage_site_handoff("/repo/Foo.java", "/repo"),
        };

        let json = serde_json::to_string_pretty(&report).unwrap();
        assert!(json.contains("\"lsp_verified\""));
        assert!(json.contains("\"java_type_at\""));
        assert!(json.contains("\"resolved\": true"));
        assert!(json.contains("\"contents\":"));
        assert!(json.contains("bbox_refactor_status"));
    }

    /// Live integration test: hover type at `greet()` declaration.
    ///
    /// Fixture: `tests/fixtures/java/Hello.java`
    ///   - `greet` method name at 1-based line=3, col=19 → 0-based line=2, col=18
    ///   - Expected: resolved=true, contents mentions "String"
    ///
    /// Run with:
    ///   BLACKBOX_JDTLS_BIN=/usr/bin/jdtls cargo test --lib code_nav::semantic::tests::live_jdtls_hover -- --ignored --nocapture
    #[test]
    #[ignore = "requires a live JDTLS (/usr/bin/jdtls); ~60s cold start"]
    fn live_jdtls_hover() {
        let _serial = live_lsp_guard();
        let fixtures_dir =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/java");
        let source_path = fixtures_dir.join("Hello.java");

        // Anchor: `greet` method name.
        // 1-based: line=3, col=19
        // → 0-based LSP: line=2, col=18
        let lsp_line: u32 = 3u32.saturating_sub(1); // = 2
        let lsp_col: u32 = 19u32.saturating_sub(1); // = 18

        let manager = LspSessionManager::new();
        let result = java_type_at(&manager, &fixtures_dir, &source_path, lsp_line, lsp_col);
        match result {
            Ok(report) => {
                let json = serde_json::to_string_pretty(&report)
                    .expect("serialise TypeAtReport");
                println!("--- live_jdtls_hover output ---\n{json}\n---");
                assert_eq!(
                    report.semantic_status, SEMANTIC_STATUS_LSP_VERIFIED,
                    "semantic_status must be lsp_verified"
                );
                assert!(
                    report.resolved,
                    "resolved must be true for `greet`; got report: {json}"
                );
                assert!(
                    report.contents.contains("String"),
                    "expected hover contents to mention 'String'; got: {}",
                    report.contents
                );
            }
            Err(e) => {
                panic!("live JDTLS hover failed: {e:#}");
            }
        }
        manager.shutdown_all();
    }

    // --- workspace_symbols tests ---

    /// Fail-closed (RX-V3) for java_workspace_symbols.
    #[test]
    fn workspace_symbols_fail_closed_when_lsp_unavailable() {
        use tempfile::tempdir;

        let dir = tempdir().unwrap();
        let _source_path = dir.path().join("Foo.java");
        std::fs::write(&_source_path, "public class Foo {}\n").unwrap();

        let manager = LspSessionManager::new();
        manager.shutdown_all();

        let err = java_workspace_symbols(&manager, dir.path(), "Greeter")
            .expect_err("expected error.lsp_unavailable");

        let msg = format!("{err:#}");
        assert!(
            msg.contains("error.lsp_unavailable"),
            "expected 'error.lsp_unavailable' in error message, got: {msg}"
        );
    }

    /// Verify WorkspaceSymbolsReport serialises correctly.
    #[test]
    fn workspace_symbols_report_serializes() {
        let (interface_kind_str, interface_kind_num) = symbol_kind_info(lsp_types::SymbolKind::INTERFACE);
        let (class_kind_str, class_kind_num) = symbol_kind_info(lsp_types::SymbolKind::CLASS);
        let report = WorkspaceSymbolsReport {
            kind: "java_workspace_symbols".to_string(),
            semantic_status: SEMANTIC_STATUS_LSP_VERIFIED.to_string(),
            query: "Greeter".to_string(),
            resolved: true,
            symbol_count: 2,
            symbols: vec![
                WorkspaceSymbolItem {
                    name: "Greeter".to_string(),
                    kind: interface_kind_str,
                    kind_number: interface_kind_num,
                    path: "/repo/Hello.java".to_string(),
                    line: 14,
                    character: 10,
                    handoff: usage_site_handoff("/repo/Hello.java", "/repo"),
                },
                WorkspaceSymbolItem {
                    name: "FriendlyGreeter".to_string(),
                    kind: class_kind_str,
                    kind_number: class_kind_num,
                    path: "/repo/Hello.java".to_string(),
                    line: 19,
                    character: 0,
                    handoff: usage_site_handoff("/repo/Hello.java", "/repo"),
                },
            ],
        };

        let json = serde_json::to_string_pretty(&report).unwrap();
        assert!(json.contains("\"lsp_verified\""));
        assert!(json.contains("\"java_workspace_symbols\""));
        assert!(json.contains("\"resolved\": true"));
        assert!(json.contains("\"symbol_count\": 2"));
        assert!(json.contains("bbox_refactor_status"));
        assert!(json.contains("\"Greeter\""));
        assert!(json.contains("\"interface\""));
    }

    /// Live integration test: workspace symbols query for "Greeter".
    ///
    /// Fixture: `tests/fixtures/java/Hello.java`
    ///   - `interface Greeter` at 1-based line=15
    ///   - `class FriendlyGreeter implements Greeter` at 1-based line=20
    ///   - Expected: >=1 symbol match
    ///
    /// Run with:
    ///   BLACKBOX_JDTLS_BIN=/usr/bin/jdtls cargo test --lib code_nav::semantic::tests::live_jdtls_workspace_symbols -- --ignored --nocapture
    #[test]
    #[ignore = "requires a live JDTLS (/usr/bin/jdtls); ~60s cold start"]
    fn live_jdtls_workspace_symbols() {
        let _serial = live_lsp_guard();
        let fixtures_dir =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/java");
        assert!(
            fixtures_dir.exists(),
            "Java fixture directory missing: {}",
            fixtures_dir.display()
        );

        let manager = LspSessionManager::new();
        let result = java_workspace_symbols(&manager, &fixtures_dir, "Greeter");
        match result {
            Ok(report) => {
                let json = serde_json::to_string_pretty(&report)
                    .expect("serialise WorkspaceSymbolsReport");
                println!("--- live_jdtls_workspace_symbols output ---\n{json}\n---");
                assert_eq!(
                    report.semantic_status, SEMANTIC_STATUS_LSP_VERIFIED,
                    "semantic_status must be lsp_verified"
                );
                assert!(
                    report.symbol_count >= 1,
                    "expected >=1 symbol for 'Greeter'; got {}: {json}",
                    report.symbol_count
                );
                // At least one symbol should match
                assert!(
                    report.resolved,
                    "resolved must be true for 'Greeter' query; got report: {json}"
                );
            }
            Err(e) => {
                panic!("live JDTLS workspace symbols failed: {e:#}");
            }
        }
        manager.shutdown_all();
    }
}
