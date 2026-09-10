//! Synchronous apply layer for the codex patch format.
//!
//! Blackbox-authored. Ports the essence of codex's `compute_replacements`,
//! `apply_replacements`, and `derive_new_contents_from_chunks`
//! (`codex-rs/apply-patch/src/lib.rs`, Apache-2.0 — see NOTICE) onto `std::fs`
//! against a base directory, without Codex's async `ExecutorFileSystem`,
//! sandbox context, `AbsolutePathBuf`, or `similar`-based diffs. Every hunk
//! path is resolved against `base`: relative paths join `base` and `..`
//! components are collapsed lexically; absolute paths are accepted as-is —
//! no containment check (gap-e0ae3e7d).

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

/// One completed filesystem mutation, captured before another hunk can change
/// the same path again. Empty bytes represent an absent file for the edit sink.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppliedEdit {
    pub path: PathBuf,
    pub before: Vec<u8>,
    pub after: Vec<u8>,
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct ApplyOutcome {
    pub changes: Vec<FileChange>,
    pub edits: Vec<AppliedEdit>,
    /// Directories actually created while preparing successful or failed writes.
    pub created_directories: Vec<PathBuf>,
    /// A failed write may have truncated or partially written these paths.
    /// They are not presented as exact, completed mutations in `edits`.
    pub uncertain_paths: Vec<PathBuf>,
}

/// Failure preserves the completed prefix. Applying a patch is sequential and
/// does not roll back earlier mutations when a later operation fails.
#[derive(Debug, Error)]
#[error("{error}")]
pub struct ApplyFailure {
    #[source]
    pub error: ApplyError,
    pub outcome: Box<ApplyOutcome>,
}

impl From<ApplyError> for ApplyFailure {
    fn from(error: ApplyError) -> Self {
        Self {
            error,
            outcome: Box::default(),
        }
    }
}

/// Apply a codex envelope sequentially under `base`, preserving committed edit
/// evidence on failure. Paths retain the existing permissive resolution rules;
/// this function does not promise containment, rollback, or atomic multi-file IO.
pub fn apply_patch(patch_text: &str, base: &Path) -> Result<ApplyOutcome, ApplyFailure> {
    let parsed = parse_patch(patch_text).map_err(ApplyError::from)?;
    let mut outcome = ApplyOutcome::default();
    for hunk in &parsed.hunks {
        if let Err(error) = apply_hunk(hunk, base, &mut outcome) {
            return Err(ApplyFailure {
                error,
                outcome: Box::new(outcome),
            });
        }
    }
    Ok(outcome)
}

// Synchronous apply-layer IO; the harness adapter owns the blocking executor.
#[allow(clippy::disallowed_methods)]
fn apply_hunk(hunk: &Hunk, base: &Path, outcome: &mut ApplyOutcome) -> Result<(), ApplyError> {
    match hunk {
        Hunk::AddFile { path, contents } => {
            let abs = resolve_within(base, path)?;
            if abs.exists() {
                return Err(ApplyError::Conflict(format!(
                    "{}: Add File target already exists",
                    path.display()
                )));
            }
            create_parents(&abs, base, outcome)?;
            write_file(path, &abs, &[], contents.as_bytes(), outcome)?;
            outcome.changes.push(FileChange {
                path: path.clone(),
                action: FileAction::Added,
                moved_from: None,
            });
        }
        Hunk::DeleteFile { path } => {
            let abs = resolve_within(base, path)?;
            let before = std::fs::read(&abs).map_err(|error| io_err("read", path, error))?;
            std::fs::remove_file(&abs).map_err(|error| io_err("delete", path, error))?;
            outcome.edits.push(AppliedEdit {
                path: path.clone(),
                before,
                after: Vec::new(),
            });
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
                std::fs::read_to_string(&src_abs).map_err(|error| io_err("read", path, error))?;
            let new_contents = derive_new_contents(&original, path, chunks)?;
            let dest_rel = move_path.as_deref().unwrap_or(path);
            let dest_abs = resolve_within(base, dest_rel)?;
            let moved = move_path.is_some() && dest_abs != src_abs;
            let before_destination = if moved {
                match std::fs::read(&dest_abs) {
                    Ok(bytes) => Some(bytes),
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
                    Err(error) => return Err(io_err("read destination", dest_rel, error)),
                }
            } else {
                Some(original.as_bytes().to_vec())
            };
            if move_path.is_some() {
                create_parents(&dest_abs, base, outcome)?;
            }
            write_file(
                dest_rel,
                &dest_abs,
                before_destination.as_deref().unwrap_or_default(),
                new_contents.as_bytes(),
                outcome,
            )?;
            // Account for the completed destination write even if removing the
            // source fails. A move is complete only after both mutations succeed.
            outcome.changes.push(FileChange {
                path: dest_rel.to_path_buf(),
                action: if before_destination.is_some() {
                    FileAction::Updated
                } else {
                    FileAction::Added
                },
                moved_from: None,
            });
            if moved {
                // A destination alias can also change the source bytes. Capture
                // the deletion pre-image after the completed destination write.
                let before_delete = std::fs::read(&src_abs)
                    .map_err(|error| io_err("read before remove", path, error))?;
                std::fs::remove_file(&src_abs)
                    .map_err(|error| io_err("remove original", path, error))?;
                outcome.edits.push(AppliedEdit {
                    path: path.clone(),
                    before: before_delete,
                    after: Vec::new(),
                });
                let change = outcome
                    .changes
                    .last_mut()
                    .expect("destination write recorded");
                change.action = FileAction::Moved;
                change.moved_from = Some(path.clone());
            }
        }
    }
    Ok(())
}

// Synchronous apply-layer IO; the harness adapter owns the blocking executor.
#[allow(clippy::disallowed_methods)]
fn write_file(
    path: &Path,
    absolute: &Path,
    before: &[u8],
    after: &[u8],
    outcome: &mut ApplyOutcome,
) -> Result<(), ApplyError> {
    if let Err(error) = std::fs::write(absolute, after) {
        outcome.uncertain_paths.push(path.to_path_buf());
        return Err(io_err("write", path, error));
    }
    outcome.edits.push(AppliedEdit {
        path: path.to_path_buf(),
        before: before.to_vec(),
        after: after.to_vec(),
    });
    Ok(())
}

// Synchronous apply-layer IO; the harness adapter owns the blocking executor.
#[allow(clippy::disallowed_methods)]
fn create_parents(path: &Path, base: &Path, outcome: &mut ApplyOutcome) -> Result<(), ApplyError> {
    let mut missing = Vec::new();
    let mut cursor = path.parent();
    while let Some(parent) = cursor {
        if parent.exists() {
            break;
        }
        missing.push(parent.to_path_buf());
        cursor = parent.parent();
    }
    for directory in missing.into_iter().rev() {
        match std::fs::create_dir(&directory) {
            Ok(()) => outcome.created_directories.push(
                directory
                    .strip_prefix(base)
                    .unwrap_or(&directory)
                    .to_path_buf(),
            ),
            Err(error)
                if error.kind() == std::io::ErrorKind::AlreadyExists && directory.is_dir() => {}
            Err(error) => return Err(io_err("create dir", &directory, error)),
        }
    }
    Ok(())
}

fn derive_new_contents(
    original: &str,
    path: &Path,
    chunks: &[UpdateFileChunk],
) -> Result<String, ApplyError> {
    // Only a uniformly CRLF source selects CRLF output. Mixed line endings
    // retain the existing per-line bytes rather than guessing a dominant style.
    let bytes = original.as_bytes();
    let uniform_crlf = original.contains("\r\n")
        && bytes.iter().enumerate().all(|(i, byte)| match byte {
            b'\n' => i > 0 && bytes[i - 1] == b'\r',
            b'\r' => bytes.get(i + 1) == Some(&b'\n'),
            _ => true,
        });
    let normalized;
    let original = if uniform_crlf {
        normalized = original.replace("\r\n", "\n");
        normalized.as_str()
    } else {
        original
    };
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
    let result = new_lines.join("\n");
    Ok(if uniform_crlf {
        result.replace("\n", "\r\n")
    } else {
        result
    })
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
            if let Some(idx) = seek_sequence(
                original_lines,
                std::slice::from_ref(ctx_line),
                line_index,
                false,
            ) {
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
                message: format!(
                    "failed to find expected lines:\n{}",
                    chunk.old_lines.join("\n")
                ),
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

/// Resolve `rel` against `base`. Relative paths are joined and `..`
/// components collapsed lexically; absolute paths are returned as-is
/// (normalized). No containment check — see module docs. `..` is prevented
/// from popping past the path's leading prefix + root so absolute paths
/// stay absolute (e.g. `/foo/..` → `/`, `C:\foo\..` → `C:\`).
fn resolve_within(base: &Path, rel: &Path) -> Result<PathBuf, ApplyError> {
    let mut out = if rel.is_absolute() {
        PathBuf::new()
    } else {
        base.to_path_buf()
    };
    let mut prefix_count: usize = 0;
    for comp in rel.components() {
        match comp {
            Component::Prefix(_) | Component::RootDir => {
                out.push(comp.as_os_str());
                prefix_count += 1;
            }
            Component::CurDir => {}
            Component::ParentDir => {
                if out.components().count() > prefix_count {
                    out.pop();
                }
            }
            Component::Normal(c) => out.push(c),
        }
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
    fn update_preserves_uniform_crlf_bytes_and_edit_evidence() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let path = root.join("source.txt");
        let before = b"first\r\nold\r\nlast\r\n";
        let after = b"first\r\nnew\r\nextra\r\nlast\r\n";
        std::fs::write(&path, before).unwrap();
        let outcome = apply_patch("*** Begin Patch\n*** Update File: source.txt\n@@\n first\n-old\n+new\n+extra\n last\n*** End Patch", &root).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), after);
        assert_eq!(outcome.edits[0].before, before);
        assert_eq!(outcome.edits[0].after, after);
    }

    #[test]
    fn mixed_newlines_do_not_select_uniform_crlf_output() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let path = root.join("source.txt");
        std::fs::write(&path, b"first\r\nold\nlast\r\n").unwrap();
        apply_patch(
            "*** Begin Patch\n*** Update File: source.txt\n@@\n-old\n+new\n*** End Patch",
            &root,
        )
        .unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"first\r\nnew\nlast\r\n");
    }

    #[test]
    fn later_context_failure_preserves_ordered_exact_delta() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let failure = apply_patch("*** Begin Patch\n*** Add File: nested/item.txt\n+first\n*** Update File: nested/item.txt\n@@\n-first\n+second\n*** Update File: nested/item.txt\n@@\n-not present\n+third\n*** End Patch", &root).unwrap_err();
        assert!(matches!(&failure.error, ApplyError::Context { .. }));
        assert_eq!(
            std::fs::read(root.join("nested/item.txt")).unwrap(),
            b"second\n"
        );
        assert_eq!(
            failure.outcome.edits,
            vec![
                AppliedEdit {
                    path: "nested/item.txt".into(),
                    before: Vec::new(),
                    after: b"first\n".to_vec()
                },
                AppliedEdit {
                    path: "nested/item.txt".into(),
                    before: b"first\n".to_vec(),
                    after: b"second\n".to_vec()
                },
            ]
        );
        assert_eq!(
            failure.outcome.created_directories,
            vec![PathBuf::from("nested")]
        );
        assert!(failure.outcome.uncertain_paths.is_empty());
    }

    #[test]
    fn failed_write_keeps_completed_delete_and_marks_attempt_uncertain() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        std::fs::write(root.join("old.bin"), [0xff, 0x00]).unwrap();
        std::fs::write(root.join("obstacle"), b"file").unwrap();
        let failure = apply_patch("*** Begin Patch\n*** Delete File: old.bin\n*** Add File: obstacle/child.txt\n+blocked\n*** End Patch", &root).unwrap_err();
        assert!(matches!(&failure.error, ApplyError::Io { op: "write", .. }));
        assert!(!root.join("old.bin").exists());
        assert_eq!(
            failure.outcome.edits,
            vec![AppliedEdit {
                path: "old.bin".into(),
                before: vec![0xff, 0],
                after: Vec::new(),
            }]
        );
        assert_eq!(
            failure.outcome.uncertain_paths,
            vec![PathBuf::from("obstacle/child.txt")]
        );
        assert_eq!(std::fs::read(root.join("obstacle")).unwrap(), b"file");
    }

    #[test]
    fn overwritten_move_destination_is_part_of_completed_delta() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        std::fs::write(root.join("source.txt"), b"old\n").unwrap();
        std::fs::write(root.join("target.txt"), [0xff, 0x00]).unwrap();
        let outcome = apply_patch("*** Begin Patch\n*** Update File: source.txt\n*** Move to: target.txt\n@@\n-old\n+new\n*** End Patch", &root).unwrap();
        assert_eq!(outcome.changes[0].action, FileAction::Moved);
        assert_eq!(
            outcome.edits,
            vec![
                AppliedEdit {
                    path: "target.txt".into(),
                    before: vec![0xff, 0],
                    after: b"new\n".to_vec()
                },
                AppliedEdit {
                    path: "source.txt".into(),
                    before: b"old\n".to_vec(),
                    after: Vec::new()
                },
            ]
        );
        assert_eq!(std::fs::read(root.join("target.txt")).unwrap(), b"new\n");
        assert!(!root.join("source.txt").exists());
    }

    #[test]
    fn moved_alias_records_the_actual_deletion_preimage() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        std::fs::write(root.join("source.txt"), b"old\n").unwrap();
        std::fs::hard_link(root.join("source.txt"), root.join("alias.txt")).unwrap();
        let outcome = apply_patch("*** Begin Patch\n*** Update File: source.txt\n*** Move to: alias.txt\n@@\n-old\n+new\n*** End Patch", &root).unwrap();
        assert_eq!(outcome.edits[0].before, b"old\n");
        assert_eq!(outcome.edits[1].before, b"new\n");
        assert_eq!(std::fs::read(root.join("alias.txt")).unwrap(), b"new\n");
        assert!(!root.join("source.txt").exists());
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
        assert!(matches!(&err.error, ApplyError::Conflict(_)), "{err:?}");
    }

    #[test]
    fn dotdot_components_normalize_lexically() {
        // A path with `..` collapses to its canonical form under `base` —
        // no rejection. The file is written at the normalized location.
        let dir = base();
        let sub = dir.path().join("a").join("b");
        std::fs::create_dir_all(&sub).unwrap();
        apply_patch(
            "*** Begin Patch\n*** Add File: a/b/../c.txt\n+hi\n*** End Patch",
            dir.path(),
        )
        .unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.path().join("a/c.txt")).unwrap(),
            "hi\n"
        );
        assert!(!dir.path().join("a/b/c.txt").exists());
    }

    #[test]
    fn dotdot_escape_lands_outside_base() {
        // `..` past the base lands the file outside it — no rejection.
        // We use a sibling tempdir as the target to keep the test hermetic.
        let base_dir = base();
        let outside_dir = base();
        let target = outside_dir.path().join("escaped.txt");
        let sibling_name = outside_dir
            .path()
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap();
        let patch = format!(
            "*** Begin Patch\n*** Add File: ../{sibling_name}/escaped.txt\n+escaped\n*** End Patch"
        );
        apply_patch(&patch, base_dir.path()).unwrap();
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "escaped\n");
    }

    #[test]
    fn absolute_paths_are_accepted() {
        // Absolute paths are accepted as-is. The file is written at the
        // absolute path (here, a tempdir we control so the test isn't
        // touching real filesystem state).
        let dir = base();
        let target = dir.path().join("abs_marker.txt");
        let patch = format!(
            "*** Begin Patch\n*** Add File: {}\n+absolute\n*** End Patch",
            target.display()
        );
        apply_patch(&patch, dir.path()).unwrap();
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "absolute\n");
    }

    #[test]
    fn tilde_is_a_literal_path_component() {
        // `~` is not expanded by `Path` (no shell involved) — it remains a
        // literal path component. The patch path starts with `~/marker.txt`,
        // so the resolved path is `<base>/~/marker.txt`. An implementation
        // that expanded leading `~/...` would land the file at
        // `$HOME/marker.txt` instead, and the assertion would fail.
        let dir = base();
        let literal_tilde_dir = dir.path().join("~");
        std::fs::create_dir_all(&literal_tilde_dir).unwrap();
        let patch = "*** Begin Patch\n*** Add File: ~/marker.txt\n+tilde\n*** End Patch";
        apply_patch(patch, dir.path()).unwrap();
        let landed = literal_tilde_dir.join("marker.txt");
        assert_eq!(std::fs::read_to_string(&landed).unwrap(), "tilde\n");
    }

    #[cfg(windows)]
    #[test]
    fn windows_drive_absolute_path_preserves_root() {
        // Regression for the drive-absolute bug: a path like `C:\a\..\b.txt`
        // must normalize to `C:\b.txt`, not `C:some\where\a\..\b.txt` and not
        // the drive-relative `C:b.txt`. Test the resolver directly so the
        // assertion is hermetic (no need to actually write at the drive
        // root, which would require elevation on some systems).
        let base = Path::new(r"C:\work\repo");
        let rel = Path::new(r"C:\a\..\b.txt");
        let resolved = resolve_within(base, rel).unwrap();
        assert_eq!(resolved, PathBuf::from(r"C:\b.txt"));
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
        assert!(
            matches!(&err.error, ApplyError::Io { op: "read", .. }),
            "{err:?}"
        );
    }
}
