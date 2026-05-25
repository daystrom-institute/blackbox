use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow};
use ignore::{DirEntry, WalkBuilder};
use rmcp::schemars;
use serde::{Deserialize, Serialize};
use tree_sitter::{Query, QueryCursor, StreamingIterator, Tree};

use crate::chunker::code::{language_for_path, parser_for_language, ts_language_for_name};
use crate::index::{first_text, first_u64, optional_text};
use crate::projects::ProjectRecord;
use crate::refactor::{
    ParseReport, RefactorStatus, RefactorStatusParams, SyntaxItem, parse_report, resolve_path,
};

#[cfg(test)]
mod tests;

pub(crate) mod semantic;

/// Top-level `semantic_status` value for every code-nav tool response.
///
/// Code-nav tools (`bbox_code_query`, `bbox_code_node_describe`,
/// `bbox_code_symbols`) are syntax locators, not binding-aware lookups.
/// Per the design boundary in `design/archive/code-nav-symbolic-exploration.md`,
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
pub fn refactor_kind_for(language: &str, symbol_kind: &str, parent_kind: Option<&str>) -> String {
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
pub fn symbol_kind_from_refactor(language: &str, refactor_kind: &str) -> (String, Option<String>) {
    match (language, refactor_kind) {
        ("rust", "impl_method") => ("function_item".to_string(), Some("impl_item".to_string())),
        _ => (refactor_kind.to_string(), None),
    }
}

/// Build the tantivy boolean clauses that match a single
/// `item_kinds` token on the indexed lane. The token may be either a
/// raw tree-sitter node kind (matches `symbol_kind` directly) OR a
/// refactor synthetic kind that decomposes into a (language,
/// symbol_kind, parent_kind) constraint.
///
/// Returns a vector of clauses — one entry for the raw-kind probe
/// plus, when the token has a documented synthesis, an additional
/// `BooleanQuery(Must language=L AND Must symbol_kind=X AND Must
/// parent_kind=Y)`. Caller ORs them together inside a containing
/// `BooleanQuery`.
///
/// Synthesis cases live here AND in `refactor_kind_for` — the two
/// must stay in sync. New synthesis needs an entry in BOTH. The
/// language guard inside the synthesis BooleanQuery mirrors the
/// `(language, symbol_kind, parent_kind)` match in
/// `refactor_kind_for` so a non-Rust grammar that happens to emit a
/// `function_item` under an `impl_item` does not get reported as
/// `impl_method`.
fn indexed_kind_filter_for(
    fields: crate::index::FieldHandles,
    kind: &str,
) -> Vec<Box<dyn tantivy::query::Query>> {
    use tantivy::Term;
    use tantivy::query::{BooleanQuery, Occur, Query, TermQuery};
    use tantivy::schema::IndexRecordOption;

    let raw_probe: Box<dyn Query> = Box::new(TermQuery::new(
        Term::from_field_text(fields.symbol_kind, kind),
        IndexRecordOption::Basic,
    ));

    // Synthesis decompositions. Mirror of refactor_kind_for cases —
    // grep "refactor_kind_for" before adding a new case here.
    let synth: Option<Box<dyn Query>> = match kind {
        "impl_method" => Some(Box::new(BooleanQuery::new(vec![
            (
                Occur::Must,
                Box::new(TermQuery::new(
                    Term::from_field_text(fields.language, "rust"),
                    IndexRecordOption::Basic,
                )) as Box<dyn Query>,
            ),
            (
                Occur::Must,
                Box::new(TermQuery::new(
                    Term::from_field_text(fields.symbol_kind, "function_item"),
                    IndexRecordOption::Basic,
                )) as Box<dyn Query>,
            ),
            (
                Occur::Must,
                Box::new(TermQuery::new(
                    Term::from_field_text(fields.parent_kind, "impl_item"),
                    IndexRecordOption::Basic,
                )) as Box<dyn Query>,
            ),
        ]))),
        _ => None,
    };

    match synth {
        Some(s) => vec![raw_probe, s],
        None => vec![raw_probe],
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
    /// Stable machine-readable code. Documented values (the
    /// `sm-refactor` system-memory entry lists the same set and is
    /// the agent-facing source of truth):
    /// - `file_too_large_for_code_nav` — source exceeds
    ///   `MAX_CODE_NAV_FILE_BYTES`. Carries `file_bytes` +
    ///   `max_bytes`.
    /// - `project_not_registered` — `project_dir` is not a
    ///   registered root nor a descendant of one. Carries
    ///   `registered_projects` list.
    /// - `invalid_code_symbols_mode` —
    ///   `bbox_code_symbols(mode=...)` was not `"indexed"` /
    ///   `"live"`.
    /// - `invalid_code_refs_kind` —
    ///   `bbox_code_refs(kind=...)` was not `"calls"` /
    ///   `"imports"` / `"fields"` / `"identifiers"` / `"all"`.
    /// - `unsupported_language_for_code_refs` — language has no
    ///   curated reference query and the requested `kind` is not
    ///   `"identifiers"`. Suggestion names the fallback.
    /// New codes added in future tools should follow the same
    /// `<surface>_<failure>` shape and be added to this list +
    /// the `sm-refactor` error vocabulary in lock step.
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
        suggestion: "Narrow the request: target a smaller file, or use bbox_refactor_status with item_kinds to locate a specific symbol without reparsing the whole file.".to_string(),
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
        message: format!("{project_dir} is not a registered project root or a descendant of one"),
        suggestion: format!(
            "Either pass a project_dir at or under one of `registered_projects[*].canonical_path`, \
             or call `bbox_project_register(path=\"{project_dir}\")` to register this directory first."
        ),
        file: None,
        file_bytes: None,
        max_bytes: None,
        project_dir: Some(project_dir.to_string()),
        registered_projects: registered
            .iter()
            .map(CodeNavProjectHint::from_record)
            .collect(),
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
    /// Why the response is truncated, when it is. Stable strings:
    /// - `"limit_reached"`: more items matched than `limit` allowed
    ///   to be returned; `matching_items` is exact. Bump `limit` or
    ///   narrow the filters.
    /// - `"file_limit_reached"`: **live lane only** — the walker hit
    ///   `MAX_CODE_NAV_SCANNED_FILES` before finishing. Narrow
    ///   `path_contains` / `languages` / `project_dir`.
    /// - `"scan_cap_reached"`: **indexed lane only** — the tantivy
    ///   match count exceeded the post-filter scan cap (5000) AND
    ///   the result must be post-filtered (`query` or
    ///   `path_contains` is set). `matching_items` is a lower
    ///   bound, not exact. Narrow the search.
    /// Omitted from JSON when `truncated=false`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub truncation_reason: Option<String>,
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

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct CodeRefsParams {
    /// Source file to extract references from. Absolute or
    /// project-relative when `project_dir` is set.
    pub file: String,
    /// Project root (resolves relative `file`; subject to the CN-S1
    /// file-size gate). Optional — when omitted the file path must
    /// be absolute and the gate still applies.
    #[serde(default)]
    pub project_dir: Option<String>,
    /// Which references to extract. Stable values:
    /// - `"calls"`: function-call sites
    /// - `"imports"`: import / use declarations
    /// - `"fields"`: field-access expressions
    /// - `"identifiers"`: every identifier occurrence
    /// - `"all"`: union of the above
    pub kind: String,
    /// Optional case-sensitive substring filter on the reference's
    /// display name. Useful for narrowing `"identifiers"` (which is
    /// otherwise huge) to a single symbol.
    #[serde(default)]
    pub query: Option<String>,
    /// Maximum returned records. Defaults to 200, capped at 1000.
    /// Truncation is signalled via `truncation_reason` in the
    /// response.
    #[serde(default)]
    pub limit: Option<usize>,
    /// Include a short text excerpt on each record. Defaults false.
    #[serde(default)]
    pub include_text: Option<bool>,
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
pub struct CodeRefsResponse {
    pub status: String,
    pub path: String,
    pub language: String,
    /// Echoes the `kind` request parameter so callers can dispatch.
    pub kind_filter: String,
    pub matching_refs: usize,
    pub returned_refs: usize,
    pub truncated: bool,
    /// Stable values: `"limit_reached"` when more refs matched than
    /// `limit` allowed; omitted when `truncated=false`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub truncation_reason: Option<String>,
    pub refs: Vec<CodeRefRecord>,
    pub parse_report: ParseReport,
    /// Always `"syntax_only"` — these are tree-sitter captures, not
    /// binding-aware references. Use LSP / refactor graph for binding
    /// authority.
    pub semantic_status: String,
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
pub struct CodeRefRecord {
    /// Reference category: `"call"`, `"import"`, `"field"`, or
    /// `"identifier"`.
    pub kind: String,
    /// Displayed name (the identifier text at the capture site).
    pub name: String,
    /// Raw tree-sitter node kind that produced the capture.
    pub node_kind: String,
    pub byte_range: (usize, usize),
    pub line_range: (usize, usize),
    pub column_range: (usize, usize),
    /// Nearest enclosing symbol-producing ancestor's display name,
    /// when walking the parent chain finds one cheaply. Same notion
    /// of "containing symbol" as `Chunk.parent_kind`. `None` at file
    /// top level or when no symbol-producing ancestor is found.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub containing_symbol: Option<String>,
    /// Always `"heuristic"`. These are syntax captures, not binding
    /// resolution — even when a call site's `name` matches an
    /// indexed symbol exactly, that's a name collision until LSP /
    /// the refactor graph confirms.
    pub edge_confidence: String,
    /// Source excerpt when the caller requested `include_text=true`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    /// Pre-filled handoff suggestions for the next call. Same shape
    /// as `bbox_code_symbols` / `bbox_code_node_describe` records:
    /// agents can read `.handoff.refactor_status.arguments` to find
    /// the exact refactor inventory call that grounds this ref.
    /// Use these instead of guessing argument shapes.
    pub handoff: CodeRefactorHandoff,
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
            | "record_declaration"
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
        Some(raw) => match CodeSymbolMode::from_param(Some(raw)) {
            Ok(m) => m,
            Err(_) => return err_invalid_code_symbols_mode(raw),
        },
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

/// Build the JSON for a `invalid_code_symbols_mode` error response.
fn err_invalid_code_symbols_mode(raw: &str) -> Result<String> {
    let response = CodeNavErrorResponse {
        status: "error".to_string(),
        code: "invalid_code_symbols_mode".to_string(),
        message: format!(
            "mode {raw:?} is not valid for bbox_code_symbols; expected \"indexed\" or \"live\""
        ),
        suggestion: "Pass mode=\"indexed\" (default when the daemon has a populated index) or mode=\"live\" to walk and reparse the project tree.".to_string(),
        file: None,
        file_bytes: None,
        max_bytes: None,
        project_dir: None,
        registered_projects: Vec::new(),
        semantic_status: SEMANTIC_STATUS_SYNTAX_ONLY.to_string(),
    };
    Ok(serde_json::to_string_pretty(&response)?)
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

        // Per-file size gate. refactor::status will read the whole file,
        // so stat first and skip files that exceed MAX_CODE_NAV_FILE_BYTES.
        // Report a typed per-file error so the agent sees what was skipped
        // rather than wondering why a matching symbol is missing.
        if let Ok(metadata) = fs::metadata(path) {
            if metadata.len() > MAX_CODE_NAV_FILE_BYTES {
                if errors.len() < 20 {
                    errors.push(CodeSymbolSearchError {
                        file: rel_path.clone(),
                        error: format!(
                            "file_too_large_for_code_nav: {} bytes (cap {})",
                            metadata.len(),
                            MAX_CODE_NAV_FILE_BYTES
                        ),
                    });
                }
                continue;
            }
        }

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
    // File-limit-hit dominates: if we stopped walking, the user
    // needs to narrow the scan first; the limit-vs-fetched mismatch
    // is a secondary concern they can't fix until the walk
    // completes.
    let truncation_reason = if file_limit_hit {
        Some("file_limit_reached".to_string())
    } else if matching_items > items.len() {
        Some("limit_reached".to_string())
    } else {
        None
    };
    let response = CodeSymbolSearchResponse {
        status: "ok".to_string(),
        project_dir: project_dir_arg,
        mode: CodeSymbolMode::Live.label().to_string(),
        scanned_files,
        matched_files: matched_file_paths.len(),
        matching_items,
        returned_items: items.len(),
        truncated,
        truncation_reason,
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
        .ok_or_else(|| anyhow!("internal: project_dir passed gate but no project_id resolved"))?;
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

    // Kinds: accept BOTH vocabularies on a single filter list:
    // - raw tree-sitter kinds (e.g. `"function_item"`) match the
    //   stored `symbol_kind` field directly
    // - refactor synthetic kinds (e.g. `"impl_method"`) decompose
    //   into a constraint on (symbol_kind, parent_kind), e.g.
    //   impl_method => symbol_kind=function_item AND
    //                  parent_kind=impl_item
    // The set of synthesis cases lives in `indexed_kind_filter_for`
    // alongside `refactor_kind_for`.
    if !kind_filter.is_empty() {
        let kind_clauses: Vec<(Occur, Box<dyn QueryTrait>)> = kind_filter
            .iter()
            .flat_map(|kind| indexed_kind_filter_for(fields, kind))
            .map(|q| (Occur::Should, q))
            .collect();
        clauses.push((Occur::Must, Box::new(BooleanQuery::new(kind_clauses))));
    }

    let query: Box<dyn QueryTrait> = Box::new(BooleanQuery::new(clauses));
    // Snapshot the shared daemon reader (cheap; avoids the per-call
    // reader-build that codex round-1 review flagged).
    let searcher = idx.searcher();

    // The indexed lane can't push `query`/`path_contains` substring
    // filters into tantivy — they're free-text substrings, not
    // tokenisable. We over-fetch up to a high cap and post-filter; if
    // we hit the cap with `matching_items > items.len()` we set
    // `truncation_reason = "scan_cap_reached"` so the caller knows the
    // count is a lower bound. Without an explicit `query`/path filter
    // the tantivy-level filter is exact, so `limit` itself is the
    // truthful upper bound.
    let has_post_filter = p.query.as_deref().is_some_and(|q| !q.is_empty())
        || p.path_contains.as_deref().is_some_and(|q| !q.is_empty());
    const INDEXED_SCAN_CAP: usize = 5000;

    // Three honest paths:
    //
    // 1. has_post_filter=false: every tantivy hit is a valid match.
    //    Count() gives us the exact total; fetch limit + small
    //    headroom; `truncated = total > items.len()` =>
    //    `limit_reached`. `scan_cap_reached` is NEVER reported on
    //    this path because the fetch ceiling is bound by `limit`,
    //    not by what tantivy could return.
    //
    // 2. has_post_filter=true and tantivy match count <=
    //    INDEXED_SCAN_CAP: fetch all of them. Post-filter walks the
    //    full set; the post-filtered count is exact. Truncation is
    //    `limit_reached` if post-filtered > items.len().
    //
    // 3. has_post_filter=true and tantivy matches > INDEXED_SCAN_CAP:
    //    fetch the cap, walk what we got, and set
    //    `truncation_reason = "scan_cap_reached"` so the caller
    //    knows there may be more matches past the cap and that
    //    `matching_items` is a lower bound.
    use tantivy::collector::Count;
    let total_hits = searcher.search(&*query, &Count)?;
    let fetch_cap = if has_post_filter {
        total_hits.min(INDEXED_SCAN_CAP)
    } else {
        // No post-filter: just enough to fill `limit` + headroom so
        // we have a coverage signal if anything (shouldn't) gets
        // dropped during stored-field load.
        limit.saturating_mul(2).max(64).min(total_hits)
    };
    let hits = searcher.search(&*query, &TopDocs::with_limit(fetch_cap))?;
    let scan_cap_hit = has_post_filter && total_hits > INDEXED_SCAN_CAP;

    let mut items: Vec<CodeSymbolSearchItem> = Vec::new();
    let mut matched_file_paths = std::collections::HashSet::new();
    let mut matching_items = 0usize;

    for (_score, addr) in hits {
        let doc: TantivyDocument = searcher.doc(addr)?;
        let stored_file_path = first_text(&doc, fields.file_path);
        let rel_path =
            if let Ok(rel) = std::path::Path::new(&stored_file_path).strip_prefix(&project_dir) {
                rel.to_string_lossy().into_owned()
            } else {
                stored_file_path.clone()
            };

        if let Some(path_contains) = p.path_contains.as_deref().filter(|s| !s.is_empty()) {
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
        let refactor_kind = refactor_kind_for(&language, &symbol_kind, parent_kind_raw.as_deref());
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

    let truncated = matching_items > items.len() || scan_cap_hit;
    // scan_cap_hit dominates limit_reached because the caller needs
    // to know the count itself is a lower bound — more matches may
    // exist past the cap that we never even inspected.
    let truncation_reason = if scan_cap_hit {
        Some("scan_cap_reached".to_string())
    } else if matching_items > items.len() {
        Some("limit_reached".to_string())
    } else {
        None
    };
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
        truncation_reason,
        items,
        errors: Vec::new(),
        semantic_status: SEMANTIC_STATUS_SYNTAX_ONLY.to_string(),
    };
    Ok(serde_json::to_string_pretty(&response)?)
}

/// Per-language tree-sitter S-expression query strings for
/// `bbox_code_refs`. Each query emits four capture names:
/// `@call`, `@import`, `@field`, `@identifier`. The caller filters
/// by capture name to honour the requested `kind`. Adding a new
/// language: emit those four capture names; if the grammar has no
/// useful pattern for a kind, omit it — the kind will return empty
/// records for that language rather than erroring.
fn code_refs_query_for(language: &str) -> Option<&'static str> {
    match language {
        "rust" => Some(
            r#"
            (call_expression function: (_) @call)
            (use_declaration argument: (_) @import)
            (field_expression field: (_) @field)
            (identifier) @identifier
            "#,
        ),
        "java" => Some(
            r#"
            (method_invocation name: (_) @call)
            (import_declaration) @import
            (field_access field: (_) @field)
            (identifier) @identifier
            "#,
        ),
        "python" => Some(
            r#"
            (call function: (_) @call)
            (import_statement) @import
            (import_from_statement) @import
            (attribute attribute: (_) @field)
            (identifier) @identifier
            "#,
        ),
        "typescript" | "javascript" => Some(
            r#"
            (call_expression function: (_) @call)
            (import_statement) @import
            (member_expression property: (_) @field)
            (identifier) @identifier
            "#,
        ),
        "go" => Some(
            r#"
            (call_expression function: (_) @call)
            (import_spec) @import
            (selector_expression field: (_) @field)
            (identifier) @identifier
            "#,
        ),
        _ => None,
    }
}

/// Return the set of capture names that match the requested `kind`
/// filter. `"all"` accepts every capture; named filters accept only
/// the matching capture. Unknown kinds return `Ok(None)`; callers
/// translate that into the `invalid_code_refs_kind` typed error.
fn code_refs_capture_filter(kind: &str) -> Option<&'static [&'static str]> {
    match kind {
        "calls" => Some(&["call"]),
        "imports" => Some(&["import"]),
        "fields" => Some(&["field"]),
        "identifiers" => Some(&["identifier"]),
        "all" => Some(&["call", "import", "field", "identifier"]),
        _ => None,
    }
}

/// Build the JSON for an `invalid_code_refs_kind` error response.
fn err_invalid_code_refs_kind(raw: &str) -> Result<String> {
    let response = CodeNavErrorResponse {
        status: "error".to_string(),
        code: "invalid_code_refs_kind".to_string(),
        message: format!(
            "kind {raw:?} is not valid for bbox_code_refs; expected one of \
             \"calls\", \"imports\", \"fields\", \"identifiers\", \"all\""
        ),
        suggestion: "Pass kind=\"calls\" / \"imports\" / \"fields\" / \"identifiers\" / \"all\". \
             Use kind=\"all\" if you want the union; narrow with `query` for substring filtering."
            .to_string(),
        file: None,
        file_bytes: None,
        max_bytes: None,
        project_dir: None,
        registered_projects: Vec::new(),
        semantic_status: SEMANTIC_STATUS_SYNTAX_ONLY.to_string(),
    };
    Ok(serde_json::to_string_pretty(&response)?)
}

/// Returns true when `kind` is a tree-sitter node kind that can act as a
/// *containing symbol* for a reference. Built from the one canonical
/// symbol-node set (`chunker::code::is_symbol_node`) rather than a
/// hand-copied subset, plus `impl_item` (which `is_symbol_node` does not
/// list — the chunker special-cases it in `symbol_name`), minus kinds that
/// are not usable containing scopes:
/// - parser/root wrappers (`is_root_kind`: source_file/program/module/...),
/// - `package_declaration` (a symbol node, but not a parser root, so
///   `is_root_kind` does not exclude it; a package is not a containing scope),
/// - `field_declaration` (a member, not an enclosing scope).
fn is_containing_symbol_kind(kind: &str) -> bool {
    (crate::chunker::code::is_symbol_node(kind) || kind == "impl_item")
        && !is_root_kind(kind)
        && !matches!(kind, "field_declaration" | "package_declaration")
}

/// Resolve the display name of a containing-symbol ancestor node, or `None`
/// when this particular node carries no usable name (e.g. an anonymous Go
/// `interface_type`). `None` here means "keep climbing", not "give up".
fn containing_symbol_name(
    parent: tree_sitter::Node<'_>,
    source: &str,
    language: &str,
) -> Option<String> {
    if let Some(name_node) = parent.child_by_field_name("name") {
        if let Ok(s) = name_node.utf8_text(source.as_bytes()) {
            return Some(s.to_string());
        }
    }
    // Rust impl headers have no `name` field — use the whole header text
    // up to the body's `{`, mirroring `impl_header` from the chunker.
    if language == "rust" && parent.kind() == "impl_item" {
        if let Some(body) = parent.child_by_field_name("body") {
            let header = &source[parent.start_byte()..body.start_byte()];
            return Some(header.trim().to_string());
        }
    }
    None
}

/// Walk up the tree from `node` and return the display name of the
/// nearest *named* symbol-producing ancestor (same notion of "containing
/// symbol" as `SymbolSpec.parent_kind`). Best-effort — returns `None` at
/// file top level or when no named symbol ancestor exists.
///
/// A matched-but-nameless container (e.g. an anonymous Go `interface_type`)
/// does NOT terminate the walk: we keep climbing to the next named symbol
/// rather than returning `None` prematurely.
fn containing_symbol_for(
    node: tree_sitter::Node<'_>,
    source: &str,
    language: &str,
) -> Option<String> {
    let mut current = node.parent();
    while let Some(parent) = current {
        if is_containing_symbol_kind(parent.kind()) {
            if let Some(name) = containing_symbol_name(parent, source, language) {
                return Some(name);
            }
            // Nameless container — keep climbing instead of giving up.
        }
        current = parent.parent();
    }
    None
}

pub fn code_refs(p: &CodeRefsParams) -> Result<String> {
    use tree_sitter::{Query, QueryCursor, StreamingIterator};

    let path = resolve_path(p.project_dir.as_deref(), &p.file)?;
    if let Some(err_json) = check_code_nav_file_size(&path)? {
        return Ok(err_json);
    }
    let parsed = parse_code_nav_source(&path, None)?;
    let language = parsed.language.clone();

    let capture_filter = match code_refs_capture_filter(&p.kind) {
        Some(filter) => filter,
        None => return err_invalid_code_refs_kind(&p.kind),
    };
    let query_text = match code_refs_query_for(&language) {
        Some(q) => q,
        None => {
            // Unsupported language: only `identifiers` can fall back
            // to a generic walker. For other kinds, return a typed
            // error so the caller knows to switch tools.
            if p.kind == "identifiers" {
                // Generic walker fallback (rare path).
                return code_refs_generic_identifiers(&parsed, p);
            }
            let response = CodeNavErrorResponse {
                status: "error".to_string(),
                code: "unsupported_language_for_code_refs".to_string(),
                message: format!(
                    "{language} has no curated tree-sitter query for {kind:?}; \
                     `identifiers` is the only kind that falls back to the generic walker.",
                    kind = p.kind
                ),
                suggestion:
                    "Either pass kind=\"identifiers\" (shape-only fallback — emits records \
                     for nodes literally named `identifier`; may return zero on grammars \
                     that use different identifier-like kinds, e.g. Erlang's `atom`/`variable`), \
                     or use bbox_code_query with a grammar-native S-expression."
                        .to_string(),
                file: Some(path.to_string_lossy().into_owned()),
                file_bytes: None,
                max_bytes: None,
                project_dir: p.project_dir.clone(),
                registered_projects: Vec::new(),
                semantic_status: SEMANTIC_STATUS_SYNTAX_ONLY.to_string(),
            };
            return Ok(serde_json::to_string_pretty(&response)?);
        }
    };

    let ts_lang = ts_language_for_name(&language)?;
    let query = Query::new(&ts_lang, query_text)
        .map_err(|e| anyhow!("internal: invalid code_refs query for {language}: {e}"))?;
    let capture_names = query.capture_names();

    let limit = p.limit.unwrap_or(200).min(1000);
    let include_text = p.include_text.unwrap_or(false);
    let name_filter = p.query.as_deref().filter(|s| !s.is_empty());

    let mut cursor = QueryCursor::new();
    let mut matches = cursor.matches(&query, parsed.tree.root_node(), parsed.source.as_bytes());

    let mut matching_refs = 0usize;
    let mut records: Vec<CodeRefRecord> = Vec::new();

    while let Some(m) = matches.next() {
        for cap in m.captures {
            let capture_name = capture_names[cap.index as usize];
            if !capture_filter.contains(&capture_name) {
                continue;
            }
            let node = cap.node;
            let Ok(name) = node.utf8_text(parsed.source.as_bytes()) else {
                continue;
            };
            if let Some(filter) = name_filter {
                if !name.contains(filter) {
                    continue;
                }
            }
            matching_refs += 1;
            if records.len() >= limit {
                continue;
            }
            let start = node.start_position();
            let end = node.end_position();
            let text = if include_text {
                Some(excerpt(name, 200))
            } else {
                None
            };
            let containing_symbol = containing_symbol_for(node, &parsed.source, &language);
            let byte_range = (node.start_byte(), node.end_byte());
            let line_range = (start.row + 1, end.row + 1);
            let column_range = (start.column + 1, end.column + 1);
            let handoff = code_ref_handoff(
                &p.file,
                p.project_dir.as_deref(),
                &language,
                name,
                node.kind(),
                byte_range,
                line_range,
                column_range,
                containing_symbol.as_deref(),
            );
            records.push(CodeRefRecord {
                kind: capture_to_ref_kind(capture_name).to_string(),
                name: name.to_string(),
                node_kind: node.kind().to_string(),
                byte_range,
                line_range,
                column_range,
                containing_symbol,
                edge_confidence: "heuristic".to_string(),
                text,
                handoff,
            });
        }
    }

    // Stable ordering by (byte_start, byte_end) so successive calls
    // return records in the same order.
    records.sort_by_key(|r| (r.byte_range.0, r.byte_range.1));

    let truncated = matching_refs > records.len();
    let truncation_reason = if truncated {
        Some("limit_reached".to_string())
    } else {
        None
    };

    let response = CodeRefsResponse {
        status: "ok".to_string(),
        path: path.to_string_lossy().into_owned(),
        language,
        kind_filter: p.kind.clone(),
        matching_refs,
        returned_refs: records.len(),
        truncated,
        truncation_reason,
        refs: records,
        parse_report: parse_report(parsed.tree.root_node()),
        semantic_status: SEMANTIC_STATUS_SYNTAX_ONLY.to_string(),
    };
    Ok(serde_json::to_string_pretty(&response)?)
}

/// Handoff builder for `bbox_code_refs` records. Same shape as
/// `bbox_code_symbols` / `bbox_code_node_describe` records so agents
/// don't have to special-case. The `nearest_refactor_item` reports
/// the ref site itself (its kind / name / byte_range / line_range);
/// `refactor_status` pre-fills item_names with the containing symbol
/// when known (a more specific target for refactor planning), or the
/// ref name otherwise; `project_refs` pre-fills query with the same
/// name so callers can ground via project_file entity refs.
fn code_ref_handoff(
    file: &str,
    project_dir: Option<&str>,
    language: &str,
    name: &str,
    node_kind: &str,
    byte_range: (usize, usize),
    line_range: (usize, usize),
    column_range: (usize, usize),
    containing_symbol: Option<&str>,
) -> CodeRefactorHandoff {
    let nearest_refactor_item = Some(CodeNodeSummary {
        kind: node_kind.to_string(),
        name: Some(name.to_string()),
        byte_range,
        line_range,
        column_range,
    });
    let status_target = containing_symbol.unwrap_or(name);
    let refactor_status = Some(CodeRefactorStatusHint {
        tool: "bbox_refactor_status".to_string(),
        arguments: CodeRefactorStatusHintArgs {
            file: file.to_string(),
            project_dir: project_dir.map(str::to_string),
            item_names: vec![status_target.to_string()],
            // We don't know the refactor item kind from a ref site
            // (a `call` capture is an identifier inside a call_expr,
            // not the definition); leave item_kinds empty so
            // bbox_refactor_status matches on item_names alone.
            item_kinds: Vec::new(),
            limit: 50,
            include_attributes: false,
        },
    });
    CodeRefactorHandoff {
        nearest_refactor_item,
        refactor_status,
        project_refs: CodeProjectRefsHint {
            tool: "bbox_refactor_project_refs".to_string(),
            arguments: CodeProjectRefsHintArgs {
                file: file.to_string(),
                project_dir: project_dir.map(str::to_string),
                query: Some(status_target.to_string()),
                limit: 20,
                include_excerpt: false,
            },
        },
        note: format!(
            "Syntax-only reference capture for {language}. edge_confidence=\"heuristic\" — this is the syntax anchor at the reference site; it does NOT identify the callee/definition (use LSP via bbox_refactor_plan or graph traversal via bbox_inspect_entity for that). The handoff `item_names` targets the *containing* refactor item, i.e. the edit container surrounding this site — use bbox_refactor_status to confirm it before planning edits in this file; use bbox_refactor_project_refs to ground project_file entity refs."
        ),
    }
}

/// Map a query capture name to the public `kind` value on
/// `CodeRefRecord`. The two vocabularies diverge by a trailing `s`
/// — the request param is plural (`"calls"`), but each record's own
/// kind is singular (`"call"`).
fn capture_to_ref_kind(capture: &str) -> &'static str {
    match capture {
        "call" => "call",
        "import" => "import",
        "field" => "field",
        "identifier" => "identifier",
        _ => "identifier",
    }
}

/// Generic identifier-only walker for languages without a curated
/// code_refs query. Walks every named node and emits a record for
/// each `identifier` node it finds.
fn code_refs_generic_identifiers(
    parsed: &CodeNavParsedSource,
    p: &CodeRefsParams,
) -> Result<String> {
    let limit = p.limit.unwrap_or(200).min(1000);
    let include_text = p.include_text.unwrap_or(false);
    let name_filter = p.query.as_deref().filter(|s| !s.is_empty());

    let mut cursor = parsed.tree.walk();
    let mut stack = vec![parsed.tree.root_node()];
    let mut matching_refs = 0usize;
    let mut records: Vec<CodeRefRecord> = Vec::new();

    while let Some(node) = stack.pop() {
        if node.kind() == "identifier" {
            if let Ok(name) = node.utf8_text(parsed.source.as_bytes()) {
                let matches_filter = name_filter.map(|f| name.contains(f)).unwrap_or(true);
                if matches_filter {
                    matching_refs += 1;
                    if records.len() < limit {
                        let start = node.start_position();
                        let end = node.end_position();
                        let byte_range = (node.start_byte(), node.end_byte());
                        let line_range = (start.row + 1, end.row + 1);
                        let column_range = (start.column + 1, end.column + 1);
                        let containing_symbol =
                            containing_symbol_for(node, &parsed.source, &parsed.language);
                        let handoff = code_ref_handoff(
                            &p.file,
                            p.project_dir.as_deref(),
                            &parsed.language,
                            name,
                            node.kind(),
                            byte_range,
                            line_range,
                            column_range,
                            containing_symbol.as_deref(),
                        );
                        records.push(CodeRefRecord {
                            kind: "identifier".to_string(),
                            name: name.to_string(),
                            node_kind: node.kind().to_string(),
                            byte_range,
                            line_range,
                            column_range,
                            containing_symbol,
                            edge_confidence: "heuristic".to_string(),
                            text: if include_text {
                                Some(excerpt(name, 200))
                            } else {
                                None
                            },
                            handoff,
                        });
                    }
                }
            }
        }
        for child in node.named_children(&mut cursor) {
            stack.push(child);
        }
    }

    records.sort_by_key(|r| (r.byte_range.0, r.byte_range.1));

    let truncated = matching_refs > records.len();
    let truncation_reason = if truncated {
        Some("limit_reached".to_string())
    } else {
        None
    };

    let response = CodeRefsResponse {
        status: "ok".to_string(),
        path: parsed.path.to_string_lossy().into_owned(),
        language: parsed.language.clone(),
        kind_filter: p.kind.clone(),
        matching_refs,
        returned_refs: records.len(),
        truncated,
        truncation_reason,
        refs: records,
        parse_report: parse_report(parsed.tree.root_node()),
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
