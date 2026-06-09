//! Synchronous apply layer for the codex patch format.
//!
//! Blackbox-authored. Ports the essence of codex's `compute_replacements`,
//! `apply_replacements`, and `derive_new_contents_from_chunks`
//! (`codex-rs/apply-patch/src/lib.rs`, Apache-2.0 — see NOTICE) onto `std::fs`
//! against a base directory, without Codex's async `ExecutorFileSystem`,
//! sandbox context, `AbsolutePathBuf`, or `similar`-based diffs. Every hunk path
//! is resolved relative to `base` and confined to it.

use crate::parser::{Hunk, UpdateFileChunk, parse_patch};
use crate::seek_sequence::seek_sequence;
use std::path::{Component, Path, PathBuf};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum ApplyError {
    #[error("invalid patch: {0}")]
    Parse(#[from] crate::parser::ParseError),
    #[error("{path}: {message}")]
    Path { path: String, message: String },
    #[error("failed to {op} {path}: {source}")]
    Io {
        op: &'static str,
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("{path}: {message}")]
    Context { path: String, message: String },
    #[error("{0}")]
    Conflict(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileAction {
    Added,
    Deleted,
    Updated,
    Moved,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileChange {
    /// Path relative to the base dir (the move destination for renames).
    pub path: PathBuf,
    pub action: FileAction,
    /// For a rename, the original path (relative to base).
    pub moved_from: Option<PathBuf>,
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct ApplyOutcome {
    pub changes: Vec<FileChange>,
}

/// Apply a codex `*** Begin Patch` envelope under `base`. Every hunk path is
/// resolved relative to `base`; absolute or escaping paths are rejected, so a
/// patch can only touch files inside the worktree.
pub fn apply_patch(patch_text: &str, base: &Path) -> Result<ApplyOutcome, ApplyError> {
    let parsed = parse_patch(patch_text)?;
    let mut outcome = ApplyOutcome::default();

    for hunk in &parsed.hunks {
        match hunk {
            Hunk::AddFile { path, contents } => {
                let abs = resolve_within(base, path)?;
                if abs.exists() {
                    return Err(ApplyError::Conflict(format!(
                        "{}: Add File target already exists",
                        path.display()
                    )));
                }
                if let Some(parent) = abs.parent() {
                    std::fs::create_dir_all(parent).map_err(|e| io_err("create dir for", path, e))?;
                }
                std::fs::write(&abs, contents).map_err(|e| io_err("write", path, e))?;
                outcome.changes.push(FileChange {
                    path: path.clone(),
                    action: FileAction::Added,
                    moved_from: None,
                });
            }
            Hunk::DeleteFile { path } => {
                let abs = resolve_within(base, path)?;
                std::fs::remove_file(&abs).map_err(|e| io_err("delete", path, e))?;
                outcome.changes.push(FileChange {
                    path: path.clone(),
                    action: FileAction::Deleted,
                    moved_from: None,
                });
            }
            Hunk::UpdateFile {
                path,
                move_path,
                chunks,
            } => {
                let src_abs = resolve_within(base, path)?;
                let original =
                    std::fs::read_to_string(&src_abs).map_err(|e| io_err("read", path, e))?;
                let new_contents = derive_new_contents(&original, path, chunks)?;

                let dest_rel = move_path.as_deref().unwrap_or(path);
                let dest_abs = resolve_within(base, dest_rel)?;
                if move_path.is_some()
                    && let Some(parent) = dest_abs.parent()
                {
                    std::fs::create_dir_all(parent)
                        .map_err(|e| io_err("create dir for", dest_rel, e))?;
                }
                std::fs::write(&dest_abs, &new_contents).map_err(|e| io_err("write", dest_rel, e))?;

                if move_path.is_some() && dest_abs != src_abs {
                    std::fs::remove_file(&src_abs)
                        .map_err(|e| io_err("remove original", path, e))?;
                    outcome.changes.push(FileChange {
                        path: dest_rel.to_path_buf(),
                        action: FileAction::Moved,
                        moved_from: Some(path.clone()),
                    });
                } else {
                    outcome.changes.push(FileChange {
                        path: path.clone(),
                        action: FileAction::Updated,
                        moved_from: None,
                    });
                }
            }
        }
    }

    Ok(outcome)
}

fn derive_new_contents(
    original: &str,
    path: &Path,
    chunks: &[UpdateFileChunk],
) -> Result<String, ApplyError> {
    let mut original_lines: Vec<String> = original.split('\n').map(String::from).collect();
    // Drop the trailing empty element from the final newline so line counts
    // match standard `diff` behaviour.
    if original_lines.last().is_some_and(String::is_empty) {
        original_lines.pop();
    }

    let replacements = compute_replacements(&original_lines, path, chunks)?;
    let mut new_lines = apply_replacements(original_lines, &replacements);
    if !new_lines.last().is_some_and(String::is_empty) {
        new_lines.push(String::new());
    }
    Ok(new_lines.join("\n"))
}

/// Compute `(start_index, old_len, new_lines)` replacements that transform
/// `original_lines` per the chunks. Ported from codex `compute_replacements`.
fn compute_replacements(
    original_lines: &[String],
    path: &Path,
    chunks: &[UpdateFileChunk],
) -> Result<Vec<(usize, usize, Vec<String>)>, ApplyError> {
    let mut replacements: Vec<(usize, usize, Vec<String>)> = Vec::new();
    let mut line_index: usize = 0;

    for chunk in chunks {
        // A `change_context` narrows down where the chunk applies.
        if let Some(ctx_line) = &chunk.change_context {
            if let Some(idx) =
                seek_sequence(original_lines, std::slice::from_ref(ctx_line), line_index, false)
            {
                line_index = idx + 1;
            } else {
                return Err(ApplyError::Context {
                    path: path.display().to_string(),
                    message: format!("failed to find context '{ctx_line}'"),
                });
            }
        }

        if chunk.old_lines.is_empty() {
            // Pure addition: insert before the final empty line if present.
            let insertion_idx = if original_lines.last().is_some_and(String::is_empty) {
                original_lines.len() - 1
            } else {
                original_lines.len()
            };
            replacements.push((insertion_idx, 0, chunk.new_lines.clone()));
            continue;
        }

        let mut pattern: &[String] = &chunk.old_lines;
        let mut found = seek_sequence(original_lines, pattern, line_index, chunk.is_end_of_file);
        let mut new_slice: &[String] = &chunk.new_lines;

        // A trailing empty `old_lines` element represents the file's final
        // newline, which is stripped from `original_lines`. Retry without it.
        if found.is_none() && pattern.last().is_some_and(String::is_empty) {
            pattern = &pattern[..pattern.len() - 1];
            if new_slice.last().is_some_and(String::is_empty) {
                new_slice = &new_slice[..new_slice.len() - 1];
            }
            found = seek_sequence(original_lines, pattern, line_index, chunk.is_end_of_file);
        }

        if let Some(start_idx) = found {
            replacements.push((start_idx, pattern.len(), new_slice.to_vec()));
            line_index = start_idx + pattern.len();
        } else {
            return Err(ApplyError::Context {
                path: path.display().to_string(),
                message: format!("failed to find expected lines:\n{}", chunk.old_lines.join("\n")),
            });
        }
    }

    replacements.sort_by_key(|(index, _, _)| *index);
    Ok(replacements)
}

/// Apply `(start_index, old_len, new_lines)` replacements. Ported verbatim from
/// codex `apply_replacements`.
fn apply_replacements(
    mut lines: Vec<String>,
    replacements: &[(usize, usize, Vec<String>)],
) -> Vec<String> {
    // Descending order so earlier replacements don't shift later indices.
    for (start_idx, old_len, new_segment) in replacements.iter().rev() {
        let start_idx = *start_idx;
        let old_len = *old_len;
        for _ in 0..old_len {
            if start_idx < lines.len() {
                lines.remove(start_idx);
            }
        }
        for (offset, new_line) in new_segment.iter().enumerate() {
            lines.insert(start_idx + offset, new_line.clone());
        }
    }
    lines
}

/// Resolve `rel` against `base`, rejecting absolute paths and any `..` that
/// would escape the worktree. Lexical only (no symlink resolution) — enough to
/// keep an apply confined to the base dir.
fn resolve_within(base: &Path, rel: &Path) -> Result<PathBuf, ApplyError> {
    let mut out = base.to_path_buf();
    for comp in rel.components() {
        match comp {
            Component::Normal(c) => out.push(c),
            Component::CurDir => {}
            Component::ParentDir => {
                if !out.pop() || !out.starts_with(base) {
                    return Err(ApplyError::Path {
                        path: rel.display().to_string(),
                        message: "path escapes the worktree".to_string(),
                    });
                }
            }
            Component::RootDir | Component::Prefix(_) => {
                return Err(ApplyError::Path {
                    path: rel.display().to_string(),
                    message: "absolute paths are not allowed".to_string(),
                });
            }
        }
    }
    if !out.starts_with(base) {
        return Err(ApplyError::Path {
            path: rel.display().to_string(),
            message: "path escapes the worktree".to_string(),
        });
    }
    Ok(out)
}

fn io_err(op: &'static str, path: &Path, source: std::io::Error) -> ApplyError {
    ApplyError::Io {
        op,
        path: path.display().to_string(),
        source,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base() -> tempfile::TempDir {
        tempfile::tempdir().unwrap()
    }

    #[test]
    fn add_then_update_then_move_then_delete() {
        let dir = base();
        let root = dir.path();

        // Add.
        let out = apply_patch(
            "*** Begin Patch\n*** Add File: src/a.txt\n+one\n+two\n+three\n*** End Patch",
            root,
        )
        .unwrap();
        assert_eq!(out.changes.len(), 1);
        assert_eq!(out.changes[0].action, FileAction::Added);
        assert_eq!(
            std::fs::read_to_string(root.join("src/a.txt")).unwrap(),
            "one\ntwo\nthree\n"
        );

        // Update (context-located).
        apply_patch(
            "*** Begin Patch\n*** Update File: src/a.txt\n@@\n two\n-three\n+THREE\n*** End Patch",
            root,
        )
        .unwrap();
        assert_eq!(
            std::fs::read_to_string(root.join("src/a.txt")).unwrap(),
            "one\ntwo\nTHREE\n"
        );

        // Move (rename) + edit in one update hunk.
        apply_patch(
            "*** Begin Patch\n*** Update File: src/a.txt\n*** Move to: src/b.txt\n@@\n-one\n+ONE\n*** End Patch",
            root,
        )
        .unwrap();
        assert!(!root.join("src/a.txt").exists());
        assert_eq!(
            std::fs::read_to_string(root.join("src/b.txt")).unwrap(),
            "ONE\ntwo\nTHREE\n"
        );

        // Delete.
        let out = apply_patch(
            "*** Begin Patch\n*** Delete File: src/b.txt\n*** End Patch",
            root,
        )
        .unwrap();
        assert_eq!(out.changes[0].action, FileAction::Deleted);
        assert!(!root.join("src/b.txt").exists());
    }

    #[test]
    fn add_over_existing_is_a_conflict() {
        let dir = base();
        std::fs::write(dir.path().join("x.txt"), "hi\n").unwrap();
        let err = apply_patch(
            "*** Begin Patch\n*** Add File: x.txt\n+nope\n*** End Patch",
            dir.path(),
        )
        .unwrap_err();
        assert!(matches!(err, ApplyError::Conflict(_)), "{err:?}");
    }

    #[test]
    fn escaping_path_is_rejected_and_nothing_is_written() {
        let dir = base();
        let err = apply_patch(
            "*** Begin Patch\n*** Add File: ../escape.txt\n+pwned\n*** End Patch",
            dir.path(),
        )
        .unwrap_err();
        assert!(matches!(err, ApplyError::Path { .. }), "{err:?}");
        assert!(!dir.path().parent().unwrap().join("escape.txt").exists());
    }

    #[test]
    fn fuzzy_context_locates_despite_trailing_whitespace() {
        // The file has trailing whitespace the patch context omits. The
        // seek_sequence rstrip pass still LOCATES the region (the edit applies
        // at all), and — codex-faithfully — the matched region is rewritten from
        // the patch text, so the context line normalizes to the patch's `alpha`.
        let dir = base();
        std::fs::write(dir.path().join("f.txt"), "alpha   \nbeta\n").unwrap();
        apply_patch(
            "*** Begin Patch\n*** Update File: f.txt\n@@\n alpha\n-beta\n+BETA\n*** End Patch",
            dir.path(),
        )
        .unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.path().join("f.txt")).unwrap(),
            "alpha\nBETA\n"
        );
    }

    #[test]
    fn missing_update_target_is_an_io_error() {
        let dir = base();
        let err = apply_patch(
            "*** Begin Patch\n*** Update File: nope.txt\n@@\n-x\n+y\n*** End Patch",
            dir.path(),
        )
        .unwrap_err();
        assert!(matches!(err, ApplyError::Io { op: "read", .. }), "{err:?}");
    }
}
