//! Shared LSP ↔ byte-offset conversion helpers.
//!
//! Hoisted from `crate::refactor::rust` so every language backend
//! (Rust, C#, Java) can share them without going through the Rust
//! refactor module.

use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use lsp_types::{DocumentChanges, Position, WorkspaceEdit};

use crate::refactor::{FileEdit, TextEdit, path_string, sha256_hex};

/// Convert a UTF-8 byte offset into an LSP [`Position`] (line + UTF-16 column).
pub(crate) fn byte_to_lsp_position(source: &str, byte: usize) -> Position {
    let line = source[..byte].bytes().filter(|b| *b == b'\n').count() as u32;
    let line_start = line_start_before(source, byte);
    let character = source[line_start..byte].encode_utf16().count() as u32;
    Position { line, character }
}

/// Convert an LSP (line, UTF-16 character) pair into a UTF-8 byte offset.
pub(crate) fn lsp_position_to_byte(source: &str, line: u32, character: u32) -> Result<usize> {
    let mut current_line = 0u32;
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
    let mut utf16 = 0u32;
    for (offset, ch) in source[line_start..line_end].char_indices() {
        if utf16 == character {
            return Ok(line_start + offset);
        }
        utf16 += ch.len_utf16() as u32;
        if utf16 > character {
            bail!("character {character} is not on a UTF-16 boundary");
        }
    }
    if utf16 == character {
        return Ok(line_end);
    }
    bail!("character {character} is outside line {line}");
}

/// Flatten an LSP [`WorkspaceEdit`] into a list of [`FileEdit`] values
/// that `apply_file_edits` can consume.
pub(crate) fn workspace_edit_to_file_edits(workspace_edit: WorkspaceEdit) -> Result<Vec<FileEdit>> {
    let mut grouped: BTreeMap<PathBuf, Vec<lsp_types::TextEdit>> = BTreeMap::new();

    if let Some(changes) = workspace_edit.changes {
        for (url, edits) in changes {
            if let Ok(path) = url.to_file_path() {
                grouped.entry(path).or_default().extend(edits);
            }
        }
    }

    if let Some(document_changes) = workspace_edit.document_changes {
        match document_changes {
            DocumentChanges::Edits(doc_edits) => {
                for doc_edit in doc_edits {
                    if let Ok(path) = doc_edit.text_document.uri.to_file_path() {
                        let edits = doc_edit.edits.into_iter().map(|e| match e {
                            lsp_types::OneOf::Left(te) => te,
                            lsp_types::OneOf::Right(ate) => lsp_types::TextEdit {
                                range: ate.text_edit.range,
                                new_text: ate.text_edit.new_text,
                            },
                        });
                        grouped.entry(path).or_default().extend(edits);
                    }
                }
            }
            DocumentChanges::Operations(ops) => {
                for op in ops {
                    if let lsp_types::DocumentChangeOperation::Edit(doc_edit) = op {
                        if let Ok(path) = doc_edit.text_document.uri.to_file_path() {
                            let edits = doc_edit.edits.into_iter().map(|e| match e {
                                lsp_types::OneOf::Left(te) => te,
                                lsp_types::OneOf::Right(ate) => lsp_types::TextEdit {
                                    range: ate.text_edit.range,
                                    new_text: ate.text_edit.new_text,
                                },
                            });
                            grouped.entry(path).or_default().extend(edits);
                        }
                    }
                }
            }
        }
    }

    let mut file_edits = Vec::new();
    for (path, edits) in grouped {
        let source = fs::read_to_string(&path)
            .with_context(|| format!("failed to read LSP edit target {}", path.display()))?;
        let mut text_edits = Vec::new();
        for edit in edits {
            let byte_start =
                lsp_position_to_byte(&source, edit.range.start.line, edit.range.start.character)
                    .with_context(|| format!("invalid LSP start range for {}", path.display()))?;
            let byte_end =
                lsp_position_to_byte(&source, edit.range.end.line, edit.range.end.character)
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
            new_text: None,
        });
    }
    Ok(file_edits)
}

fn line_start_before(source: &str, idx: usize) -> usize {
    source[..idx.min(source.len())]
        .rfind('\n')
        .map(|pos| pos + 1)
        .unwrap_or(0)
}
