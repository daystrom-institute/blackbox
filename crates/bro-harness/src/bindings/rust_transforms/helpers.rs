//! Shared helpers for the rust.* transform bindings.
//!
//! The porting recipe (bindings/AGENTS.md) is identical across transforms:
//! run the v1 planner via `bbox_refactor::plan`, decode the `RefactorPlan`,
//! and project its `FileEdit`s into the edits-algebra shape. These helpers
//! centralize the two load-bearing projections so each transform stays a
//! thin adapter:
//!
//!   - `plan_to_changes_creates`: splits FileEdits into hash-anchored
//!     `changes` (for `edits.merge`) and whole-content `creates` (for
//!     `edits.createFile`). The v1 planner emits NEW files as whole-content
//!     `0..0` edits against the empty-file hash; the algebra's stale_span
//!     check would bounce those, so they convert to `creates`.
//!   - `relativize`: workspace-relative form of a plan-emitted absolute
//!     path, tolerant of the canonicalized-root mismatch on symlinked
//!     tempdirs (macOS `/var` vs `/private/var`).
//!
//! NEVER writes. Transforms return `{changes, creates, findings}` for
//! `edits.merge` / `edits.createFile`; `edits.apply` is the only writer.

use std::path::{Component, Path, PathBuf};

use serde_json::{Value, json};

use crate::bindings::ledger::{AuthorityTier, ProvenanceLedger};

/// Empty-file sha256; the v1 planner emits new files as whole-content
/// `0..0` edits against this hash (its apply created missing files).
pub(super) fn empty_sha() -> String {
    bbox_refactor::sha256_hex(&[])
}

/// Workspace-relative form of a plan-emitted absolute path, tolerant of the
/// canonicalized-root mismatch on symlinked tempdirs.
pub(super) fn relativize(root: &Path, path: &str) -> Result<String, String> {
    let p = Path::new(path);
    if let Ok(rel) = p.strip_prefix(root) {
        return Ok(rel.to_string_lossy().to_string());
    }
    if let Ok(canon) = root.canonicalize() {
        if let Ok(rel) = p.strip_prefix(&canon) {
            return Ok(rel.to_string_lossy().to_string());
        }
    }
    Err(format!("plan touches `{path}` outside the worktree root"))
}

/// Reject workspace-escape file args up front: a binding's `file`/`target`
/// must be workspace-relative without `..`.
pub(super) fn resolve_workspace_file(root: &Path, file: &str, tool: &str) -> Result<PathBuf, String> {
    let rel = Path::new(file);
    if rel.is_absolute()
        || rel
            .components()
            .any(|component| matches!(component, Component::ParentDir))
    {
        return Err(format!(
            "{tool}: file must be a workspace-relative path without `..`: {file}"
        ));
    }
    Ok(root.join(rel))
}

/// Split a planner's FileEdits into:
///   - `changes`: hash-anchored span edits for `edits.merge` (existing files)
///   - `creates`: whole-content `{path, content}` for `edits.createFile`
///                (new files the planner emitted as `0..0` empty-hash edits)
///   - `would_change_files` / `would_create_files`: preview metadata
///
/// Mirrors the java.* adapter shape verbatim. `preview_only` zeros the
/// `changes`/`creates` payloads while keeping the metadata, matching the
/// java.* `previewOnly` contract.
pub(super) fn plan_to_changes_creates(
    root: &Path,
    tool: &str,
    file_edits: &[bbox_refactor::FileEdit],
    preview_only: bool,
) -> Result<PlanProjection, String> {
    let empty = empty_sha();
    let mut changes: Vec<Value> = Vec::new();
    let mut creates: Vec<Value> = Vec::new();
    let mut would_change_files: Vec<Value> = Vec::new();
    let mut would_create_files: Vec<Value> = Vec::new();
    for file_edit in file_edits {
        let rel = relativize(root, &file_edit.path)?;
        let is_new_file = file_edit.original_sha256 == empty
            && file_edit
                .edits
                .iter()
                .all(|edit| edit.byte_start == 0 && edit.byte_end == 0);
        if is_new_file {
            let content: String = file_edit
                .edits
                .iter()
                .map(|edit| edit.replacement.as_str())
                .collect();
            would_create_files.push(json!({ "path": rel, "bytes": content.len() }));
            if !preview_only {
                creates.push(json!({ "path": rel, "content": content }));
            }
            continue;
        }
        if !file_edit.edits.is_empty() {
            let replacement_bytes: usize =
                file_edit.edits.iter().map(|edit| edit.replacement.len()).sum();
            would_change_files.push(json!({
                "path": rel,
                "edit_count": file_edit.edits.len(),
                "replacement_bytes": replacement_bytes,
            }));
        }
        if preview_only {
            continue;
        }
        for edit in &file_edit.edits {
            changes.push(json!({
                "span": {
                    "file": rel,
                    "byte_start": edit.byte_start,
                    "byte_end": edit.byte_end,
                    "content_sha256": file_edit.original_sha256,
                },
                "new_text": edit.replacement,
            }));
        }
    }
    let _ = tool; // reserved for richer error context if needed later
    Ok(PlanProjection {
        changes,
        creates,
        would_change_files,
        would_create_files,
    })
}

/// Result of projecting a planner's FileEdits into the edits-algebra shape.
pub(super) struct PlanProjection {
    pub changes: Vec<Value>,
    pub creates: Vec<Value>,
    pub would_change_files: Vec<Value>,
    pub would_create_files: Vec<Value>,
}

/// Decorate a planner error with the DONE-signal hint when the failure is
/// the target-exists refusal (transforms are NOT idempotent over their own
/// output; re-calling after a successful apply hits this refusal).
pub(super) fn done_hint(message: &str) -> String {
    if message.contains("already exists and is non-empty")
        || message.contains("already exists")
        || message.contains("missing or empty target")
    {
        " - if a prior cell already applied this transform, the work is DONE (verify with code.items on the source file); re-calling is only valid against a clean target. store() the result when you need it in later cells."
            .to_string()
    } else {
        String::new()
    }
}

/// Record host-authored changes at `syntax_only` so `edits.apply` can
/// compute the `semantic_status` lineage. The ledger takes a slice of
/// `(&Span, &str)`; this helper owns the intermediate `Vec` so callers
/// stay one-liners. Rust transforms are tree-sitter backed (no LSP
/// authority), so the floor is always `syntax_only`.
pub(super) fn record_in_ledger(
    ledger: &ProvenanceLedger,
    producer: &'static str,
    changes: &[Value],
) {
    let owned: Vec<(crate::bindings::code_facts::Span, String)> = changes
        .iter()
        .filter_map(|change| {
            let span_json = change.get("span")?;
            let span: crate::bindings::code_facts::Span =
                serde_json::from_value(span_json.clone()).ok()?;
            let new_text = change.get("new_text")?.as_str()?.to_string();
            Some((span, new_text))
        })
        .collect();
    let refs: Vec<(&crate::bindings::code_facts::Span, &str)> = owned
        .iter()
        .map(|(span, new_text)| (span, new_text.as_str()))
        .collect();
    ledger.record_changes(producer, AuthorityTier::SyntaxOnly, refs.iter().copied());
}
