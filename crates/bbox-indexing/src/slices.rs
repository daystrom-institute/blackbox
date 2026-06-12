use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use rmcp::schemars;
use serde::de::Error as _;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::projects::ProjectRecord;
use bbox_refactor as refactor;
use bbox_refactor::{
    self, FileEdit, PlanStatus, RefactorApplyParams, RefactorPlan, RefactorStatusParams,
    SemanticStatus, TextEdit, parse_validation_step_for_path, path_string, sha256_hex,
    validate_plan_shape,
};

#[derive(Debug, Clone, Deserialize, schemars::JsonSchema)]
pub struct SliceReadParams {
    pub file: String,
    #[serde(default)]
    pub project_dir: Option<String>,
    /// Selector as a JSON object or MCP-friendly JSON string, e.g. {"type":"lines","start_line":10,"end_line":20}.
    #[serde(deserialize_with = "deserialize_slice_range_selector")]
    pub range: SliceRangeSelector,
    /// Number of surrounding whole lines to return before and after the selected text. Defaults to 3.
    #[serde(default)]
    pub context_lines: Option<usize>,
}

#[derive(Debug, Clone, Deserialize, schemars::JsonSchema)]
pub struct SliceMoveParams {
    #[serde(default)]
    pub project_dir: Option<String>,
    pub source: String,
    /// Selector as a JSON object or MCP-friendly JSON string.
    #[serde(deserialize_with = "deserialize_slice_range_selector")]
    pub source_range: SliceRangeSelector,
    pub target: String,
    /// Insert selector as a JSON object or MCP-friendly JSON string, e.g. {"type":"append"}.
    #[serde(deserialize_with = "deserialize_insert_selector")]
    pub insert: InsertSelector,
    #[serde(flatten)]
    pub apply: SliceApplyOptions,
}

#[derive(Debug, Clone, Deserialize, schemars::JsonSchema)]
pub struct SliceCopyParams {
    #[serde(default)]
    pub project_dir: Option<String>,
    pub source: String,
    /// Selector as a JSON object or MCP-friendly JSON string.
    #[serde(deserialize_with = "deserialize_slice_range_selector")]
    pub source_range: SliceRangeSelector,
    pub target: String,
    /// Insert selector as a JSON object or MCP-friendly JSON string.
    #[serde(deserialize_with = "deserialize_insert_selector")]
    pub insert: InsertSelector,
    #[serde(flatten)]
    pub apply: SliceApplyOptions,
}

#[derive(Debug, Clone, Deserialize, schemars::JsonSchema)]
pub struct SliceDeleteParams {
    #[serde(default)]
    pub project_dir: Option<String>,
    pub source: String,
    /// Selector as a JSON object or MCP-friendly JSON string.
    #[serde(deserialize_with = "deserialize_slice_range_selector")]
    pub source_range: SliceRangeSelector,
    #[serde(flatten)]
    pub apply: SliceApplyOptions,
}

#[derive(Debug, Clone, Deserialize, schemars::JsonSchema)]
pub struct SliceInsertTextParams {
    #[serde(default)]
    pub project_dir: Option<String>,
    pub target: String,
    /// Insert selector as a JSON object or MCP-friendly JSON string.
    #[serde(deserialize_with = "deserialize_insert_selector")]
    pub insert: InsertSelector,
    pub text: String,
    #[serde(flatten)]
    pub apply: SliceApplyOptions,
}

#[derive(Debug, Clone, Deserialize, schemars::JsonSchema)]
pub struct SliceReplaceParams {
    #[serde(default)]
    pub project_dir: Option<String>,
    pub target: String,
    /// Selector as a JSON object or MCP-friendly JSON string.
    #[serde(deserialize_with = "deserialize_slice_range_selector")]
    pub target_range: SliceRangeSelector,
    #[serde(default, alias = "text")]
    pub new_text: Option<String>,
    #[serde(default)]
    pub source: Option<String>,
    /// Selector as a JSON object or MCP-friendly JSON string.
    #[serde(
        default,
        deserialize_with = "deserialize_optional_slice_range_selector"
    )]
    pub source_range: Option<SliceRangeSelector>,
    #[serde(flatten)]
    pub apply: SliceApplyOptions,
}

#[derive(Debug, Clone, Default, Deserialize, schemars::JsonSchema)]
pub struct SliceApplyOptions {
    /// Must be true to write. When omitted/false, mutation tools return a dry-run plan.
    #[serde(default)]
    pub confirm: Option<bool>,
    /// Default false. When false, confirmed writes refuse files that are dirty in git.
    #[serde(default)]
    pub allow_dirty_worktree: Option<bool>,
    /// Default false. When false, confirmed writes refuse paths outside registered projects.
    #[serde(default)]
    pub allow_unregistered_paths: Option<bool>,
    /// Caller working directory for cross-worktree apply protection. Defaults to project_dir.
    #[serde(default)]
    pub cwd: Option<String>,
    /// Default false. Bypasses cross-worktree apply protection.
    #[serde(default)]
    pub force_path: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SliceRangeSelector {
    /// 1-based inclusive line range. The selected bytes include line endings.
    Lines { start_line: usize, end_line: usize },
    /// Select text between two literal markers. Markers are excluded by default.
    Markers {
        start_marker: String,
        end_marker: String,
        #[serde(default)]
        include_markers: bool,
        #[serde(default)]
        occurrence: Option<usize>,
    },
    /// Select a literal text occurrence. `occurrence` is 1-based; omit it only when unique.
    ExactText {
        text: String,
        #[serde(default)]
        occurrence: Option<usize>,
    },
    /// Select a UTF-8-aligned byte range, end-exclusive.
    Bytes { start: usize, end: usize },
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum InsertSelector {
    /// Insert before or after a 1-based line. On an empty target, line 1 maps to byte 0.
    Line {
        line: usize,
        placement: LinePlacement,
    },
    BeforeMarker {
        marker: String,
        #[serde(default)]
        occurrence: Option<usize>,
    },
    AfterMarker {
        marker: String,
        #[serde(default)]
        occurrence: Option<usize>,
    },
    Prepend,
    Append,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum LinePlacement {
    Before,
    After,
}

fn deserialize_slice_range_selector<'de, D>(
    deserializer: D,
) -> std::result::Result<SliceRangeSelector, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = Value::deserialize(deserializer)?;
    slice_range_selector_from_value(value).map_err(D::Error::custom)
}

fn deserialize_optional_slice_range_selector<'de, D>(
    deserializer: D,
) -> std::result::Result<Option<SliceRangeSelector>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let Some(value) = Option::<Value>::deserialize(deserializer)? else {
        return Ok(None);
    };
    slice_range_selector_from_value(value)
        .map(Some)
        .map_err(D::Error::custom)
}

fn deserialize_insert_selector<'de, D>(
    deserializer: D,
) -> std::result::Result<InsertSelector, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = Value::deserialize(deserializer)?;
    insert_selector_from_value(value).map_err(D::Error::custom)
}

fn slice_range_selector_from_value(
    value: Value,
) -> std::result::Result<SliceRangeSelector, serde_json::Error> {
    match value {
        Value::String(raw) => serde_json::from_str(&raw),
        other => serde_json::from_value(other),
    }
}

fn insert_selector_from_value(
    value: Value,
) -> std::result::Result<InsertSelector, serde_json::Error> {
    match value {
        Value::String(raw) => serde_json::from_str(&raw),
        other => serde_json::from_value(other),
    }
}

#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct SliceReadResponse {
    pub status: String,
    pub file: String,
    pub hashes: SliceReadHashes,
    pub range: ResolvedRange,
    pub text: String,
    pub before_context: String,
    pub after_context: String,
    pub parse_status: String,
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct SliceReadHashes {
    pub file_sha256: String,
    pub slice_sha256: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct SliceMutationResponse {
    pub status: String,
    pub operation: String,
    pub dry_run: bool,
    pub previews: Vec<SliceFilePreview>,
    pub plan: RefactorPlan,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub apply_result: Option<Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct SliceFilePreview {
    pub path: String,
    pub original_sha256: String,
    pub new_sha256: String,
    pub edit_count: usize,
}

#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct ResolvedRange {
    pub byte_start: usize,
    pub byte_end: usize,
    pub line_start: usize,
    pub column_start: usize,
    pub line_end: usize,
    pub column_end: usize,
}

#[derive(Debug, Clone)]
struct ResolvedSlice {
    range: ResolvedRange,
    text: String,
}

#[derive(Debug, Clone)]
struct FileSnapshot {
    path: PathBuf,
    text: String,
    original_sha256: String,
}

pub fn read_slice(p: &SliceReadParams) -> Result<String> {
    let path = refactor::resolve_path(p.project_dir.as_deref(), &p.file)?;
    let snapshot = read_existing_snapshot(&path)?;
    let slice = resolve_slice(&snapshot.text, &p.range)?;
    let context_lines = p.context_lines.unwrap_or(3).min(50);
    let (before_context, after_context) =
        surrounding_context(&snapshot.text, &slice.range, context_lines);
    let (parse_status, mut warnings) = parse_status_for_read(&snapshot.path);
    if slice.text.is_empty() {
        warnings.push("empty_slice".to_string());
    }
    Ok(serde_json::to_string_pretty(&SliceReadResponse {
        status: "ok".to_string(),
        file: path_string(&snapshot.path),
        hashes: SliceReadHashes {
            file_sha256: snapshot.original_sha256,
            slice_sha256: sha256_hex(slice.text.as_bytes()),
        },
        range: slice.range,
        text: slice.text,
        before_context,
        after_context,
        parse_status,
        warnings,
    })?)
}

pub fn move_slice(p: &SliceMoveParams, projects: &[ProjectRecord]) -> Result<String> {
    let source_path = refactor::resolve_path(p.project_dir.as_deref(), &p.source)?;
    let target_path = refactor::resolve_path(p.project_dir.as_deref(), &p.target)?;
    let source = read_existing_snapshot(&source_path)?;
    let target = read_snapshot_or_empty(&target_path)?;
    let slice = resolve_slice(&source.text, &p.source_range)?;
    let insert = resolve_insert(&target.text, &p.insert)?;

    if same_path(&source.path, &target.path) {
        validate_same_file_move(slice.range.byte_start, slice.range.byte_end, insert)?;
        let edits = vec![
            TextEdit {
                byte_start: slice.range.byte_start,
                byte_end: slice.range.byte_end,
                replacement: String::new(),
            },
            TextEdit {
                byte_start: insert,
                byte_end: insert,
                replacement: slice.text,
            },
        ];
        let plan = plan_from_edits("bbox_slice_move", "move text slice", vec![(source, edits)])?;
        finish_mutation(
            "bbox_slice_move",
            p.project_dir.as_deref(),
            &p.apply,
            plan,
            projects,
        )
    } else {
        let source_edit = TextEdit {
            byte_start: slice.range.byte_start,
            byte_end: slice.range.byte_end,
            replacement: String::new(),
        };
        let target_edit = TextEdit {
            byte_start: insert,
            byte_end: insert,
            replacement: slice.text,
        };
        let plan = plan_from_edits(
            "bbox_slice_move",
            "move text slice",
            vec![(source, vec![source_edit]), (target, vec![target_edit])],
        )?;
        finish_mutation(
            "bbox_slice_move",
            p.project_dir.as_deref(),
            &p.apply,
            plan,
            projects,
        )
    }
}

pub fn copy_slice(p: &SliceCopyParams, projects: &[ProjectRecord]) -> Result<String> {
    let source_path = refactor::resolve_path(p.project_dir.as_deref(), &p.source)?;
    let target_path = refactor::resolve_path(p.project_dir.as_deref(), &p.target)?;
    let source = read_existing_snapshot(&source_path)?;
    let target = read_snapshot_or_empty(&target_path)?;
    let slice = resolve_slice(&source.text, &p.source_range)?;
    let insert = resolve_insert(&target.text, &p.insert)?;
    let edit = TextEdit {
        byte_start: insert,
        byte_end: insert,
        replacement: slice.text,
    };
    let plan = plan_from_edits(
        "bbox_slice_copy",
        "copy text slice",
        vec![(target, vec![edit])],
    )?;
    finish_mutation(
        "bbox_slice_copy",
        p.project_dir.as_deref(),
        &p.apply,
        plan,
        projects,
    )
}

pub fn delete_slice(p: &SliceDeleteParams, projects: &[ProjectRecord]) -> Result<String> {
    let source_path = refactor::resolve_path(p.project_dir.as_deref(), &p.source)?;
    let source = read_existing_snapshot(&source_path)?;
    let slice = resolve_slice(&source.text, &p.source_range)?;
    let edit = TextEdit {
        byte_start: slice.range.byte_start,
        byte_end: slice.range.byte_end,
        replacement: String::new(),
    };
    let plan = plan_from_edits(
        "bbox_slice_delete",
        "delete text slice",
        vec![(source, vec![edit])],
    )?;
    finish_mutation(
        "bbox_slice_delete",
        p.project_dir.as_deref(),
        &p.apply,
        plan,
        projects,
    )
}

pub fn insert_text_slice(p: &SliceInsertTextParams, projects: &[ProjectRecord]) -> Result<String> {
    let target_path = refactor::resolve_path(p.project_dir.as_deref(), &p.target)?;
    let target = read_snapshot_or_empty(&target_path)?;
    let insert = resolve_insert(&target.text, &p.insert)?;
    let edit = TextEdit {
        byte_start: insert,
        byte_end: insert,
        replacement: p.text.clone(),
    };
    let plan = plan_from_edits(
        "bbox_slice_insert_text",
        "insert text slice",
        vec![(target, vec![edit])],
    )?;
    finish_mutation(
        "bbox_slice_insert_text",
        p.project_dir.as_deref(),
        &p.apply,
        plan,
        projects,
    )
}

pub fn replace_slice(p: &SliceReplaceParams, projects: &[ProjectRecord]) -> Result<String> {
    let replacement = match (&p.new_text, &p.source, &p.source_range) {
        (Some(_), Some(_), _) | (Some(_), _, Some(_)) => {
            bail!("error.replacement_ambiguous: pass either text or source+source_range, not both")
        }
        (None, Some(_), None) | (None, None, Some(_)) | (None, None, None) => {
            bail!("error.replacement_missing: pass either text or source+source_range")
        }
        (Some(new_text), None, None) => new_text.clone(),
        (None, Some(source), Some(source_range)) => {
            let source_path = refactor::resolve_path(p.project_dir.as_deref(), source)?;
            let source = read_existing_snapshot(&source_path)?;
            resolve_slice(&source.text, source_range)?.text
        }
    };
    let target_path = refactor::resolve_path(p.project_dir.as_deref(), &p.target)?;
    let target = read_existing_snapshot(&target_path)?;
    let target_slice = resolve_slice(&target.text, &p.target_range)?;
    let edit = TextEdit {
        byte_start: target_slice.range.byte_start,
        byte_end: target_slice.range.byte_end,
        replacement,
    };
    let plan = plan_from_edits(
        "bbox_slice_replace",
        "replace text slice",
        vec![(target, vec![edit])],
    )?;
    finish_mutation(
        "bbox_slice_replace",
        p.project_dir.as_deref(),
        &p.apply,
        plan,
        projects,
    )
}

fn finish_mutation(
    operation: &str,
    project_dir: Option<&str>,
    options: &SliceApplyOptions,
    plan: RefactorPlan,
    projects: &[ProjectRecord],
) -> Result<String> {
    let previews = previews_for_plan(&plan)?;
    let effective_projects = effective_slice_projects(project_dir, projects);
    if options.confirm == Some(true) {
        let plan_value = serde_json::to_value(&plan)?;
        let apply_result_raw = refactor::apply(
            &RefactorApplyParams {
                plan: plan_value,
                plan_path: None,
                confirm: Some(true),
                allow_dirty_worktree: options.allow_dirty_worktree,
                allow_unregistered_paths: options.allow_unregistered_paths,
                cwd: options
                    .cwd
                    .clone()
                    .or_else(|| project_dir.map(str::to_string)),
                force_path: options.force_path,
            },
            &effective_projects,
        )?;
        let apply_result: Value =
            serde_json::from_str(&apply_result_raw).unwrap_or(Value::String(apply_result_raw));
        let apply_status = apply_result
            .get("status")
            .and_then(Value::as_str)
            .unwrap_or("applied")
            .to_string();
        let mut response_plan = plan;
        let status = match apply_status.as_str() {
            "ok" => {
                response_plan.dry_run = false;
                response_plan.plan_status = PlanStatus::Applied;
                "applied"
            }
            "validation_failed" => "blocked",
            _ => "error",
        }
        .to_string();
        Ok(serde_json::to_string_pretty(&SliceMutationResponse {
            status,
            operation: operation.to_string(),
            dry_run: false,
            previews,
            plan: response_plan,
            apply_result: Some(apply_result),
        })?)
    } else {
        Ok(serde_json::to_string_pretty(&SliceMutationResponse {
            status: "dry_run".to_string(),
            operation: operation.to_string(),
            dry_run: true,
            previews,
            plan,
            apply_result: None,
        })?)
    }
}

fn effective_slice_projects(
    project_dir: Option<&str>,
    projects: &[ProjectRecord],
) -> Vec<ProjectRecord> {
    let mut out = projects.to_vec();
    if let Some(record) = crate::projects::managed_fleet_worktree_project(project_dir, projects) {
        out.push(record);
    }
    out
}

fn plan_from_edits(
    kind: &str,
    title: &str,
    files: Vec<(FileSnapshot, Vec<TextEdit>)>,
) -> Result<RefactorPlan> {
    let mut seen = BTreeSet::new();
    let mut edits = Vec::new();
    for (snapshot, file_edits) in files {
        let path = path_string(&snapshot.path);
        if !seen.insert(path.clone()) {
            bail!("error.duplicate_path: generated duplicate edit entry for {path}");
        }
        refactor::apply_text_edits(&snapshot.text, &file_edits)
            .with_context(|| format!("previewing edits for {path}"))?;
        edits.push(FileEdit {
            path,
            original_sha256: snapshot.original_sha256,
            edits: file_edits,
            new_text: None,
        });
    }
    let validations = edits
        .iter()
        .flat_map(|edit| parse_validation_step_for_path(Path::new(&edit.path)))
        .collect::<Vec<_>>();
    let plan = RefactorPlan {
        title: title.to_string(),
        kind: kind.to_string(),
        semantic_status: SemanticStatus::SyntaxOnly,
        dry_run: true,
        file_moves: Vec::new(),
        file_creates: Vec::new(),
        edits,
        validations,
        items: Vec::new(),
        leftovers: Vec::new(),
        captured_variables: Vec::new(),
        remaining_source_accessors: Vec::new(),
        remaining_source_constant_refs: Vec::new(),
        external_calls: Vec::new(),
        inherited_dependencies: Vec::new(),
        deep_analysis: None,
        plan_status: PlanStatus::Planned,
        fixme_count: None,
        operator_opt_outs_used: Vec::new(),
    };
    validate_plan_shape(&plan)?;
    Ok(plan)
}

fn previews_for_plan(plan: &RefactorPlan) -> Result<Vec<SliceFilePreview>> {
    plan.edits
        .iter()
        .map(|edit| {
            let original = read_original_for_preview(Path::new(&edit.path), &edit.original_sha256)?;
            let original_text = String::from_utf8(original)
                .with_context(|| format!("{} is not valid utf-8", edit.path))?;
            let next = refactor::apply_text_edits(&original_text, &edit.edits)?;
            Ok(SliceFilePreview {
                path: edit.path.clone(),
                original_sha256: edit.original_sha256.clone(),
                new_sha256: sha256_hex(next.as_bytes()),
                edit_count: edit.edits.len(),
            })
        })
        .collect()
}

fn parse_status_for_read(path: &Path) -> (String, Vec<String>) {
    if parse_validation_step_for_path(path).is_empty() {
        return ("unsupported".to_string(), Vec::new());
    }
    let params = RefactorStatusParams {
        file: path_string(path),
        project_dir: None,
        item_names: None,
        item_kinds: None,
        limit: Some(0),
        include_attributes: Some(false),
    };
    match refactor::status(&params) {
        Ok(raw) => {
            let parsed = serde_json::from_str::<Value>(&raw).unwrap_or(Value::Null);
            let parse = parsed.get("parse").unwrap_or(&Value::Null);
            let has_error = parse
                .get("has_error")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let error_nodes = parse
                .get("error_nodes")
                .and_then(Value::as_u64)
                .unwrap_or(0);
            let missing_nodes = parse
                .get("missing_nodes")
                .and_then(Value::as_u64)
                .unwrap_or(0);
            if has_error || error_nodes > 0 || missing_nodes > 0 {
                ("has_error".to_string(), Vec::new())
            } else {
                ("ok".to_string(), Vec::new())
            }
        }
        Err(err) => (
            "has_error".to_string(),
            vec![format!("parse_status_unavailable: {err:#}")],
        ),
    }
}

fn surrounding_context(
    source: &str,
    range: &ResolvedRange,
    context_lines: usize,
) -> (String, String) {
    if context_lines == 0 || source.is_empty() {
        return (String::new(), String::new());
    }
    let selected_start_line = range.line_start;
    let selected_end_line = selection_end_line(source, range.byte_end, range.line_end);
    let before_start = selected_start_line.saturating_sub(context_lines).max(1);
    let before_end = selected_start_line.saturating_sub(1);
    let after_start = selected_end_line.saturating_add(1);
    let after_end = selected_end_line.saturating_add(context_lines);
    (
        line_text_range(source, before_start, before_end),
        line_text_range(source, after_start, after_end),
    )
}

fn selection_end_line(source: &str, byte_end: usize, fallback_line: usize) -> usize {
    if byte_end == 0 {
        return fallback_line;
    }
    let bytes = source.as_bytes();
    if byte_end <= bytes.len() && bytes.get(byte_end.saturating_sub(1)) == Some(&b'\n') {
        return line_col(source, byte_end - 1).0;
    }
    fallback_line
}

fn line_text_range(source: &str, start_line: usize, end_line: usize) -> String {
    if start_line == 0 || start_line > end_line {
        return String::new();
    }
    let spans = line_spans(source);
    let Some((start, _)) = spans.get(start_line - 1).copied() else {
        return String::new();
    };
    let end = spans
        .get(end_line.saturating_sub(1))
        .map(|(_, end)| *end)
        .unwrap_or(source.len());
    source[start..end].to_string()
}

fn line_spans(source: &str) -> Vec<(usize, usize)> {
    let mut spans = Vec::new();
    let mut start = 0;
    for (idx, byte) in source.bytes().enumerate() {
        if byte == b'\n' {
            spans.push((start, idx + 1));
            start = idx + 1;
        }
    }
    if start < source.len() {
        spans.push((start, source.len()));
    }
    spans
}

fn read_existing_snapshot(path: &Path) -> Result<FileSnapshot> {
    let bytes = fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    let text = String::from_utf8(bytes)
        .with_context(|| format!("{} is not valid utf-8", path.display()))?;
    Ok(FileSnapshot {
        path: path.to_path_buf(),
        original_sha256: sha256_hex(text.as_bytes()),
        text,
    })
}

fn read_snapshot_or_empty(path: &Path) -> Result<FileSnapshot> {
    match fs::read(path) {
        Ok(bytes) => {
            let text = String::from_utf8(bytes)
                .with_context(|| format!("{} is not valid utf-8", path.display()))?;
            Ok(FileSnapshot {
                path: path.to_path_buf(),
                original_sha256: sha256_hex(text.as_bytes()),
                text,
            })
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(FileSnapshot {
            path: path.to_path_buf(),
            original_sha256: sha256_hex(&[]),
            text: String::new(),
        }),
        Err(err) => Err(err).with_context(|| format!("reading {}", path.display())),
    }
}

fn read_original_for_preview(path: &Path, expected_sha256: &str) -> Result<Vec<u8>> {
    match fs::read(path) {
        Ok(bytes) => Ok(bytes),
        Err(err)
            if err.kind() == std::io::ErrorKind::NotFound && expected_sha256 == sha256_hex(&[]) =>
        {
            Ok(Vec::new())
        }
        Err(err) => Err(err).with_context(|| format!("reading {}", path.display())),
    }
}

fn resolve_slice(source: &str, selector: &SliceRangeSelector) -> Result<ResolvedSlice> {
    let (byte_start, byte_end) = match selector {
        SliceRangeSelector::Lines {
            start_line,
            end_line,
        } => resolve_line_range(source, *start_line, *end_line)?,
        SliceRangeSelector::Markers {
            start_marker,
            end_marker,
            include_markers,
            occurrence,
        } => resolve_marker_range(
            source,
            start_marker,
            end_marker,
            *include_markers,
            *occurrence,
        )?,
        SliceRangeSelector::ExactText { text, occurrence } => {
            let start = resolve_literal_occurrence(source, text, *occurrence, "exact_text")?;
            (start, start + text.len())
        }
        SliceRangeSelector::Bytes { start, end } => resolve_byte_range(source, *start, *end)?,
    };
    let (line_start, column_start) = line_col(source, byte_start);
    let (line_end, column_end) = line_col(source, byte_end);
    Ok(ResolvedSlice {
        range: ResolvedRange {
            byte_start,
            byte_end,
            line_start,
            column_start,
            line_end,
            column_end,
        },
        text: source[byte_start..byte_end].to_string(),
    })
}

fn resolve_insert(source: &str, selector: &InsertSelector) -> Result<usize> {
    match selector {
        InsertSelector::Line { line, placement } => match placement {
            LinePlacement::Before => line_start_offset(source, *line),
            LinePlacement::After => line_after_offset(source, *line),
        },
        InsertSelector::BeforeMarker { marker, occurrence } => {
            resolve_literal_occurrence(source, marker, *occurrence, "marker")
        }
        InsertSelector::AfterMarker { marker, occurrence } => {
            resolve_literal_occurrence(source, marker, *occurrence, "marker")
                .map(|start| start + marker.len())
        }
        InsertSelector::Prepend => Ok(0),
        InsertSelector::Append => Ok(source.len()),
    }
}

fn resolve_line_range(source: &str, start_line: usize, end_line: usize) -> Result<(usize, usize)> {
    if start_line == 0 || end_line == 0 || start_line > end_line {
        bail!("error.invalid_line_range: expected 1-based start_line <= end_line");
    }
    let start = line_start_offset(source, start_line)?;
    let end = match line_start_offset(source, end_line.saturating_add(1)) {
        Ok(offset) => offset,
        Err(_) => source.len(),
    };
    Ok((start, end))
}

fn resolve_byte_range(source: &str, start: usize, end: usize) -> Result<(usize, usize)> {
    if start > end || end > source.len() {
        bail!("error.invalid_byte_range: expected 0 <= start <= end <= file length");
    }
    if !source.is_char_boundary(start) || !source.is_char_boundary(end) {
        bail!("error.invalid_byte_range: range is not UTF-8 aligned");
    }
    Ok((start, end))
}

fn resolve_marker_range(
    source: &str,
    start_marker: &str,
    end_marker: &str,
    include_markers: bool,
    occurrence: Option<usize>,
) -> Result<(usize, usize)> {
    if start_marker.is_empty() || end_marker.is_empty() {
        bail!("error.empty_marker: markers must be non-empty");
    }
    let start = resolve_literal_occurrence(source, start_marker, occurrence, "start_marker")?;
    let after_start = start + start_marker.len();
    let Some(relative_end) = source[after_start..].find(end_marker) else {
        bail!("error.end_marker_not_found: end_marker was not found after start_marker");
    };
    let end_start = after_start + relative_end;
    if include_markers {
        Ok((start, end_start + end_marker.len()))
    } else {
        Ok((after_start, end_start))
    }
}

fn resolve_literal_occurrence(
    source: &str,
    needle: &str,
    occurrence: Option<usize>,
    label: &str,
) -> Result<usize> {
    if needle.is_empty() {
        bail!("error.empty_{label}: selector text must be non-empty");
    }
    let matches = source
        .match_indices(needle)
        .map(|(idx, _)| idx)
        .collect::<Vec<_>>();
    if matches.is_empty() {
        bail!("error.{label}_not_found: selector text was not found");
    }
    if let Some(occurrence) = occurrence {
        if occurrence == 0 {
            bail!("error.invalid_occurrence: occurrence is 1-based");
        }
        return matches.get(occurrence - 1).copied().ok_or_else(|| {
            anyhow!("error.{label}_not_found: occurrence {occurrence} was not found")
        });
    }
    if matches.len() > 1 {
        bail!(
            "error.ambiguous_{label}: selector matched {} times; pass occurrence",
            matches.len()
        );
    }
    Ok(matches[0])
}

fn line_start_offset(source: &str, line: usize) -> Result<usize> {
    if line == 0 {
        bail!("error.invalid_line: line is 1-based");
    }
    if source.is_empty() {
        if line == 1 {
            return Ok(0);
        }
        bail!("error.line_out_of_range: line {line} is outside the empty target");
    }
    let starts = line_starts(source);
    starts
        .get(line - 1)
        .copied()
        .ok_or_else(|| anyhow!("error.line_out_of_range: line {line} is outside the file"))
}

fn line_after_offset(source: &str, line: usize) -> Result<usize> {
    if line == 0 {
        bail!("error.invalid_line: line is 1-based");
    }
    if source.is_empty() {
        if line == 1 {
            return Ok(0);
        }
        bail!("error.line_out_of_range: line {line} is outside the empty target");
    }
    let starts = line_starts(source);
    if line > starts.len() {
        bail!("error.line_out_of_range: line {line} is outside the file");
    }
    Ok(starts.get(line).copied().unwrap_or(source.len()))
}

fn line_starts(source: &str) -> Vec<usize> {
    let mut starts = vec![0];
    for (idx, byte) in source.bytes().enumerate() {
        if byte == b'\n' && idx + 1 < source.len() {
            starts.push(idx + 1);
        }
    }
    starts
}

fn line_col(source: &str, idx: usize) -> (usize, usize) {
    let idx = idx.min(source.len());
    let line = source[..idx].bytes().filter(|b| *b == b'\n').count() + 1;
    let line_start = source[..idx].rfind('\n').map(|pos| pos + 1).unwrap_or(0);
    (line, idx - line_start + 1)
}

fn validate_same_file_move(start: usize, end: usize, insert: usize) -> Result<()> {
    if insert == start || insert == end {
        bail!("error.same_file_noop: insertion point leaves the selected slice in place");
    }
    if start < insert && insert < end {
        bail!("error.insert_inside_selection: insertion point is inside the selected slice");
    }
    Ok(())
}

fn same_path(left: &Path, right: &Path) -> bool {
    if left == right {
        return true;
    }
    let canonical_left = left.canonicalize();
    let canonical_right = right.canonicalize();
    matches!((canonical_left, canonical_right), (Ok(l), Ok(r)) if l == r)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    fn lines(start_line: usize, end_line: usize) -> SliceRangeSelector {
        SliceRangeSelector::Lines {
            start_line,
            end_line,
        }
    }

    #[test]
    fn line_selector_includes_trailing_newline() {
        let slice = resolve_slice("one\ntwo\nthree\n", &lines(2, 2)).unwrap();
        assert_eq!(slice.text, "two\n");
        assert_eq!(slice.range.byte_start, 4);
        assert_eq!(slice.range.byte_end, 8);
    }

    #[test]
    fn marker_selector_defaults_to_inner_text() {
        let selector = SliceRangeSelector::Markers {
            start_marker: "<a>".into(),
            end_marker: "</a>".into(),
            include_markers: false,
            occurrence: None,
        };
        let slice = resolve_slice("x<a>body</a>y", &selector).unwrap();
        assert_eq!(slice.text, "body");
    }

    #[test]
    fn read_slice_returns_context_hashes_and_parse_status() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("lib.rs");
        fs::write(&path, "fn a() {}\nfn b() {}\nfn c() {}\n").unwrap();
        let p = SliceReadParams {
            file: "lib.rs".into(),
            project_dir: Some(path_string(dir.path())),
            range: lines(2, 2),
            context_lines: Some(1),
        };
        let response: Value = serde_json::from_str(&read_slice(&p).unwrap()).unwrap();
        assert_eq!(response["text"], "fn b() {}\n");
        assert_eq!(response["before_context"], "fn a() {}\n");
        assert_eq!(response["after_context"], "fn c() {}\n");
        assert_eq!(response["parse_status"], "ok");
        assert_eq!(
            response["hashes"]["slice_sha256"],
            sha256_hex(b"fn b() {}\n")
        );
    }

    #[test]
    fn params_accept_mcp_advertised_json_string_selectors() {
        let read: SliceReadParams = serde_json::from_value(serde_json::json!({
            "file": "lib.rs",
            "range": "{\"type\":\"lines\",\"start_line\":1,\"end_line\":2}"
        }))
        .unwrap();
        assert!(matches!(
            read.range,
            SliceRangeSelector::Lines {
                start_line: 1,
                end_line: 2
            }
        ));

        let mv: SliceMoveParams = serde_json::from_value(serde_json::json!({
            "source": "a.rs",
            "source_range": "{\"type\":\"exact_text\",\"text\":\"fn a() {}\"}",
            "target": "b.rs",
            "insert": "{\"type\":\"append\"}"
        }))
        .unwrap();
        assert!(matches!(mv.insert, InsertSelector::Append));
        assert!(matches!(
            mv.source_range,
            SliceRangeSelector::ExactText { ref text, occurrence: None } if text == "fn a() {}"
        ));
    }

    #[test]
    fn exact_text_requires_occurrence_when_ambiguous() {
        let selector = SliceRangeSelector::ExactText {
            text: "x".into(),
            occurrence: None,
        };
        let err = resolve_slice("x x", &selector).unwrap_err().to_string();
        assert!(err.contains("error.ambiguous_exact_text"));
    }

    #[test]
    fn same_file_move_down_uses_original_insert_offset() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.txt");
        fs::write(&path, "a\nb\nc\n").unwrap();
        let snapshot = read_existing_snapshot(&path).unwrap();
        let slice = resolve_slice(&snapshot.text, &lines(2, 2)).unwrap();
        let insert = resolve_insert(
            &snapshot.text,
            &InsertSelector::Line {
                line: 3,
                placement: LinePlacement::After,
            },
        )
        .unwrap();
        validate_same_file_move(slice.range.byte_start, slice.range.byte_end, insert).unwrap();
        let next = refactor::apply_text_edits(
            &snapshot.text,
            &[
                TextEdit {
                    byte_start: slice.range.byte_start,
                    byte_end: slice.range.byte_end,
                    replacement: String::new(),
                },
                TextEdit {
                    byte_start: insert,
                    byte_end: insert,
                    replacement: slice.text,
                },
            ],
        )
        .unwrap();
        assert_eq!(next, "a\nc\nb\n");
    }

    #[test]
    fn same_file_move_rejects_noop_boundary() {
        let err = validate_same_file_move(2, 5, 5).unwrap_err().to_string();
        assert!(err.contains("error.same_file_noop"));
    }

    #[test]
    fn byte_selector_rejects_non_utf8_boundary() {
        let selector = SliceRangeSelector::Bytes { start: 1, end: 2 };
        let err = resolve_slice("é", &selector).unwrap_err().to_string();
        assert!(err.contains("UTF-8"));
    }

    #[test]
    fn replace_requires_one_replacement_source() {
        let p = SliceReplaceParams {
            project_dir: None,
            target: "x".into(),
            target_range: lines(1, 1),
            new_text: Some("a".into()),
            source: Some("x".into()),
            source_range: None,
            apply: SliceApplyOptions::default(),
        };
        let err = replace_slice(&p, &[]).unwrap_err().to_string();
        assert!(err.contains("error.replacement_ambiguous"));
    }

    #[test]
    fn plan_preview_hashes_new_content() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.txt");
        fs::write(&path, "a\nb\n").unwrap();
        let snapshot = read_existing_snapshot(&path).unwrap();
        let plan = plan_from_edits(
            "bbox_slice_delete",
            "delete text slice",
            vec![(
                snapshot,
                vec![TextEdit {
                    byte_start: 2,
                    byte_end: 4,
                    replacement: String::new(),
                }],
            )],
        )
        .unwrap();
        let previews = previews_for_plan(&plan).unwrap();
        assert_eq!(previews[0].new_sha256, sha256_hex(b"a\n"));
    }

    #[test]
    fn confirmed_insert_text_writes_via_apply() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.txt");
        fs::write(&path, "a\n").unwrap();
        let p = SliceInsertTextParams {
            project_dir: Some(path_string(dir.path())),
            target: "a.txt".into(),
            insert: InsertSelector::Append,
            text: "b\n".into(),
            apply: SliceApplyOptions {
                confirm: Some(true),
                allow_dirty_worktree: None,
                allow_unregistered_paths: Some(true),
                cwd: Some(path_string(dir.path())),
                force_path: None,
            },
        };
        let response: SliceMutationResponse =
            serde_json::from_str(&insert_text_slice(&p, &[]).unwrap()).unwrap();
        assert_eq!(response.status, "applied");
        assert!(!response.dry_run);
        assert_eq!(fs::read_to_string(&path).unwrap(), "a\nb\n");
    }

    #[test]
    fn confirmed_insert_allows_managed_fleet_worktree_for_registered_base_repo() {
        let repo = tempfile::tempdir().unwrap();
        if !git_ok(repo.path(), &["init", "-b", "main"]) {
            return;
        }
        git(repo.path(), &["config", "user.email", "test@example.com"]);
        git(repo.path(), &["config", "user.name", "Test User"]);
        fs::write(repo.path().join("a.txt"), "a\n").unwrap();
        git(repo.path(), &["add", "a.txt"]);
        git(repo.path(), &["commit", "-m", "init"]);

        let worktrees = tempfile::tempdir().unwrap();
        let worktree = worktrees.path().join("task");
        git(
            repo.path(),
            &[
                "worktree",
                "add",
                "-b",
                "bro-fleet/slice-test",
                worktree.to_str().unwrap(),
            ],
        );

        let base = ProjectRecord {
            project_id: "base".into(),
            repo_id: None,
            canonical_path: path_string(&repo.path().canonicalize().unwrap()),
            registered_at: "test".into(),
            is_git_repo: true,
            languages: BTreeSet::new(),
            aliases: Default::default(),
        };
        let p = SliceInsertTextParams {
            project_dir: Some(path_string(&worktree.canonicalize().unwrap())),
            target: "a.txt".into(),
            insert: InsertSelector::Append,
            text: "b\n".into(),
            apply: SliceApplyOptions {
                confirm: Some(true),
                allow_dirty_worktree: None,
                allow_unregistered_paths: None,
                cwd: Some(path_string(&worktree)),
                force_path: None,
            },
        };
        let response: SliceMutationResponse =
            serde_json::from_str(&insert_text_slice(&p, &[base]).unwrap()).unwrap();
        assert_eq!(response.status, "applied");
        assert_eq!(
            fs::read_to_string(worktree.join("a.txt")).unwrap(),
            "a\nb\n"
        );

        git(
            repo.path(),
            &["worktree", "remove", "--force", worktree.to_str().unwrap()],
        );
    }

    #[test]
    fn cross_file_move_dry_run_carries_two_file_edits() {
        let dir = tempfile::tempdir().unwrap();
        let source_path = dir.path().join("a.txt");
        let target_path = dir.path().join("b.txt");
        fs::write(&source_path, "keep\nmove\n").unwrap();
        fs::write(&target_path, "target\n").unwrap();
        let p = SliceMoveParams {
            project_dir: Some(path_string(dir.path())),
            source: "a.txt".into(),
            source_range: lines(2, 2),
            target: "b.txt".into(),
            insert: InsertSelector::Append,
            apply: SliceApplyOptions::default(),
        };
        let response: SliceMutationResponse =
            serde_json::from_str(&move_slice(&p, &[]).unwrap()).unwrap();
        assert_eq!(response.status, "dry_run");
        assert!(response.dry_run);
        assert_eq!(response.plan.edits.len(), 2);
        assert_eq!(response.previews.len(), 2);
    }

    fn git_ok(cwd: &Path, args: &[&str]) -> bool {
        Command::new("git")
            .arg("-C")
            .arg(cwd)
            .args(args)
            .output()
            .map(|out| out.status.success())
            .unwrap_or(false)
    }

    fn git(cwd: &Path, args: &[&str]) {
        let out = Command::new("git")
            .arg("-C")
            .arg(cwd)
            .args(args)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "git {} failed:\nstdout={}\nstderr={}",
            args.join(" "),
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
    }
}
