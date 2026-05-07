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
    /// Optional exact item names to return.
    #[serde(default)]
    pub item_names: Option<Vec<String>>,
    /// Optional item kinds to return, e.g. function_item, struct_item, impl_method.
    #[serde(default)]
    pub item_kinds: Option<Vec<String>>,
    /// Maximum matching items to return. Defaults to 200 and is capped at 1000.
    #[serde(default)]
    pub limit: Option<usize>,
    /// Include syntax attributes in returned items. Defaults true.
    #[serde(default)]
    pub include_attributes: Option<bool>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct RefactorPlanParams {
    /// Plan kind. Supports "extract_rust_items", "extract_rust_impl_methods",
    /// and "add_rust_router_to_sum".
    pub kind: String,
    /// Source Rust file. Relative paths resolve against project_dir or cwd.
    pub source: String,
    /// Target Rust file for extracted items. Required for writable plan kinds.
    #[serde(default)]
    pub target: Option<String>,
    /// Item names to extract. Names are exact; extract_rust_impl_methods uses method names.
    #[serde(default)]
    pub item_names: Option<Vec<String>>,
    /// Optional item kinds, e.g. function_item, struct_item, impl_item.
    #[serde(default)]
    pub item_kinds: Option<Vec<String>>,
    /// Optional exact impl header filter for extract_rust_impl_methods,
    /// e.g. "impl BlackboxServer".
    #[serde(default)]
    pub impl_name: Option<String>,
    /// Optional router name for extract_rust_impl_methods. When present, the
    /// generated target wrapper is annotated as #[tool_router(router = name)].
    #[serde(default)]
    pub router_name: Option<String>,
    /// Optional explicit router call for add_rust_router_to_sum, e.g.
    /// "refactor_tools::router()". Defaults to "Self::<router_name>()".
    #[serde(default)]
    pub router_call: Option<String>,
    /// Optional helper function name generated before a new router wrapper.
    /// Use with router_name when extracting tool impls into a child module.
    #[serde(default)]
    pub router_export_name: Option<String>,
    /// Optional text inserted before the generated target wrapper.
    #[serde(default)]
    pub target_prelude: Option<String>,
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
    /// Default false. When false, refuses paths outside registered projects.
    /// Use true only for disposable practice worktrees or isolated smoke tests.
    #[serde(default)]
    pub allow_unregistered_paths: Option<bool>,
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
    pub total_items: usize,
    pub matching_items: usize,
    pub returned_items: usize,
    pub truncated: bool,
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

#[derive(Debug, Clone)]
struct RustImplMethod {
    impl_name: String,
    impl_byte_start: usize,
    item: SyntaxItem,
}

#[derive(Debug, Clone, Copy)]
struct TargetImplInsertion {
    byte: usize,
    body_is_empty: bool,
}

pub fn status(p: &RefactorStatusParams) -> Result<String> {
    let path = resolve_path(p.project_dir.as_deref(), &p.file)?;
    let parsed = parse_source_file(&path)?;
    let report = parse_report(parsed.tree.root_node());
    let mut items = if parsed.language == "rust" {
        rust_status_items(&parsed)
    } else {
        generic_top_level_items(&parsed)
    };
    let total_items = items.len();
    if let Some(kinds) = p.item_kinds.as_deref().filter(|kinds| !kinds.is_empty()) {
        let kinds = kinds.iter().map(String::as_str).collect::<HashSet<_>>();
        items.retain(|item| kinds.contains(item.kind.as_str()));
    }
    if let Some(names) = p.item_names.as_deref().filter(|names| !names.is_empty()) {
        let names = names.iter().map(String::as_str).collect::<HashSet<_>>();
        items.retain(|item| {
            item.name
                .as_deref()
                .is_some_and(|name| names.contains(name))
        });
    }
    if p.include_attributes == Some(false) {
        for item in &mut items {
            item.attributes.clear();
        }
    }
    let matching_items = items.len();
    let limit = p.limit.unwrap_or(200).min(1000);
    let truncated = matching_items > limit;
    items.truncate(limit);
    let returned_items = items.len();
    let response = RefactorStatus {
        status: "ok".to_string(),
        path: path_string(&path),
        language: parsed.language.to_string(),
        sha256: sha256_hex(parsed.source.as_bytes()),
        parse: report,
        total_items,
        matching_items,
        returned_items,
        truncated,
        items,
    };
    Ok(serde_json::to_string_pretty(&response)?)
}

pub fn plan(p: &RefactorPlanParams) -> Result<String> {
    match p.kind.as_str() {
        "extract_rust_items" => plan_extract_rust_items(p),
        "extract_rust_impl_methods" => plan_extract_rust_impl_methods(p),
        "add_rust_router_to_sum" => plan_add_rust_router_to_sum(p),
        other => bail!(
            "unsupported refactor plan kind `{other}`; supported: extract_rust_items, extract_rust_impl_methods, add_rust_router_to_sum"
        ),
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
        if p.allow_unregistered_paths != Some(true) {
            ensure_path_in_registered_project(&path, projects)?;
        }
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

fn plan_extract_rust_impl_methods(p: &RefactorPlanParams) -> Result<String> {
    let source_path = resolve_path(p.project_dir.as_deref(), &p.source)?;
    let target_path = p
        .target
        .as_deref()
        .ok_or_else(|| anyhow!("target is required for extract_rust_impl_methods"))
        .and_then(|target| resolve_path(p.project_dir.as_deref(), target))?;
    if source_path == target_path {
        bail!("source and target must be different files");
    }
    if let Some(router_name) = p.router_name.as_deref() {
        validate_rust_identifier(router_name, "router_name")?;
    }
    if let Some(export_name) = p.router_export_name.as_deref() {
        validate_rust_identifier(export_name, "router_export_name")?;
        if p.router_name.is_none() {
            bail!("router_export_name requires router_name");
        }
    }
    if let Some(kinds) = p.item_kinds.as_deref() {
        if !kinds.iter().all(|kind| kind == "impl_method") {
            bail!("extract_rust_impl_methods only supports item_kinds impl_method");
        }
    }

    let names = p
        .item_names
        .as_deref()
        .filter(|names| !names.is_empty())
        .ok_or_else(|| anyhow!("item_names is required for extract_rust_impl_methods"))?;

    let parsed = parse_rust_file(&source_path)?;
    let methods = rust_impl_methods(&parsed);
    let candidates = methods
        .iter()
        .filter(|method| {
            p.impl_name
                .as_deref()
                .is_none_or(|impl_name| method.impl_name == impl_name)
        })
        .collect::<Vec<_>>();
    if candidates.is_empty() {
        if let Some(impl_name) = p.impl_name.as_deref() {
            bail!("no impl block matching `{impl_name}` found");
        }
        bail!("no Rust impl methods found");
    }

    let mut selected = Vec::new();
    for expected in names {
        let matches = candidates
            .iter()
            .copied()
            .filter(|method| method.item.name.as_deref() == Some(expected.as_str()))
            .collect::<Vec<_>>();
        match matches.as_slice() {
            [] => bail!("requested impl method `{expected}` was not found"),
            [method] => selected.push((*method).clone()),
            _ => bail!(
                "requested impl method `{expected}` matched multiple impl blocks; pass impl_name"
            ),
        }
    }

    let impl_starts = selected
        .iter()
        .map(|method| method.impl_byte_start)
        .collect::<HashSet<_>>();
    if impl_starts.len() > 1 {
        bail!("extract_rust_impl_methods can only extract methods from one impl block per plan");
    }

    let selected_ids = selected
        .iter()
        .map(|method| method.item.plan_local_id.clone())
        .collect::<HashSet<_>>();
    let leftovers = methods
        .iter()
        .filter(|method| !selected_ids.contains(&method.item.plan_local_id))
        .map(|method| {
            format!(
                "impl_method {} in {} bytes {}..{}",
                method.item.name.as_deref().unwrap_or("(unnamed)"),
                method.impl_name,
                method.item.byte_start,
                method.item.byte_end
            )
        })
        .collect::<Vec<_>>();

    let target_source = fs::read_to_string(&target_path).unwrap_or_default();
    let target_edits = rust_impl_methods_target_edits(
        &target_path,
        &target_source,
        p.target_prelude.as_deref(),
        p.router_name.as_deref(),
        p.router_export_name.as_deref(),
        &selected[0].impl_name,
        &parsed.source,
        &selected,
    )?;

    let source_edits = selected
        .iter()
        .map(|method| TextEdit {
            byte_start: method.item.leading_trivia_start,
            byte_end: method.item.trailing_trivia_end,
            replacement: String::new(),
        })
        .collect::<Vec<_>>();
    ensure_non_overlapping(&source_edits)?;

    let plan = RefactorPlan {
        title: format!(
            "extract {} Rust impl method(s) from {} to {}",
            selected.len(),
            path_string(&source_path),
            path_string(&target_path)
        ),
        kind: "extract_rust_impl_methods".to_string(),
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
        items: selected.into_iter().map(|method| method.item).collect(),
        leftovers,
    };

    validate_plan_shape(&plan)?;
    Ok(serde_json::to_string_pretty(&plan)?)
}

fn plan_add_rust_router_to_sum(p: &RefactorPlanParams) -> Result<String> {
    let source_path = resolve_path(p.project_dir.as_deref(), &p.source)?;
    let router_call = if let Some(router_call) = p.router_call.as_deref() {
        validate_rust_router_call(router_call, "router_call")?;
        router_call.to_string()
    } else {
        let router_name = p
            .router_name
            .as_deref()
            .ok_or_else(|| anyhow!("router_name is required for add_rust_router_to_sum"))?;
        validate_rust_identifier(router_name, "router_name")?;
        format!("Self::{router_name}()")
    };

    let parsed = parse_rust_file(&source_path)?;
    let field = find_rust_field_initializer(&parsed, "tool_router")
        .ok_or_else(|| anyhow!("no `tool_router:` field initializer found"))?;
    let field_text = parsed
        .source
        .get(field.start_byte()..field.end_byte())
        .ok_or_else(|| anyhow!("invalid tool_router field range"))?;
    if field_text.contains(&router_call) {
        bail!("tool_router already contains {router_call}");
    }
    let expr_end = rust_field_value_end(&parsed.source, field)
        .ok_or_else(|| anyhow!("could not locate tool_router expression end"))?;

    let edit = TextEdit {
        byte_start: expr_end,
        byte_end: expr_end,
        replacement: format!(" + {router_call}"),
    };
    let plan = RefactorPlan {
        title: format!(
            "add Rust router call {router_call} to tool_router sum in {}",
            path_string(&source_path)
        ),
        kind: "add_rust_router_to_sum".to_string(),
        semantic_status: SemanticStatus::StructuralOnly,
        dry_run: true,
        edits: vec![FileEdit {
            path: path_string(&source_path),
            original_sha256: sha256_hex(parsed.source.as_bytes()),
            edits: vec![edit],
        }],
        validations: vec![ValidationStep::TreeSitterNoErrors {
            path: path_string(&source_path),
            byte_range: None,
        }],
        items: Vec::new(),
        leftovers: vec![format!("existing tool_router field: {}", field_text.trim())],
    };

    validate_plan_shape(&plan)?;
    Ok(serde_json::to_string_pretty(&plan)?)
}

fn rust_impl_methods_target_edits(
    target_path: &Path,
    target_source: &str,
    target_prelude: Option<&str>,
    router_name: Option<&str>,
    router_export_name: Option<&str>,
    impl_name: &str,
    source: &str,
    selected: &[RustImplMethod],
) -> Result<Vec<TextEdit>> {
    if let Some(insertion) =
        existing_target_impl_insert_byte(target_path, target_source, impl_name, router_name)?
    {
        let mut replacement = String::new();
        if !insertion.body_is_empty {
            replacement.push('\n');
        }
        replacement.push_str(&rust_impl_methods_block(source, selected)?);
        return Ok(vec![TextEdit {
            byte_start: insertion.byte,
            byte_end: insertion.byte,
            replacement,
        }]);
    }

    let wrapper = rust_impl_methods_target_wrapper(
        target_source,
        router_name,
        router_export_name,
        impl_name,
        source,
        selected,
    )?;
    let Some(prelude) = target_prelude
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .filter(|text| !rust_prelude_present(target_source, text))
    else {
        return Ok(vec![TextEdit {
            byte_start: target_source.len(),
            byte_end: target_source.len(),
            replacement: wrapper,
        }]);
    };

    if target_source.trim().is_empty() {
        return Ok(vec![TextEdit {
            byte_start: 0,
            byte_end: 0,
            replacement: format!("{prelude}\n\n{wrapper}"),
        }]);
    }

    let prelude_insert = rust_prelude_insert_byte(target_source);
    if prelude_insert == target_source.len() {
        return Ok(vec![TextEdit {
            byte_start: target_source.len(),
            byte_end: target_source.len(),
            replacement: format!("{prelude}\n\n{wrapper}"),
        }]);
    }

    Ok(vec![
        TextEdit {
            byte_start: prelude_insert,
            byte_end: prelude_insert,
            replacement: format!("{prelude}\n\n"),
        },
        TextEdit {
            byte_start: target_source.len(),
            byte_end: target_source.len(),
            replacement: wrapper,
        },
    ])
}

fn find_rust_field_initializer<'a>(parsed: &'a ParsedSource, field_name: &str) -> Option<Node<'a>> {
    find_node(parsed.tree.root_node(), |node| {
        node.kind() == "field_initializer"
            && rust_field_initializer_name(node, &parsed.source).as_deref() == Some(field_name)
    })
}

fn find_node<'tree>(
    root: Node<'tree>,
    mut predicate: impl FnMut(Node<'tree>) -> bool,
) -> Option<Node<'tree>> {
    let mut stack = vec![root];
    while let Some(node) = stack.pop() {
        if predicate(node) {
            return Some(node);
        }
        let mut cursor = node.walk();
        for child in node.named_children(&mut cursor) {
            stack.push(child);
        }
    }
    None
}

fn rust_field_initializer_name(node: Node<'_>, source: &str) -> Option<String> {
    node.child_by_field_name("name")
        .or_else(|| node.child_by_field_name("field"))
        .and_then(|child| child.utf8_text(source.as_bytes()).ok())
        .map(str::to_string)
        .or_else(|| {
            node.utf8_text(source.as_bytes())
                .ok()
                .and_then(|text| text.split(':').next())
                .map(str::trim)
                .filter(|text| !text.is_empty())
                .map(str::to_string)
        })
}

fn rust_field_value_end(source: &str, node: Node<'_>) -> Option<usize> {
    if let Some(value) = node
        .child_by_field_name("value")
        .or_else(|| node.child_by_field_name("body"))
    {
        return Some(value.end_byte());
    }
    let text = source.get(node.start_byte()..node.end_byte())?;
    let colon = text.find(':')?;
    let mut end = node.end_byte();
    let value = &text[colon + 1..];
    let trimmed_len = value.trim_end().len();
    end -= value.len().saturating_sub(trimmed_len);
    Some(end)
}

fn rust_prelude_present(target_source: &str, prelude: &str) -> bool {
    let prelude = prelude.trim();
    if prelude.contains('\n') {
        return target_source.contains(prelude);
    }
    target_source.lines().any(|line| line.trim() == prelude)
}

fn rust_prelude_insert_byte(target_source: &str) -> usize {
    let bytes = target_source.as_bytes();
    let mut idx = 0usize;
    while idx < target_source.len() {
        let line_end = target_source[idx..]
            .find('\n')
            .map(|offset| idx + offset + 1)
            .unwrap_or(target_source.len());
        let line = &target_source[idx..line_end];
        let trimmed = line.trim();
        let is_shebang = idx == 0 && trimmed.starts_with("#!") && !trimmed.starts_with("#![");
        if trimmed.starts_with("/*!") {
            let Some(close) = target_source[idx..].find("*/") else {
                return target_source.len();
            };
            idx += close + 2;
            while idx < bytes.len() && matches!(bytes[idx], b'\n' | b'\r') {
                idx += 1;
            }
            continue;
        }
        let is_inner = trimmed.starts_with("#![") || trimmed.starts_with("//!");
        if trimmed.is_empty() || is_shebang || is_inner {
            idx = line_end;
            continue;
        }
        break;
    }
    while idx < bytes.len() && matches!(bytes[idx], b'\n' | b'\r') {
        idx += 1;
    }
    idx
}

fn existing_target_impl_insert_byte(
    target_path: &Path,
    target_source: &str,
    impl_name: &str,
    router_name: Option<&str>,
) -> Result<Option<TargetImplInsertion>> {
    if target_source.trim().is_empty() {
        return Ok(None);
    }
    let tree = parse_source("rust", target_source)
        .with_context(|| format!("parsing target {}", target_path.display()))?;
    let parsed = ParsedSource {
        path: target_path.to_path_buf(),
        language: "rust",
        source: target_source.to_string(),
        tree,
    };
    let root = parsed.tree.root_node();
    let mut cursor = root.walk();
    for impl_node in root
        .named_children(&mut cursor)
        .filter(|node| node.kind() == "impl_item")
    {
        let Some(name) = item_name(impl_node, &parsed.source, parsed.language) else {
            continue;
        };
        if name != impl_name {
            continue;
        }
        if !router_matches(&parsed, impl_node, router_name) {
            continue;
        }
        let Some(body) = impl_declaration_list(impl_node) else {
            continue;
        };
        let close = parsed
            .source
            .get(body.start_byte()..body.end_byte())
            .and_then(|text| text.rfind('}').map(|offset| body.start_byte() + offset))
            .ok_or_else(|| anyhow!("matching target impl has no closing brace"))?;
        return Ok(Some(TargetImplInsertion {
            byte: close,
            body_is_empty: parsed.source[body.start_byte() + 1..close]
                .trim()
                .is_empty(),
        }));
    }
    Ok(None)
}

fn router_matches(parsed: &ParsedSource, impl_node: Node<'_>, router_name: Option<&str>) -> bool {
    let Some(router_name) = router_name else {
        return true;
    };
    let leading = leading_trivia_start(&parsed.source, impl_node);
    let exact_router = format!("router={router_name}");
    attached_attributes(&parsed.source, leading, impl_node.start_byte())
        .iter()
        .map(|attr| {
            attr.chars()
                .filter(|ch| !ch.is_whitespace())
                .collect::<String>()
        })
        .any(|attr| {
            let router_arg = attr.contains(&format!("{exact_router})"))
                || attr.contains(&format!("{exact_router},"));
            attr.contains("tool_router") && router_arg
        })
}

fn impl_declaration_list(impl_node: Node<'_>) -> Option<Node<'_>> {
    let mut cursor = impl_node.walk();
    let body = impl_node
        .named_children(&mut cursor)
        .find(|child| child.kind() == "declaration_list");
    body
}

fn rust_impl_methods_target_wrapper(
    target_source: &str,
    router_name: Option<&str>,
    router_export_name: Option<&str>,
    impl_name: &str,
    source: &str,
    selected: &[RustImplMethod],
) -> Result<String> {
    let mut wrapper = String::new();
    if let Some(export_name) = router_export_name {
        let router_name =
            router_name.ok_or_else(|| anyhow!("router_export_name requires router_name"))?;
        wrapper.push_str("pub(super) fn ");
        wrapper.push_str(export_name);
        wrapper.push_str("() -> ToolRouter<BlackboxServer> {\n    BlackboxServer::");
        wrapper.push_str(router_name);
        wrapper.push_str("()\n}\n\n");
    }
    if let Some(router_name) = router_name {
        wrapper.push_str("#[tool_router(router = ");
        wrapper.push_str(router_name);
        wrapper.push_str(")]\n");
    }
    wrapper.push_str(impl_name);
    wrapper.push_str(" {\n");
    wrapper.push_str(&rust_impl_methods_block(source, selected)?);
    wrapper.push_str("}\n");

    if target_source.trim().is_empty() {
        Ok(wrapper)
    } else {
        Ok(format!(
            "{}{}",
            if target_source.ends_with('\n') {
                "\n"
            } else {
                "\n\n"
            },
            wrapper
        ))
    }
}

fn rust_impl_methods_block(source: &str, selected: &[RustImplMethod]) -> Result<String> {
    let mut block = String::new();
    for (idx, method) in selected.iter().enumerate() {
        let text = source
            .get(method.item.leading_trivia_start..method.item.byte_end)
            .ok_or_else(|| {
                anyhow!(
                    "invalid impl method range for {}",
                    method.item.plan_local_id
                )
            })?
            .trim_matches('\n');
        if idx > 0 {
            block.push('\n');
        }
        block.push_str(text);
        block.push('\n');
    }
    Ok(block)
}

fn validate_rust_identifier(value: &str, field: &str) -> Result<()> {
    let mut chars = value.chars();
    let Some(first) = chars.next() else {
        bail!("{field} must not be empty");
    };
    if !(first == '_' || first.is_ascii_alphabetic()) {
        bail!("{field} must be a Rust identifier");
    }
    if !chars.all(|ch| ch == '_' || ch.is_ascii_alphanumeric()) {
        bail!("{field} must be a Rust identifier");
    }
    Ok(())
}

fn validate_rust_router_call(value: &str, field: &str) -> Result<()> {
    if value.trim() != value {
        bail!("{field} must not have leading or trailing whitespace");
    }
    let Some(path) = value.strip_suffix("()") else {
        bail!("{field} must be a zero-argument Rust path call");
    };
    if path.is_empty() {
        bail!("{field} must be a zero-argument Rust path call");
    }
    for segment in path.split("::") {
        validate_rust_identifier(segment, field)?;
    }
    Ok(())
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

fn rust_status_items(parsed: &ParsedSource) -> Vec<SyntaxItem> {
    let mut items = rust_items(parsed);
    items.extend(
        rust_impl_methods(parsed)
            .into_iter()
            .map(|method| method.item),
    );
    items
}

fn rust_impl_methods(parsed: &ParsedSource) -> Vec<RustImplMethod> {
    let root = parsed.tree.root_node();
    let mut cursor = root.walk();
    root.named_children(&mut cursor)
        .filter(|node| node.kind() == "impl_item")
        .flat_map(|impl_node| rust_impl_methods_in(parsed, impl_node))
        .collect()
}

fn rust_impl_methods_in(parsed: &ParsedSource, impl_node: Node<'_>) -> Vec<RustImplMethod> {
    let impl_name = item_name(impl_node, &parsed.source, parsed.language)
        .unwrap_or_else(|| "(unnamed impl)".to_string());
    let mut cursor = impl_node.walk();
    impl_node
        .named_children(&mut cursor)
        .filter(|child| child.kind() == "declaration_list")
        .flat_map(|body| {
            let mut body_cursor = body.walk();
            body.named_children(&mut body_cursor)
                .filter(|member| member.kind() == "function_item")
                .map(|member| RustImplMethod {
                    impl_name: impl_name.clone(),
                    impl_byte_start: impl_node.start_byte(),
                    item: syntax_item_with_kind(parsed, member, "impl_method"),
                })
                .collect::<Vec<_>>()
        })
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
    syntax_item_with_kind(parsed, node, node.kind())
}

fn syntax_item_with_kind(parsed: &ParsedSource, node: Node<'_>, kind: &str) -> SyntaxItem {
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
            kind,
            display_name
        ),
        kind: kind.to_string(),
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
            item_names: None,
            item_kinds: None,
            limit: None,
            include_attributes: None,
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
    fn status_lists_rust_impl_methods() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("lib.rs");
        fs::write(
            &path,
            "struct Server;\n\nimpl Server {\n    #[tool(description = \"x\")]\n    fn find(&self) {}\n}\n",
        )
        .unwrap();

        let text = status(&RefactorStatusParams {
            file: path_string(&path),
            project_dir: None,
            item_names: None,
            item_kinds: None,
            limit: None,
            include_attributes: None,
        })
        .unwrap();
        let parsed: RefactorStatus = serde_json::from_str(&text).unwrap();
        let method = parsed
            .items
            .iter()
            .find(|item| item.kind == "impl_method" && item.name.as_deref() == Some("find"))
            .expect("impl method should be listed");
        assert!(method
            .attributes
            .iter()
            .any(|attr| attr == "#[tool(description = \"x\")]"));
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
            impl_name: None,
            router_name: None,
            router_call: None,
            router_export_name: None,
            target_prelude: None,
            project_dir: None,
        })
        .unwrap();
        let plan_value: serde_json::Value = serde_json::from_str(&plan_text).unwrap();
        let response = apply(
            &RefactorApplyParams {
                plan: plan_value,
                confirm: Some(true),
                allow_dirty_worktree: None,
                allow_unregistered_paths: None,
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
    fn extract_impl_methods_wraps_target_router_and_applies() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("main.rs");
        let target = dir.path().join("tools.rs");
        fs::write(
            &source,
            "struct BlackboxServer;\n\n#[tool_router(router = old_tools)]\nimpl BlackboxServer {\n    #[tool(description = \"move\")]\n    fn move_me(&self) -> usize {\n        1\n    }\n\n    fn keep(&self) -> usize {\n        2\n    }\n}\n",
        )
        .unwrap();

        let plan_text = plan(&RefactorPlanParams {
            kind: "extract_rust_impl_methods".into(),
            source: path_string(&source),
            target: Some(path_string(&target)),
            item_names: Some(vec!["move_me".into()]),
            item_kinds: Some(vec!["impl_method".into()]),
            impl_name: Some("impl BlackboxServer".into()),
            router_name: Some("moved_tools".into()),
            router_call: None,
            router_export_name: None,
            target_prelude: Some("use super::*;".into()),
            project_dir: None,
        })
        .unwrap();
        let plan_value: serde_json::Value = serde_json::from_str(&plan_text).unwrap();
        let response = apply(
            &RefactorApplyParams {
                plan: plan_value,
                confirm: Some(true),
                allow_dirty_worktree: None,
                allow_unregistered_paths: None,
            },
            &[project_record(dir.path())],
        )
        .unwrap();
        let applied: RefactorApplyResponse = serde_json::from_str(&response).unwrap();
        assert_eq!(applied.status, "ok");

        let source_text = fs::read_to_string(&source).unwrap();
        assert!(!source_text.contains("fn move_me"));
        assert!(source_text.contains("fn keep"));

        let target_text = fs::read_to_string(&target).unwrap();
        assert!(target_text.contains("use super::*;"));
        assert!(target_text.contains("#[tool_router(router = moved_tools)]"));
        assert!(target_text.contains("#[tool(description = \"move\")]"));
        assert!(target_text.contains("impl BlackboxServer"));
        assert!(target_text.contains("fn move_me"));
    }

    #[test]
    fn extract_impl_methods_can_generate_router_export_helper() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("main.rs");
        let target = dir.path().join("tools.rs");
        fs::write(
            &source,
            "struct BlackboxServer;\n\nimpl BlackboxServer {\n    fn move_me(&self) -> usize {\n        1\n    }\n}\n",
        )
        .unwrap();

        let plan_text = plan(&RefactorPlanParams {
            kind: "extract_rust_impl_methods".into(),
            source: path_string(&source),
            target: Some(path_string(&target)),
            item_names: Some(vec!["move_me".into()]),
            item_kinds: Some(vec!["impl_method".into()]),
            impl_name: Some("impl BlackboxServer".into()),
            router_name: Some("moved_tools".into()),
            router_call: None,
            router_export_name: Some("router".into()),
            target_prelude: Some("use super::*;".into()),
            project_dir: None,
        })
        .unwrap();
        let plan_value: serde_json::Value = serde_json::from_str(&plan_text).unwrap();
        let response = apply(
            &RefactorApplyParams {
                plan: plan_value,
                confirm: Some(true),
                allow_dirty_worktree: None,
                allow_unregistered_paths: None,
            },
            &[project_record(dir.path())],
        )
        .unwrap();
        let applied: RefactorApplyResponse = serde_json::from_str(&response).unwrap();
        assert_eq!(applied.status, "ok");

        let target_text = fs::read_to_string(&target).unwrap();
        assert!(target_text.contains("pub(super) fn router() -> ToolRouter<BlackboxServer>"));
        assert!(target_text.contains("BlackboxServer::moved_tools()"));
        assert!(target_text.contains("#[tool_router(router = moved_tools)]"));
    }

    #[test]
    fn extract_impl_methods_appends_to_existing_target_impl() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("main.rs");
        let target = dir.path().join("tools.rs");
        fs::write(
            &source,
            "struct BlackboxServer;\n\nimpl BlackboxServer {\n    fn move_me(&self) -> usize {\n        1\n    }\n}\n",
        )
        .unwrap();
        fs::write(
            &target,
            "use super::*;\n\n#[tool_router(router = moved_tools)]\nimpl BlackboxServer {\n    fn already_here(&self) {}\n}\n",
        )
        .unwrap();

        let plan_text = plan(&RefactorPlanParams {
            kind: "extract_rust_impl_methods".into(),
            source: path_string(&source),
            target: Some(path_string(&target)),
            item_names: Some(vec!["move_me".into()]),
            item_kinds: Some(vec!["impl_method".into()]),
            impl_name: Some("impl BlackboxServer".into()),
            router_name: Some("moved_tools".into()),
            router_call: None,
            router_export_name: None,
            target_prelude: None,
            project_dir: None,
        })
        .unwrap();
        let plan_value: serde_json::Value = serde_json::from_str(&plan_text).unwrap();
        let response = apply(
            &RefactorApplyParams {
                plan: plan_value,
                confirm: Some(true),
                allow_dirty_worktree: None,
                allow_unregistered_paths: None,
            },
            &[project_record(dir.path())],
        )
        .unwrap();
        let applied: RefactorApplyResponse = serde_json::from_str(&response).unwrap();
        assert_eq!(applied.status, "ok");

        let target_text = fs::read_to_string(&target).unwrap();
        assert_eq!(target_text.matches("impl BlackboxServer").count(), 1);
        assert!(target_text.contains("fn already_here"));
        assert!(target_text.contains("fn move_me"));
    }

    #[test]
    fn extract_impl_methods_does_not_merge_different_router_prefix() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("main.rs");
        let target = dir.path().join("tools.rs");
        fs::write(
            &source,
            "struct BlackboxServer;\n\nimpl BlackboxServer {\n    fn move_me(&self) {}\n}\n",
        )
        .unwrap();
        fs::write(
            &target,
            "#[tool_router(router = search_tools_extra)]\nimpl BlackboxServer {\n    fn already_here(&self) {}\n}\n",
        )
        .unwrap();

        let plan_text = plan(&RefactorPlanParams {
            kind: "extract_rust_impl_methods".into(),
            source: path_string(&source),
            target: Some(path_string(&target)),
            item_names: Some(vec!["move_me".into()]),
            item_kinds: Some(vec!["impl_method".into()]),
            impl_name: Some("impl BlackboxServer".into()),
            router_name: Some("search_tools".into()),
            router_call: None,
            router_export_name: None,
            target_prelude: None,
            project_dir: None,
        })
        .unwrap();
        let plan_value: serde_json::Value = serde_json::from_str(&plan_text).unwrap();
        let response = apply(
            &RefactorApplyParams {
                plan: plan_value,
                confirm: Some(true),
                allow_dirty_worktree: None,
                allow_unregistered_paths: None,
            },
            &[project_record(dir.path())],
        )
        .unwrap();
        let applied: RefactorApplyResponse = serde_json::from_str(&response).unwrap();
        assert_eq!(applied.status, "ok");
        let target_text = fs::read_to_string(&target).unwrap();
        assert_eq!(target_text.matches("impl BlackboxServer").count(), 2);
        assert!(target_text.contains("#[tool_router(router = search_tools)]"));
    }

    #[test]
    fn extract_impl_methods_inserts_prelude_at_top_of_existing_target() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("main.rs");
        let target = dir.path().join("tools.rs");
        fs::write(
            &source,
            "struct BlackboxServer;\n\nimpl BlackboxServer {\n    fn move_me(&self) {}\n}\n",
        )
        .unwrap();
        fs::write(&target, "pub fn helper() {}\n").unwrap();

        let plan_text = plan(&RefactorPlanParams {
            kind: "extract_rust_impl_methods".into(),
            source: path_string(&source),
            target: Some(path_string(&target)),
            item_names: Some(vec!["move_me".into()]),
            item_kinds: Some(vec!["impl_method".into()]),
            impl_name: Some("impl BlackboxServer".into()),
            router_name: None,
            router_call: None,
            router_export_name: None,
            target_prelude: Some("use super::*;".into()),
            project_dir: None,
        })
        .unwrap();
        let plan_value: serde_json::Value = serde_json::from_str(&plan_text).unwrap();
        let response = apply(
            &RefactorApplyParams {
                plan: plan_value,
                confirm: Some(true),
                allow_dirty_worktree: None,
                allow_unregistered_paths: None,
            },
            &[project_record(dir.path())],
        )
        .unwrap();
        let applied: RefactorApplyResponse = serde_json::from_str(&response).unwrap();
        assert_eq!(applied.status, "ok");
        let target_text = fs::read_to_string(&target).unwrap();
        assert!(target_text.starts_with("use super::*;\n\npub fn helper()"));
        assert!(target_text.contains("impl BlackboxServer"));
    }

    #[test]
    fn extract_impl_methods_inserts_prelude_after_inner_attrs() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("main.rs");
        let target = dir.path().join("tools.rs");
        fs::write(
            &source,
            "struct BlackboxServer;\n\nimpl BlackboxServer {\n    fn move_me(&self) {}\n}\n",
        )
        .unwrap();
        fs::write(
            &target,
            "#![allow(dead_code)]\n//! module docs\n\n// use super::*;\npub fn helper() {}\n",
        )
        .unwrap();

        let plan_text = plan(&RefactorPlanParams {
            kind: "extract_rust_impl_methods".into(),
            source: path_string(&source),
            target: Some(path_string(&target)),
            item_names: Some(vec!["move_me".into()]),
            item_kinds: Some(vec!["impl_method".into()]),
            impl_name: Some("impl BlackboxServer".into()),
            router_name: None,
            router_call: None,
            router_export_name: None,
            target_prelude: Some("use super::*;".into()),
            project_dir: None,
        })
        .unwrap();
        let plan_value: serde_json::Value = serde_json::from_str(&plan_text).unwrap();
        let response = apply(
            &RefactorApplyParams {
                plan: plan_value,
                confirm: Some(true),
                allow_dirty_worktree: None,
                allow_unregistered_paths: None,
            },
            &[project_record(dir.path())],
        )
        .unwrap();
        let applied: RefactorApplyResponse = serde_json::from_str(&response).unwrap();
        assert_eq!(applied.status, "ok");

        let target_text = fs::read_to_string(&target).unwrap();
        assert!(target_text.starts_with("#![allow(dead_code)]\n//! module docs\n\nuse super::*;"));
        assert_eq!(target_text.matches("use super::*;").count(), 2);
    }

    #[test]
    fn extract_impl_methods_inserts_prelude_after_inner_block_doc() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("main.rs");
        let target = dir.path().join("tools.rs");
        fs::write(
            &source,
            "struct BlackboxServer;\n\nimpl BlackboxServer {\n    fn move_me(&self) {}\n}\n",
        )
        .unwrap();
        fs::write(&target, "/*!\nmodule docs\n*/\n\npub fn helper() {}\n").unwrap();

        let plan_text = plan(&RefactorPlanParams {
            kind: "extract_rust_impl_methods".into(),
            source: path_string(&source),
            target: Some(path_string(&target)),
            item_names: Some(vec!["move_me".into()]),
            item_kinds: Some(vec!["impl_method".into()]),
            impl_name: Some("impl BlackboxServer".into()),
            router_name: None,
            router_call: None,
            router_export_name: None,
            target_prelude: Some("use super::*;".into()),
            project_dir: None,
        })
        .unwrap();
        let plan_value: serde_json::Value = serde_json::from_str(&plan_text).unwrap();
        let response = apply(
            &RefactorApplyParams {
                plan: plan_value,
                confirm: Some(true),
                allow_dirty_worktree: None,
                allow_unregistered_paths: None,
            },
            &[project_record(dir.path())],
        )
        .unwrap();
        let applied: RefactorApplyResponse = serde_json::from_str(&response).unwrap();
        assert_eq!(applied.status, "ok");
        assert!(fs::read_to_string(&target)
            .unwrap()
            .starts_with("/*!\nmodule docs\n*/\n\nuse super::*;"));
    }

    #[test]
    fn extract_impl_methods_handles_generic_impl_header() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("lib.rs");
        let target = dir.path().join("moved.rs");
        fs::write(
            &source,
            "struct Boxed<T>(T);\n\nimpl<T> Boxed<T>\nwhere\n    T: Clone,\n{\n    fn clone_inner(&self) -> T {\n        self.0.clone()\n    }\n}\n",
        )
        .unwrap();

        let header = "impl<T> Boxed<T> where T: Clone,";
        let plan_text = plan(&RefactorPlanParams {
            kind: "extract_rust_impl_methods".into(),
            source: path_string(&source),
            target: Some(path_string(&target)),
            item_names: Some(vec!["clone_inner".into()]),
            item_kinds: Some(vec!["impl_method".into()]),
            impl_name: Some(header.into()),
            router_name: None,
            router_call: None,
            router_export_name: None,
            target_prelude: None,
            project_dir: None,
        })
        .unwrap();
        let plan_value: serde_json::Value = serde_json::from_str(&plan_text).unwrap();
        let response = apply(
            &RefactorApplyParams {
                plan: plan_value,
                confirm: Some(true),
                allow_dirty_worktree: None,
                allow_unregistered_paths: None,
            },
            &[project_record(dir.path())],
        )
        .unwrap();
        let applied: RefactorApplyResponse = serde_json::from_str(&response).unwrap();
        assert_eq!(applied.status, "ok");
        assert!(fs::read_to_string(&target).unwrap().contains(header));
    }

    #[test]
    fn extract_impl_method_requires_impl_filter_when_method_name_is_ambiguous() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("lib.rs");
        let target = dir.path().join("moved.rs");
        fs::write(
            &source,
            "struct A;\nstruct B;\nimpl A { fn same(&self) {} }\nimpl B { fn same(&self) {} }\n",
        )
        .unwrap();

        let err = plan(&RefactorPlanParams {
            kind: "extract_rust_impl_methods".into(),
            source: path_string(&source),
            target: Some(path_string(&target)),
            item_names: Some(vec!["same".into()]),
            item_kinds: None,
            impl_name: None,
            router_name: None,
            router_call: None,
            router_export_name: None,
            target_prelude: None,
            project_dir: None,
        })
        .unwrap_err();
        assert!(err.to_string().contains("matched multiple impl blocks"));
    }

    #[test]
    fn extract_impl_method_rejects_misleading_function_item_kind() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("lib.rs");
        let target = dir.path().join("moved.rs");
        fs::write(&source, "struct A;\nimpl A { fn method(&self) {} }\n").unwrap();

        let err = plan(&RefactorPlanParams {
            kind: "extract_rust_impl_methods".into(),
            source: path_string(&source),
            target: Some(path_string(&target)),
            item_names: Some(vec!["method".into()]),
            item_kinds: Some(vec!["function_item".into()]),
            impl_name: Some("impl A".into()),
            router_name: None,
            router_call: None,
            router_export_name: None,
            target_prelude: None,
            project_dir: None,
        })
        .unwrap_err();
        assert!(err
            .to_string()
            .contains("only supports item_kinds impl_method"));
    }

    #[test]
    fn add_rust_router_to_sum_appends_router_call() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("main.rs");
        fs::write(
            &source,
            "struct Server { tool_router: usize }\nimpl Server {\n    fn new() -> Self {\n        Self {\n            tool_router: Self::bbox_tools() + Self::bro_tools(),\n        }\n    }\n}\n",
        )
        .unwrap();

        let plan_text = plan(&RefactorPlanParams {
            kind: "add_rust_router_to_sum".into(),
            source: path_string(&source),
            target: None,
            item_names: None,
            item_kinds: None,
            impl_name: None,
            router_name: Some("search_tools".into()),
            router_call: None,
            router_export_name: None,
            target_prelude: None,
            project_dir: None,
        })
        .unwrap();
        let plan_value: serde_json::Value = serde_json::from_str(&plan_text).unwrap();
        let response = apply(
            &RefactorApplyParams {
                plan: plan_value,
                confirm: Some(true),
                allow_dirty_worktree: None,
                allow_unregistered_paths: None,
            },
            &[project_record(dir.path())],
        )
        .unwrap();
        let applied: RefactorApplyResponse = serde_json::from_str(&response).unwrap();
        assert_eq!(applied.status, "ok");
        let source_text = fs::read_to_string(&source).unwrap();
        assert!(source_text.contains(
            "tool_router: Self::bbox_tools() + Self::bro_tools() + Self::search_tools(),"
        ));
    }

    #[test]
    fn add_rust_router_to_sum_accepts_module_router_call() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("main.rs");
        fs::write(
            &source,
            "struct Server { tool_router: usize }\nimpl Server {\n    fn new() -> Self {\n        Self {\n            tool_router: Self::bbox_tools() + Self::bro_tools(),\n        }\n    }\n}\n",
        )
        .unwrap();

        let plan_text = plan(&RefactorPlanParams {
            kind: "add_rust_router_to_sum".into(),
            source: path_string(&source),
            target: None,
            item_names: None,
            item_kinds: None,
            impl_name: None,
            router_name: None,
            router_call: Some("refactor_tools::router()".into()),
            router_export_name: None,
            target_prelude: None,
            project_dir: None,
        })
        .unwrap();
        let plan_value: serde_json::Value = serde_json::from_str(&plan_text).unwrap();
        let response = apply(
            &RefactorApplyParams {
                plan: plan_value,
                confirm: Some(true),
                allow_dirty_worktree: None,
                allow_unregistered_paths: None,
            },
            &[project_record(dir.path())],
        )
        .unwrap();
        let applied: RefactorApplyResponse = serde_json::from_str(&response).unwrap();
        assert_eq!(applied.status, "ok");
        let source_text = fs::read_to_string(&source).unwrap();
        assert!(source_text.contains(
            "tool_router: Self::bbox_tools() + Self::bro_tools() + refactor_tools::router(),"
        ));
    }

    #[test]
    fn add_rust_router_to_sum_rejects_duplicate_router() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("main.rs");
        fs::write(
            &source,
            "struct Server { tool_router: usize }\nimpl Server { fn new() -> Self { Self { tool_router: Self::bbox_tools() + Self::search_tools(), } } }\n",
        )
        .unwrap();

        let err = plan(&RefactorPlanParams {
            kind: "add_rust_router_to_sum".into(),
            source: path_string(&source),
            target: None,
            item_names: None,
            item_kinds: None,
            impl_name: None,
            router_name: Some("search_tools".into()),
            router_call: None,
            router_export_name: None,
            target_prelude: None,
            project_dir: None,
        })
        .unwrap_err();
        assert!(err.to_string().contains("already contains"));
    }

    #[test]
    fn status_lists_generic_tree_sitter_items() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sample.ts");
        fs::write(&path, "export function helper() { return 1; }\n").unwrap();

        let text = status(&RefactorStatusParams {
            file: path_string(&path),
            project_dir: None,
            item_names: None,
            item_kinds: None,
            limit: None,
            include_attributes: None,
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
            impl_name: None,
            router_name: None,
            router_call: None,
            router_export_name: None,
            target_prelude: None,
            project_dir: None,
        })
        .unwrap();
        let plan_value: serde_json::Value = serde_json::from_str(&plan_text).unwrap();
        let response = apply(
            &RefactorApplyParams {
                plan: plan_value,
                confirm: Some(true),
                allow_dirty_worktree: None,
                allow_unregistered_paths: None,
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
                allow_unregistered_paths: None,
            },
            &[project_record(project.path())],
        )
        .unwrap_err();
        assert!(err.to_string().contains("outside registered projects"));
    }

    #[test]
    fn apply_can_allow_unregistered_paths_for_practice_worktrees() {
        let outside = tempfile::tempdir().unwrap();
        let source = outside.path().join("lib.rs");
        fs::write(&source, "fn f() {}\n").unwrap();
        let plan = RefactorPlan {
            title: "practice".into(),
            kind: "extract_rust_items".into(),
            semantic_status: SemanticStatus::StructuralOnly,
            dry_run: true,
            edits: vec![FileEdit {
                path: path_string(&source),
                original_sha256: sha256_hex(b"fn f() {}\n"),
                edits: vec![TextEdit {
                    byte_start: 3,
                    byte_end: 4,
                    replacement: "g".into(),
                }],
            }],
            validations: Vec::new(),
            items: Vec::new(),
            leftovers: Vec::new(),
        };

        let response = apply(
            &RefactorApplyParams {
                plan: serde_json::to_value(plan).unwrap(),
                confirm: Some(true),
                allow_dirty_worktree: None,
                allow_unregistered_paths: Some(true),
            },
            &[],
        )
        .unwrap();

        let applied: RefactorApplyResponse = serde_json::from_str(&response).unwrap();
        assert_eq!(applied.status, "ok");
        assert_eq!(fs::read_to_string(source).unwrap(), "fn g() {}\n");
    }
}
