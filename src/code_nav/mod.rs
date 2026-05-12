use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context, Result};
use ignore::{DirEntry, WalkBuilder};
use rmcp::schemars;
use serde::{Deserialize, Serialize};
use tree_sitter::{Query, QueryCursor, StreamingIterator, Tree};

use crate::chunker::code::{language_for_path, parser_for_language, ts_language_for_name};
use crate::index::{first_text, first_u64, optional_text};
use crate::projects::ProjectRecord;
use crate::refactor::{
    parse_report, resolve_path, ParseReport, RefactorStatus, RefactorStatusParams, SyntaxItem,
};

#[cfg(test)]
mod tests;

/// Top-level `semantic_status` value for every code-nav tool response.
///
/// Code-nav tools (`bbox_code_query`, `bbox_code_node_describe`,
/// `bbox_code_symbols`) are syntax locators, not binding-aware lookups.
/// Per the design boundary in `design/proposed/code-nav-symbolic-exploration.md`,
/// any tool that returns syntactic references must label them as
/// syntax-derived so agents do not mistake them for semantic facts.
pub const SEMANTIC_STATUS_SYNTAX_ONLY: &str = "syntax_only";

/// Maximum source-file size accepted by any code-nav tool.
/// Larger files are rejected with a typed `file_too_large_for_code_nav`
/// error so the agent can chunk the request or pick a smaller target.
pub const MAX_CODE_NAV_FILE_BYTES: u64 = 2 * 1024 * 1024;

/// Maximum source files scanned in one `bbox_code_symbols` walk.
/// The scan stops at this many files and reports `truncated=true` so
/// the caller can narrow with `path_contains`, `languages`, or a tighter
/// `project_dir` and re-run.
pub const MAX_CODE_NAV_SCANNED_FILES: usize = 5000;

/// Derive the **refactor-vocabulary** kind from a raw tree-sitter node
/// kind plus the nearest enclosing symbol kind.
///
/// `refactor::status` (the live `bbox_code_symbols` source) synthesises
/// a small number of kinds that do not exist as raw tree-sitter nodes —
/// most notably Rust `impl_method` for a `function_item` whose parent
/// symbol is an `impl_item`. The indexed lane only has access to the
/// raw tree-sitter kinds (`symbol_kind`) and the enclosing-symbol kind
/// (`parent_kind`); this function reproduces the synthesis so indexed
/// records can carry the same `kind` value as live records.
///
/// New synthesis cases must be added here AND mirrored in
/// `refactor::status` (or vice versa) to keep the two lanes in sync.
/// As of CN-T2 there is exactly one documented case: Rust impl methods.
/// See `src/refactor/rust.rs:rust_impl_methods_in` for the live-side
/// synthesis that this function inverts.
pub fn refactor_kind_for(
    language: &str,
    symbol_kind: &str,
    parent_kind: Option<&str>,
) -> String {
    match (language, symbol_kind, parent_kind) {
        ("rust", "function_item", Some("impl_item")) => "impl_method".to_string(),
        _ => symbol_kind.to_string(),
    }
}

/// Reverse of `refactor_kind_for`: derive `(symbol_kind, parent_kind)`
/// from a refactor-vocabulary kind. Used by the live lane when it has
/// `refactor_kind` from `refactor::status` but needs to surface the
/// raw tree-sitter kinds on the response shape too. Returns
/// `(refactor_kind.to_string(), None)` for kinds with no documented
/// synthesis — honest about the parent-context loss in that direction.
pub fn symbol_kind_from_refactor(
    language: &str,
    refactor_kind: &str,
) -> (String, Option<String>) {
    match (language, refactor_kind) {
        ("rust", "impl_method") => {
            ("function_item".to_string(), Some("impl_item".to_string()))
        }
        _ => (refactor_kind.to_string(), None),
    }
}

/// Typed error response shared by every code-nav tool. Recoverable
/// failure modes (file too large, project not registered, file not
/// under any registered project) return this shape as JSON instead of
/// bailing — the agent reads `code`, `suggestion`, and any populated
/// recovery fields and re-issues the call.
#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
pub struct CodeNavErrorResponse {
    /// Always `"error"`. Pair with `code` for typed dispatch; pair with
    /// `message` for human display.
    pub status: String,
    /// Stable machine-readable code. One of:
    /// `file_too_large_for_code_nav`, `project_not_registered`,
    /// `file_outside_registered_projects`.
    pub code: String,
    /// One-line human-readable summary.
    pub message: String,
    /// Concrete next call the agent should make to recover.
    pub suggestion: String,
    /// Echoed file path when the error is file-scoped.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file: Option<String>,
    /// Actual file size in bytes (only for `file_too_large_*`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file_bytes: Option<u64>,
    /// Cap in bytes (only for `file_too_large_*`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_bytes: Option<u64>,
    /// Echoed project_dir when the error is project-scoped.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub project_dir: Option<String>,
    /// Registered project roots, so the agent can pick the right one
    /// or call `bbox_project_register` for a new one.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub registered_projects: Vec<CodeNavProjectHint>,
    /// Always `"syntax_only"` — error responses honour the same labeling
    /// invariant as success responses, so agents never have to special-case.
    pub semantic_status: String,
}

/// Compact project descriptor surfaced in error recovery hints. Only
/// the canonical_path + project_id are emitted — full ProjectRecord is
/// available via `bbox_project_list`.
#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
pub struct CodeNavProjectHint {
    pub canonical_path: String,
    pub project_id: String,
}

impl CodeNavProjectHint {
    fn from_record(record: &ProjectRecord) -> Self {
        Self {
            canonical_path: record.canonical_path.clone(),
            project_id: record.project_id.clone(),
        }
    }
}

/// Build the JSON for a `file_too_large_for_code_nav` error response.
/// Centralised so every code-nav tool reports the cap the same way.
fn err_file_too_large(file: &Path, bytes: u64) -> Result<String> {
    let response = CodeNavErrorResponse {
        status: "error".to_string(),
        code: "file_too_large_for_code_nav".to_string(),
        message: format!(
            "{} is {} bytes; code-nav tools cap at {} bytes",
            file.display(),
            bytes,
            MAX_CODE_NAV_FILE_BYTES
        ),
        suggestion: format!(
            "Narrow the request: target a smaller file, or use bbox_refactor_status with item_kinds to locate a specific symbol without reparsing the whole file."
        ),
        file: Some(file.to_string_lossy().into_owned()),
        file_bytes: Some(bytes),
        max_bytes: Some(MAX_CODE_NAV_FILE_BYTES),
        project_dir: None,
        registered_projects: Vec::new(),
        semantic_status: SEMANTIC_STATUS_SYNTAX_ONLY.to_string(),
    };
    Ok(serde_json::to_string_pretty(&response)?)
}

/// Build the JSON for a `project_not_registered` error response.
fn err_project_not_registered(project_dir: &str, registered: &[ProjectRecord]) -> Result<String> {
    let response = CodeNavErrorResponse {
        status: "error".to_string(),
        code: "project_not_registered".to_string(),
        message: format!(
            "{project_dir} is not a registered project root or a descendant of one"
        ),
        suggestion: format!(
            "Either pass a project_dir at or under one of `registered_projects[*].canonical_path`, \
             or call `bbox_project_register(path=\"{project_dir}\")` to register this directory first."
        ),
        file: None,
        file_bytes: None,
        max_bytes: None,
        project_dir: Some(project_dir.to_string()),
        registered_projects: registered.iter().map(CodeNavProjectHint::from_record).collect(),
        semantic_status: SEMANTIC_STATUS_SYNTAX_ONLY.to_string(),
    };
    Ok(serde_json::to_string_pretty(&response)?)
}

/// File-size gate. Returns `Ok(Some(error_json))` if the file is too
/// large for code-nav to parse (caller should return the JSON
/// verbatim); `Ok(None)` if the file is within the cap and parsing
/// should proceed; `Err(_)` for I/O errors (e.g. stat failure).
fn check_code_nav_file_size(path: &Path) -> Result<Option<String>> {
    let metadata = fs::metadata(path)
        .with_context(|| format!("failed to stat {} for size check", path.display()))?;
    let bytes = metadata.len();
    if bytes > MAX_CODE_NAV_FILE_BYTES {
        Ok(Some(err_file_too_large(path, bytes)?))
    } else {
        Ok(None)
    }
}

/// Registered-project gate for project-scoped code-nav tools.
/// `project_dir` must equal a registered `canonical_path` or be a
/// strict descendant of one. Returns `Ok(Some(error_json))` if the
/// directory is unregistered; `Ok(None)` if accepted. Comparison is
/// done on canonicalised paths so symlink aliases collapse.
fn check_project_dir_registered(
    canonical_project_dir: &Path,
    registered: &[ProjectRecord],
) -> Result<Option<String>> {
    for record in registered {
        let root = PathBuf::from(&record.canonical_path);
        // Canonicalise the registered root too — handles symlink aliases.
        let canonical_root = root.canonicalize().unwrap_or(root);
        if canonical_project_dir == canonical_root
            || canonical_project_dir.starts_with(&canonical_root)
        {
            return Ok(None);
        }
    }
    Ok(Some(err_project_not_registered(
        canonical_project_dir.to_string_lossy().as_ref(),
        registered,
    )?))
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct CodeQueryParams {
    pub file: String,
    pub query: String,
    #[serde(default)]
    pub project_dir: Option<String>,
    #[serde(default)]
    pub language: Option<String>,
    #[serde(default)]
    pub limit: Option<usize>,
    #[serde(default)]
    pub include_text: Option<bool>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct CodeSymbolSearchParams {
    /// Project root to scan. Required because this is intentionally project-scoped.
    pub project_dir: String,
    /// Optional case-sensitive substring matched against item name, item kind, and relative path.
    #[serde(default)]
    pub query: Option<String>,
    /// Optional language filter, e.g. ["rust", "java"].
    #[serde(default)]
    pub languages: Option<Vec<String>>,
    /// Optional exact refactor item kinds, e.g. ["impl_method", "method_declaration"].
    /// Matches against either the refactor synthetic kind (`kind`) OR
    /// the raw tree-sitter `symbol_kind`, so the same filter works on
    /// both lanes — passing `["function_item"]` and `["impl_method"]`
    /// to an indexed lane both return Rust impl methods.
    #[serde(default)]
    pub item_kinds: Option<Vec<String>>,
    /// Optional case-sensitive substring matched against the relative path before parsing.
    #[serde(default)]
    pub path_contains: Option<String>,
    /// Maximum returned items. Defaults to 100 and is capped at 1000.
    #[serde(default)]
    pub limit: Option<usize>,
    /// Maximum supported source files to parse. Defaults to 5000 and is capped at 5000.
    /// Only meaningful for `mode="live"` — the indexed lane reads the
    /// project's tantivy docs directly and has no per-file parse cost.
    #[serde(default)]
    pub file_limit: Option<usize>,
    /// Include syntax attributes from bbox_refactor_status. Defaults false.
    /// Only meaningful for `mode="live"` — the indexed lane does not
    /// surface attributes.
    #[serde(default)]
    pub include_attributes: Option<bool>,
    /// Which lane answers the query:
    /// - `"indexed"` (default after CN-D3 lands a populated index):
    ///   reads stored project_file docs from tantivy. Fast, no parse
    ///   cost, no walker. Returns the same record shape as `"live"`.
    ///   If the tantivy index does not yet contain `symbol_kind` for
    ///   this project, the response is empty — caller falls back to
    ///   `mode="live"`.
    /// - `"live"`: walks the project tree and parses every supported
    ///   source file via `bbox_refactor_status`. Honours
    ///   `file_limit`, `include_attributes`, and respects the
    ///   shared `MAX_CODE_NAV_SCANNED_FILES` cap.
    /// Default: `"indexed"`.
    #[serde(default)]
    pub mode: Option<String>,
}

/// Which lane `code_symbols` should answer with. Stable string values
/// chosen so a future "auto" mode can be added without breaking the
/// "indexed" / "live" enum identity.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CodeSymbolMode {
    Indexed,
    Live,
}

impl CodeSymbolMode {
    pub fn from_param(raw: Option<&str>) -> Result<Self> {
        match raw.map(str::trim).filter(|s| !s.is_empty()) {
            None | Some("indexed") => Ok(Self::Indexed),
            Some("live") => Ok(Self::Live),
            Some(other) => Err(anyhow!(
                "unknown mode {other:?}; expected \"indexed\" or \"live\""
            )),
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Indexed => "indexed",
            Self::Live => "live",
        }
    }
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
pub struct CodeQueryResponse {
    pub status: String,
    pub path: String,
    pub language: String,
    pub matching_captures: usize,
    pub returned_captures: usize,
    pub truncated: bool,
    pub captures: Vec<CodeQueryCapture>,
    pub parse_report: ParseReport,
    pub semantic_status: String,
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
pub struct CodeSymbolSearchResponse {
    pub status: String,
    pub project_dir: String,
    /// Which lane answered the query: `"indexed"` or `"live"`. Always
    /// present so callers can compare results across modes
    /// programmatically.
    pub mode: String,
    pub scanned_files: usize,
    pub matched_files: usize,
    pub matching_items: usize,
    pub returned_items: usize,
    pub truncated: bool,
    pub items: Vec<CodeSymbolSearchItem>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub errors: Vec<CodeSymbolSearchError>,
    pub semantic_status: String,
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
pub struct CodeSymbolSearchItem {
    pub file: String,
    pub language: String,
    /// Refactor synthetic kind, matching the `bbox_refactor_status` /
    /// `bbox_refactor_plan` vocabulary. For Rust impl methods this is
    /// `"impl_method"`. Use this for refactor-plan filtering.
    pub kind: String,
    /// Raw tree-sitter node kind (CN-D1). For Rust impl methods this
    /// is `"function_item"`. Use this for grammar-shape filtering.
    /// `None` for live-lane records whose refactor kind has no
    /// documented synthesis (lookup is best-effort).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub symbol_kind: Option<String>,
    /// Kind of the nearest enclosing symbol-producing ancestor.
    /// `None` at file top level OR on live-lane records whose
    /// `refactor_kind` does not have a documented parent synthesis.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_kind: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    pub byte_range: (usize, usize),
    pub line_range: (usize, usize),
    pub handoff: CodeRefactorHandoff,
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
pub struct CodeSymbolSearchError {
    pub file: String,
    pub error: String,
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
pub struct CodeQueryCapture {
    pub capture_name: String,
    pub node_kind: String,
    pub byte_range: (usize, usize),
    pub line_range: (usize, usize),
    pub column_range: (usize, usize),
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_kind: Option<String>,
    pub handoff: CodeRefactorHandoff,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct CodeNodeDescribeParams {
    pub file: String,
    pub line: usize,
    pub column: usize,
    #[serde(default)]
    pub project_dir: Option<String>,
    #[serde(default)]
    pub include_siblings: Option<bool>,
    #[serde(default)]
    pub include_text: Option<bool>,
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
pub struct CodeNodeDescribeResponse {
    pub status: String,
    pub path: String,
    pub language: String,
    pub node_kind: String,
    pub byte_range: (usize, usize),
    pub line_range: (usize, usize),
    pub column_range: (usize, usize),
    #[serde(skip_serializing_if = "Option::is_none")]
    pub field_in_parent: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    pub named_children: Vec<CodeNodeChild>,
    pub parent_chain: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub previous_sibling: Option<CodeNodeSibling>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_sibling: Option<CodeNodeSibling>,
    pub handoff: CodeRefactorHandoff,
    pub parse_report: ParseReport,
    pub semantic_status: String,
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
pub struct CodeNodeChild {
    pub kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub field: Option<String>,
    pub byte_range: (usize, usize),
    pub line_range: (usize, usize),
    pub column_range: (usize, usize),
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
pub struct CodeNodeSibling {
    pub kind: String,
    pub byte_range: (usize, usize),
    pub line_range: (usize, usize),
    pub column_range: (usize, usize),
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct CodeRefactorHandoff {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub nearest_refactor_item: Option<CodeNodeSummary>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub refactor_status: Option<CodeRefactorStatusHint>,
    pub project_refs: CodeProjectRefsHint,
    pub note: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct CodeNodeSummary {
    pub kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    pub byte_range: (usize, usize),
    pub line_range: (usize, usize),
    pub column_range: (usize, usize),
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct CodeRefactorStatusHint {
    pub tool: String,
    pub arguments: CodeRefactorStatusHintArgs,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct CodeRefactorStatusHintArgs {
    pub file: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub project_dir: Option<String>,
    pub item_names: Vec<String>,
    pub item_kinds: Vec<String>,
    pub limit: usize,
    pub include_attributes: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct CodeProjectRefsHint {
    pub tool: String,
    pub arguments: CodeProjectRefsHintArgs,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct CodeProjectRefsHintArgs {
    pub file: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub project_dir: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub query: Option<String>,
    pub limit: usize,
    pub include_excerpt: bool,
}

struct CodeNavParsedSource {
    path: PathBuf,
    language: String,
    source: String,
    tree: Tree,
}

const CODE_SYMBOL_SKIP_DIRS: &[&str] = &[
    ".git",
    ".hg",
    ".svn",
    "target",
    "node_modules",
    "dist",
    "build",
    ".gradle",
    ".idea",
    ".vscode",
];

fn excerpt(text: &str, max_chars: usize) -> String {
    let compact = text.split_whitespace().collect::<Vec<_>>().join(" ");
    compact.chars().take(max_chars).collect()
}

fn node_text(source: &str, node: tree_sitter::Node<'_>) -> Option<String> {
    source
        .get(node.start_byte()..node.end_byte())
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .map(str::to_string)
}

fn node_name(source: &str, node: tree_sitter::Node<'_>) -> Option<String> {
    if node.kind() == "impl_item" {
        return node_text(source, node)
            .and_then(|text| text.split('{').next().map(str::trim).map(str::to_string))
            .filter(|text| !text.is_empty());
    }
    node.child_by_field_name("name")
        .and_then(|child| node_text(source, child))
        .or_else(|| {
            if matches!(
                node.kind(),
                "identifier" | "type_identifier" | "field_identifier" | "property_identifier"
            ) {
                node_text(source, node)
            } else {
                None
            }
        })
}

fn is_refactor_item_kind(kind: &str) -> bool {
    matches!(
        kind,
        "function_item"
            | "function_definition"
            | "function_declaration"
            | "function_declarator"
            | "method_definition"
            | "method_declaration"
            | "constructor_declaration"
            | "class_definition"
            | "class_declaration"
            | "struct_item"
            | "struct_specifier"
            | "enum_item"
            | "enum_declaration"
            | "trait_item"
            | "interface_declaration"
            | "interface_type"
            | "impl_item"
            | "mod_item"
            | "type_item"
            | "type_declaration"
            | "type_spec"
            | "const_item"
            | "static_item"
            | "macro_definition"
    )
}

fn node_summary(source: &str, node: tree_sitter::Node<'_>) -> CodeNodeSummary {
    CodeNodeSummary {
        kind: node.kind().to_string(),
        name: node_name(source, node),
        byte_range: (node.start_byte(), node.end_byte()),
        line_range: (node.start_position().row + 1, node.end_position().row + 1),
        column_range: (
            node.start_position().column + 1,
            node.end_position().column + 1,
        ),
    }
}

fn nearest_refactor_item(source: &str, node: tree_sitter::Node<'_>) -> Option<CodeNodeSummary> {
    let mut current = Some(node);
    while let Some(candidate) = current {
        if is_refactor_item_kind(candidate.kind()) {
            return Some(node_summary(source, candidate));
        }
        current = candidate.parent();
    }
    None
}

fn is_root_kind(kind: &str) -> bool {
    matches!(
        kind,
        "source_file" | "program" | "module" | "translation_unit" | "compilation_unit"
    )
}

fn has_ancestor_kind(mut node: tree_sitter::Node<'_>, kind: &str) -> bool {
    while let Some(parent) = node.parent() {
        if parent.kind() == kind {
            return true;
        }
        node = parent;
    }
    false
}

fn refactor_status_item(
    source: &str,
    language: &str,
    node: tree_sitter::Node<'_>,
) -> Option<CodeNodeSummary> {
    let mut current = Some(node);
    while let Some(candidate) = current {
        if is_refactor_item_kind(candidate.kind()) {
            if language == "rust"
                && candidate.kind() == "function_item"
                && has_ancestor_kind(candidate, "impl_item")
            {
                let mut summary = node_summary(source, candidate);
                summary.kind = "impl_method".to_string();
                return Some(summary);
            }
            if language == "java"
                && matches!(
                    candidate.kind(),
                    "method_declaration"
                        | "constructor_declaration"
                        | "class_declaration"
                        | "interface_declaration"
                        | "record_declaration"
                        | "enum_declaration"
                )
            {
                return Some(node_summary(source, candidate));
            }
            if candidate
                .parent()
                .is_some_and(|parent| is_root_kind(parent.kind()))
            {
                return Some(node_summary(source, candidate));
            }
        }
        current = candidate.parent();
    }
    None
}

fn handoff_query(
    selected_text: Option<&str>,
    selected: &CodeNodeSummary,
    nearest_item: Option<&CodeNodeSummary>,
) -> Option<String> {
    nearest_item
        .and_then(|item| item.name.clone())
        .or_else(|| selected.name.clone())
        .or_else(|| {
            selected_text
                .map(str::trim)
                .filter(|text| !text.is_empty())
                .map(str::to_string)
        })
}

fn refactor_handoff(
    file: &str,
    project_dir: Option<&str>,
    language: &str,
    source: &str,
    node: tree_sitter::Node<'_>,
    selected_text: Option<&str>,
) -> CodeRefactorHandoff {
    let selected = node_summary(source, node);
    let nearest_item = nearest_refactor_item(source, node);
    let status_item = refactor_status_item(source, language, node);
    let refactor_status = status_item
        .as_ref()
        .and_then(|item| item.name.as_ref().map(|name| (item, name)))
        .map(|(item, name)| CodeRefactorStatusHint {
            tool: "bbox_refactor_status".to_string(),
            arguments: CodeRefactorStatusHintArgs {
                file: file.to_string(),
                project_dir: project_dir.map(str::to_string),
                item_names: vec![name.clone()],
                item_kinds: vec![item.kind.clone()],
                limit: 50,
                include_attributes: false,
            },
        });
    let query = handoff_query(selected_text, &selected, nearest_item.as_ref());
    CodeRefactorHandoff {
        nearest_refactor_item: nearest_item,
        refactor_status,
        project_refs: CodeProjectRefsHint {
            tool: "bbox_refactor_project_refs".to_string(),
            arguments: CodeProjectRefsHintArgs {
                file: file.to_string(),
                project_dir: project_dir.map(str::to_string),
                query,
                limit: 20,
                include_excerpt: false,
            },
        },
        note: "Syntax-only locator. Use bbox_refactor_status for refactorable item names/kinds before planning edits; use bbox_refactor_project_refs when you need current project_file entity refs.".to_string(),
    }
}

/// Handoff builder for the indexed lane. Mirrors `status_item_handoff`
/// but takes the raw fields stored in tantivy (name/kind/ranges)
/// instead of a `SyntaxItem`, so the indexed lane can produce the
/// exact same shape without re-parsing.
fn indexed_handoff(
    file: &str,
    project_dir: &str,
    language: &str,
    name: Option<&str>,
    refactor_kind: &str,
    byte_range: (usize, usize),
    line_range: (usize, usize),
) -> CodeRefactorHandoff {
    let nearest_refactor_item = Some(CodeNodeSummary {
        kind: refactor_kind.to_string(),
        name: name.map(str::to_string),
        byte_range,
        line_range,
        column_range: (1, 1),
    });
    let refactor_status = name.map(|n| CodeRefactorStatusHint {
        tool: "bbox_refactor_status".to_string(),
        arguments: CodeRefactorStatusHintArgs {
            file: file.to_string(),
            project_dir: Some(project_dir.to_string()),
            item_names: vec![n.to_string()],
            item_kinds: vec![refactor_kind.to_string()],
            limit: 50,
            include_attributes: false,
        },
    });
    let query = name
        .map(str::to_string)
        .or_else(|| Some(refactor_kind.to_string()));
    CodeRefactorHandoff {
        nearest_refactor_item,
        refactor_status,
        project_refs: CodeProjectRefsHint {
            tool: "bbox_refactor_project_refs".to_string(),
            arguments: CodeProjectRefsHintArgs {
                file: file.to_string(),
                project_dir: Some(project_dir.to_string()),
                query,
                limit: 20,
                include_excerpt: false,
            },
        },
        note: format!(
            "Indexed code-symbol match for {language}. Same handoff shape as live mode; the indexed lane reads stored project_file docs from tantivy without parsing. Use bbox_refactor_status to confirm exact item names/kinds before planning edits."
        ),
    }
}

fn status_item_handoff(
    file: &str,
    project_dir: Option<&str>,
    language: &str,
    item: &SyntaxItem,
) -> CodeRefactorHandoff {
    let nearest_refactor_item = Some(CodeNodeSummary {
        kind: item.kind.clone(),
        name: item.name.clone(),
        byte_range: (item.byte_start, item.byte_end),
        line_range: (item.line_start, item.line_end),
        column_range: (1, 1),
    });
    let refactor_status = item.name.as_ref().map(|name| CodeRefactorStatusHint {
        tool: "bbox_refactor_status".to_string(),
        arguments: CodeRefactorStatusHintArgs {
            file: file.to_string(),
            project_dir: project_dir.map(str::to_string),
            item_names: vec![name.clone()],
            item_kinds: vec![item.kind.clone()],
            limit: 50,
            include_attributes: false,
        },
    });
    let query = item.name.clone().or_else(|| Some(item.kind.clone()));
    CodeRefactorHandoff {
        nearest_refactor_item,
        refactor_status,
        project_refs: CodeProjectRefsHint {
            tool: "bbox_refactor_project_refs".to_string(),
            arguments: CodeProjectRefsHintArgs {
                file: file.to_string(),
                project_dir: project_dir.map(str::to_string),
                query,
                limit: 20,
                include_excerpt: false,
            },
        },
        note: format!(
            "Project-scoped syntax symbol match for {language}. Use bbox_refactor_status to confirm exact item names/kinds before planning edits; use bbox_refactor_project_refs when you need current project_file entity refs."
        ),
    }
}

fn is_skipped_symbol_entry(entry: &DirEntry) -> bool {
    entry
        .file_name()
        .to_str()
        .is_some_and(|name| CODE_SYMBOL_SKIP_DIRS.contains(&name))
}

fn item_matches_query(item: &SyntaxItem, rel_file: &str, query: Option<&str>) -> bool {
    let Some(query) = query.filter(|query| !query.is_empty()) else {
        return true;
    };
    item.name
        .as_deref()
        .is_some_and(|name| name.contains(query))
        || item.kind.contains(query)
        || rel_file.contains(query)
}

fn parse_code_nav_source(
    path: &Path,
    language_override: Option<&str>,
) -> Result<CodeNavParsedSource> {
    let language = match language_override {
        Some(language) if !language.trim().is_empty() => language.trim().to_string(),
        _ => language_for_path(path)
            .ok_or_else(|| anyhow!("unsupported source file extension"))?
            .to_string(),
    };
    let source =
        fs::read_to_string(path).with_context(|| format!("failed to read {}", path.display()))?;
    let mut parser = parser_for_language(&language)?;
    let tree = parser
        .parse(&source, None)
        .ok_or_else(|| anyhow!("tree-sitter {language} parser returned no tree"))?;
    Ok(CodeNavParsedSource {
        path: path.to_path_buf(),
        language,
        source,
        tree,
    })
}

/// Dispatcher: `bbox_code_symbols` entry point. Reads `params.mode`
/// and routes to `code_symbols_indexed` or `code_symbols_live`.
///
/// `idx` is `Some` when the call is coming through the MCP handler
/// (which has the daemon's TranscriptIndex). Tests pass `None` to
/// exercise the live lane directly; if such a test asks for
/// `mode="indexed"` it gets a typed error telling it to upgrade.
pub fn code_symbols(
    p: &CodeSymbolSearchParams,
    registered: &[ProjectRecord],
    idx: Option<&crate::index::TranscriptIndex>,
) -> Result<String> {
    // Default mode rules:
    // - Caller explicitly set `mode`: honour it (Indexed requires idx).
    // - Caller left `mode` unset and we have an index: default to
    //   Indexed (fast path).
    // - Caller left `mode` unset and we have no index (test path):
    //   default to Live.
    let mode = match p.mode.as_deref() {
        Some(raw) => CodeSymbolMode::from_param(Some(raw))?,
        None if idx.is_some() => CodeSymbolMode::Indexed,
        None => CodeSymbolMode::Live,
    };
    match mode {
        CodeSymbolMode::Indexed => match idx {
            Some(index) => code_symbols_indexed(p, registered, index),
            None => Err(anyhow!(
                "mode=\"indexed\" requires a TranscriptIndex; pass one via the MCP handler or use mode=\"live\""
            )),
        },
        CodeSymbolMode::Live => code_symbols_live(p, registered),
    }
}

/// Live lane. Walks the project tree, parses each supported source via
/// `refactor::status`, and returns refactor-vocabulary records. Suffers
/// per-file parse cost; capped by `MAX_CODE_NAV_SCANNED_FILES`.
pub fn code_symbols_live(
    p: &CodeSymbolSearchParams,
    registered: &[ProjectRecord],
) -> Result<String> {
    let project_dir = PathBuf::from(&p.project_dir)
        .canonicalize()
        .with_context(|| format!("failed to resolve project_dir {}", p.project_dir))?;
    if !project_dir.is_dir() {
        return Err(anyhow!("project_dir must be a directory"));
    }
    if let Some(err_json) = check_project_dir_registered(&project_dir, registered)? {
        return Ok(err_json);
    }

    let language_filter = p
        .languages
        .as_ref()
        .filter(|languages| !languages.is_empty())
        .map(|languages| languages.iter().map(String::as_str).collect::<Vec<_>>());
    let kind_filter = p.item_kinds.clone().filter(|kinds| !kinds.is_empty());
    let limit = p.limit.unwrap_or(100).min(1000);
    let file_limit = p
        .file_limit
        .unwrap_or(MAX_CODE_NAV_SCANNED_FILES)
        .min(MAX_CODE_NAV_SCANNED_FILES);
    let project_dir_arg = project_dir.to_string_lossy().into_owned();

    let mut scanned_files = 0usize;
    let mut matched_file_paths = std::collections::HashSet::new();
    let mut matching_items = 0usize;
    let mut items = Vec::new();
    let mut errors = Vec::new();
    let mut file_limit_hit = false;

    let walker = WalkBuilder::new(&project_dir)
        .hidden(false)
        .filter_entry(|entry| !is_skipped_symbol_entry(entry))
        .build();

    for entry in walker.filter_map(|entry| entry.ok()) {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let Some(language) = language_for_path(path) else {
            continue;
        };
        if let Some(languages) = language_filter.as_deref() {
            if !languages.contains(&language) {
                continue;
            }
        }

        let rel_path = path
            .strip_prefix(&project_dir)
            .unwrap_or(path)
            .to_string_lossy()
            .to_string();
        if let Some(path_contains) = p.path_contains.as_deref().filter(|s| !s.is_empty()) {
            if !rel_path.contains(path_contains) {
                continue;
            }
        }

        if scanned_files >= file_limit {
            file_limit_hit = true;
            break;
        }
        scanned_files += 1;

        let status_params = RefactorStatusParams {
            file: rel_path.clone(),
            project_dir: Some(project_dir_arg.clone()),
            item_names: None,
            item_kinds: kind_filter.clone(),
            limit: Some(1000),
            include_attributes: p.include_attributes.or(Some(false)),
        };
        let status_json = match crate::refactor::status(&status_params) {
            Ok(json) => json,
            Err(err) => {
                if errors.len() < 20 {
                    errors.push(CodeSymbolSearchError {
                        file: rel_path,
                        error: err.to_string(),
                    });
                }
                continue;
            }
        };
        let status: RefactorStatus = serde_json::from_str(&status_json)
            .context("failed to parse bbox_refactor_status response")?;

        for item in status.items {
            if !item_matches_query(&item, &rel_path, p.query.as_deref()) {
                continue;
            }
            matching_items += 1;
            matched_file_paths.insert(rel_path.clone());
            if items.len() >= limit {
                continue;
            }
            let (symbol_kind, parent_kind) =
                symbol_kind_from_refactor(&status.language, &item.kind);
            items.push(CodeSymbolSearchItem {
                file: rel_path.clone(),
                language: status.language.clone(),
                kind: item.kind.clone(),
                symbol_kind: Some(symbol_kind),
                parent_kind,
                name: item.name.clone(),
                byte_range: (item.byte_start, item.byte_end),
                line_range: (item.line_start, item.line_end),
                handoff: status_item_handoff(&rel_path, Some(&project_dir_arg), language, &item),
            });
        }
    }

    items.sort_by(|a, b| {
        a.file
            .cmp(&b.file)
            .then(a.byte_range.0.cmp(&b.byte_range.0))
    });

    let truncated = file_limit_hit || matching_items > items.len();
    let response = CodeSymbolSearchResponse {
        status: "ok".to_string(),
        project_dir: project_dir_arg,
        mode: CodeSymbolMode::Live.label().to_string(),
        scanned_files,
        matched_files: matched_file_paths.len(),
        matching_items,
        returned_items: items.len(),
        truncated,
        items,
        errors,
        semantic_status: SEMANTIC_STATUS_SYNTAX_ONLY.to_string(),
    };
    Ok(serde_json::to_string_pretty(&response)?)
}

/// Indexed lane. Reads `project_file` docs from tantivy and returns
/// the same record shape as the live lane but without any parse cost.
/// Requires CN-D3 fields (`symbol_kind`, `parent_kind`, `byte_end`,
/// `line_start`, `line_end`, `project_id`) to be populated, which
/// happens on first reindex after the daemon picks up the bumped
/// schema version.
pub fn code_symbols_indexed(
    p: &CodeSymbolSearchParams,
    registered: &[ProjectRecord],
    idx: &crate::index::TranscriptIndex,
) -> Result<String> {
    use tantivy::collector::TopDocs;
    use tantivy::query::{BooleanQuery, Occur, Query as QueryTrait, TermQuery};
    use tantivy::schema::IndexRecordOption;
    use tantivy::{TantivyDocument, Term};

    let project_dir = PathBuf::from(&p.project_dir)
        .canonicalize()
        .with_context(|| format!("failed to resolve project_dir {}", p.project_dir))?;
    if !project_dir.is_dir() {
        return Err(anyhow!("project_dir must be a directory"));
    }
    if let Some(err_json) = check_project_dir_registered(&project_dir, registered)? {
        return Ok(err_json);
    }

    // Map project_dir → project_id via the registered-roots scan
    // (registered_for the same check above already accepted us).
    let project_id = registered
        .iter()
        .filter(|rec| {
            let root = PathBuf::from(&rec.canonical_path);
            let canon = root.canonicalize().unwrap_or(root);
            project_dir == canon || project_dir.starts_with(&canon)
        })
        // Prefer the deepest registered ancestor when worktrees nest
        // (e.g. .claude/worktrees/foo under transcript-search).
        .max_by_key(|rec| rec.canonical_path.len())
        .map(|rec| rec.project_id.clone())
        .ok_or_else(|| {
            anyhow!("internal: project_dir passed gate but no project_id resolved")
        })?;
    let project_dir_arg = project_dir.to_string_lossy().into_owned();

    let limit = p.limit.unwrap_or(100).min(1000);
    let language_filter: Vec<String> = p
        .languages
        .clone()
        .unwrap_or_default()
        .into_iter()
        .filter(|s| !s.is_empty())
        .collect();
    let kind_filter: Vec<String> = p
        .item_kinds
        .clone()
        .unwrap_or_default()
        .into_iter()
        .filter(|s| !s.is_empty())
        .collect();

    let fields = idx.field_handles();
    let mut clauses: Vec<(Occur, Box<dyn QueryTrait>)> = vec![
        (
            Occur::Must,
            Box::new(TermQuery::new(
                Term::from_field_text(fields.doc_type, "project_file"),
                IndexRecordOption::Basic,
            )),
        ),
        (
            Occur::Must,
            Box::new(TermQuery::new(
                Term::from_field_text(fields.project_id, &project_id),
                IndexRecordOption::Basic,
            )),
        ),
    ];

    // Languages: union (Should) across the requested set, wrapped in
    // a Must so any-match is required.
    if !language_filter.is_empty() {
        let lang_clauses: Vec<(Occur, Box<dyn QueryTrait>)> = language_filter
            .iter()
            .map(|lang| {
                (
                    Occur::Should,
                    Box::new(TermQuery::new(
                        Term::from_field_text(fields.language, lang),
                        IndexRecordOption::Basic,
                    )) as Box<dyn QueryTrait>,
                )
            })
            .collect();
        clauses.push((Occur::Must, Box::new(BooleanQuery::new(lang_clauses))));
    }

    // Kinds: accept either the refactor-vocabulary token (matches the
    // synthesised `kind` after live-side projection) or the raw
    // tree-sitter `symbol_kind` token. We probe both fields.
    if !kind_filter.is_empty() {
        let kind_clauses: Vec<(Occur, Box<dyn QueryTrait>)> = kind_filter
            .iter()
            .flat_map(|kind| {
                [
                    (
                        Occur::Should,
                        Box::new(TermQuery::new(
                            Term::from_field_text(fields.symbol_kind, kind),
                            IndexRecordOption::Basic,
                        )) as Box<dyn QueryTrait>,
                    ),
                    (
                        Occur::Should,
                        Box::new(TermQuery::new(
                            Term::from_field_text(fields.symbol_kind, kind),
                            IndexRecordOption::Basic,
                        )) as Box<dyn QueryTrait>,
                    ),
                ]
            })
            .collect();
        clauses.push((Occur::Must, Box::new(BooleanQuery::new(kind_clauses))));
    }

    let query: Box<dyn QueryTrait> = Box::new(BooleanQuery::new(clauses));
    let index_handle = idx.index_handle();
    let reader = index_handle
        .reader_builder()
        .reload_policy(tantivy::ReloadPolicy::Manual)
        .try_into()?;
    let searcher: tantivy::Searcher = {
        let r: tantivy::IndexReader = reader;
        r.searcher()
    };

    // Tantivy can't post-filter on substring; we over-fetch a bit and
    // apply path/name/excerpt-style filtering after stored-field load.
    let over_fetch = limit.saturating_mul(4).max(64);
    let hits = searcher.search(&*query, &TopDocs::with_limit(over_fetch))?;

    let mut items: Vec<CodeSymbolSearchItem> = Vec::new();
    let mut matched_file_paths = std::collections::HashSet::new();
    let mut matching_items = 0usize;

    for (_score, addr) in hits {
        let doc: TantivyDocument = searcher.doc(addr)?;
        let stored_file_path = first_text(&doc, fields.file_path);
        let rel_path = if let Ok(rel) =
            std::path::Path::new(&stored_file_path).strip_prefix(&project_dir)
        {
            rel.to_string_lossy().into_owned()
        } else {
            stored_file_path.clone()
        };

        if let Some(path_contains) =
            p.path_contains.as_deref().filter(|s| !s.is_empty())
        {
            if !rel_path.contains(path_contains) {
                continue;
            }
        }

        let language = optional_text(&doc, fields.language).unwrap_or_default();
        let symbol_kind_raw = optional_text(&doc, fields.symbol_kind);
        let parent_kind_raw = optional_text(&doc, fields.parent_kind);
        // Indexed-only docs that predate CN-D3 don't have symbol_kind.
        // Skip them — the live lane is the fallback for pre-reindex
        // states.
        let Some(symbol_kind) = symbol_kind_raw.clone() else {
            continue;
        };
        let refactor_kind =
            refactor_kind_for(&language, &symbol_kind, parent_kind_raw.as_deref());
        let symbol_display = optional_text(&doc, fields.symbol);
        let symbol_exact = optional_text(&doc, fields.symbol_exact);
        let name = symbol_exact.or(symbol_display.clone());

        // Substring filter on name/path/kind, mirroring live lane.
        if let Some(query) = p.query.as_deref().filter(|s| !s.is_empty()) {
            let in_name = name.as_deref().is_some_and(|n| n.contains(query));
            let in_kind = refactor_kind.contains(query) || symbol_kind.contains(query);
            let in_path = rel_path.contains(query);
            if !(in_name || in_kind || in_path) {
                continue;
            }
        }

        let byte_start = first_u64(&doc, fields.byte_offset) as usize;
        let byte_end = first_u64(&doc, fields.byte_end) as usize;
        let line_start = first_u64(&doc, fields.line_start) as usize;
        let line_end = first_u64(&doc, fields.line_end) as usize;

        matching_items += 1;
        matched_file_paths.insert(rel_path.clone());
        if items.len() >= limit {
            continue;
        }

        let handoff = indexed_handoff(
            &rel_path,
            &project_dir_arg,
            &language,
            name.as_deref(),
            &refactor_kind,
            (byte_start, byte_end),
            (line_start, line_end),
        );

        items.push(CodeSymbolSearchItem {
            file: rel_path,
            language,
            kind: refactor_kind,
            symbol_kind: Some(symbol_kind),
            parent_kind: parent_kind_raw,
            name,
            byte_range: (byte_start, byte_end),
            line_range: (line_start, line_end),
            handoff,
        });
    }

    items.sort_by(|a, b| {
        a.file
            .cmp(&b.file)
            .then(a.byte_range.0.cmp(&b.byte_range.0))
    });

    let truncated = matching_items > items.len();
    let response = CodeSymbolSearchResponse {
        status: "ok".to_string(),
        project_dir: project_dir_arg,
        mode: CodeSymbolMode::Indexed.label().to_string(),
        // Indexed lane has no per-file scan; report 0 to keep the
        // field present (response shape stable across modes) while
        // signalling the asymmetry honestly.
        scanned_files: 0,
        matched_files: matched_file_paths.len(),
        matching_items,
        returned_items: items.len(),
        truncated,
        items,
        errors: Vec::new(),
        semantic_status: SEMANTIC_STATUS_SYNTAX_ONLY.to_string(),
    };
    Ok(serde_json::to_string_pretty(&response)?)
}

pub fn code_query(p: &CodeQueryParams) -> Result<String> {
    let path = resolve_path(p.project_dir.as_deref(), &p.file)?;
    if let Some(err_json) = check_code_nav_file_size(&path)? {
        return Ok(err_json);
    }
    let parsed = parse_code_nav_source(&path, p.language.as_deref())?;
    let ts_lang = ts_language_for_name(&parsed.language)?;

    let query =
        Query::new(&ts_lang, &p.query).map_err(|e| anyhow!("invalid tree-sitter query: {}", e))?;

    let mut cursor = QueryCursor::new();
    let mut matches = cursor.matches(&query, parsed.tree.root_node(), parsed.source.as_bytes());

    let mut captures = Vec::new();
    let mut matching_captures = 0usize;
    let limit = p.limit.unwrap_or(200).min(1000);

    while let Some(m) = matches.next() {
        for cap in m.captures {
            matching_captures += 1;
            let node = cap.node;
            let capture_name = query.capture_names()[cap.index as usize].to_string();

            if captures.len() >= limit {
                continue;
            }

            let text = if p.include_text.unwrap_or(false) {
                Some(excerpt(
                    &parsed.source[node.start_byte()..node.end_byte()],
                    1024,
                ))
            } else {
                None
            };

            let parent_kind = node.parent().map(|p| p.kind().to_string());
            let handoff = refactor_handoff(
                &p.file,
                p.project_dir.as_deref(),
                &parsed.language,
                &parsed.source,
                node,
                text.as_deref(),
            );

            captures.push(CodeQueryCapture {
                capture_name,
                node_kind: node.kind().to_string(),
                byte_range: (node.start_byte(), node.end_byte()),
                line_range: (node.start_position().row + 1, node.end_position().row + 1),
                column_range: (
                    node.start_position().column + 1,
                    node.end_position().column + 1,
                ),
                text,
                parent_kind,
                handoff,
            });
        }
    }

    let report = parse_report(parsed.tree.root_node());

    let response = CodeQueryResponse {
        status: "ok".to_string(),
        path: path.to_string_lossy().to_string(),
        language: parsed.language.clone(),
        matching_captures,
        returned_captures: captures.len(),
        truncated: matching_captures > captures.len(),
        captures,
        parse_report: report,
        semantic_status: SEMANTIC_STATUS_SYNTAX_ONLY.to_string(),
    };

    Ok(serde_json::to_string_pretty(&response)?)
}

pub fn code_node_describe(p: &CodeNodeDescribeParams) -> Result<String> {
    let path = resolve_path(p.project_dir.as_deref(), &p.file)?;
    if let Some(err_json) = check_code_nav_file_size(&path)? {
        return Ok(err_json);
    }
    let parsed = parse_code_nav_source(&path, None)?;

    let row = p.line.saturating_sub(1);
    let column = p.column.saturating_sub(1);
    let point = tree_sitter::Point::new(row, column);

    let root = parsed.tree.root_node();

    let node = root
        .named_descendant_for_point_range(point, point)
        .ok_or_else(|| anyhow!("no named node found at line {} column {}", p.line, p.column))?;

    let text = if p.include_text.unwrap_or(false) {
        Some(excerpt(
            &parsed.source[node.start_byte()..node.end_byte()],
            1024,
        ))
    } else {
        None
    };

    let mut parent_chain = Vec::new();
    let mut current = node.parent();
    while let Some(parent) = current {
        parent_chain.push(parent.kind().to_string());
        current = parent.parent();
    }

    let field_in_parent = node.parent().and_then(|parent| {
        let mut cursor = parent.walk();
        if cursor.goto_first_child() {
            loop {
                if cursor.node() == node {
                    return cursor.field_name().map(|s| s.to_string());
                }
                if !cursor.goto_next_sibling() {
                    break;
                }
            }
        }
        None
    });

    let mut named_children = Vec::new();
    let mut cursor = node.walk();
    if cursor.goto_first_child() {
        loop {
            let child = cursor.node();
            if child.is_named() {
                named_children.push(CodeNodeChild {
                    kind: child.kind().to_string(),
                    field: cursor.field_name().map(|s| s.to_string()),
                    byte_range: (child.start_byte(), child.end_byte()),
                    line_range: (child.start_position().row + 1, child.end_position().row + 1),
                    column_range: (
                        child.start_position().column + 1,
                        child.end_position().column + 1,
                    ),
                    text: p.include_text.unwrap_or(false).then(|| {
                        excerpt(&parsed.source[child.start_byte()..child.end_byte()], 256)
                    }),
                });
            }
            if !cursor.goto_next_sibling() {
                break;
            }
        }
    }

    let previous_sibling = if p.include_siblings.unwrap_or(false) {
        node.prev_named_sibling().map(|s| CodeNodeSibling {
            kind: s.kind().to_string(),
            byte_range: (s.start_byte(), s.end_byte()),
            line_range: (s.start_position().row + 1, s.end_position().row + 1),
            column_range: (s.start_position().column + 1, s.end_position().column + 1),
            text: p
                .include_text
                .unwrap_or(false)
                .then(|| excerpt(&parsed.source[s.start_byte()..s.end_byte()], 256)),
        })
    } else {
        None
    };

    let next_sibling = if p.include_siblings.unwrap_or(false) {
        node.next_named_sibling().map(|s| CodeNodeSibling {
            kind: s.kind().to_string(),
            byte_range: (s.start_byte(), s.end_byte()),
            line_range: (s.start_position().row + 1, s.end_position().row + 1),
            column_range: (s.start_position().column + 1, s.end_position().column + 1),
            text: p
                .include_text
                .unwrap_or(false)
                .then(|| excerpt(&parsed.source[s.start_byte()..s.end_byte()], 256)),
        })
    } else {
        None
    };

    let report = parse_report(root);
    let handoff = refactor_handoff(
        &p.file,
        p.project_dir.as_deref(),
        &parsed.language,
        &parsed.source,
        node,
        text.as_deref(),
    );

    let response = CodeNodeDescribeResponse {
        status: "ok".to_string(),
        path: parsed.path.to_string_lossy().to_string(),
        language: parsed.language.clone(),
        node_kind: node.kind().to_string(),
        byte_range: (node.start_byte(), node.end_byte()),
        line_range: (node.start_position().row + 1, node.end_position().row + 1),
        column_range: (
            node.start_position().column + 1,
            node.end_position().column + 1,
        ),
        field_in_parent,
        text,
        named_children,
        parent_chain,
        previous_sibling,
        next_sibling,
        handoff,
        parse_report: report,
        semantic_status: SEMANTIC_STATUS_SYNTAX_ONLY.to_string(),
    };

    Ok(serde_json::to_string_pretty(&response)?)
}
