use std::collections::HashSet;
use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{anyhow, bail, Context, Result};
use rmcp::schemars;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tree_sitter::{Node, Tree};

use crate::chunker::code::{language_for_path, parser_for_language};
use crate::projects::ProjectRecord;

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct RefactorStatusParams {
    /// Source file to inspect. Relative paths resolve against project_dir or cwd.
    pub file: String,
    /// Optional project root used to resolve relative paths.
    #[serde(default)]
    pub project_dir: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct RefactorPlanParams {
    /// Plan kind. V1 supports "extract_rust_items".
    pub kind: String,
    /// Source Rust file. Relative paths resolve against project_dir or cwd.
    pub source: String,
    /// Target Rust file for extracted items. Required for extract_rust_items.
    #[serde(default)]
    pub target: Option<String>,
    /// Item names to extract. Names are exact for named items; impl blocks use their header.
    #[serde(default)]
    pub item_names: Option<Vec<String>>,
    /// Optional item kinds, e.g. function_item, struct_item, impl_item.
    #[serde(default)]
    pub item_kinds: Option<Vec<String>>,
    /// Optional project root used to resolve relative paths.
    #[serde(default)]
    pub project_dir: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct RefactorApplyParams {
    /// RefactorPlan JSON returned by bbox_refactor_plan.
    pub plan: serde_json::Value,
    /// Must be true. Prevents accidental writes from copied dry-run output.
    #[serde(default)]
    pub confirm: Option<bool>,
    /// Default false. When false, refuses to modify files that are dirty in git.
    #[serde(default)]
    pub allow_dirty_worktree: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, schemars::JsonSchema)]
pub struct SyntaxItem {
    pub plan_local_id: String,
    pub kind: String,
    #[serde(default)]
    pub name: Option<String>,
    pub byte_start: usize,
    pub byte_end: usize,
    pub leading_trivia_start: usize,
    pub trailing_trivia_end: usize,
    pub line_start: usize,
    pub line_end: usize,
    #[serde(default)]
    pub attributes: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, schemars::JsonSchema)]
pub struct TextEdit {
    pub byte_start: usize,
    pub byte_end: usize,
    pub replacement: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, schemars::JsonSchema)]
pub struct FileEdit {
    pub path: String,
    pub original_sha256: String,
    pub edits: Vec<TextEdit>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SemanticStatus {
    StructuralOnly,
    LspVerified,
    Unverified,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, schemars::JsonSchema)]
#[serde(rename_all = "snake_case", tag = "type")]
pub enum ValidationStep {
    TreeSitterNoErrors {
        path: String,
        #[serde(default)]
        byte_range: Option<(usize, usize)>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, schemars::JsonSchema)]
pub struct RefactorPlan {
    pub title: String,
    pub kind: String,
    pub semantic_status: SemanticStatus,
    pub dry_run: bool,
    pub edits: Vec<FileEdit>,
    pub validations: Vec<ValidationStep>,
    pub items: Vec<SyntaxItem>,
    #[serde(default)]
    pub leftovers: Vec<String>,
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
pub struct RefactorStatus {
    pub status: String,
    pub path: String,
    pub language: String,
    pub sha256: String,
    pub parse: ParseReport,
    pub items: Vec<SyntaxItem>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, schemars::JsonSchema)]
pub struct ParseReport {
    pub has_error: bool,
    pub error_nodes: usize,
    pub missing_nodes: usize,
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
pub struct RefactorApplyResponse {
    pub status: String,
    pub files_written: Vec<String>,
    pub validations: Vec<ParseValidationResult>,
    pub rolled_back: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub rollback_errors: Vec<String>,
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ParseValidationResult {
    pub path: String,
    pub has_error: bool,
    pub error_nodes: usize,
    pub missing_nodes: usize,
}

struct ParsedSource {
    path: PathBuf,
    language: &'static str,
    source: String,
    tree: Tree,
}

pub fn status(p: &RefactorStatusParams) -> Result<String> {
    let path = resolve_path(p.project_dir.as_deref(), &p.file)?;
    let parsed = parse_source_file(&path)?;
    let report = parse_report(parsed.tree.root_node());
    let items = if parsed.language == "rust" {
        rust_items(&parsed)
    } else {
        generic_top_level_items(&parsed)
    };
    let response = RefactorStatus {
        status: "ok".to_string(),
        path: path_string(&path),
        language: parsed.language.to_string(),
        sha256: sha256_hex(parsed.source.as_bytes()),
        parse: report,
        items,
    };
    Ok(serde_json::to_string_pretty(&response)?)
}

pub fn plan(p: &RefactorPlanParams) -> Result<String> {
    match p.kind.as_str() {
        "extract_rust_items" => plan_extract_rust_items(p),
        other => bail!("unsupported refactor plan kind `{other}`; supported: extract_rust_items"),
    }
}

pub fn apply(p: &RefactorApplyParams, projects: &[ProjectRecord]) -> Result<String> {
    if p.confirm != Some(true) {
        bail!("confirm=true is required to apply a refactor plan");
    }
    let plan: RefactorPlan = serde_json::from_value(p.plan.clone())
        .context("plan must be a RefactorPlan JSON object returned by bbox_refactor_plan")?;
    validate_plan_shape(&plan)?;

    let mut originals = Vec::new();
    let mut rewritten = Vec::new();
    for edit in &plan.edits {
        let path = PathBuf::from(&edit.path);
        ensure_path_in_registered_project(&path, projects)?;
        if p.allow_dirty_worktree != Some(true) {
            ensure_git_clean_for_path(&path)?;
        }
        let original = read_original_for_edit(&path, &edit.original_sha256)?;
        let original_hash = sha256_hex(&original);
        if original_hash != edit.original_sha256 {
            bail!(
                "refusing to apply {}: file hash changed (expected {}, got {})",
                path.display(),
                edit.original_sha256,
                original_hash
            );
        }
        let original_text = String::from_utf8(original.clone())
            .with_context(|| format!("{} is not valid utf-8", path.display()))?;
        let next = apply_text_edits(&original_text, &edit.edits)
            .with_context(|| format!("failed to apply edits for {}", path.display()))?;
        originals.push((path.clone(), original));
        rewritten.push((path, next.into_bytes()));
    }

    let validations = validate_rewritten_files(&rewritten)?;
    if validations
        .iter()
        .any(|v| v.has_error || v.error_nodes > 0 || v.missing_nodes > 0)
    {
        return Ok(serde_json::to_string_pretty(&RefactorApplyResponse {
            status: "validation_failed".to_string(),
            files_written: Vec::new(),
            validations,
            rolled_back: false,
            error: None,
            rollback_errors: Vec::new(),
        })?);
    }

    let mut files_written = Vec::new();
    let mut wrote = Vec::new();
    for (path, bytes) in &rewritten {
        if p.allow_dirty_worktree != Some(true) {
            if let Err(err) = ensure_git_clean_for_path(path) {
                let mut rollback_errors = Vec::new();
                for (restore_path, restore_bytes) in
                    originals.iter().filter(|(p, _)| wrote.contains(p))
                {
                    if let Err(restore_err) = write_atomic(restore_path, restore_bytes) {
                        rollback_errors
                            .push(format!("{}: {restore_err:#}", restore_path.display()));
                    }
                }
                return Ok(serde_json::to_string_pretty(&RefactorApplyResponse {
                    status: "dirty_worktree".to_string(),
                    files_written,
                    validations,
                    rolled_back: rollback_errors.is_empty(),
                    error: Some(err.to_string()),
                    rollback_errors,
                })?);
            }
        }
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        if let Err(err) = write_atomic(path, bytes) {
            let mut rollback_errors = Vec::new();
            for (restore_path, restore_bytes) in originals.iter().filter(|(p, _)| wrote.contains(p))
            {
                if let Err(restore_err) = write_atomic(restore_path, restore_bytes) {
                    rollback_errors.push(format!("{}: {restore_err:#}", restore_path.display()));
                }
            }
            return Ok(serde_json::to_string_pretty(&RefactorApplyResponse {
                status: "write_failed".to_string(),
                files_written,
                validations,
                rolled_back: rollback_errors.is_empty(),
                error: Some(format!("failed to write {}: {err:#}", path.display())),
                rollback_errors,
            })?);
        }
        wrote.push(path.clone());
        files_written.push(path_string(path));
    }

    Ok(serde_json::to_string_pretty(&RefactorApplyResponse {
        status: "ok".to_string(),
        files_written,
        validations,
        rolled_back: false,
        error: None,
        rollback_errors: Vec::new(),
    })?)
}

fn plan_extract_rust_items(p: &RefactorPlanParams) -> Result<String> {
    let source_path = resolve_path(p.project_dir.as_deref(), &p.source)?;
    let target_path = p
        .target
        .as_deref()
        .ok_or_else(|| anyhow!("target is required for extract_rust_items"))
        .and_then(|target| resolve_path(p.project_dir.as_deref(), target))?;
    if source_path == target_path {
        bail!("source and target must be different files");
    }

    let parsed = parse_rust_file(&source_path)?;
    let items = rust_items(&parsed);
    let selected = select_items(&items, p.item_names.as_deref(), p.item_kinds.as_deref())?;
    let selected_ids: HashSet<_> = selected
        .iter()
        .map(|item| item.plan_local_id.clone())
        .collect();
    let leftovers = items
        .iter()
        .filter(|item| !selected_ids.contains(&item.plan_local_id))
        .map(|item| {
            format!(
                "{} {} bytes {}..{}",
                item.kind,
                item.name.as_deref().unwrap_or("(unnamed)"),
                item.byte_start,
                item.byte_end
            )
        })
        .collect::<Vec<_>>();

    let mut moved = String::new();
    for item in &selected {
        let text = parsed
            .source
            .get(item.leading_trivia_start..item.byte_end)
            .ok_or_else(|| anyhow!("invalid item range for {}", item.plan_local_id))?
            .trim_matches('\n');
        if !moved.is_empty() {
            moved.push_str("\n\n");
        }
        moved.push_str(text);
        moved.push('\n');
    }

    let target_source = fs::read_to_string(&target_path).unwrap_or_default();
    let target_insert = if target_source.trim().is_empty() {
        moved
    } else {
        format!(
            "{}{}",
            if target_source.ends_with('\n') {
                "\n"
            } else {
                "\n\n"
            },
            moved
        )
    };

    let source_edits = selected
        .iter()
        .map(|item| TextEdit {
            byte_start: item.leading_trivia_start,
            byte_end: item.trailing_trivia_end,
            replacement: String::new(),
        })
        .collect::<Vec<_>>();
    ensure_non_overlapping(&source_edits)?;

    let mut target_edits = Vec::new();
    if !target_insert.is_empty() {
        target_edits.push(TextEdit {
            byte_start: target_source.len(),
            byte_end: target_source.len(),
            replacement: target_insert,
        });
    }

    let plan = RefactorPlan {
        title: format!(
            "extract {} Rust item(s) from {} to {}",
            selected.len(),
            path_string(&source_path),
            path_string(&target_path)
        ),
        kind: "extract_rust_items".to_string(),
        semantic_status: SemanticStatus::StructuralOnly,
        dry_run: true,
        edits: vec![
            FileEdit {
                path: path_string(&source_path),
                original_sha256: sha256_hex(parsed.source.as_bytes()),
                edits: source_edits,
            },
            FileEdit {
                path: path_string(&target_path),
                original_sha256: sha256_hex(target_source.as_bytes()),
                edits: target_edits,
            },
        ],
        validations: vec![
            ValidationStep::TreeSitterNoErrors {
                path: path_string(&source_path),
                byte_range: None,
            },
            ValidationStep::TreeSitterNoErrors {
                path: path_string(&target_path),
                byte_range: None,
            },
        ],
        items: selected.into_iter().cloned().collect(),
        leftovers,
    };

    validate_plan_shape(&plan)?;
    Ok(serde_json::to_string_pretty(&plan)?)
}

fn resolve_path(project_dir: Option<&str>, path: &str) -> Result<PathBuf> {
    let path = PathBuf::from(path);
    let full = if path.is_absolute() {
        path
    } else if let Some(project_dir) = project_dir {
        PathBuf::from(project_dir).join(path)
    } else {
        std::env::current_dir()?.join(path)
    };
    Ok(full)
}

fn parse_source_file(path: &Path) -> Result<ParsedSource> {
    let language =
        language_for_path(path).ok_or_else(|| anyhow!("unsupported source file extension"))?;
    let source =
        fs::read_to_string(path).with_context(|| format!("failed to read {}", path.display()))?;
    let tree = parse_source(language, &source)?;
    Ok(ParsedSource {
        path: path.to_path_buf(),
        language,
        source,
        tree,
    })
}

fn parse_rust_file(path: &Path) -> Result<ParsedSource> {
    let parsed = parse_source_file(path)?;
    if parsed.language != "rust" {
        bail!("{} is not a Rust source file", path.display());
    }
    Ok(parsed)
}

fn parse_source(language: &str, source: &str) -> Result<Tree> {
    let mut parser = parser_for_language(language)?;
    parser
        .parse(source, None)
        .ok_or_else(|| anyhow!("tree-sitter {language} parser returned no tree"))
}

fn rust_items(parsed: &ParsedSource) -> Vec<SyntaxItem> {
    let root = parsed.tree.root_node();
    let mut cursor = root.walk();
    root.named_children(&mut cursor)
        .filter(|node| is_top_level_item(node.kind()))
        .map(|node| syntax_item(parsed, node))
        .collect()
}

fn generic_top_level_items(parsed: &ParsedSource) -> Vec<SyntaxItem> {
    let root = parsed.tree.root_node();
    let mut cursor = root.walk();
    root.named_children(&mut cursor)
        .map(|node| syntax_item(parsed, node))
        .collect()
}

fn syntax_item(parsed: &ParsedSource, node: Node<'_>) -> SyntaxItem {
    let name = item_name(node, &parsed.source, parsed.language);
    let leading_trivia_start = leading_trivia_start(&parsed.source, node);
    let trailing_trivia_end = trailing_trivia_end(&parsed.source, node.end_byte());
    let attributes = attached_attributes(&parsed.source, leading_trivia_start, node.start_byte());
    let (line_start, _) = line_col(&parsed.source, node.start_byte());
    let (line_end, _) = line_col(&parsed.source, node.end_byte());
    let display_name = name.clone().unwrap_or_else(|| "(unnamed)".to_string());
    SyntaxItem {
        plan_local_id: format!(
            "{}:{}:{}:{}:{}",
            path_string(&parsed.path),
            node.start_byte(),
            node.end_byte(),
            node.kind(),
            display_name
        ),
        kind: node.kind().to_string(),
        name,
        byte_start: node.start_byte(),
        byte_end: node.end_byte(),
        leading_trivia_start,
        trailing_trivia_end,
        line_start,
        line_end,
        attributes,
    }
}

fn is_top_level_item(kind: &str) -> bool {
    matches!(
        kind,
        "mod_item"
            | "use_declaration"
            | "struct_item"
            | "enum_item"
            | "trait_item"
            | "function_item"
            | "impl_item"
            | "macro_definition"
            | "const_item"
            | "static_item"
            | "type_item"
    )
}

fn item_name(node: Node<'_>, source: &str, language: &str) -> Option<String> {
    if language == "rust" && node.kind() == "impl_item" {
        return node
            .utf8_text(source.as_bytes())
            .ok()
            .and_then(|text| text.split('{').next())
            .map(|text| text.split_whitespace().collect::<Vec<_>>().join(" "))
            .filter(|text| !text.is_empty());
    }
    node.child_by_field_name("name")
        .and_then(|child| child.utf8_text(source.as_bytes()).ok())
        .map(str::to_string)
        .or_else(|| {
            node.utf8_text(source.as_bytes())
                .ok()
                .and_then(first_identifier_after_keyword)
        })
}

fn first_identifier_after_keyword(text: &str) -> Option<String> {
    const KEYWORDS: &[&str] = &[
        "fn",
        "function",
        "class",
        "struct",
        "enum",
        "trait",
        "interface",
        "type",
        "const",
        "let",
        "var",
        "def",
        "module",
    ];
    let mut prev_was_keyword = false;
    for token in text.split(|ch: char| ch != '_' && !ch.is_ascii_alphanumeric()) {
        if token.is_empty() {
            continue;
        }
        if prev_was_keyword {
            return Some(token.to_string());
        }
        prev_was_keyword = KEYWORDS.contains(&token);
    }
    None
}

fn leading_trivia_start(source: &str, node: Node<'_>) -> usize {
    let mut line_start = line_start_before(source, node.start_byte());
    let mut prev = node.prev_named_sibling();
    while let Some(sibling) = prev {
        let gap = source
            .get(sibling.end_byte()..line_start)
            .unwrap_or_default();
        if !gap.trim().is_empty() || !is_attachable_trivia_node(sibling.kind()) {
            break;
        }
        line_start = line_start_before(source, sibling.start_byte());
        prev = sibling.prev_named_sibling();
    }

    let mut current = line_start;
    let mut attached = false;
    while current > 0 {
        let prev_end = current.saturating_sub(1);
        let prev_start = line_start_before(source, prev_end);
        let line = &source[prev_start..current];
        let trimmed = line.trim();
        if trimmed.is_empty() {
            break;
        }
        let is_attr_or_doc = trimmed.starts_with("#[")
            || trimmed.starts_with("#![")
            || trimmed.starts_with("///")
            || trimmed.starts_with("//!");
        let is_comment = trimmed.starts_with("//");
        if is_attr_or_doc || (attached && is_comment) {
            line_start = prev_start;
            current = prev_start;
            attached = true;
        } else {
            break;
        }
    }
    line_start
}

fn is_attachable_trivia_node(kind: &str) -> bool {
    matches!(kind, "attribute_item" | "line_comment" | "block_comment")
}

fn trailing_trivia_end(source: &str, end: usize) -> usize {
    let bytes = source.as_bytes();
    let mut idx = end;
    let mut newline_count = 0usize;
    while idx < bytes.len() {
        match bytes[idx] {
            b' ' | b'\t' | b'\r' => idx += 1,
            b'\n' => {
                idx += 1;
                newline_count += 1;
                if newline_count >= 2 {
                    break;
                }
            }
            _ => break,
        }
    }
    idx
}

fn attached_attributes(source: &str, trivia_start: usize, item_start: usize) -> Vec<String> {
    source[trivia_start..item_start]
        .lines()
        .map(str::trim)
        .filter(|line| line.starts_with("#[") || line.starts_with("#!["))
        .map(str::to_string)
        .collect()
}

fn line_start_before(source: &str, idx: usize) -> usize {
    source[..idx.min(source.len())]
        .rfind('\n')
        .map(|pos| pos + 1)
        .unwrap_or(0)
}

fn line_col(source: &str, idx: usize) -> (usize, usize) {
    let idx = idx.min(source.len());
    let line = source[..idx].bytes().filter(|b| *b == b'\n').count() + 1;
    let col = idx - line_start_before(source, idx) + 1;
    (line, col)
}

fn select_items<'a>(
    items: &'a [SyntaxItem],
    names: Option<&[String]>,
    kinds: Option<&[String]>,
) -> Result<Vec<&'a SyntaxItem>> {
    let has_names = names.is_some_and(|xs| !xs.is_empty());
    let has_kinds = kinds.is_some_and(|xs| !xs.is_empty());
    if !has_names && !has_kinds {
        bail!("at least one of item_names or item_kinds is required");
    }
    let name_set = names.map(|xs| xs.iter().map(String::as_str).collect::<HashSet<_>>());
    let kind_set = kinds.map(|xs| xs.iter().map(String::as_str).collect::<HashSet<_>>());
    let selected = items
        .iter()
        .filter(|item| {
            name_set
                .as_ref()
                .is_none_or(|set| item.name.as_deref().is_some_and(|name| set.contains(name)))
        })
        .filter(|item| {
            kind_set
                .as_ref()
                .is_none_or(|set| set.contains(item.kind.as_str()))
        })
        .collect::<Vec<_>>();
    if selected.is_empty() {
        bail!("no matching Rust items found");
    }
    if let Some(names) = names {
        for expected in names {
            if !selected
                .iter()
                .any(|item| item.name.as_deref() == Some(expected.as_str()))
            {
                bail!("requested item `{expected}` was not found");
            }
        }
    }
    Ok(selected)
}

fn validate_plan_shape(plan: &RefactorPlan) -> Result<()> {
    if plan.edits.is_empty() {
        bail!("plan has no edits");
    }
    for edit in &plan.edits {
        ensure_non_overlapping(&edit.edits)
            .with_context(|| format!("overlapping edits in {}", edit.path))?;
    }
    Ok(())
}

fn ensure_non_overlapping(edits: &[TextEdit]) -> Result<()> {
    let mut ranges = edits
        .iter()
        .map(|edit| (edit.byte_start, edit.byte_end))
        .collect::<Vec<_>>();
    ranges.sort_unstable();
    for pair in ranges.windows(2) {
        if pair[0].1 > pair[1].0 {
            bail!(
                "overlapping edits: {}..{} overlaps {}..{}",
                pair[0].0,
                pair[0].1,
                pair[1].0,
                pair[1].1
            );
        }
    }
    Ok(())
}

fn apply_text_edits(source: &str, edits: &[TextEdit]) -> Result<String> {
    ensure_non_overlapping(edits)?;
    let mut out = source.to_string();
    let mut sorted = edits.iter().collect::<Vec<_>>();
    sorted.sort_by_key(|edit| edit.byte_start);
    for edit in sorted.into_iter().rev() {
        if edit.byte_start > edit.byte_end || edit.byte_end > out.len() {
            bail!("invalid edit range {}..{}", edit.byte_start, edit.byte_end);
        }
        if !out.is_char_boundary(edit.byte_start) || !out.is_char_boundary(edit.byte_end) {
            bail!(
                "edit range {}..{} is not UTF-8 aligned",
                edit.byte_start,
                edit.byte_end
            );
        }
        out.replace_range(edit.byte_start..edit.byte_end, &edit.replacement);
    }
    Ok(out)
}

fn read_original_for_edit(path: &Path, expected_sha256: &str) -> Result<Vec<u8>> {
    match fs::read(path) {
        Ok(bytes) => Ok(bytes),
        Err(err)
            if err.kind() == std::io::ErrorKind::NotFound && expected_sha256 == sha256_hex(&[]) =>
        {
            Ok(Vec::new())
        }
        Err(err) => Err(err).with_context(|| format!("failed to read {}", path.display())),
    }
}

fn ensure_path_in_registered_project(path: &Path, projects: &[ProjectRecord]) -> Result<()> {
    if projects.is_empty() {
        bail!("refactor apply requires at least one registered project");
    }
    let scoped = canonicalize_existing_or_parent(path)?;
    let allowed = projects.iter().any(|project| {
        let root = Path::new(&project.canonical_path);
        scoped == root || scoped.starts_with(root)
    });
    if !allowed {
        bail!(
            "refusing to apply {}: path is outside registered projects",
            path.display()
        );
    }
    Ok(())
}

fn canonicalize_existing_or_parent(path: &Path) -> Result<PathBuf> {
    if path.exists() {
        return fs::canonicalize(path)
            .with_context(|| format!("canonicalizing {}", path.display()));
    }
    let parent = path
        .parent()
        .ok_or_else(|| anyhow!("{} has no parent directory", path.display()))?;
    let file_name = path
        .file_name()
        .ok_or_else(|| anyhow!("{} has no final component", path.display()))?;
    let parent = fs::canonicalize(parent)
        .with_context(|| format!("canonicalizing parent {}", parent.display()))?;
    Ok(parent.join(file_name))
}

fn ensure_git_clean_for_path(path: &Path) -> Result<()> {
    let Some(root) = crate::entity_ref::git_root_for_path(path) else {
        return Ok(());
    };
    let rel = path.strip_prefix(&root).unwrap_or(path);
    let output = Command::new("git")
        .arg("-C")
        .arg(&root)
        .arg("status")
        .arg("--porcelain")
        .arg("--")
        .arg(rel)
        .output()
        .with_context(|| format!("checking git status for {}", path.display()))?;
    if !output.status.success() {
        bail!(
            "git status failed for {}: {}",
            path.display(),
            String::from_utf8_lossy(&output.stderr)
        );
    }
    if !output.stdout.is_empty() {
        bail!(
            "refusing to apply {}: file is dirty in git; pass allow_dirty_worktree=true to override",
            path.display()
        );
    }
    Ok(())
}

fn validate_rewritten_files(files: &[(PathBuf, Vec<u8>)]) -> Result<Vec<ParseValidationResult>> {
    files
        .iter()
        .filter_map(|(path, bytes)| {
            let language = language_for_path(path)?;
            Some((path, bytes, language))
        })
        .map(|(path, bytes, language)| {
            let source = std::str::from_utf8(bytes)
                .with_context(|| format!("{} is not valid utf-8", path.display()))?;
            let tree = parse_source(language, source)?;
            let report = parse_report(tree.root_node());
            Ok(ParseValidationResult {
                path: path_string(path),
                has_error: report.has_error,
                error_nodes: report.error_nodes,
                missing_nodes: report.missing_nodes,
            })
        })
        .collect()
}

fn parse_report(root: Node<'_>) -> ParseReport {
    let mut report = ParseReport {
        has_error: root.has_error(),
        error_nodes: 0,
        missing_nodes: 0,
    };
    collect_parse_report(root, &mut report);
    report
}

fn collect_parse_report(node: Node<'_>, report: &mut ParseReport) {
    if node.is_error() {
        report.error_nodes += 1;
    }
    if node.is_missing() {
        report.missing_nodes += 1;
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        collect_parse_report(child, report);
    }
}

fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| anyhow!("{} has no parent directory", path.display()))?;
    fs::create_dir_all(parent)?;
    let mut tmp = tempfile::NamedTempFile::new_in(parent)?;
    tmp.write_all(bytes)?;
    tmp.as_file_mut().sync_all()?;
    tmp.persist(path)
        .map_err(|err| anyhow!("persist failed: {}", err.error))?;
    if let Ok(dir) = fs::File::open(parent) {
        let _ = dir.sync_all();
    }
    Ok(())
}

fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hex::encode(hasher.finalize())
}

fn path_string(path: &Path) -> String {
    path.to_string_lossy().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn project_record(path: &Path) -> ProjectRecord {
        ProjectRecord {
            project_id: "test-project".to_string(),
            repo_id: None,
            canonical_path: fs::canonicalize(path)
                .unwrap()
                .to_string_lossy()
                .into_owned(),
            registered_at: "2026-05-07T00:00:00Z".to_string(),
            is_git_repo: false,
        }
    }

    #[test]
    fn status_lists_top_level_rust_items_with_attrs() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("lib.rs");
        fs::write(
            &path,
            "#[derive(Debug)]\npub struct Thing;\n\nfn helper() {}\n",
        )
        .unwrap();

        let text = status(&RefactorStatusParams {
            file: path_string(&path),
            project_dir: None,
        })
        .unwrap();
        let parsed: RefactorStatus = serde_json::from_str(&text).unwrap();
        assert_eq!(parsed.language, "rust");
        assert_eq!(parsed.parse.error_nodes, 0);
        assert!(parsed
            .items
            .iter()
            .any(|item| item.kind == "struct_item" && item.name.as_deref() == Some("Thing")));
        assert!(parsed.items.iter().any(|item| {
            item.attributes
                .iter()
                .any(|attr| attr == "#[derive(Debug)]")
        }));
    }

    #[test]
    fn multiline_rust_attribute_moves_with_item() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("lib.rs");
        let target = dir.path().join("moved.rs");
        fs::write(
            &source,
            "#[derive(\n    Debug,\n    Clone,\n)]\npub struct MoveMe;\n\nfn keep() {}\n",
        )
        .unwrap();

        let plan_text = plan(&RefactorPlanParams {
            kind: "extract_rust_items".into(),
            source: path_string(&source),
            target: Some(path_string(&target)),
            item_names: Some(vec!["MoveMe".into()]),
            item_kinds: Some(vec!["struct_item".into()]),
            project_dir: None,
        })
        .unwrap();
        let plan_value: serde_json::Value = serde_json::from_str(&plan_text).unwrap();
        let response = apply(
            &RefactorApplyParams {
                plan: plan_value,
                confirm: Some(true),
                allow_dirty_worktree: None,
            },
            &[project_record(dir.path())],
        )
        .unwrap();
        let applied: RefactorApplyResponse = serde_json::from_str(&response).unwrap();
        assert_eq!(applied.status, "ok");
        let target_text = fs::read_to_string(&target).unwrap();
        assert!(target_text.contains("#[derive("));
        assert!(target_text.contains("pub struct MoveMe"));
        let source_text = fs::read_to_string(&source).unwrap();
        assert!(!source_text.contains("#[derive("));
        assert!(source_text.contains("fn keep()"));
    }

    #[test]
    fn status_lists_generic_tree_sitter_items() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sample.ts");
        fs::write(&path, "export function helper() { return 1; }\n").unwrap();

        let text = status(&RefactorStatusParams {
            file: path_string(&path),
            project_dir: None,
        })
        .unwrap();
        let parsed: RefactorStatus = serde_json::from_str(&text).unwrap();
        assert_eq!(parsed.language, "typescript");
        assert_eq!(parsed.parse.error_nodes, 0);
        assert!(parsed
            .items
            .iter()
            .any(|item| item.kind.contains("export") || item.name.as_deref() == Some("helper")));
    }

    #[test]
    fn extract_plan_moves_named_item_and_apply_writes_target() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("lib.rs");
        let target = dir.path().join("moved.rs");
        fs::write(&source, "fn keep() {}\n\nfn move_me() {}\n").unwrap();

        let plan_text = plan(&RefactorPlanParams {
            kind: "extract_rust_items".into(),
            source: path_string(&source),
            target: Some(path_string(&target)),
            item_names: Some(vec!["move_me".into()]),
            item_kinds: None,
            project_dir: None,
        })
        .unwrap();
        let plan_value: serde_json::Value = serde_json::from_str(&plan_text).unwrap();
        let response = apply(
            &RefactorApplyParams {
                plan: plan_value,
                confirm: Some(true),
                allow_dirty_worktree: None,
            },
            &[project_record(dir.path())],
        )
        .unwrap();
        let applied: RefactorApplyResponse = serde_json::from_str(&response).unwrap();
        assert_eq!(applied.status, "ok");
        assert!(fs::read_to_string(&source).unwrap().contains("keep"));
        assert!(!fs::read_to_string(&source).unwrap().contains("move_me"));
        assert!(fs::read_to_string(&target).unwrap().contains("move_me"));
    }

    #[test]
    fn overlapping_edits_are_rejected() {
        let edits = vec![
            TextEdit {
                byte_start: 0,
                byte_end: 5,
                replacement: String::new(),
            },
            TextEdit {
                byte_start: 4,
                byte_end: 6,
                replacement: String::new(),
            },
        ];
        assert!(ensure_non_overlapping(&edits).is_err());
    }

    #[test]
    fn apply_text_edits_sorts_unsorted_non_overlapping_edits() {
        let edits = vec![
            TextEdit {
                byte_start: 6,
                byte_end: 11,
                replacement: "earth".into(),
            },
            TextEdit {
                byte_start: 0,
                byte_end: 5,
                replacement: "hello".into(),
            },
        ];
        assert_eq!(
            apply_text_edits("world there", &edits).unwrap(),
            "hello earth"
        );
    }

    #[test]
    fn selecting_without_filters_is_rejected() {
        let items = vec![SyntaxItem {
            plan_local_id: "x".into(),
            kind: "function_item".into(),
            name: Some("f".into()),
            byte_start: 0,
            byte_end: 6,
            leading_trivia_start: 0,
            trailing_trivia_end: 7,
            line_start: 1,
            line_end: 1,
            attributes: Vec::new(),
        }];
        assert!(select_items(&items, None, None).is_err());
    }

    #[test]
    fn apply_rejects_paths_outside_registered_project() {
        let project = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let source = outside.path().join("lib.rs");
        fs::write(&source, "fn f() {}\n").unwrap();
        let plan = RefactorPlan {
            title: "bad".into(),
            kind: "extract_rust_items".into(),
            semantic_status: SemanticStatus::StructuralOnly,
            dry_run: true,
            edits: vec![FileEdit {
                path: path_string(&source),
                original_sha256: sha256_hex(b"fn f() {}\n"),
                edits: Vec::new(),
            }],
            validations: Vec::new(),
            items: Vec::new(),
            leftovers: Vec::new(),
        };
        let err = apply(
            &RefactorApplyParams {
                plan: serde_json::to_value(plan).unwrap(),
                confirm: Some(true),
                allow_dirty_worktree: None,
            },
            &[project_record(project.path())],
        )
        .unwrap_err();
        assert!(err.to_string().contains("outside registered projects"));
    }
}
