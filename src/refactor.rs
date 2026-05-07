use std::collections::{BTreeMap, HashSet};
use std::fs;
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{anyhow, bail, Context, Result};
use reqwest::Url;
use rmcp::schemars;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tree_sitter::{Node, Tree};

use crate::chunker::code::{language_for_path, parser_for_language};
use crate::projects::ProjectRecord;

const BLACKBOX_SERVICE_ENV_VARS: &[&str] = &[
    "BBOX_PORT",
    "BRO_PORT",
    "BLACKBOX_MCP_NAME",
    "BLACKBOX_MCP_URL",
    "BLACKBOX_STATE_DIR",
    "BLACKBOX_KNOWLEDGE_PATH",
    "BLACKBOX_THREADS_PATH",
    "BLACKBOX_NOTES_PATH",
    "BLACKBOX_GLOBAL_CLAUDE_MD",
    "BLACKBOX_GLOBAL_CODEX_MD",
    "BLACKBOX_GLOBAL_GEMINI_MD",
    "BLACKBOX_BACKUP_DIR",
    "BRO_HOME",
    "TRANSCRIPT_SEARCH_ROOTS",
    "TRANSCRIPT_SEARCH_CODEX_ROOT",
    "TRANSCRIPT_SEARCH_INDEX_PATH",
];

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

#[derive(Debug, Clone, Deserialize, schemars::JsonSchema)]
pub struct RefactorPlanParams {
    /// Supported generic or language-scoped plan kind. Pull sm-refactor first.
    pub kind: String,
    /// Source file. Relative paths resolve against project_dir or cwd.
    pub source: String,
    /// Optional target file. Required by plan kinds that write or copy elsewhere.
    #[serde(default)]
    pub target: Option<String>,
    /// Optional exact item names. Meaning is plan-specific.
    #[serde(default)]
    pub item_names: Option<Vec<String>>,
    /// Optional syntax item kinds. Values are language-specific.
    #[serde(default)]
    pub item_kinds: Option<Vec<String>>,
    /// Optional plan-specific implementation/scope filter.
    #[serde(default)]
    pub impl_name: Option<String>,
    /// Optional plan-specific module/declaration name.
    #[serde(default)]
    pub module_name: Option<String>,
    /// Optional plan-specific visibility value.
    #[serde(default)]
    pub visibility: Option<String>,
    /// Optional plan-specific import/use path.
    #[serde(default)]
    pub use_path: Option<String>,
    /// Optional plan-specific router name.
    #[serde(default)]
    pub router_name: Option<String>,
    /// Optional plan-specific router call expression.
    #[serde(default)]
    pub router_call: Option<String>,
    /// Optional plan-specific router export helper name.
    #[serde(default)]
    pub router_export_name: Option<String>,
    /// Optional plan-specific text inserted before generated target content.
    #[serde(default)]
    pub target_prelude: Option<String>,
    /// Optional exact text to replace. Used by generic text replacement plans.
    #[serde(default)]
    pub old_text: Option<String>,
    /// Optional replacement text or complete file content, depending on plan kind.
    #[serde(default)]
    pub new_text: Option<String>,
    /// Optional toggle for replacing every exact match instead of requiring one.
    #[serde(default)]
    pub replace_all: Option<bool>,
    /// Optional TOML table name for structured TOML edit plans.
    #[serde(default)]
    pub toml_table: Option<String>,
    /// Optional TOML key/value entries for structured TOML edit plans.
    #[serde(default)]
    pub toml_entries: Option<BTreeMap<String, serde_json::Value>>,
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

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct RefactorRunParams {
    /// Human-readable compound run title.
    pub title: String,
    /// Project root used for relative paths.
    pub project_dir: String,
    /// Ordered primitive-plan steps.
    pub steps: Vec<RefactorRunStep>,
    /// Must be true to write files. Otherwise returns a plan-only report.
    #[serde(default)]
    pub confirm: Option<bool>,
    /// Default false. When false, refuses initially dirty files touched by primitive plans.
    #[serde(default)]
    pub allow_dirty_worktree: Option<bool>,
    /// Default false. When false, refuses paths outside registered projects.
    #[serde(default)]
    pub allow_unregistered_paths: Option<bool>,
}

#[derive(Debug, Clone, Deserialize, schemars::JsonSchema)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum RefactorRunStep {
    Plan {
        #[serde(flatten)]
        params: RefactorPlanParams,
    },
    Command {
        command: String,
        #[serde(default)]
        args: Vec<String>,
        #[serde(default)]
        cwd: Option<String>,
        /// Optional extra files the command may mutate. Relative paths resolve against project_dir.
        #[serde(default)]
        touches: Vec<String>,
        /// Defaults true. Required command failure rolls back prior plan writes.
        #[serde(default)]
        required: Option<bool>,
    },
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
pub struct FileMove {
    pub source_path: String,
    pub target_path: String,
    pub original_sha256: String,
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
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub file_moves: Vec<FileMove>,
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
pub struct RefactorRunResponse {
    pub status: String,
    pub title: String,
    pub dry_run: bool,
    pub steps: Vec<RefactorRunStepReport>,
    pub files_written: Vec<String>,
    pub rolled_back: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub rollback_errors: Vec<String>,
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
pub struct RefactorRunStepReport {
    pub index: usize,
    pub op: String,
    pub status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub files: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub validations: Vec<ParseValidationResult>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
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
        "delete_rust_items" => plan_delete_rust_items(p),
        "add_rust_router_to_sum" => plan_add_rust_router_to_sum(p),
        "add_rust_mod_decl" => plan_add_rust_mod_decl(p),
        "add_rust_use_decl" => plan_add_rust_use_decl(p),
        "copy_rust_mod_decls" => plan_copy_rust_mod_decls(p),
        "rewrite_rust_mod_visibility" => plan_rewrite_rust_mod_visibility(p),
        "rust_lsp_rename" => plan_rust_lsp_rename(p),
        "rust_organize_imports" => plan_rust_organize_imports(p),
        "move_file" => plan_move_file(p),
        "replace_text" => plan_replace_text(p),
        "write_file" => plan_write_file(p),
        "ensure_toml_table" => plan_ensure_toml_table(p),
        other => bail!(
            "unsupported refactor plan kind `{other}`; supported: extract_rust_items, extract_rust_impl_methods, delete_rust_items, add_rust_router_to_sum, add_rust_mod_decl, add_rust_use_decl, copy_rust_mod_decls, rewrite_rust_mod_visibility, rust_lsp_rename, rust_organize_imports, move_file, replace_text, write_file, ensure_toml_table"
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

    let mut originals: Vec<(PathBuf, Option<Vec<u8>>)> = Vec::new();
    let mut moved_files = Vec::new();
    for file_move in &plan.file_moves {
        let source_path = PathBuf::from(&file_move.source_path);
        let target_path = PathBuf::from(&file_move.target_path);
        if p.allow_unregistered_paths != Some(true) {
            ensure_path_in_registered_project(&source_path, projects)?;
            ensure_path_in_registered_project(&target_path, projects)?;
        }
        if p.allow_dirty_worktree != Some(true) {
            ensure_git_clean_for_path(&source_path)?;
            ensure_git_clean_for_path(&target_path)?;
        }
        if target_path.exists() {
            bail!(
                "refusing to move {} to {}: target already exists",
                source_path.display(),
                target_path.display()
            );
        }
        let original = read_original_for_edit(&source_path, &file_move.original_sha256)?;
        let original_hash = sha256_hex(&original);
        if original_hash != file_move.original_sha256 {
            bail!(
                "refusing to move {}: file hash changed (expected {}, got {})",
                source_path.display(),
                file_move.original_sha256,
                original_hash
            );
        }
        originals.push((source_path.clone(), Some(original.clone())));
        originals.push((target_path.clone(), None));
        moved_files.push((source_path, target_path, original));
    }

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
        originals.push((path.clone(), Some(original)));
        rewritten.push((path, next.into_bytes()));
    }

    let mut validation_inputs = rewritten.clone();
    validation_inputs.extend(
        moved_files
            .iter()
            .map(|(_, target_path, bytes)| (target_path.clone(), bytes.clone())),
    );
    let validations = validate_rewritten_files(&validation_inputs)?;
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
    for (source_path, target_path, bytes) in &moved_files {
        if let Some(parent) = target_path.parent() {
            if let Err(err) = fs::create_dir_all(parent) {
                let rollback_errors = restore_snapshots(&originals);
                return Ok(serde_json::to_string_pretty(&RefactorApplyResponse {
                    status: "write_failed".to_string(),
                    files_written,
                    validations,
                    rolled_back: rollback_errors.is_empty(),
                    error: Some(format!(
                        "failed to create parent directory {}: {err:#}",
                        parent.display()
                    )),
                    rollback_errors,
                })?);
            }
        }
        if let Err(err) = write_atomic(target_path, bytes) {
            let rollback_errors = restore_snapshots(&originals);
            return Ok(serde_json::to_string_pretty(&RefactorApplyResponse {
                status: "write_failed".to_string(),
                files_written,
                validations,
                rolled_back: rollback_errors.is_empty(),
                error: Some(format!(
                    "failed to write {}: {err:#}",
                    target_path.display()
                )),
                rollback_errors,
            })?);
        }
        files_written.push(path_string(target_path));
        if let Err(err) = fs::remove_file(source_path) {
            let rollback_errors = restore_snapshots(&originals);
            return Ok(serde_json::to_string_pretty(&RefactorApplyResponse {
                status: "remove_failed".to_string(),
                files_written,
                validations,
                rolled_back: rollback_errors.is_empty(),
                error: Some(format!(
                    "failed to remove {}: {err:#}",
                    source_path.display()
                )),
                rollback_errors,
            })?);
        }
    }

    for (path, bytes) in &rewritten {
        if p.allow_dirty_worktree != Some(true) {
            if let Err(err) = ensure_git_clean_for_path(path) {
                let rollback_errors = restore_snapshots(&originals);
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
            let rollback_errors = restore_snapshots(&originals);
            return Ok(serde_json::to_string_pretty(&RefactorApplyResponse {
                status: "write_failed".to_string(),
                files_written,
                validations,
                rolled_back: rollback_errors.is_empty(),
                error: Some(format!("failed to write {}: {err:#}", path.display())),
                rollback_errors,
            })?);
        }
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

pub fn run(p: &RefactorRunParams, projects: &[ProjectRecord]) -> Result<String> {
    let project_dir = resolve_path(None, &p.project_dir)?;
    if !project_dir.is_dir() {
        bail!(
            "project_dir must be an existing directory: {}",
            project_dir.display()
        );
    }
    let confirmed = p.confirm == Some(true);
    let mut snapshots: Vec<(PathBuf, Option<Vec<u8>>)> = Vec::new();
    let mut snapshot_paths: HashSet<PathBuf> = HashSet::new();
    let mut reports = Vec::new();
    let mut files_written = Vec::new();

    for (idx, step) in p.steps.iter().enumerate() {
        match step {
            RefactorRunStep::Plan { params } => {
                let mut step_params = params.clone();
                if step_params.project_dir.is_none() {
                    step_params.project_dir = Some(path_string(&project_dir));
                }
                let plan_text = match plan(&step_params) {
                    Ok(plan_text) => plan_text,
                    Err(err) => {
                        let rollback_errors = restore_snapshots(&snapshots);
                        return Ok(serde_json::to_string_pretty(&RefactorRunResponse {
                            status: "step_failed".to_string(),
                            title: p.title.clone(),
                            dry_run: !confirmed,
                            steps: append_report(
                                reports,
                                RefactorRunStepReport {
                                    index: idx,
                                    op: "plan".to_string(),
                                    status: "plan_failed".to_string(),
                                    kind: Some(step_params.kind),
                                    title: None,
                                    files: Vec::new(),
                                    validations: Vec::new(),
                                    error: Some(err.to_string()),
                                },
                            ),
                            files_written,
                            rolled_back: rollback_errors.is_empty(),
                            error: Some(err.to_string()),
                            rollback_errors,
                        })?);
                    }
                };
                let plan_value: serde_json::Value = serde_json::from_str(&plan_text)?;
                let refactor_plan: RefactorPlan = serde_json::from_value(plan_value.clone())?;
                validate_plan_shape(&refactor_plan)?;
                let mut step_files = refactor_plan
                    .file_moves
                    .iter()
                    .flat_map(|file_move| {
                        [file_move.source_path.clone(), file_move.target_path.clone()]
                    })
                    .collect::<Vec<_>>();
                step_files.extend(refactor_plan.edits.iter().map(|edit| edit.path.clone()));

                if confirmed {
                    for file in &step_files {
                        let path = PathBuf::from(file);
                        if p.allow_unregistered_paths != Some(true) {
                            if let Err(err) = ensure_path_in_registered_project(&path, projects) {
                                let rollback_errors = restore_snapshots(&snapshots);
                                return Ok(serde_json::to_string_pretty(&RefactorRunResponse {
                                    status: "step_failed".to_string(),
                                    title: p.title.clone(),
                                    dry_run: false,
                                    steps: reports,
                                    files_written,
                                    rolled_back: rollback_errors.is_empty(),
                                    error: Some(err.to_string()),
                                    rollback_errors,
                                })?);
                            }
                        }
                        if !snapshot_paths.contains(&path) {
                            if p.allow_dirty_worktree != Some(true) {
                                if let Err(err) = ensure_git_clean_for_path(&path) {
                                    let rollback_errors = restore_snapshots(&snapshots);
                                    return Ok(serde_json::to_string_pretty(
                                        &RefactorRunResponse {
                                            status: "step_failed".to_string(),
                                            title: p.title.clone(),
                                            dry_run: false,
                                            steps: reports,
                                            files_written,
                                            rolled_back: rollback_errors.is_empty(),
                                            error: Some(err.to_string()),
                                            rollback_errors,
                                        },
                                    )?);
                                }
                            }
                            let original = match fs::read(&path) {
                                Ok(bytes) => Some(bytes),
                                Err(err) if err.kind() == std::io::ErrorKind::NotFound => None,
                                Err(err) => {
                                    let rollback_errors = restore_snapshots(&snapshots);
                                    return Ok(serde_json::to_string_pretty(
                                        &RefactorRunResponse {
                                            status: "step_failed".to_string(),
                                            title: p.title.clone(),
                                            dry_run: false,
                                            steps: reports,
                                            files_written,
                                            rolled_back: rollback_errors.is_empty(),
                                            error: Some(format!(
                                                "failed to read {}: {err}",
                                                path.display()
                                            )),
                                            rollback_errors,
                                        },
                                    )?);
                                }
                            };
                            snapshot_paths.insert(path.clone());
                            snapshots.push((path, original));
                        }
                    }

                    let apply_text = match apply(
                        &RefactorApplyParams {
                            plan: plan_value,
                            confirm: Some(true),
                            allow_dirty_worktree: Some(true),
                            allow_unregistered_paths: p.allow_unregistered_paths,
                        },
                        projects,
                    ) {
                        Ok(apply_text) => apply_text,
                        Err(err) => {
                            let rollback_errors = restore_snapshots(&snapshots);
                            return Ok(serde_json::to_string_pretty(&RefactorRunResponse {
                                status: "step_failed".to_string(),
                                title: p.title.clone(),
                                dry_run: false,
                                steps: reports,
                                files_written,
                                rolled_back: rollback_errors.is_empty(),
                                error: Some(err.to_string()),
                                rollback_errors,
                            })?);
                        }
                    };
                    let apply_response: RefactorApplyResponse =
                        match serde_json::from_str(&apply_text) {
                            Ok(response) => response,
                            Err(err) => {
                                let rollback_errors = restore_snapshots(&snapshots);
                                return Ok(serde_json::to_string_pretty(&RefactorRunResponse {
                                    status: "step_failed".to_string(),
                                    title: p.title.clone(),
                                    dry_run: false,
                                    steps: reports,
                                    files_written,
                                    rolled_back: rollback_errors.is_empty(),
                                    error: Some(err.to_string()),
                                    rollback_errors,
                                })?);
                            }
                        };
                    let status = apply_response.status.clone();
                    let validations = apply_response.validations.clone();
                    let step_written = apply_response.files_written.clone();
                    files_written.extend(step_written.iter().cloned());
                    reports.push(RefactorRunStepReport {
                        index: idx,
                        op: "plan".to_string(),
                        status: status.clone(),
                        kind: Some(refactor_plan.kind),
                        title: Some(refactor_plan.title),
                        files: step_written,
                        validations,
                        error: apply_response.error.clone(),
                    });
                    if status != "ok" {
                        let rollback_errors = restore_snapshots(&snapshots);
                        return Ok(serde_json::to_string_pretty(&RefactorRunResponse {
                            status: "step_failed".to_string(),
                            title: p.title.clone(),
                            dry_run: false,
                            steps: reports,
                            files_written,
                            rolled_back: rollback_errors.is_empty(),
                            error: apply_response.error,
                            rollback_errors,
                        })?);
                    }
                } else {
                    reports.push(RefactorRunStepReport {
                        index: idx,
                        op: "plan".to_string(),
                        status: "planned".to_string(),
                        kind: Some(refactor_plan.kind),
                        title: Some(refactor_plan.title),
                        files: step_files,
                        validations: Vec::new(),
                        error: None,
                    });
                }
            }
            RefactorRunStep::Command {
                command,
                args,
                cwd,
                touches,
                required,
            } => {
                if !confirmed {
                    reports.push(RefactorRunStepReport {
                        index: idx,
                        op: "command".to_string(),
                        status: "planned".to_string(),
                        kind: None,
                        title: Some(command_display(command, args)),
                        files: touches.clone(),
                        validations: Vec::new(),
                        error: None,
                    });
                    continue;
                }

                let touched_paths = touches
                    .iter()
                    .map(|path| resolve_path(Some(&path_string(&project_dir)), path))
                    .collect::<Result<Vec<_>>>()?;
                for path in &touched_paths {
                    if p.allow_unregistered_paths != Some(true) {
                        ensure_path_in_registered_project(path, projects)?;
                    }
                    if !snapshot_paths.contains(path) {
                        if p.allow_dirty_worktree != Some(true) {
                            ensure_git_clean_for_path(path)?;
                        }
                        let original = match fs::read(path) {
                            Ok(bytes) => Some(bytes),
                            Err(err) if err.kind() == std::io::ErrorKind::NotFound => None,
                            Err(err) => {
                                let rollback_errors = restore_snapshots(&snapshots);
                                return Ok(serde_json::to_string_pretty(&RefactorRunResponse {
                                    status: "step_failed".to_string(),
                                    title: p.title.clone(),
                                    dry_run: false,
                                    steps: reports,
                                    files_written,
                                    rolled_back: rollback_errors.is_empty(),
                                    error: Some(format!(
                                        "failed to read command touch {}: {err}",
                                        path.display()
                                    )),
                                    rollback_errors,
                                })?);
                            }
                        };
                        snapshot_paths.insert(path.clone());
                        snapshots.push((path.clone(), original));
                    }
                }
                let command_result = run_validation_command(&project_dir, command, args, cwd)?;
                let command_required = required.unwrap_or(true);
                files_written.extend(touched_paths.iter().map(|path| path_string(path)));
                reports.push(RefactorRunStepReport {
                    index: idx,
                    op: "command".to_string(),
                    status: if command_result.success {
                        "ok".to_string()
                    } else if command_required {
                        "failed".to_string()
                    } else {
                        "failed_optional".to_string()
                    },
                    kind: None,
                    title: Some(command_display(command, args)),
                    files: touched_paths.iter().map(|path| path_string(path)).collect(),
                    validations: Vec::new(),
                    error: command_result.error,
                });
                if !command_result.success && command_required {
                    let rollback_errors = restore_snapshots(&snapshots);
                    return Ok(serde_json::to_string_pretty(&RefactorRunResponse {
                        status: "step_failed".to_string(),
                        title: p.title.clone(),
                        dry_run: false,
                        steps: reports,
                        files_written,
                        rolled_back: rollback_errors.is_empty(),
                        error: Some(format!(
                            "command failed: {}",
                            command_display(command, args)
                        )),
                        rollback_errors,
                    })?);
                }
            }
        }
    }

    Ok(serde_json::to_string_pretty(&RefactorRunResponse {
        status: if confirmed { "ok" } else { "planned" }.to_string(),
        title: p.title.clone(),
        dry_run: !confirmed,
        steps: reports,
        files_written,
        rolled_back: false,
        error: None,
        rollback_errors: Vec::new(),
    })?)
}

fn append_report(
    mut reports: Vec<RefactorRunStepReport>,
    report: RefactorRunStepReport,
) -> Vec<RefactorRunStepReport> {
    reports.push(report);
    reports
}

struct CommandStepResult {
    success: bool,
    error: Option<String>,
}

fn run_validation_command(
    project_dir: &Path,
    command: &str,
    args: &[String],
    cwd: &Option<String>,
) -> Result<CommandStepResult> {
    let working_dir = match cwd.as_deref() {
        Some(cwd) => resolve_path(Some(&path_string(project_dir)), cwd)?,
        None => project_dir.to_path_buf(),
    };
    let mut cmd = Command::new(command);
    cmd.args(args).current_dir(&working_dir);
    for key in BLACKBOX_SERVICE_ENV_VARS {
        cmd.env_remove(key);
    }
    let output = cmd.output().with_context(|| {
        format!(
            "running validation command `{}` in {}",
            command_display(command, args),
            working_dir.display()
        )
    })?;
    if output.status.success() {
        return Ok(CommandStepResult {
            success: true,
            error: None,
        });
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    Ok(CommandStepResult {
        success: false,
        error: Some(format!(
            "exit status: {}\nstdout:\n{}\nstderr:\n{}",
            output.status,
            truncate_for_report(&stdout, 4000),
            truncate_for_report(&stderr, 4000)
        )),
    })
}

fn command_display(command: &str, args: &[String]) -> String {
    std::iter::once(command)
        .chain(args.iter().map(String::as_str))
        .collect::<Vec<_>>()
        .join(" ")
}

fn truncate_for_report(value: &str, max_chars: usize) -> String {
    let char_count = value.chars().count();
    if char_count <= max_chars {
        return value.to_string();
    }
    if max_chars < 32 {
        return value.chars().take(max_chars).collect();
    }
    let marker = "\n[truncated middle]\n";
    let budget = max_chars.saturating_sub(marker.chars().count());
    let head_len = budget / 2;
    let tail_len = budget - head_len;
    let head = value.chars().take(head_len).collect::<String>();
    let tail = value
        .chars()
        .skip(char_count.saturating_sub(tail_len))
        .collect::<String>();
    format!("{head}{marker}{tail}")
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
        file_moves: Vec::new(),
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
        file_moves: Vec::new(),
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

fn plan_delete_rust_items(p: &RefactorPlanParams) -> Result<String> {
    let source_path = resolve_path(p.project_dir.as_deref(), &p.source)?;
    let has_names = p
        .item_names
        .as_deref()
        .is_some_and(|names| !names.is_empty());
    if !has_names {
        bail!("delete_rust_items requires non-empty item_names; use item_kinds only to narrow deletion matches");
    }
    let wants_impl_methods = p
        .item_kinds
        .as_deref()
        .is_some_and(|kinds| kinds.iter().any(|kind| kind == "impl_method"));
    if wants_impl_methods {
        plan_delete_rust_impl_methods(p, &source_path)
    } else {
        plan_delete_rust_top_level_items(p, &source_path)
    }
}

fn plan_delete_rust_top_level_items(p: &RefactorPlanParams, source_path: &Path) -> Result<String> {
    if let Some(kinds) = p.item_kinds.as_deref() {
        if kinds.iter().any(|kind| kind == "impl_method") {
            bail!("delete_rust_items cannot mix impl_method with top-level item kinds");
        }
    }
    let parsed = parse_rust_file(source_path)?;
    let items = rust_items(&parsed);
    let selected = select_items(&items, p.item_names.as_deref(), p.item_kinds.as_deref())?;
    build_delete_rust_plan(&parsed, "top-level Rust item(s)", &items, selected)
}

fn plan_delete_rust_impl_methods(p: &RefactorPlanParams, source_path: &Path) -> Result<String> {
    if let Some(kinds) = p.item_kinds.as_deref() {
        if !kinds.iter().all(|kind| kind == "impl_method") {
            bail!("delete_rust_items cannot mix impl_method with top-level item kinds");
        }
    }
    let parsed = parse_rust_file(source_path)?;
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

    let items = candidates
        .iter()
        .map(|method| method.item.clone())
        .collect::<Vec<_>>();
    let selected = select_items(&items, p.item_names.as_deref(), p.item_kinds.as_deref())?;
    if p.impl_name.is_none() {
        if let Some(names) = p.item_names.as_deref() {
            for name in names {
                let matches = selected
                    .iter()
                    .filter(|item| item.name.as_deref() == Some(name.as_str()))
                    .count();
                if matches > 1 {
                    bail!(
                        "requested impl method `{name}` matched multiple impl blocks; pass impl_name"
                    );
                }
            }
        }
    }
    build_delete_rust_plan(&parsed, "Rust impl method(s)", &items, selected)
}

fn build_delete_rust_plan(
    parsed: &ParsedSource,
    label: &str,
    all_items: &[SyntaxItem],
    selected: Vec<&SyntaxItem>,
) -> Result<String> {
    let selected_ids: HashSet<_> = selected
        .iter()
        .map(|item| item.plan_local_id.clone())
        .collect();
    let leftovers = all_items
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
    let source_edits = selected
        .iter()
        .map(|item| TextEdit {
            byte_start: item.leading_trivia_start,
            byte_end: item.trailing_trivia_end,
            replacement: String::new(),
        })
        .collect::<Vec<_>>();
    ensure_non_overlapping(&source_edits)?;

    let plan = RefactorPlan {
        title: format!(
            "delete {} {} from {}",
            selected.len(),
            label,
            path_string(&parsed.path)
        ),
        kind: "delete_rust_items".to_string(),
        semantic_status: SemanticStatus::StructuralOnly,
        dry_run: true,
        file_moves: Vec::new(),
        edits: vec![FileEdit {
            path: path_string(&parsed.path),
            original_sha256: sha256_hex(parsed.source.as_bytes()),
            edits: source_edits,
        }],
        validations: vec![ValidationStep::TreeSitterNoErrors {
            path: path_string(&parsed.path),
            byte_range: None,
        }],
        items: selected.into_iter().cloned().collect(),
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
        file_moves: Vec::new(),
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

fn plan_add_rust_mod_decl(p: &RefactorPlanParams) -> Result<String> {
    let source_path = resolve_path(p.project_dir.as_deref(), &p.source)?;
    let module_name = p
        .module_name
        .as_deref()
        .or_else(|| {
            p.item_names
                .as_deref()
                .and_then(|names| names.first().map(String::as_str))
        })
        .ok_or_else(|| anyhow!("module_name is required for add_rust_mod_decl"))?;
    validate_rust_identifier(module_name, "module_name")?;
    let visibility = rust_decl_visibility_prefix(p.visibility.as_deref())?;
    let declaration = format!("{visibility}mod {module_name};");

    let parsed = parse_rust_file(&source_path)?;
    let items = rust_items(&parsed);
    if items
        .iter()
        .any(|item| item.kind == "mod_item" && item.name.as_deref() == Some(module_name))
    {
        bail!("module declaration `{module_name}` already exists");
    }

    let last_mod = items
        .iter()
        .filter(|item| item.kind == "mod_item")
        .max_by_key(|item| item.byte_end);
    let (insert_at, replacement) = if let Some(item) = last_mod {
        (item.byte_end, format!("\n{declaration}"))
    } else {
        (
            rust_module_decl_fallback_insert_byte(&parsed.source),
            format!("{declaration}\n"),
        )
    };
    let plan = RefactorPlan {
        title: format!(
            "add Rust module declaration {declaration} to {}",
            path_string(&source_path)
        ),
        kind: "add_rust_mod_decl".to_string(),
        semantic_status: SemanticStatus::StructuralOnly,
        dry_run: true,
        file_moves: Vec::new(),
        edits: vec![FileEdit {
            path: path_string(&source_path),
            original_sha256: sha256_hex(parsed.source.as_bytes()),
            edits: vec![TextEdit {
                byte_start: insert_at,
                byte_end: insert_at,
                replacement,
            }],
        }],
        validations: vec![ValidationStep::TreeSitterNoErrors {
            path: path_string(&source_path),
            byte_range: None,
        }],
        items: Vec::new(),
        leftovers: Vec::new(),
    };

    validate_plan_shape(&plan)?;
    Ok(serde_json::to_string_pretty(&plan)?)
}

fn plan_add_rust_use_decl(p: &RefactorPlanParams) -> Result<String> {
    let source_path = resolve_path(p.project_dir.as_deref(), &p.source)?;
    let use_path = p
        .use_path
        .as_deref()
        .ok_or_else(|| anyhow!("use_path is required for add_rust_use_decl"))?;
    validate_rust_use_path(use_path)?;
    let visibility = rust_decl_visibility_prefix(p.visibility.as_deref())?;
    let declaration = format!("{visibility}use {use_path};");

    let parsed = parse_rust_file(&source_path)?;
    if parsed.source.lines().any(|line| line.trim() == declaration) {
        bail!("use declaration `{declaration}` already exists");
    }
    let items = rust_items(&parsed);
    let insert_at = items
        .iter()
        .filter(|item| item.kind == "use_declaration")
        .max_by_key(|item| item.byte_end)
        .map(|item| item.byte_end)
        .or_else(|| {
            items
                .iter()
                .filter(|item| item.kind == "mod_item")
                .max_by_key(|item| item.byte_end)
                .map(|item| item.trailing_trivia_end)
        })
        .unwrap_or_else(|| rust_module_decl_fallback_insert_byte(&parsed.source));
    let replacement = if parsed.source[insert_at..].starts_with('\n') {
        format!("\n{declaration}")
    } else if insert_at == parsed.source.len() || parsed.source[..insert_at].ends_with('\n') {
        format!("{declaration}\n")
    } else {
        format!("\n{declaration}\n")
    };
    let plan = RefactorPlan {
        title: format!(
            "add Rust use declaration {declaration} to {}",
            path_string(&source_path)
        ),
        kind: "add_rust_use_decl".to_string(),
        semantic_status: SemanticStatus::StructuralOnly,
        dry_run: true,
        file_moves: Vec::new(),
        edits: vec![FileEdit {
            path: path_string(&source_path),
            original_sha256: sha256_hex(parsed.source.as_bytes()),
            edits: vec![TextEdit {
                byte_start: insert_at,
                byte_end: insert_at,
                replacement,
            }],
        }],
        validations: vec![ValidationStep::TreeSitterNoErrors {
            path: path_string(&source_path),
            byte_range: None,
        }],
        items: Vec::new(),
        leftovers: Vec::new(),
    };

    validate_plan_shape(&plan)?;
    Ok(serde_json::to_string_pretty(&plan)?)
}

fn plan_copy_rust_mod_decls(p: &RefactorPlanParams) -> Result<String> {
    let source_path = resolve_path(p.project_dir.as_deref(), &p.source)?;
    let target_path = p
        .target
        .as_deref()
        .ok_or_else(|| anyhow!("target is required for copy_rust_mod_decls"))
        .and_then(|target| resolve_path(p.project_dir.as_deref(), target))?;
    if source_path == target_path {
        bail!("source and target must be different files");
    }
    let parsed = parse_rust_file(&source_path)?;
    let source_items = rust_items(&parsed);
    let mod_items = source_items
        .iter()
        .filter(|item| item.kind == "mod_item")
        .cloned()
        .collect::<Vec<_>>();
    let mod_kind = vec!["mod_item".to_string()];
    let selected = select_items(&mod_items, p.item_names.as_deref(), Some(&mod_kind))?;
    let mut declarations = Vec::new();
    let visibility = rust_decl_visibility_prefix(p.visibility.as_deref())?;
    for item in &selected {
        ensure_rust_mod_declaration(&parsed.source, item)?;
        let name = item
            .name
            .as_deref()
            .ok_or_else(|| anyhow!("selected mod_item has no module name"))?;
        declarations.push(format!("{visibility}mod {name};"));
    }

    let target_source = fs::read_to_string(&target_path).unwrap_or_default();
    let existing = rust_existing_mod_decl_names(&target_path, &target_source)?;
    let declarations = declarations
        .into_iter()
        .filter(|declaration| {
            let name = declaration
                .trim_end_matches(';')
                .split_whitespace()
                .last()
                .unwrap_or_default();
            !existing.contains(name)
        })
        .collect::<Vec<_>>();
    if declarations.is_empty() {
        bail!("all selected module declarations already exist in target");
    }

    let insert_at = rust_mod_decl_insert_byte(&target_path, &target_source)?;
    let replacement = rust_decl_batch_insert_text(&target_source, insert_at, &declarations);
    let plan = RefactorPlan {
        title: format!(
            "copy {} Rust module declaration(s) from {} to {}",
            declarations.len(),
            path_string(&source_path),
            path_string(&target_path)
        ),
        kind: "copy_rust_mod_decls".to_string(),
        semantic_status: SemanticStatus::StructuralOnly,
        dry_run: true,
        file_moves: Vec::new(),
        edits: vec![FileEdit {
            path: path_string(&target_path),
            original_sha256: sha256_hex(target_source.as_bytes()),
            edits: vec![TextEdit {
                byte_start: insert_at,
                byte_end: insert_at,
                replacement,
            }],
        }],
        validations: vec![ValidationStep::TreeSitterNoErrors {
            path: path_string(&target_path),
            byte_range: None,
        }],
        items: selected.into_iter().cloned().collect(),
        leftovers: Vec::new(),
    };

    validate_plan_shape(&plan)?;
    Ok(serde_json::to_string_pretty(&plan)?)
}

fn plan_rewrite_rust_mod_visibility(p: &RefactorPlanParams) -> Result<String> {
    let source_path = resolve_path(p.project_dir.as_deref(), &p.source)?;
    let module_name = p
        .module_name
        .as_deref()
        .or_else(|| {
            p.item_names
                .as_deref()
                .and_then(|names| names.first().map(String::as_str))
        })
        .ok_or_else(|| {
            anyhow!("module_name or item_names[0] is required for rewrite_rust_mod_visibility")
        })?;
    validate_rust_identifier(module_name, "module_name")?;
    let visibility = rust_decl_visibility_prefix(p.visibility.as_deref())?;

    let parsed = parse_rust_file(&source_path)?;
    let items = rust_items(&parsed);
    let selected = items
        .iter()
        .filter(|item| item.kind == "mod_item" && item.name.as_deref() == Some(module_name))
        .collect::<Vec<_>>();
    if selected.is_empty() {
        bail!("module declaration `{module_name}` was not found");
    }
    if selected.len() > 1 {
        bail!("module declaration `{module_name}` matched multiple items");
    }
    let item = selected[0];
    ensure_rust_mod_declaration(&parsed.source, item)?;
    let mod_keyword = rust_mod_keyword_byte(&parsed.source, item)?;
    let visibility_start = rust_mod_visibility_start_byte(&parsed.source, mod_keyword);
    let current_prefix = &parsed.source[visibility_start..mod_keyword];
    if current_prefix == visibility {
        bail!("module declaration `{module_name}` already has requested visibility");
    }
    let plan = RefactorPlan {
        title: format!(
            "rewrite Rust module declaration {module_name} visibility in {}",
            path_string(&source_path)
        ),
        kind: "rewrite_rust_mod_visibility".to_string(),
        semantic_status: SemanticStatus::StructuralOnly,
        dry_run: true,
        file_moves: Vec::new(),
        edits: vec![FileEdit {
            path: path_string(&source_path),
            original_sha256: sha256_hex(parsed.source.as_bytes()),
            edits: vec![TextEdit {
                byte_start: visibility_start,
                byte_end: mod_keyword,
                replacement: visibility.to_string(),
            }],
        }],
        validations: vec![ValidationStep::TreeSitterNoErrors {
            path: path_string(&source_path),
            byte_range: None,
        }],
        items: vec![item.clone()],
        leftovers: Vec::new(),
    };

    validate_plan_shape(&plan)?;
    Ok(serde_json::to_string_pretty(&plan)?)
}

fn plan_move_file(p: &RefactorPlanParams) -> Result<String> {
    let source_path = resolve_path(p.project_dir.as_deref(), &p.source)?;
    let target_path = p
        .target
        .as_deref()
        .ok_or_else(|| anyhow!("target is required for move_file"))
        .and_then(|target| resolve_path(p.project_dir.as_deref(), target))?;
    if source_path == target_path {
        bail!("source and target must be different files");
    }
    if !source_path.is_file() {
        bail!("source file does not exist: {}", source_path.display());
    }
    if target_path.exists() {
        bail!("target already exists: {}", target_path.display());
    }
    let original = fs::read(&source_path)
        .with_context(|| format!("failed to read {}", source_path.display()))?;
    let validations = parse_validation_step_for_path(&target_path);
    let plan = RefactorPlan {
        title: format!(
            "move file {} to {}",
            path_string(&source_path),
            path_string(&target_path)
        ),
        kind: "move_file".to_string(),
        semantic_status: SemanticStatus::StructuralOnly,
        dry_run: true,
        file_moves: vec![FileMove {
            source_path: path_string(&source_path),
            target_path: path_string(&target_path),
            original_sha256: sha256_hex(&original),
        }],
        edits: Vec::new(),
        validations,
        items: Vec::new(),
        leftovers: Vec::new(),
    };

    validate_plan_shape(&plan)?;
    Ok(serde_json::to_string_pretty(&plan)?)
}

fn plan_rust_lsp_rename(p: &RefactorPlanParams) -> Result<String> {
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
        .ok_or_else(|| anyhow!("item_names[0] or old_text is required for rust_lsp_rename"))?;
    validate_rust_identifier(old_name, "item_names[0]")?;
    let new_name = p
        .new_text
        .as_deref()
        .ok_or_else(|| anyhow!("new_text is required for rust_lsp_rename"))?;
    validate_rust_identifier(new_name, "new_text")?;
    if old_name == new_name {
        bail!("rust_lsp_rename requires different old and new names");
    }
    let parsed = parse_rust_file(&source_path)?;
    let position_byte = rust_rename_position_byte(&parsed, old_name)?;
    let position = byte_to_lsp_position(&parsed.source, position_byte);
    let lsp_edits = rust_analyzer_rename(&project_dir, &source_path, position, new_name)?;
    if lsp_edits.is_empty() {
        bail!("rust-analyzer returned no edits for rename `{old_name}`");
    }
    let file_edits = lsp_edits_to_file_edits(lsp_edits)?;
    let validations = file_edits
        .iter()
        .flat_map(|edit| parse_validation_step_for_path(Path::new(&edit.path)))
        .collect::<Vec<_>>();
    let plan = RefactorPlan {
        title: format!("rust-analyzer rename {old_name} to {new_name}"),
        kind: "rust_lsp_rename".to_string(),
        semantic_status: SemanticStatus::LspVerified,
        dry_run: true,
        file_moves: Vec::new(),
        edits: file_edits,
        validations,
        items: Vec::new(),
        leftovers: Vec::new(),
    };

    validate_plan_shape(&plan)?;
    Ok(serde_json::to_string_pretty(&plan)?)
}

fn plan_rust_organize_imports(p: &RefactorPlanParams) -> Result<String> {
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
    parse_rust_file(&source_path)?;
    let lsp_edits = rust_analyzer_organize_imports(&project_dir, &source_path)?;
    if lsp_edits.is_empty() {
        bail!("rust-analyzer returned no import organization edits");
    }
    let file_edits = lsp_edits_to_file_edits(lsp_edits)?;
    let validations = file_edits
        .iter()
        .flat_map(|edit| parse_validation_step_for_path(Path::new(&edit.path)))
        .collect::<Vec<_>>();
    let plan = RefactorPlan {
        title: format!(
            "rust-analyzer organize imports in {}",
            path_string(&source_path)
        ),
        kind: "rust_organize_imports".to_string(),
        semantic_status: SemanticStatus::LspVerified,
        dry_run: true,
        file_moves: Vec::new(),
        edits: file_edits,
        validations,
        items: Vec::new(),
        leftovers: Vec::new(),
    };

    validate_plan_shape(&plan)?;
    Ok(serde_json::to_string_pretty(&plan)?)
}

fn plan_replace_text(p: &RefactorPlanParams) -> Result<String> {
    let source_path = resolve_path(p.project_dir.as_deref(), &p.source)?;
    let source = fs::read_to_string(&source_path)
        .with_context(|| format!("failed to read {}", source_path.display()))?;
    let old_text = p
        .old_text
        .as_deref()
        .ok_or_else(|| anyhow!("old_text is required for replace_text"))?;
    if old_text.is_empty() {
        bail!("old_text must not be empty");
    }
    let new_text = p
        .new_text
        .as_deref()
        .ok_or_else(|| anyhow!("new_text is required for replace_text"))?;
    let matches = source.match_indices(old_text).collect::<Vec<_>>();
    if matches.is_empty() {
        bail!("old_text was not found in {}", source_path.display());
    }
    if p.replace_all != Some(true) && matches.len() > 1 {
        bail!(
            "old_text matched {} times in {}; pass replace_all=true or use a more specific old_text",
            matches.len(),
            source_path.display()
        );
    }
    let edits = if p.replace_all == Some(true) {
        matches
            .iter()
            .map(|(start, text)| TextEdit {
                byte_start: *start,
                byte_end: start + text.len(),
                replacement: new_text.to_string(),
            })
            .collect::<Vec<_>>()
    } else {
        let (start, text) = matches[0];
        vec![TextEdit {
            byte_start: start,
            byte_end: start + text.len(),
            replacement: new_text.to_string(),
        }]
    };
    let plan = RefactorPlan {
        title: format!(
            "replace exact text in {} ({} occurrence(s))",
            path_string(&source_path),
            edits.len()
        ),
        kind: "replace_text".to_string(),
        semantic_status: SemanticStatus::StructuralOnly,
        dry_run: true,
        file_moves: Vec::new(),
        edits: vec![FileEdit {
            path: path_string(&source_path),
            original_sha256: sha256_hex(source.as_bytes()),
            edits,
        }],
        validations: parse_validation_step_for_path(&source_path),
        items: Vec::new(),
        leftovers: Vec::new(),
    };

    validate_plan_shape(&plan)?;
    Ok(serde_json::to_string_pretty(&plan)?)
}

fn plan_write_file(p: &RefactorPlanParams) -> Result<String> {
    let source_path = resolve_path(p.project_dir.as_deref(), &p.source)?;
    let source = fs::read_to_string(&source_path).unwrap_or_default();
    let new_text = p
        .new_text
        .as_deref()
        .ok_or_else(|| anyhow!("new_text is required for write_file"))?;
    let plan = RefactorPlan {
        title: format!("write complete file {}", path_string(&source_path)),
        kind: "write_file".to_string(),
        semantic_status: SemanticStatus::StructuralOnly,
        dry_run: true,
        file_moves: Vec::new(),
        edits: vec![FileEdit {
            path: path_string(&source_path),
            original_sha256: sha256_hex(source.as_bytes()),
            edits: vec![TextEdit {
                byte_start: 0,
                byte_end: source.len(),
                replacement: new_text.to_string(),
            }],
        }],
        validations: parse_validation_step_for_path(&source_path),
        items: Vec::new(),
        leftovers: Vec::new(),
    };

    validate_plan_shape(&plan)?;
    Ok(serde_json::to_string_pretty(&plan)?)
}

fn plan_ensure_toml_table(p: &RefactorPlanParams) -> Result<String> {
    let source_path = resolve_path(p.project_dir.as_deref(), &p.source)?;
    let source = fs::read_to_string(&source_path)
        .with_context(|| format!("failed to read {}", source_path.display()))?;
    let table = p
        .toml_table
        .as_deref()
        .ok_or_else(|| anyhow!("toml_table is required for ensure_toml_table"))?;
    validate_toml_table_name(table)?;
    let entries = p
        .toml_entries
        .as_ref()
        .filter(|entries| !entries.is_empty())
        .ok_or_else(|| anyhow!("toml_entries is required for ensure_toml_table"))?;
    let replacement = ensure_toml_table_content(&source, table, entries)?;
    replacement
        .parse::<toml::Value>()
        .with_context(|| format!("planned TOML for {} is invalid", source_path.display()))?;
    let plan = RefactorPlan {
        title: format!(
            "ensure TOML table [{table}] in {}",
            path_string(&source_path)
        ),
        kind: "ensure_toml_table".to_string(),
        semantic_status: SemanticStatus::StructuralOnly,
        dry_run: true,
        file_moves: Vec::new(),
        edits: vec![FileEdit {
            path: path_string(&source_path),
            original_sha256: sha256_hex(source.as_bytes()),
            edits: vec![TextEdit {
                byte_start: 0,
                byte_end: source.len(),
                replacement,
            }],
        }],
        validations: Vec::new(),
        items: Vec::new(),
        leftovers: Vec::new(),
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

fn rust_module_decl_fallback_insert_byte(source: &str) -> usize {
    rust_prelude_insert_byte(source)
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

fn rust_decl_visibility_prefix(visibility: Option<&str>) -> Result<&'static str> {
    match visibility.unwrap_or("").trim() {
        "" | "private" => Ok(""),
        "pub" => Ok("pub "),
        "pub(crate)" => Ok("pub(crate) "),
        other => bail!("unsupported Rust visibility `{other}`; supported: pub, pub(crate)"),
    }
}

fn rust_mod_keyword_byte(source: &str, item: &SyntaxItem) -> Result<usize> {
    let text = source
        .get(item.byte_start..item.byte_end)
        .ok_or_else(|| anyhow!("invalid mod_item range for {}", item.plan_local_id))?;
    let name = item
        .name
        .as_deref()
        .ok_or_else(|| anyhow!("selected mod_item has no module name"))?;
    for (idx, _) in text.match_indices("mod") {
        let before = idx
            .checked_sub(1)
            .and_then(|pos| text.as_bytes().get(pos))
            .copied();
        let after = text.as_bytes().get(idx + 3).copied();
        let before_boundary = before.is_none_or(|byte| !rust_ident_byte(byte));
        let after_boundary = after.is_none_or(|byte| !rust_ident_byte(byte));
        if !before_boundary || !after_boundary {
            continue;
        }
        let rest = &text[idx + 3..];
        let trimmed = rest.trim_start();
        if trimmed
            .strip_prefix(name)
            .and_then(|suffix| suffix.as_bytes().first().copied())
            .is_some_and(|byte| rust_ident_byte(byte))
        {
            continue;
        }
        if trimmed.starts_with(name) {
            return Ok(item.byte_start + idx);
        }
    }
    bail!("could not locate `mod` keyword for {}", item.plan_local_id)
}

fn rust_mod_visibility_start_byte(source: &str, mod_keyword: usize) -> usize {
    let line_start = line_start_before(source, mod_keyword);
    let leading = source[line_start..mod_keyword]
        .bytes()
        .take_while(|byte| byte.is_ascii_whitespace())
        .count();
    line_start + leading
}

fn rust_ident_byte(byte: u8) -> bool {
    byte == b'_' || byte.is_ascii_alphanumeric()
}

fn ensure_rust_mod_declaration(source: &str, item: &SyntaxItem) -> Result<()> {
    let text = source
        .get(item.byte_start..item.byte_end)
        .ok_or_else(|| anyhow!("invalid mod_item range for {}", item.plan_local_id))?;
    let semicolon = text.find(';');
    let brace = text.find('{');
    match (semicolon, brace) {
        (Some(_), None) => Ok(()),
        (Some(semi), Some(brace)) if semi < brace => Ok(()),
        _ => bail!(
            "module `{}` is inline; only `mod name;` declarations are supported",
            item.name.as_deref().unwrap_or("(unnamed)")
        ),
    }
}

fn rust_existing_mod_decl_names(path: &Path, source: &str) -> Result<HashSet<String>> {
    if source.trim().is_empty() {
        return Ok(HashSet::new());
    }
    let tree = parse_source("rust", source)?;
    let parsed = ParsedSource {
        path: path.to_path_buf(),
        language: "rust",
        source: source.to_string(),
        tree,
    };
    let names = rust_items(&parsed)
        .into_iter()
        .filter(|item| item.kind == "mod_item")
        .filter_map(|item| {
            ensure_rust_mod_declaration(source, &item)
                .ok()
                .and_then(|_| item.name)
        })
        .collect::<HashSet<_>>();
    Ok(names)
}

fn rust_mod_decl_insert_byte(path: &Path, source: &str) -> Result<usize> {
    if source.trim().is_empty() {
        return Ok(0);
    }
    let tree = parse_source("rust", source)?;
    let parsed = ParsedSource {
        path: path.to_path_buf(),
        language: "rust",
        source: source.to_string(),
        tree,
    };
    Ok(rust_items(&parsed)
        .iter()
        .filter(|item| item.kind == "mod_item")
        .max_by_key(|item| item.byte_end)
        .map(|item| item.byte_end)
        .unwrap_or_else(|| rust_module_decl_fallback_insert_byte(source)))
}

fn rust_decl_batch_insert_text(source: &str, insert_at: usize, declarations: &[String]) -> String {
    let mut text = declarations.join("\n");
    if source.trim().is_empty() || insert_at == 0 {
        text.push('\n');
        text
    } else if source[..insert_at].ends_with('\n') {
        if !source[insert_at..].starts_with('\n') {
            text.push('\n');
        }
        text
    } else {
        format!("\n{text}")
    }
}

fn validate_rust_use_path(value: &str) -> Result<()> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        bail!("use_path must not be empty");
    }
    if trimmed != value {
        bail!("use_path must not have leading or trailing whitespace");
    }
    if value.contains('\n') || value.contains('\r') || value.contains(';') {
        bail!("use_path must be a single Rust use path without a trailing semicolon");
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

#[derive(Debug, Clone)]
struct LspTextEdit {
    path: PathBuf,
    start_line: u64,
    start_character: u64,
    end_line: u64,
    end_character: u64,
    new_text: String,
}

fn rust_rename_position_byte(parsed: &ParsedSource, old_name: &str) -> Result<usize> {
    let mut candidates = rust_status_items(parsed)
        .into_iter()
        .filter(|item| item.name.as_deref() == Some(old_name))
        .collect::<Vec<_>>();
    if candidates.is_empty() {
        bail!(
            "no Rust item named `{old_name}` found in {}",
            parsed.path.display()
        );
    }
    candidates.sort_by_key(|item| item.byte_start);
    let item = &candidates[0];
    let item_source = parsed
        .source
        .get(item.byte_start..item.byte_end)
        .ok_or_else(|| anyhow!("invalid item range for `{old_name}`"))?;
    let relative = item_source
        .find(old_name)
        .ok_or_else(|| anyhow!("could not find `{old_name}` text inside selected item"))?;
    Ok(item.byte_start + relative + old_name.len().saturating_sub(1) / 2)
}

fn byte_to_lsp_position(source: &str, byte: usize) -> serde_json::Value {
    let line = source[..byte].bytes().filter(|b| *b == b'\n').count() as u64;
    let line_start = line_start_before(source, byte);
    let character = source[line_start..byte].encode_utf16().count() as u64;
    serde_json::json!({ "line": line, "character": character })
}

fn lsp_position_to_byte(source: &str, line: u64, character: u64) -> Result<usize> {
    let mut current_line = 0u64;
    let mut line_start = 0usize;
    for (idx, byte) in source.bytes().enumerate() {
        if current_line == line {
            break;
        }
        if byte == b'\n' {
            current_line += 1;
            line_start = idx + 1;
        }
    }
    if current_line != line {
        bail!("line {line} is outside source");
    }
    let line_end = source[line_start..]
        .find('\n')
        .map(|offset| line_start + offset)
        .unwrap_or(source.len());
    let mut utf16 = 0u64;
    for (offset, ch) in source[line_start..line_end].char_indices() {
        if utf16 == character {
            return Ok(line_start + offset);
        }
        utf16 += ch.len_utf16() as u64;
        if utf16 > character {
            bail!("character {character} is not on a UTF-16 boundary");
        }
    }
    if utf16 == character {
        return Ok(line_end);
    }
    bail!("character {character} is outside line {line}");
}

fn lsp_edits_to_file_edits(lsp_edits: Vec<LspTextEdit>) -> Result<Vec<FileEdit>> {
    let mut grouped: BTreeMap<PathBuf, Vec<LspTextEdit>> = BTreeMap::new();
    for edit in lsp_edits {
        grouped.entry(edit.path.clone()).or_default().push(edit);
    }
    let mut file_edits = Vec::new();
    for (path, edits) in grouped {
        let source = fs::read_to_string(&path)
            .with_context(|| format!("failed to read LSP edit target {}", path.display()))?;
        let mut text_edits = Vec::new();
        for edit in edits {
            let byte_start =
                lsp_position_to_byte(&source, edit.start_line, edit.start_character)
                    .with_context(|| format!("invalid LSP start range for {}", path.display()))?;
            let byte_end = lsp_position_to_byte(&source, edit.end_line, edit.end_character)
                .with_context(|| format!("invalid LSP end range for {}", path.display()))?;
            text_edits.push(TextEdit {
                byte_start,
                byte_end,
                replacement: edit.new_text,
            });
        }
        file_edits.push(FileEdit {
            path: path_string(&path),
            original_sha256: sha256_hex(source.as_bytes()),
            edits: text_edits,
        });
    }
    Ok(file_edits)
}

fn rust_analyzer_rename(
    project_dir: &Path,
    source_path: &Path,
    position: serde_json::Value,
    new_name: &str,
) -> Result<Vec<LspTextEdit>> {
    let mut child = Command::new("rust-analyzer")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .context("spawning rust-analyzer")?;
    let mut stdin = child.stdin.take().context("rust-analyzer stdin")?;
    let stdout = child.stdout.take().context("rust-analyzer stdout")?;
    let mut reader = std::io::BufReader::new(stdout);
    let root_uri = Url::from_directory_path(project_dir)
        .map_err(|_| anyhow!("failed to convert {} to file URL", project_dir.display()))?
        .to_string();
    let source_uri = Url::from_file_path(source_path)
        .map_err(|_| anyhow!("failed to convert {} to file URL", source_path.display()))?
        .to_string();
    send_lsp(
        &mut stdin,
        &serde_json::json!({
            "jsonrpc":"2.0",
            "id":1,
            "method":"initialize",
            "params":{
                "processId": std::process::id(),
                "rootUri": root_uri,
                "rootPath": project_dir.to_string_lossy(),
                "workspaceFolders": [{"uri": root_uri, "name": "refactor-root"}],
                "capabilities": {
                    "workspace": {
                        "applyEdit": false,
                        "workspaceEdit": {
                            "documentChanges": true,
                            "resourceOperations": ["create", "rename", "delete"]
                        }
                    }
                }
            }
        }),
    )?;
    read_lsp_response(&mut reader, 1)?;
    send_lsp(
        &mut stdin,
        &serde_json::json!({"jsonrpc":"2.0","method":"initialized","params":{}}),
    )?;
    std::thread::sleep(std::time::Duration::from_millis(2000));
    send_lsp(
        &mut stdin,
        &serde_json::json!({
            "jsonrpc":"2.0",
            "id":2,
            "method":"textDocument/rename",
            "params":{
                "textDocument":{"uri":source_uri},
                "position": position,
                "newName": new_name
            }
        }),
    )?;
    let response = read_lsp_response(&mut reader, 2)?;
    let _ = send_lsp(
        &mut stdin,
        &serde_json::json!({"jsonrpc":"2.0","id":3,"method":"shutdown"}),
    );
    let _ = send_lsp(
        &mut stdin,
        &serde_json::json!({"jsonrpc":"2.0","method":"exit"}),
    );
    let _ = child.wait();
    workspace_edit_to_text_edits(&response["result"])
}

fn rust_analyzer_organize_imports(
    project_dir: &Path,
    source_path: &Path,
) -> Result<Vec<LspTextEdit>> {
    let mut child = Command::new("rust-analyzer")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .context("spawning rust-analyzer")?;
    let mut stdin = child.stdin.take().context("rust-analyzer stdin")?;
    let stdout = child.stdout.take().context("rust-analyzer stdout")?;
    let mut reader = std::io::BufReader::new(stdout);
    let root_uri = Url::from_directory_path(project_dir)
        .map_err(|_| anyhow!("failed to convert {} to file URL", project_dir.display()))?
        .to_string();
    let source_uri = Url::from_file_path(source_path)
        .map_err(|_| anyhow!("failed to convert {} to file URL", source_path.display()))?
        .to_string();
    send_lsp(
        &mut stdin,
        &serde_json::json!({
            "jsonrpc":"2.0",
            "id":1,
            "method":"initialize",
            "params":{
                "processId": std::process::id(),
                "rootUri": root_uri,
                "rootPath": project_dir.to_string_lossy(),
                "workspaceFolders": [{"uri": root_uri, "name": "refactor-root"}],
                "capabilities": {
                    "textDocument": {
                        "codeAction": {
                            "codeActionLiteralSupport": {
                                "codeActionKind": {"valueSet": ["source.organizeImports"]}
                            }
                        }
                    },
                    "workspace": {
                        "workspaceEdit": {"documentChanges": true}
                    }
                }
            }
        }),
    )?;
    read_lsp_response(&mut reader, 1)?;
    send_lsp(
        &mut stdin,
        &serde_json::json!({"jsonrpc":"2.0","method":"initialized","params":{}}),
    )?;
    let source = fs::read_to_string(source_path)
        .with_context(|| format!("reading {}", source_path.display()))?;
    let end_position = byte_to_lsp_position(&source, source.len());
    std::thread::sleep(std::time::Duration::from_millis(2000));
    send_lsp(
        &mut stdin,
        &serde_json::json!({
            "jsonrpc":"2.0",
            "id":2,
            "method":"textDocument/codeAction",
            "params":{
                "textDocument":{"uri":source_uri},
                "range":{"start":{"line":0,"character":0},"end":end_position},
                "context":{"diagnostics":[],"only":["source.organizeImports"]}
            }
        }),
    )?;
    let response = read_lsp_response(&mut reader, 2)?;
    let _ = send_lsp(
        &mut stdin,
        &serde_json::json!({"jsonrpc":"2.0","id":3,"method":"shutdown"}),
    );
    let _ = send_lsp(
        &mut stdin,
        &serde_json::json!({"jsonrpc":"2.0","method":"exit"}),
    );
    let _ = child.wait();
    code_actions_to_text_edits(&response["result"])
}

fn send_lsp(stdin: &mut impl Write, value: &serde_json::Value) -> Result<()> {
    let body = serde_json::to_vec(value)?;
    write!(stdin, "Content-Length: {}\r\n\r\n", body.len())?;
    stdin.write_all(&body)?;
    stdin.flush()?;
    Ok(())
}

fn read_lsp_response(reader: &mut impl BufRead, expected_id: u64) -> Result<serde_json::Value> {
    loop {
        let value = read_lsp_message(reader)?;
        if value.get("id").and_then(serde_json::Value::as_u64) != Some(expected_id) {
            continue;
        }
        if let Some(error) = value.get("error") {
            bail!("rust-analyzer returned error: {error}");
        }
        return Ok(value);
    }
}

fn read_lsp_message(reader: &mut impl BufRead) -> Result<serde_json::Value> {
    let mut content_length = None;
    loop {
        let mut line = String::new();
        let bytes = reader.read_line(&mut line)?;
        if bytes == 0 {
            bail!("rust-analyzer closed stdout");
        }
        let trimmed = line.trim_end_matches(['\r', '\n']);
        if trimmed.is_empty() {
            break;
        }
        if let Some(value) = trimmed.strip_prefix("Content-Length:") {
            content_length = Some(value.trim().parse::<usize>()?);
        }
    }
    let len = content_length.context("LSP message missing Content-Length")?;
    let mut body = vec![0u8; len];
    reader.read_exact(&mut body)?;
    Ok(serde_json::from_slice(&body)?)
}

fn workspace_edit_to_text_edits(value: &serde_json::Value) -> Result<Vec<LspTextEdit>> {
    let mut edits = Vec::new();
    if let Some(changes) = value.get("changes").and_then(serde_json::Value::as_object) {
        for (uri, uri_edits) in changes {
            collect_lsp_text_edits(uri, uri_edits, &mut edits)?;
        }
    }
    if let Some(document_changes) = value
        .get("documentChanges")
        .and_then(serde_json::Value::as_array)
    {
        for change in document_changes {
            if let Some(uri) = change
                .get("textDocument")
                .and_then(|td| td.get("uri"))
                .and_then(serde_json::Value::as_str)
            {
                collect_lsp_text_edits(uri, &change["edits"], &mut edits)?;
            }
        }
    }
    Ok(edits)
}

fn code_actions_to_text_edits(value: &serde_json::Value) -> Result<Vec<LspTextEdit>> {
    let actions = value
        .as_array()
        .ok_or_else(|| anyhow!("LSP codeAction result is not an array"))?;
    let mut edits = Vec::new();
    for action in actions {
        let kind = action
            .get("kind")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default();
        let title = action
            .get("title")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default();
        if kind != "source.organizeImports" && !title.to_ascii_lowercase().contains("organize") {
            continue;
        }
        if let Some(edit) = action.get("edit") {
            edits.extend(workspace_edit_to_text_edits(edit)?);
        }
    }
    Ok(edits)
}

fn collect_lsp_text_edits(
    uri: &str,
    edit_value: &serde_json::Value,
    edits: &mut Vec<LspTextEdit>,
) -> Result<()> {
    let url = Url::parse(uri).with_context(|| format!("invalid LSP uri {uri}"))?;
    let path = url
        .to_file_path()
        .map_err(|_| anyhow!("LSP uri is not a file path: {uri}"))?;
    let edit_array = edit_value
        .as_array()
        .ok_or_else(|| anyhow!("LSP edits for {uri} are not an array"))?;
    for edit in edit_array {
        let range = edit
            .get("range")
            .ok_or_else(|| anyhow!("LSP edit missing range"))?;
        edits.push(LspTextEdit {
            path: path.clone(),
            start_line: lsp_range_num(range, "start", "line")?,
            start_character: lsp_range_num(range, "start", "character")?,
            end_line: lsp_range_num(range, "end", "line")?,
            end_character: lsp_range_num(range, "end", "character")?,
            new_text: edit
                .get("newText")
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default()
                .to_string(),
        });
    }
    Ok(())
}

fn lsp_range_num(range: &serde_json::Value, endpoint: &str, field: &str) -> Result<u64> {
    range
        .get(endpoint)
        .and_then(|value| value.get(field))
        .and_then(serde_json::Value::as_u64)
        .ok_or_else(|| anyhow!("LSP range missing {endpoint}.{field}"))
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
    if plan.edits.is_empty() && plan.file_moves.is_empty() {
        bail!("plan has no edits or file moves");
    }
    let mut paths = HashSet::new();
    for file_move in &plan.file_moves {
        if file_move.source_path == file_move.target_path {
            bail!("file move source and target must differ");
        }
        if !paths.insert(file_move.source_path.clone()) {
            bail!("duplicate refactor path {}", file_move.source_path);
        }
        if !paths.insert(file_move.target_path.clone()) {
            bail!("duplicate refactor path {}", file_move.target_path);
        }
    }
    for edit in &plan.edits {
        if !paths.insert(edit.path.clone()) {
            bail!("duplicate refactor path {}", edit.path);
        }
        ensure_non_overlapping(&edit.edits)
            .with_context(|| format!("overlapping edits in {}", edit.path))?;
    }
    Ok(())
}

fn parse_validation_step_for_path(path: &Path) -> Vec<ValidationStep> {
    if language_for_path(path).is_some() {
        vec![ValidationStep::TreeSitterNoErrors {
            path: path_string(path),
            byte_range: None,
        }]
    } else {
        Vec::new()
    }
}

fn validate_toml_table_name(table: &str) -> Result<()> {
    if table.trim() != table || table.is_empty() {
        bail!("toml_table must be a non-empty top-level table name");
    }
    if table
        .chars()
        .any(|ch| !(ch.is_ascii_alphanumeric() || ch == '_' || ch == '-'))
    {
        bail!("toml_table must be an unquoted top-level TOML table name");
    }
    Ok(())
}

fn ensure_toml_table_content(
    source: &str,
    table: &str,
    entries: &BTreeMap<String, serde_json::Value>,
) -> Result<String> {
    source
        .parse::<toml::Value>()
        .context("source TOML is invalid")?;
    let formatted_entries = entries
        .iter()
        .map(|(key, value)| {
            validate_toml_key(key)?;
            Ok((key.as_str(), toml_literal(value)?))
        })
        .collect::<Result<Vec<_>>>()?;
    let header = format!("[{table}]");
    let mut out = source.to_string();
    let Some((section_start, section_end)) = toml_top_level_section_range(source, &header) else {
        if !out.ends_with('\n') {
            out.push('\n');
        }
        if !out.ends_with("\n\n") && !out.trim().is_empty() {
            out.push('\n');
        }
        out.push_str(&header);
        out.push('\n');
        for (key, value) in formatted_entries {
            out.push_str(key);
            out.push_str(" = ");
            out.push_str(&value);
            out.push('\n');
        }
        return Ok(out);
    };

    let section = &source[section_start..section_end];
    let mut section_out = section.to_string();
    for (key, value) in formatted_entries {
        let replacement_line = format!("{key} = {value}");
        if let Some((line_start, line_end)) = toml_key_line_range(&section_out, key) {
            section_out.replace_range(line_start..line_end, &replacement_line);
        } else {
            if !section_out.ends_with('\n') {
                section_out.push('\n');
            }
            section_out.push_str(&replacement_line);
            section_out.push('\n');
        }
    }
    out.replace_range(section_start..section_end, &section_out);
    Ok(out)
}

fn validate_toml_key(key: &str) -> Result<()> {
    if key.trim() != key || key.is_empty() {
        bail!("TOML entry keys must be non-empty bare keys");
    }
    if key
        .chars()
        .any(|ch| !(ch.is_ascii_alphanumeric() || ch == '_' || ch == '-'))
    {
        bail!("unsupported TOML key `{key}`; only bare keys are supported");
    }
    Ok(())
}

fn toml_top_level_section_range(source: &str, header: &str) -> Option<(usize, usize)> {
    let mut found_start = None;
    for (line_start, line) in source.split_inclusive('\n').scan(0usize, |offset, line| {
        let start = *offset;
        *offset += line.len();
        Some((start, line))
    }) {
        let trimmed = line.trim();
        if found_start.is_none() {
            if trimmed == header {
                found_start = Some(line_start);
            }
            continue;
        }
        if trimmed.starts_with('[') && trimmed.ends_with(']') {
            return found_start.map(|start| (start, line_start));
        }
    }
    found_start.map(|start| (start, source.len()))
}

fn toml_key_line_range(section: &str, key: &str) -> Option<(usize, usize)> {
    let mut offset = 0usize;
    for line in section.split_inclusive('\n') {
        let trimmed = line.trim_start();
        if !trimmed.starts_with('#') {
            let without_comment = trimmed.split('#').next().unwrap_or_default();
            if let Some((lhs, _)) = without_comment.split_once('=') {
                if lhs.trim() == key {
                    let line_end = offset + line.trim_end_matches(['\r', '\n']).len();
                    return Some((offset, line_end));
                }
            }
        }
        offset += line.len();
    }
    None
}

fn toml_literal(value: &serde_json::Value) -> Result<String> {
    Ok(match value {
        serde_json::Value::String(value) => serde_json::to_string(value)?,
        serde_json::Value::Bool(value) => value.to_string(),
        serde_json::Value::Number(value) => value.to_string(),
        serde_json::Value::Array(values) => {
            let values = values
                .iter()
                .map(toml_literal)
                .collect::<Result<Vec<_>>>()?;
            format!("[{}]", values.join(", "))
        }
        serde_json::Value::Null | serde_json::Value::Object(_) => {
            bail!("unsupported TOML value {value}; use string, bool, number, or array")
        }
    })
}

fn restore_snapshots(snapshots: &[(PathBuf, Option<Vec<u8>>)]) -> Vec<String> {
    let mut errors = Vec::new();
    for (path, original) in snapshots.iter().rev() {
        let result = match original {
            Some(bytes) => write_atomic(path, bytes),
            None => match fs::remove_file(path) {
                Ok(()) => Ok(()),
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
                Err(err) => Err(err).with_context(|| format!("removing {}", path.display())),
            },
        };
        if let Err(err) = result {
            errors.push(format!("{}: {err:#}", path.display()));
        }
    }
    errors
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
    let mut ancestor = path
        .parent()
        .ok_or_else(|| anyhow!("{} has no parent directory", path.display()))?;
    while !ancestor.exists() {
        ancestor = ancestor
            .parent()
            .ok_or_else(|| anyhow!("{} has no existing parent directory", path.display()))?;
    }
    let canonical_ancestor = fs::canonicalize(ancestor)
        .with_context(|| format!("canonicalizing parent {}", ancestor.display()))?;
    let suffix = path
        .strip_prefix(ancestor)
        .with_context(|| format!("resolving missing suffix for {}", path.display()))?;
    Ok(canonical_ancestor.join(suffix))
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
            module_name: None,
            visibility: None,
            use_path: None,
            router_name: None,
            router_call: None,
            router_export_name: None,
            target_prelude: None,
            old_text: None,
            new_text: None,
            replace_all: None,
            toml_table: None,
            toml_entries: None,
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
            module_name: None,
            visibility: None,
            use_path: None,
            router_name: Some("moved_tools".into()),
            router_call: None,
            router_export_name: None,
            target_prelude: Some("use super::*;".into()),
            old_text: None,
            new_text: None,
            replace_all: None,
            toml_table: None,
            toml_entries: None,
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
            module_name: None,
            visibility: None,
            use_path: None,
            router_name: Some("moved_tools".into()),
            router_call: None,
            router_export_name: Some("router".into()),
            target_prelude: Some("use super::*;".into()),
            old_text: None,
            new_text: None,
            replace_all: None,
            toml_table: None,
            toml_entries: None,
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
            module_name: None,
            visibility: None,
            use_path: None,
            router_name: Some("moved_tools".into()),
            router_call: None,
            router_export_name: None,
            target_prelude: None,
            old_text: None,
            new_text: None,
            replace_all: None,
            toml_table: None,
            toml_entries: None,
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
            module_name: None,
            visibility: None,
            use_path: None,
            router_name: Some("search_tools".into()),
            router_call: None,
            router_export_name: None,
            target_prelude: None,
            old_text: None,
            new_text: None,
            replace_all: None,
            toml_table: None,
            toml_entries: None,
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
            module_name: None,
            visibility: None,
            use_path: None,
            router_name: None,
            router_call: None,
            router_export_name: None,
            target_prelude: Some("use super::*;".into()),
            old_text: None,
            new_text: None,
            replace_all: None,
            toml_table: None,
            toml_entries: None,
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
            module_name: None,
            visibility: None,
            use_path: None,
            router_name: None,
            router_call: None,
            router_export_name: None,
            target_prelude: Some("use super::*;".into()),
            old_text: None,
            new_text: None,
            replace_all: None,
            toml_table: None,
            toml_entries: None,
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
            module_name: None,
            visibility: None,
            use_path: None,
            router_name: None,
            router_call: None,
            router_export_name: None,
            target_prelude: Some("use super::*;".into()),
            old_text: None,
            new_text: None,
            replace_all: None,
            toml_table: None,
            toml_entries: None,
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
            module_name: None,
            visibility: None,
            use_path: None,
            router_name: None,
            router_call: None,
            router_export_name: None,
            target_prelude: None,
            old_text: None,
            new_text: None,
            replace_all: None,
            toml_table: None,
            toml_entries: None,
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
            module_name: None,
            visibility: None,
            use_path: None,
            router_name: None,
            router_call: None,
            router_export_name: None,
            target_prelude: None,
            old_text: None,
            new_text: None,
            replace_all: None,
            toml_table: None,
            toml_entries: None,
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
            module_name: None,
            visibility: None,
            use_path: None,
            router_name: None,
            router_call: None,
            router_export_name: None,
            target_prelude: None,
            old_text: None,
            new_text: None,
            replace_all: None,
            toml_table: None,
            toml_entries: None,
            project_dir: None,
        })
        .unwrap_err();
        assert!(err
            .to_string()
            .contains("only supports item_kinds impl_method"));
    }

    #[test]
    fn delete_rust_items_removes_top_level_items_only() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("lib.rs");
        fs::write(
            &source,
            "mod remove_me;\nmod keep_mod;\n\n#[derive(Debug)]\nstruct DeleteMe;\n\nfn keep() {}\n",
        )
        .unwrap();

        let plan_text = plan(&RefactorPlanParams {
            kind: "delete_rust_items".into(),
            source: path_string(&source),
            target: None,
            item_names: Some(vec!["remove_me".into(), "DeleteMe".into()]),
            item_kinds: None,
            impl_name: None,
            module_name: None,
            visibility: None,
            use_path: None,
            router_name: None,
            router_call: None,
            router_export_name: None,
            target_prelude: None,
            old_text: None,
            new_text: None,
            replace_all: None,
            toml_table: None,
            toml_entries: None,
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
        assert!(!source_text.contains("remove_me"));
        assert!(!source_text.contains("DeleteMe"));
        assert!(!source_text.contains("#[derive(Debug)]"));
        assert!(source_text.contains("mod keep_mod;"));
        assert!(source_text.contains("fn keep()"));
    }

    #[test]
    fn delete_rust_items_removes_impl_method_with_attributes() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("lib.rs");
        fs::write(
            &source,
            "struct A;\nimpl A {\n    #[allow(dead_code)]\n    fn delete_me(&self) {}\n\n    fn keep(&self) {}\n}\n",
        )
        .unwrap();

        let plan_text = plan(&RefactorPlanParams {
            kind: "delete_rust_items".into(),
            source: path_string(&source),
            target: None,
            item_names: Some(vec!["delete_me".into()]),
            item_kinds: Some(vec!["impl_method".into()]),
            impl_name: Some("impl A".into()),
            module_name: None,
            visibility: None,
            use_path: None,
            router_name: None,
            router_call: None,
            router_export_name: None,
            target_prelude: None,
            old_text: None,
            new_text: None,
            replace_all: None,
            toml_table: None,
            toml_entries: None,
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
        assert!(!source_text.contains("delete_me"));
        assert!(!source_text.contains("#[allow(dead_code)]"));
        assert!(source_text.contains("fn keep"));
    }

    #[test]
    fn delete_rust_items_reports_leftovers_within_impl_scope() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("lib.rs");
        fs::write(
            &source,
            "struct A;\nstruct B;\nimpl A { fn delete_me(&self) {} fn keep_a(&self) {} }\nimpl B { fn keep_b(&self) {} }\n",
        )
        .unwrap();

        let plan_text = plan(&RefactorPlanParams {
            kind: "delete_rust_items".into(),
            source: path_string(&source),
            target: None,
            item_names: Some(vec!["delete_me".into()]),
            item_kinds: Some(vec!["impl_method".into()]),
            impl_name: Some("impl A".into()),
            module_name: None,
            visibility: None,
            use_path: None,
            router_name: None,
            router_call: None,
            router_export_name: None,
            target_prelude: None,
            old_text: None,
            new_text: None,
            replace_all: None,
            toml_table: None,
            toml_entries: None,
            project_dir: None,
        })
        .unwrap();
        let plan_value: serde_json::Value = serde_json::from_str(&plan_text).unwrap();
        let leftovers = plan_value["leftovers"].as_array().unwrap();
        assert!(leftovers
            .iter()
            .any(|leftover| leftover.as_str().unwrap().contains("keep_a")));
        assert!(!leftovers
            .iter()
            .any(|leftover| leftover.as_str().unwrap().contains("keep_b")));
    }

    #[test]
    fn delete_rust_items_requires_impl_filter_for_ambiguous_method_name() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("lib.rs");
        fs::write(
            &source,
            "struct A;\nstruct B;\nimpl A { fn same(&self) {} }\nimpl B { fn same(&self) {} }\n",
        )
        .unwrap();

        let err = plan(&RefactorPlanParams {
            kind: "delete_rust_items".into(),
            source: path_string(&source),
            target: None,
            item_names: Some(vec!["same".into()]),
            item_kinds: Some(vec!["impl_method".into()]),
            impl_name: None,
            module_name: None,
            visibility: None,
            use_path: None,
            router_name: None,
            router_call: None,
            router_export_name: None,
            target_prelude: None,
            old_text: None,
            new_text: None,
            replace_all: None,
            toml_table: None,
            toml_entries: None,
            project_dir: None,
        })
        .unwrap_err();
        assert!(err.to_string().contains("matched multiple impl blocks"));
    }

    #[test]
    fn delete_rust_items_requires_explicit_item_names() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("lib.rs");
        fs::write(&source, "mod remove_me;\nmod keep_mod;\n").unwrap();

        let err = plan(&RefactorPlanParams {
            kind: "delete_rust_items".into(),
            source: path_string(&source),
            target: None,
            item_names: None,
            item_kinds: Some(vec!["mod_item".into()]),
            impl_name: None,
            module_name: None,
            visibility: None,
            use_path: None,
            router_name: None,
            router_call: None,
            router_export_name: None,
            target_prelude: None,
            old_text: None,
            new_text: None,
            replace_all: None,
            toml_table: None,
            toml_entries: None,
            project_dir: None,
        })
        .unwrap_err();
        assert!(err.to_string().contains("requires non-empty item_names"));
    }

    #[test]
    fn delete_rust_items_rejects_mixed_top_level_and_impl_method_kinds() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("lib.rs");
        fs::write(&source, "struct A;\nimpl A { fn method(&self) {} }\n").unwrap();

        let err = plan(&RefactorPlanParams {
            kind: "delete_rust_items".into(),
            source: path_string(&source),
            target: None,
            item_names: Some(vec!["method".into()]),
            item_kinds: Some(vec!["struct_item".into(), "impl_method".into()]),
            impl_name: None,
            module_name: None,
            visibility: None,
            use_path: None,
            router_name: None,
            router_call: None,
            router_export_name: None,
            target_prelude: None,
            old_text: None,
            new_text: None,
            replace_all: None,
            toml_table: None,
            toml_entries: None,
            project_dir: None,
        })
        .unwrap_err();
        assert!(err.to_string().contains("cannot mix impl_method"));
    }

    #[test]
    fn refactor_run_applies_sequential_plan_steps() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("lib.rs");
        fs::write(&source, "fn keep() {}\n").unwrap();

        let response = run(
            &RefactorRunParams {
                title: "add then delete module declaration".into(),
                project_dir: path_string(dir.path()),
                steps: vec![
                    RefactorRunStep::Plan {
                        params: RefactorPlanParams {
                            kind: "add_rust_mod_decl".into(),
                            source: "lib.rs".into(),
                            target: None,
                            item_names: None,
                            item_kinds: None,
                            impl_name: None,
                            module_name: Some("newmod".into()),
                            visibility: None,
                            use_path: None,
                            router_name: None,
                            router_call: None,
                            router_export_name: None,
                            target_prelude: None,
                            old_text: None,
                            new_text: None,
                            replace_all: None,
                            toml_table: None,
                            toml_entries: None,
                            project_dir: None,
                        },
                    },
                    RefactorRunStep::Plan {
                        params: RefactorPlanParams {
                            kind: "delete_rust_items".into(),
                            source: "lib.rs".into(),
                            target: None,
                            item_names: Some(vec!["newmod".into()]),
                            item_kinds: Some(vec!["mod_item".into()]),
                            impl_name: None,
                            module_name: None,
                            visibility: None,
                            use_path: None,
                            router_name: None,
                            router_call: None,
                            router_export_name: None,
                            target_prelude: None,
                            old_text: None,
                            new_text: None,
                            replace_all: None,
                            toml_table: None,
                            toml_entries: None,
                            project_dir: None,
                        },
                    },
                ],
                confirm: Some(true),
                allow_dirty_worktree: None,
                allow_unregistered_paths: Some(true),
            },
            &[project_record(dir.path())],
        )
        .unwrap();
        let run_response: RefactorRunResponse = serde_json::from_str(&response).unwrap();
        assert_eq!(run_response.status, "ok");
        assert_eq!(run_response.steps.len(), 2);
        assert!(run_response.steps.iter().all(|step| step.status == "ok"));
        assert_eq!(fs::read_to_string(&source).unwrap(), "fn keep() {}\n");
    }

    #[test]
    fn refactor_run_rolls_back_when_later_plan_fails() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("lib.rs");
        fs::write(&source, "fn keep() {}\n").unwrap();

        let response = run(
            &RefactorRunParams {
                title: "rollback failed compound run".into(),
                project_dir: path_string(dir.path()),
                steps: vec![
                    RefactorRunStep::Plan {
                        params: RefactorPlanParams {
                            kind: "add_rust_mod_decl".into(),
                            source: "lib.rs".into(),
                            target: None,
                            item_names: None,
                            item_kinds: None,
                            impl_name: None,
                            module_name: Some("newmod".into()),
                            visibility: None,
                            use_path: None,
                            router_name: None,
                            router_call: None,
                            router_export_name: None,
                            target_prelude: None,
                            old_text: None,
                            new_text: None,
                            replace_all: None,
                            toml_table: None,
                            toml_entries: None,
                            project_dir: None,
                        },
                    },
                    RefactorRunStep::Plan {
                        params: RefactorPlanParams {
                            kind: "delete_rust_items".into(),
                            source: "lib.rs".into(),
                            target: None,
                            item_names: Some(vec!["missing_mod".into()]),
                            item_kinds: Some(vec!["mod_item".into()]),
                            impl_name: None,
                            module_name: None,
                            visibility: None,
                            use_path: None,
                            router_name: None,
                            router_call: None,
                            router_export_name: None,
                            target_prelude: None,
                            old_text: None,
                            new_text: None,
                            replace_all: None,
                            toml_table: None,
                            toml_entries: None,
                            project_dir: None,
                        },
                    },
                ],
                confirm: Some(true),
                allow_dirty_worktree: None,
                allow_unregistered_paths: Some(true),
            },
            &[project_record(dir.path())],
        )
        .unwrap();
        let run_response: RefactorRunResponse = serde_json::from_str(&response).unwrap();
        assert_eq!(run_response.status, "step_failed");
        assert!(run_response.rolled_back);
        assert_eq!(fs::read_to_string(&source).unwrap(), "fn keep() {}\n");
    }

    #[test]
    fn refactor_run_rolls_back_when_later_path_is_out_of_scope() {
        let dir = tempfile::tempdir().unwrap();
        let outside_dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("lib.rs");
        let outside = outside_dir.path().join("outside.rs");
        fs::write(&source, "fn keep() {}\n").unwrap();
        fs::write(&outside, "mod outside_mod;\n").unwrap();

        let response = run(
            &RefactorRunParams {
                title: "rollback out of scope step".into(),
                project_dir: path_string(dir.path()),
                steps: vec![
                    RefactorRunStep::Plan {
                        params: RefactorPlanParams {
                            kind: "add_rust_mod_decl".into(),
                            source: "lib.rs".into(),
                            target: None,
                            item_names: None,
                            item_kinds: None,
                            impl_name: None,
                            module_name: Some("newmod".into()),
                            visibility: None,
                            use_path: None,
                            router_name: None,
                            router_call: None,
                            router_export_name: None,
                            target_prelude: None,
                            old_text: None,
                            new_text: None,
                            replace_all: None,
                            toml_table: None,
                            toml_entries: None,
                            project_dir: None,
                        },
                    },
                    RefactorRunStep::Plan {
                        params: RefactorPlanParams {
                            kind: "delete_rust_items".into(),
                            source: path_string(&outside),
                            target: None,
                            item_names: Some(vec!["outside_mod".into()]),
                            item_kinds: Some(vec!["mod_item".into()]),
                            impl_name: None,
                            module_name: None,
                            visibility: None,
                            use_path: None,
                            router_name: None,
                            router_call: None,
                            router_export_name: None,
                            target_prelude: None,
                            old_text: None,
                            new_text: None,
                            replace_all: None,
                            toml_table: None,
                            toml_entries: None,
                            project_dir: None,
                        },
                    },
                ],
                confirm: Some(true),
                allow_dirty_worktree: None,
                allow_unregistered_paths: None,
            },
            &[project_record(dir.path())],
        )
        .unwrap();
        let run_response: RefactorRunResponse = serde_json::from_str(&response).unwrap();
        assert_eq!(run_response.status, "step_failed");
        assert!(run_response.rolled_back);
        assert!(run_response
            .error
            .unwrap()
            .contains("outside registered projects"));
        assert_eq!(fs::read_to_string(&source).unwrap(), "fn keep() {}\n");
        assert_eq!(fs::read_to_string(&outside).unwrap(), "mod outside_mod;\n");
    }

    #[test]
    fn refactor_run_rolls_back_file_move_when_later_plan_fails() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("packets.rs");
        let target = dir.path().join("packets").join("mod.rs");
        fs::write(&source, "fn keep() {}\n").unwrap();

        let response = run(
            &RefactorRunParams {
                title: "rollback moved file".into(),
                project_dir: path_string(dir.path()),
                steps: vec![
                    RefactorRunStep::Plan {
                        params: RefactorPlanParams {
                            kind: "move_file".into(),
                            source: "packets.rs".into(),
                            target: Some("packets/mod.rs".into()),
                            item_names: None,
                            item_kinds: None,
                            impl_name: None,
                            module_name: None,
                            visibility: None,
                            use_path: None,
                            router_name: None,
                            router_call: None,
                            router_export_name: None,
                            target_prelude: None,
                            old_text: None,
                            new_text: None,
                            replace_all: None,
                            toml_table: None,
                            toml_entries: None,
                            project_dir: None,
                        },
                    },
                    RefactorRunStep::Plan {
                        params: RefactorPlanParams {
                            kind: "delete_rust_items".into(),
                            source: "packets/mod.rs".into(),
                            target: None,
                            item_names: Some(vec!["missing".into()]),
                            item_kinds: Some(vec!["function_item".into()]),
                            impl_name: None,
                            module_name: None,
                            visibility: None,
                            use_path: None,
                            router_name: None,
                            router_call: None,
                            router_export_name: None,
                            target_prelude: None,
                            old_text: None,
                            new_text: None,
                            replace_all: None,
                            toml_table: None,
                            toml_entries: None,
                            project_dir: None,
                        },
                    },
                ],
                confirm: Some(true),
                allow_dirty_worktree: None,
                allow_unregistered_paths: Some(true),
            },
            &[project_record(dir.path())],
        )
        .unwrap();
        let run_response: RefactorRunResponse = serde_json::from_str(&response).unwrap();
        assert_eq!(run_response.status, "step_failed");
        assert!(run_response.rolled_back);
        assert_eq!(fs::read_to_string(&source).unwrap(), "fn keep() {}\n");
        assert!(!target.exists());
    }

    #[test]
    fn refactor_run_rolls_back_when_required_command_fails() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("lib.rs");
        fs::write(&source, "fn keep() {}\n").unwrap();

        let response = run(
            &RefactorRunParams {
                title: "rollback failed command".into(),
                project_dir: path_string(dir.path()),
                steps: vec![
                    RefactorRunStep::Plan {
                        params: RefactorPlanParams {
                            kind: "add_rust_mod_decl".into(),
                            source: "lib.rs".into(),
                            target: None,
                            item_names: None,
                            item_kinds: None,
                            impl_name: None,
                            module_name: Some("newmod".into()),
                            visibility: None,
                            use_path: None,
                            router_name: None,
                            router_call: None,
                            router_export_name: None,
                            target_prelude: None,
                            old_text: None,
                            new_text: None,
                            replace_all: None,
                            toml_table: None,
                            toml_entries: None,
                            project_dir: None,
                        },
                    },
                    RefactorRunStep::Command {
                        command: "false".into(),
                        args: Vec::new(),
                        cwd: None,
                        touches: Vec::new(),
                        required: Some(true),
                    },
                ],
                confirm: Some(true),
                allow_dirty_worktree: None,
                allow_unregistered_paths: Some(true),
            },
            &[project_record(dir.path())],
        )
        .unwrap();
        let run_response: RefactorRunResponse = serde_json::from_str(&response).unwrap();
        assert_eq!(run_response.status, "step_failed");
        assert!(run_response.rolled_back);
        assert_eq!(fs::read_to_string(&source).unwrap(), "fn keep() {}\n");
    }

    #[test]
    fn refactor_run_rolls_back_declared_command_touches() {
        let dir = tempfile::tempdir().unwrap();
        let generated = dir.path().join("generated.txt");

        let response = run(
            &RefactorRunParams {
                title: "rollback command side effects".into(),
                project_dir: path_string(dir.path()),
                steps: vec![RefactorRunStep::Command {
                    command: "sh".into(),
                    args: vec!["-c".into(), "printf created > generated.txt; false".into()],
                    cwd: None,
                    touches: vec!["generated.txt".into()],
                    required: Some(true),
                }],
                confirm: Some(true),
                allow_dirty_worktree: None,
                allow_unregistered_paths: Some(true),
            },
            &[project_record(dir.path())],
        )
        .unwrap();

        let run_response: RefactorRunResponse = serde_json::from_str(&response).unwrap();
        assert_eq!(run_response.status, "step_failed");
        assert!(run_response.rolled_back);
        assert!(!generated.exists());
    }

    #[test]
    fn command_output_truncation_preserves_failure_tail() {
        let output = (0..200)
            .map(|idx| format!("line {idx}"))
            .chain(std::iter::once("failures: important_test".to_string()))
            .collect::<Vec<_>>()
            .join("\n");

        let truncated = truncate_for_report(&output, 120);

        assert!(truncated.contains("line 0"));
        assert!(truncated.contains("[truncated middle]"));
        assert!(truncated.contains("failures: important_test"));
        assert!(truncated.chars().count() <= 120);
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
            module_name: None,
            visibility: None,
            use_path: None,
            router_name: Some("search_tools".into()),
            router_call: None,
            router_export_name: None,
            target_prelude: None,
            old_text: None,
            new_text: None,
            replace_all: None,
            toml_table: None,
            toml_entries: None,
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
            module_name: None,
            visibility: None,
            use_path: None,
            router_name: None,
            router_call: Some("refactor_tools::router()".into()),
            router_export_name: None,
            target_prelude: None,
            old_text: None,
            new_text: None,
            replace_all: None,
            toml_table: None,
            toml_entries: None,
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
    fn add_rust_mod_decl_appends_after_existing_mods() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("main.rs");
        fs::write(&source, "mod alpha;\nmod beta;\n\nuse std::fmt;\n").unwrap();

        let plan_text = plan(&RefactorPlanParams {
            kind: "add_rust_mod_decl".into(),
            source: path_string(&source),
            target: None,
            item_names: None,
            item_kinds: None,
            impl_name: None,
            module_name: Some("gamma".into()),
            visibility: None,
            use_path: None,
            router_name: None,
            router_call: None,
            router_export_name: None,
            target_prelude: None,
            old_text: None,
            new_text: None,
            replace_all: None,
            toml_table: None,
            toml_entries: None,
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
        assert_eq!(
            fs::read_to_string(&source).unwrap(),
            "mod alpha;\nmod beta;\nmod gamma;\n\nuse std::fmt;\n"
        );
    }

    #[test]
    fn add_rust_mod_decl_rejects_duplicate() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("main.rs");
        fs::write(&source, "mod alpha;\n").unwrap();

        let err = plan(&RefactorPlanParams {
            kind: "add_rust_mod_decl".into(),
            source: path_string(&source),
            target: None,
            item_names: None,
            item_kinds: None,
            impl_name: None,
            module_name: Some("alpha".into()),
            visibility: None,
            use_path: None,
            router_name: None,
            router_call: None,
            router_export_name: None,
            target_prelude: None,
            old_text: None,
            new_text: None,
            replace_all: None,
            toml_table: None,
            toml_entries: None,
            project_dir: None,
        })
        .unwrap_err();
        assert!(err.to_string().contains("already exists"));
    }

    #[test]
    fn add_rust_mod_decl_supports_visibility() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("lib.rs");
        fs::write(&source, "pub mod alpha;\n").unwrap();

        let plan_text = plan(&RefactorPlanParams {
            kind: "add_rust_mod_decl".into(),
            source: path_string(&source),
            target: None,
            item_names: None,
            item_kinds: None,
            impl_name: None,
            module_name: Some("beta".into()),
            visibility: Some("pub(crate)".into()),
            use_path: None,
            router_name: None,
            router_call: None,
            router_export_name: None,
            target_prelude: None,
            old_text: None,
            new_text: None,
            replace_all: None,
            toml_table: None,
            toml_entries: None,
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
        assert_eq!(
            fs::read_to_string(&source).unwrap(),
            "pub mod alpha;\npub(crate) mod beta;\n"
        );
    }

    #[test]
    fn copy_rust_mod_decls_copies_selected_declarations_with_visibility() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("main.rs");
        let target = dir.path().join("lib.rs");
        fs::write(
            &source,
            "mod alpha;\nmod beta;\nmod inline { fn no_copy() {} }\n\nfn main() {}\n",
        )
        .unwrap();
        fs::write(&target, "pub mod existing;\n\npub use existing::Thing;\n").unwrap();

        let plan_text = plan(&RefactorPlanParams {
            kind: "copy_rust_mod_decls".into(),
            source: path_string(&source),
            target: Some(path_string(&target)),
            item_names: Some(vec!["alpha".into(), "beta".into()]),
            item_kinds: None,
            impl_name: None,
            module_name: None,
            visibility: Some("pub".into()),
            use_path: None,
            router_name: None,
            router_call: None,
            router_export_name: None,
            target_prelude: None,
            old_text: None,
            new_text: None,
            replace_all: None,
            toml_table: None,
            toml_entries: None,
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
        assert_eq!(
            fs::read_to_string(&target).unwrap(),
            "pub mod existing;\npub mod alpha;\npub mod beta;\n\npub use existing::Thing;\n"
        );
    }

    #[test]
    fn copy_rust_mod_decls_rejects_inline_module() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("main.rs");
        let target = dir.path().join("lib.rs");
        fs::write(&source, "mod inline { fn no_copy() {} }\n").unwrap();

        let err = plan(&RefactorPlanParams {
            kind: "copy_rust_mod_decls".into(),
            source: path_string(&source),
            target: Some(path_string(&target)),
            item_names: Some(vec!["inline".into()]),
            item_kinds: None,
            impl_name: None,
            module_name: None,
            visibility: Some("pub".into()),
            use_path: None,
            router_name: None,
            router_call: None,
            router_export_name: None,
            target_prelude: None,
            old_text: None,
            new_text: None,
            replace_all: None,
            toml_table: None,
            toml_entries: None,
            project_dir: None,
        })
        .unwrap_err();
        assert!(err.to_string().contains("is inline"));
    }

    #[test]
    fn copy_rust_mod_decls_creates_missing_target_file() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("main.rs");
        let target = dir.path().join("lib.rs");
        fs::write(&source, "mod alpha;\n").unwrap();

        let plan_text = plan(&RefactorPlanParams {
            kind: "copy_rust_mod_decls".into(),
            source: path_string(&source),
            target: Some(path_string(&target)),
            item_names: Some(vec!["alpha".into()]),
            item_kinds: None,
            impl_name: None,
            module_name: None,
            visibility: Some("pub".into()),
            use_path: None,
            router_name: None,
            router_call: None,
            router_export_name: None,
            target_prelude: None,
            old_text: None,
            new_text: None,
            replace_all: None,
            toml_table: None,
            toml_entries: None,
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
        assert_eq!(fs::read_to_string(&target).unwrap(), "pub mod alpha;\n");
    }

    #[test]
    fn rewrite_rust_mod_visibility_updates_existing_declaration() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("lib.rs");
        fs::write(
            &source,
            "mod alpha;\npub(crate) mod beta;\npub mod gamma;\n",
        )
        .unwrap();

        let plan_text = plan(&RefactorPlanParams {
            kind: "rewrite_rust_mod_visibility".into(),
            source: path_string(&source),
            target: None,
            item_names: Some(vec!["beta".into()]),
            item_kinds: None,
            impl_name: None,
            module_name: None,
            visibility: Some("pub".into()),
            use_path: None,
            router_name: None,
            router_call: None,
            router_export_name: None,
            target_prelude: None,
            old_text: None,
            new_text: None,
            replace_all: None,
            toml_table: None,
            toml_entries: None,
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
        assert_eq!(
            fs::read_to_string(&source).unwrap(),
            "mod alpha;\npub mod beta;\npub mod gamma;\n"
        );
    }

    #[test]
    fn rewrite_rust_mod_visibility_preserves_attached_attribute() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("lib.rs");
        fs::write(&source, "#[path = \"alpha_impl.rs\"]\nmod alpha;\n").unwrap();

        let plan_text = plan(&RefactorPlanParams {
            kind: "rewrite_rust_mod_visibility".into(),
            source: path_string(&source),
            target: None,
            item_names: Some(vec!["alpha".into()]),
            item_kinds: None,
            impl_name: None,
            module_name: None,
            visibility: Some("pub".into()),
            use_path: None,
            router_name: None,
            router_call: None,
            router_export_name: None,
            target_prelude: None,
            old_text: None,
            new_text: None,
            replace_all: None,
            toml_table: None,
            toml_entries: None,
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
        assert_eq!(
            fs::read_to_string(&source).unwrap(),
            "#[path = \"alpha_impl.rs\"]\npub mod alpha;\n"
        );
    }

    #[test]
    fn move_file_moves_source_to_missing_target() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("packets.rs");
        let target = dir.path().join("packets").join("mod.rs");
        fs::write(&source, "pub fn packet() {}\n").unwrap();

        let plan_text = plan(&RefactorPlanParams {
            kind: "move_file".into(),
            source: path_string(&source),
            target: Some(path_string(&target)),
            item_names: None,
            item_kinds: None,
            impl_name: None,
            module_name: None,
            visibility: None,
            use_path: None,
            router_name: None,
            router_call: None,
            router_export_name: None,
            target_prelude: None,
            old_text: None,
            new_text: None,
            replace_all: None,
            toml_table: None,
            toml_entries: None,
            project_dir: None,
        })
        .unwrap();
        let plan_value: serde_json::Value = serde_json::from_str(&plan_text).unwrap();
        assert_eq!(plan_value["kind"], "move_file");
        assert_eq!(plan_value["file_moves"].as_array().unwrap().len(), 1);
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
        assert!(!source.exists());
        assert_eq!(fs::read_to_string(&target).unwrap(), "pub fn packet() {}\n");
    }

    #[test]
    fn move_file_rejects_existing_target() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("a.rs");
        let target = dir.path().join("b.rs");
        fs::write(&source, "pub fn a() {}\n").unwrap();
        fs::write(&target, "pub fn b() {}\n").unwrap();

        let err = plan(&RefactorPlanParams {
            kind: "move_file".into(),
            source: path_string(&source),
            target: Some(path_string(&target)),
            item_names: None,
            item_kinds: None,
            impl_name: None,
            module_name: None,
            visibility: None,
            use_path: None,
            router_name: None,
            router_call: None,
            router_export_name: None,
            target_prelude: None,
            old_text: None,
            new_text: None,
            replace_all: None,
            toml_table: None,
            toml_entries: None,
            project_dir: None,
        })
        .unwrap_err();
        assert!(err.to_string().contains("target already exists"));
    }

    #[test]
    fn replace_text_replaces_exact_single_match() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("lib.rs");
        fs::write(&source, "pub fn before() {}\n").unwrap();

        let plan_text = plan(&RefactorPlanParams {
            kind: "replace_text".into(),
            source: path_string(&source),
            target: None,
            item_names: None,
            item_kinds: None,
            impl_name: None,
            module_name: None,
            visibility: None,
            use_path: None,
            router_name: None,
            router_call: None,
            router_export_name: None,
            target_prelude: None,
            old_text: Some("before".into()),
            new_text: Some("after".into()),
            replace_all: None,
            toml_table: None,
            toml_entries: None,
            project_dir: None,
        })
        .unwrap();
        let response = apply(
            &RefactorApplyParams {
                plan: serde_json::from_str(&plan_text).unwrap(),
                confirm: Some(true),
                allow_dirty_worktree: None,
                allow_unregistered_paths: None,
            },
            &[project_record(dir.path())],
        )
        .unwrap();
        let applied: RefactorApplyResponse = serde_json::from_str(&response).unwrap();
        assert_eq!(applied.status, "ok");
        assert_eq!(fs::read_to_string(&source).unwrap(), "pub fn after() {}\n");
    }

    #[test]
    fn write_file_creates_missing_supported_source_file() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("src").join("lib.rs");

        let plan_text = plan(&RefactorPlanParams {
            kind: "write_file".into(),
            source: path_string(&source),
            target: None,
            item_names: None,
            item_kinds: None,
            impl_name: None,
            module_name: None,
            visibility: None,
            use_path: None,
            router_name: None,
            router_call: None,
            router_export_name: None,
            target_prelude: None,
            old_text: None,
            new_text: Some("pub mod packets;\n".into()),
            replace_all: None,
            toml_table: None,
            toml_entries: None,
            project_dir: None,
        })
        .unwrap();
        let response = apply(
            &RefactorApplyParams {
                plan: serde_json::from_str(&plan_text).unwrap(),
                confirm: Some(true),
                allow_dirty_worktree: None,
                allow_unregistered_paths: None,
            },
            &[project_record(dir.path())],
        )
        .unwrap();
        let applied: RefactorApplyResponse = serde_json::from_str(&response).unwrap();
        assert_eq!(applied.status, "ok");
        assert_eq!(fs::read_to_string(&source).unwrap(), "pub mod packets;\n");
    }

    #[test]
    fn ensure_toml_table_adds_lib_table() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("Cargo.toml");
        fs::write(
            &source,
            "[package]\nname = \"demo\"\nversion = \"0.1.0\"\n\n[[bin]]\nname = \"demo\"\npath = \"src/main.rs\"\n",
        )
        .unwrap();

        let plan_text = plan(&RefactorPlanParams {
            kind: "ensure_toml_table".into(),
            source: path_string(&source),
            target: None,
            item_names: None,
            item_kinds: None,
            impl_name: None,
            module_name: None,
            visibility: None,
            use_path: None,
            router_name: None,
            router_call: None,
            router_export_name: None,
            target_prelude: None,
            old_text: None,
            new_text: None,
            replace_all: None,
            toml_table: Some("lib".into()),
            toml_entries: Some(BTreeMap::from([
                ("name".into(), serde_json::json!("demo")),
                ("path".into(), serde_json::json!("src/lib.rs")),
            ])),
            project_dir: None,
        })
        .unwrap();
        let response = apply(
            &RefactorApplyParams {
                plan: serde_json::from_str(&plan_text).unwrap(),
                confirm: Some(true),
                allow_dirty_worktree: None,
                allow_unregistered_paths: None,
            },
            &[project_record(dir.path())],
        )
        .unwrap();
        let applied: RefactorApplyResponse = serde_json::from_str(&response).unwrap();
        assert_eq!(applied.status, "ok");
        let updated = fs::read_to_string(&source).unwrap();
        assert!(updated.contains("[lib]\nname = \"demo\"\npath = \"src/lib.rs\"\n"));
        updated.parse::<toml::Value>().unwrap();
    }

    #[test]
    fn rust_lsp_rename_renames_references() {
        if !Command::new("rust-analyzer")
            .arg("--version")
            .output()
            .is_ok_and(|output| output.status.success())
        {
            eprintln!("skipping rust_lsp_rename test: rust-analyzer unavailable");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join("src")).unwrap();
        fs::write(
            dir.path().join("Cargo.toml"),
            "[package]\nname = \"rename_fixture\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
        )
        .unwrap();
        let source = dir.path().join("src").join("lib.rs");
        fs::write(
            &source,
            "pub fn old_name() -> usize { 1 }\n\npub fn caller() -> usize { old_name() }\n",
        )
        .unwrap();

        let plan_text = plan(&RefactorPlanParams {
            kind: "rust_lsp_rename".into(),
            source: path_string(&source),
            target: None,
            item_names: Some(vec!["old_name".into()]),
            item_kinds: None,
            impl_name: None,
            module_name: None,
            visibility: None,
            use_path: None,
            router_name: None,
            router_call: None,
            router_export_name: None,
            target_prelude: None,
            old_text: None,
            new_text: Some("new_name".into()),
            replace_all: None,
            toml_table: None,
            toml_entries: None,
            project_dir: Some(path_string(dir.path())),
        })
        .unwrap();
        let plan_value: serde_json::Value = serde_json::from_str(&plan_text).unwrap();
        assert_eq!(plan_value["semantic_status"], "lsp_verified");
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
        let updated = fs::read_to_string(&source).unwrap();
        assert!(updated.contains("pub fn new_name()"));
        assert!(updated.contains("new_name() }"));
        assert!(!updated.contains("old_name"));
    }

    #[test]
    fn add_rust_use_decl_inserts_after_existing_uses() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("lib.rs");
        fs::write(&source, "mod alpha;\n\nuse std::fmt;\n\nfn main() {}\n").unwrap();

        let plan_text = plan(&RefactorPlanParams {
            kind: "add_rust_use_decl".into(),
            source: path_string(&source),
            target: None,
            item_names: None,
            item_kinds: None,
            impl_name: None,
            module_name: None,
            visibility: None,
            use_path: Some("crate::alpha::Thing".into()),
            router_name: None,
            router_call: None,
            router_export_name: None,
            target_prelude: None,
            old_text: None,
            new_text: None,
            replace_all: None,
            toml_table: None,
            toml_entries: None,
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
        assert_eq!(
            fs::read_to_string(&source).unwrap(),
            "mod alpha;\n\nuse std::fmt;\nuse crate::alpha::Thing;\n\nfn main() {}\n"
        );
    }

    #[test]
    fn add_rust_use_decl_supports_pub_use_and_rejects_duplicate() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("lib.rs");
        fs::write(&source, "pub use crate::alpha::Thing;\n").unwrap();

        let err = plan(&RefactorPlanParams {
            kind: "add_rust_use_decl".into(),
            source: path_string(&source),
            target: None,
            item_names: None,
            item_kinds: None,
            impl_name: None,
            module_name: None,
            visibility: Some("pub".into()),
            use_path: Some("crate::alpha::Thing".into()),
            router_name: None,
            router_call: None,
            router_export_name: None,
            target_prelude: None,
            old_text: None,
            new_text: None,
            replace_all: None,
            toml_table: None,
            toml_entries: None,
            project_dir: None,
        })
        .unwrap_err();
        assert!(err.to_string().contains("already exists"));
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
            module_name: None,
            visibility: None,
            use_path: None,
            router_name: Some("search_tools".into()),
            router_call: None,
            router_export_name: None,
            target_prelude: None,
            old_text: None,
            new_text: None,
            replace_all: None,
            toml_table: None,
            toml_entries: None,
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
            module_name: None,
            visibility: None,
            use_path: None,
            router_name: None,
            router_call: None,
            router_export_name: None,
            target_prelude: None,
            old_text: None,
            new_text: None,
            replace_all: None,
            toml_table: None,
            toml_entries: None,
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
            file_moves: Vec::new(),
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
            file_moves: Vec::new(),
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
