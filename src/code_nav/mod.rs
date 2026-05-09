use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context, Result};
use ignore::{DirEntry, WalkBuilder};
use rmcp::schemars;
use serde::{Deserialize, Serialize};
use tree_sitter::{Query, QueryCursor, StreamingIterator, Tree};

use crate::chunker::code::{language_for_path, parser_for_language, ts_language_for_name};
use crate::refactor::{
    parse_report, resolve_path, ParseReport, RefactorStatus, RefactorStatusParams, SyntaxItem,
};

#[cfg(test)]
mod tests;

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
    #[serde(default)]
    pub item_kinds: Option<Vec<String>>,
    /// Optional case-sensitive substring matched against the relative path before parsing.
    #[serde(default)]
    pub path_contains: Option<String>,
    /// Maximum returned items. Defaults to 100 and is capped at 1000.
    #[serde(default)]
    pub limit: Option<usize>,
    /// Maximum supported source files to parse. Defaults to 5000 and is capped at 5000.
    #[serde(default)]
    pub file_limit: Option<usize>,
    /// Include syntax attributes from bbox_refactor_status. Defaults false.
    #[serde(default)]
    pub include_attributes: Option<bool>,
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
    pub kind: String,
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

pub fn code_symbols(p: &CodeSymbolSearchParams) -> Result<String> {
    let project_dir = PathBuf::from(&p.project_dir)
        .canonicalize()
        .with_context(|| format!("failed to resolve project_dir {}", p.project_dir))?;
    if !project_dir.is_dir() {
        return Err(anyhow!("project_dir must be a directory"));
    }

    let language_filter = p
        .languages
        .as_ref()
        .filter(|languages| !languages.is_empty())
        .map(|languages| languages.iter().map(String::as_str).collect::<Vec<_>>());
    let kind_filter = p.item_kinds.clone().filter(|kinds| !kinds.is_empty());
    let limit = p.limit.unwrap_or(100).min(1000);
    let file_limit = p.file_limit.unwrap_or(5000).min(5000);
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
            items.push(CodeSymbolSearchItem {
                file: rel_path.clone(),
                language: status.language.clone(),
                kind: item.kind.clone(),
                name: item.name.clone(),
                byte_range: (item.byte_start, item.byte_end),
                line_range: (item.line_start, item.line_end),
                handoff: status_item_handoff(&rel_path, Some(&project_dir_arg), language, &item),
            });
        }
    }

    let truncated = file_limit_hit || matching_items > items.len();
    let response = CodeSymbolSearchResponse {
        status: "ok".to_string(),
        project_dir: project_dir_arg,
        scanned_files,
        matched_files: matched_file_paths.len(),
        matching_items,
        returned_items: items.len(),
        truncated,
        items,
        errors,
        semantic_status: "syntax_only".to_string(),
    };
    Ok(serde_json::to_string_pretty(&response)?)
}

pub fn code_query(p: &CodeQueryParams) -> Result<String> {
    let path = resolve_path(p.project_dir.as_deref(), &p.file)?;
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
        semantic_status: "syntax_only".to_string(),
    };

    Ok(serde_json::to_string_pretty(&response)?)
}

pub fn code_node_describe(p: &CodeNodeDescribeParams) -> Result<String> {
    let path = resolve_path(p.project_dir.as_deref(), &p.file)?;
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
    };

    Ok(serde_json::to_string_pretty(&response)?)
}
