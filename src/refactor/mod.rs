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

mod rust;
use rust::*;
mod java;
use java::*;

use crate::chunker;
use crate::chunker::code::{language_for_path, parser_for_language};
use crate::entity_ref;
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

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct RefactorProjectRefsParams {
    /// Source file to chunk. Relative paths resolve against project_dir or cwd.
    pub file: String,
    /// Optional project root used to resolve relative paths and compute the project_file project id.
    #[serde(default)]
    pub project_dir: Option<String>,
    /// Optional substring filter applied to chunk content, symbol, chunk kind, and entity_ref.
    #[serde(default)]
    pub query: Option<String>,
    /// Maximum matching chunks to return. Defaults to 100 and is capped at 1000.
    #[serde(default)]
    pub limit: Option<usize>,
    /// Include chunk content excerpts. Defaults true.
    #[serde(default)]
    pub include_excerpt: Option<bool>,
}

#[derive(Debug, Clone, Deserialize, schemars::JsonSchema)]
pub struct JavaFieldSpec {
    #[serde(default)]
    pub visibility: Option<String>,
    #[serde(rename = "type")]
    pub type_name: String,
    pub name: String,
    #[serde(default, rename = "final")]
    pub final_field: Option<bool>,
}

#[derive(Debug, Clone, Deserialize, schemars::JsonSchema)]
pub struct JavaParameterSpec {
    #[serde(rename = "type")]
    pub type_name: String,
    pub name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, schemars::JsonSchema)]
pub struct CapturedVariable {
    pub name: String,
    pub kind: String,
    pub source_type: String,
    pub source_visibility: String,
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
    /// Java field declarations for add_java_fields.
    #[serde(default)]
    pub fields: Option<Vec<JavaFieldSpec>>,
    /// Java constructor parameters for add_java_constructor.
    #[serde(default)]
    pub parameters: Option<Vec<JavaParameterSpec>>,
    /// Java constructor helper: assign this.<param> = <param>.
    #[serde(default)]
    pub assign_to_fields: Option<bool>,
    /// Java field names to move with extract_java_class.
    #[serde(default)]
    pub move_fields: Option<Vec<String>>,
    /// Java delegate field name for caller rewrites or source-side delegate wiring.
    #[serde(default)]
    pub delegate_field: Option<String>,
    /// Java delegate type for source-side delegate wiring.
    #[serde(default)]
    pub delegate_type: Option<String>,
    /// Whether move_java_constant should leave a copy of the constant in the
    /// source file. Default false.
    #[serde(default)]
    pub keep_copy: Option<bool>,
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

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash, schemars::JsonSchema)]
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
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub captured_variables: Vec<CapturedVariable>,
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

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
pub struct RefactorProjectRefs {
    pub status: String,
    pub path: String,
    pub project_dir: String,
    pub relative_path: String,
    pub project_id: String,
    pub rel_path_hash: String,
    pub total_chunks: usize,
    pub matching_chunks: usize,
    pub returned_chunks: usize,
    pub truncated: bool,
    pub chunks: Vec<ProjectFileChunkRef>,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ProjectFileChunkRef {
    pub entity_ref: String,
    pub chunk_hash: String,
    pub occurrence_idx: u32,
    pub chunk_kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub language: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub symbol: Option<String>,
    pub byte_start: u64,
    pub byte_end: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub excerpt: Option<String>,
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

pub fn status(p: &RefactorStatusParams) -> Result<String> {
    let path = resolve_path(p.project_dir.as_deref(), &p.file)?;
    let parsed = parse_source_file(&path)?;
    let report = parse_report(parsed.tree.root_node());
    let mut items = match parsed.language {
        "rust" => rust_status_items(&parsed),
        "java" => java_status_items(&parsed),
        _ => generic_top_level_items(&parsed),
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

pub fn project_refs(p: &RefactorProjectRefsParams) -> Result<String> {
    let project_dir = resolve_project_dir(p.project_dir.as_deref())?;
    let project_dir_arg = project_dir.to_string_lossy().into_owned();
    let path = resolve_path(Some(&project_dir_arg), &p.file)?;
    let relative_path = path
        .strip_prefix(&project_dir)
        .with_context(|| {
            format!(
                "{} is not under project_dir {}",
                path.display(),
                project_dir.display()
            )
        })?
        .to_path_buf();
    let project_id = entity_ref::project_id_for_path(&project_dir)?;
    let rel_path_hash = short_hash(relative_path.to_string_lossy().as_bytes());
    let chunks = chunk_file_for_refs(&path, &relative_path, &project_id, &rel_path_hash)?;
    let total_chunks = chunks.len();
    let include_excerpt = p.include_excerpt.unwrap_or(true);
    let query = p.query.as_deref().map(str::to_lowercase);
    let mut refs = chunks
        .into_iter()
        .map(|chunk| {
            let entity_ref = format!(
                "project_file:{project_id}:{rel_path_hash}:{}:{}",
                chunk.chunk_hash, chunk.occurrence_idx
            );
            ProjectFileChunkRef {
                entity_ref,
                chunk_hash: chunk.chunk_hash,
                occurrence_idx: chunk.occurrence_idx,
                chunk_kind: chunk.chunk_kind,
                language: chunk.language,
                symbol: chunk.symbol,
                byte_start: chunk.byte_start,
                byte_end: chunk.byte_end,
                excerpt: include_excerpt.then(|| excerpt(&chunk.content, 320)),
            }
        })
        .collect::<Vec<_>>();
    if let Some(query) = query.as_deref().filter(|query| !query.is_empty()) {
        refs.retain(|chunk| {
            chunk.entity_ref.to_lowercase().contains(query)
                || chunk.chunk_hash.to_lowercase().contains(query)
                || chunk.chunk_kind.to_lowercase().contains(query)
                || chunk
                    .symbol
                    .as_deref()
                    .is_some_and(|symbol| symbol.to_lowercase().contains(query))
                || chunk
                    .excerpt
                    .as_deref()
                    .is_some_and(|excerpt| excerpt.to_lowercase().contains(query))
        });
    }
    let matching_chunks = refs.len();
    let limit = p.limit.unwrap_or(100).min(1000);
    let truncated = matching_chunks > limit;
    refs.truncate(limit);
    let response = RefactorProjectRefs {
        status: "ok".to_string(),
        path: path_string(&path),
        project_dir: path_string(&project_dir),
        relative_path: relative_path.to_string_lossy().into_owned(),
        project_id,
        rel_path_hash,
        total_chunks,
        matching_chunks,
        returned_chunks: refs.len(),
        truncated,
        chunks: refs,
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
        "rewrite_rust_item_visibility" => plan_rewrite_rust_item_visibility(p),
        "rewrite_rust_field_visibility" => plan_rewrite_rust_field_visibility(p),
        "rust_lsp_rename" => plan_rust_lsp_rename(p),
        "rust_organize_imports" => plan_rust_organize_imports(p),
        "extract_java_methods" => plan_extract_java_methods(p),
        "extract_java_class" => plan_extract_java_class(p),
        "extract_java_nested_classes" => plan_extract_java_nested_classes(p),
        "add_java_fields" => plan_add_java_fields(p),
        "add_java_constructor" => plan_add_java_constructor(p),
        "move_java_field" => plan_move_java_field(p),
        "move_java_constant" => plan_move_java_constant(p),
        "update_java_callers" => plan_update_java_callers(p),
        "add_java_delegate_field" => plan_add_java_delegate_field(p),
        "rewrite_java_visibility" => plan_rewrite_java_visibility(p),
        "java_lsp_organize_imports" => plan_java_lsp_organize_imports(p),
        "add_java_implements" => plan_add_java_implements(p),
        "extract_java_interface" => plan_extract_java_interface(p),
        "migrate_java_type_usages" => plan_migrate_java_type_usages(p),
        "move_file" => plan_move_file(p),
        "replace_text" => plan_replace_text(p),
        "write_file" => plan_write_file(p),
        "ensure_toml_table" => plan_ensure_toml_table(p),
        other => bail!(
            "unsupported refactor plan kind `{other}`; supported: extract_rust_items, extract_rust_impl_methods, delete_rust_items, add_rust_router_to_sum, add_rust_mod_decl, add_rust_use_decl, copy_rust_mod_decls, rewrite_rust_mod_visibility, rewrite_rust_item_visibility, rewrite_rust_field_visibility, rust_lsp_rename, rust_organize_imports, extract_java_methods, extract_java_class, extract_java_nested_classes, add_java_fields, add_java_constructor, move_java_field, move_java_constant, update_java_callers, add_java_delegate_field, rewrite_java_visibility, java_lsp_organize_imports, add_java_implements, extract_java_interface, migrate_java_type_usages, move_file, replace_text, write_file, ensure_toml_table"
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
                let command_result = match run_validation_command(&project_dir, command, args, cwd)
                {
                    Ok(result) => result,
                    Err(err) => {
                        let title = command_display(command, args);
                        reports.push(RefactorRunStepReport {
                            index: idx,
                            op: "command".to_string(),
                            status: "failed".to_string(),
                            kind: None,
                            title: Some(title.clone()),
                            files: touched_paths.iter().map(|path| path_string(path)).collect(),
                            validations: Vec::new(),
                            error: Some(format!("{err:#}")),
                        });
                        let rollback_errors = restore_snapshots(&snapshots);
                        return Ok(serde_json::to_string_pretty(&RefactorRunResponse {
                            status: "step_failed".to_string(),
                            title: p.title.clone(),
                            dry_run: false,
                            steps: reports,
                            files_written,
                            rolled_back: rollback_errors.is_empty(),
                            error: Some(format!("command failed: {title}")),
                            rollback_errors,
                        })?);
                    }
                };
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
    if command.chars().any(char::is_whitespace) {
        return Ok(CommandStepResult {
            success: false,
            error: Some(format!(
                "command must be an executable path without whitespace; put arguments in args, e.g. command=\"cargo\", args=[\"fmt\"], not command=\"cargo fmt\""
            )),
        });
    }
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
        captured_variables: Vec::new(),
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
        captured_variables: Vec::new(),
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
        captured_variables: Vec::new(),
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
        captured_variables: Vec::new(),
    };

    validate_plan_shape(&plan)?;
    Ok(serde_json::to_string_pretty(&plan)?)
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

pub(crate) fn resolve_path(project_dir: Option<&str>, path: &str) -> Result<PathBuf> {
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

fn resolve_project_dir(project_dir: Option<&str>) -> Result<PathBuf> {
    let root = match project_dir {
        Some(project_dir) => PathBuf::from(project_dir),
        None => std::env::current_dir()?,
    };
    root.canonicalize()
        .with_context(|| format!("failed to canonicalize project_dir {}", root.display()))
}

fn chunk_file_for_refs(
    abs_path: &Path,
    rel_path: &Path,
    project_id: &str,
    rel_path_hash: &str,
) -> Result<Vec<chunker::Chunk>> {
    let bytes =
        fs::read(abs_path).with_context(|| format!("failed to read {}", abs_path.display()))?;
    let sniff_len = bytes.len().min(4096);
    let mut chunks = Vec::new();
    for candidate in chunker::default_registry() {
        if !candidate.claims(rel_path, &bytes[..sniff_len]) {
            continue;
        }
        chunks = candidate.chunk(rel_path, &bytes)?.0;
        break;
    }
    if chunks.is_empty() && !bytes.is_empty() {
        bail!("no chunker claimed {}", rel_path.display());
    }
    let chunks = chunks
        .into_iter()
        .enumerate()
        .map(|(idx, mut chunk)| {
            chunk.project_id = project_id.to_string();
            chunk.file_path = rel_path.to_path_buf();
            chunk.rel_path_hash = rel_path_hash.to_string();
            chunk.chunk_hash = sha256_hex(chunk.content.as_bytes());
            chunk.occurrence_idx = idx as u32;
            chunk
        })
        .collect::<Vec<_>>();
    Ok(bound_chunks_for_refs(&chunks))
}

fn bound_chunks_for_refs(chunks: &[chunker::Chunk]) -> Vec<chunker::Chunk> {
    chunks
        .iter()
        .flat_map(|chunk| {
            if chunk.content.len() <= chunker::MAX_CHUNK_BYTES {
                return vec![chunk.clone()];
            }
            let mut out = Vec::new();
            let mut start = 0usize;
            while start < chunk.content.len() {
                let mut end = (start + chunker::MAX_CHUNK_BYTES).min(chunk.content.len());
                while !chunk.content.is_char_boundary(end) {
                    end -= 1;
                }
                let mut split = chunk.clone();
                split.content = chunk.content[start..end].to_string();
                split.byte_start = chunk.byte_start + start as u64;
                split.byte_end = chunk.byte_start + end as u64;
                split.chunk_hash = sha256_hex(split.content.as_bytes());
                split.occurrence_idx = out.len() as u32;
                out.push(split);
                start = end;
            }
            out
        })
        .enumerate()
        .map(|(idx, mut chunk)| {
            chunk.occurrence_idx = idx as u32;
            chunk
        })
        .collect()
}

fn short_hash(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    let digest = hasher.finalize();
    hex::encode(&digest[..4])
}

fn excerpt(text: &str, max_chars: usize) -> String {
    let compact = text.split_whitespace().collect::<Vec<_>>().join(" ");
    compact.chars().take(max_chars).collect()
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

fn parse_source(language: &str, source: &str) -> Result<Tree> {
    let mut parser = parser_for_language(language)?;
    parser
        .parse(source, None)
        .ok_or_else(|| anyhow!("tree-sitter {language} parser returned no tree"))
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

pub(crate) fn parse_report(root: Node<'_>) -> ParseReport {
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
