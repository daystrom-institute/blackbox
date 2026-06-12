//! `lsp.*` — language-server authority, projected into cells.
//!
//! Session-backed bindings (code-mode-cell-dsl.md §6): first use in a
//! workspace warms a harness-owned language server (bro-lsp [`SessionPool`],
//! keyed by root + language, idle-evicted); the session outlives the cell
//! and the turn, never the workspace. **Fail closed (RX-V3):** an
//! unavailable or timed-out server is an error, never a silent downgrade to
//! a syntax-only approximation.
//!
//! Spans in, spans out: positions convert at the binding edge
//! (byte offset ↔ UTF-16 line/character), and server-authored edits come
//! back as the same hash-anchored `{span, new_text}` shape the edits
//! algebra consumes (`edits.merge`). Until the provenance ledger lands,
//! applied EditSets still stamp `syntax_only` even when wholly
//! server-authored — conservative by design.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use bro_lsp::{Language, LspConfig, OpenDocument, SessionPool};
use bro_tools::{Tool, ToolAnnotations, ToolCx, ToolResult};
use serde::Deserialize;
use serde_json::{Value, json};

use super::code_facts::{Span, span_schema_pub};

/// Session-scoped LSP binding state: the warm pool plus the documents this
/// session has opened (with the content generation last sent to the server,
/// so disk changes — e.g. an `edits.apply` — re-sync lazily on next use).
pub struct LspState {
    pool: SessionPool,
    docs: tokio::sync::Mutex<HashMap<PathBuf, DocEntry>>,
}

struct DocEntry {
    doc: OpenDocument,
    last_sha: String,
}

impl Default for LspState {
    fn default() -> Self {
        Self {
            pool: SessionPool::new(LspConfig::default()),
            docs: tokio::sync::Mutex::new(HashMap::new()),
        }
    }
}

fn err(msg: impl std::fmt::Display) -> ToolResult {
    ToolResult::Error(msg.to_string())
}

/// Byte offset → LSP position (UTF-16 line/character), per the LSP default
/// position encoding.
fn byte_to_position(source: &str, byte: usize) -> lsp_types::Position {
    let byte = byte.min(source.len());
    let mut line = 0u32;
    let mut line_start = 0usize;
    for (idx, ch) in source.char_indices() {
        if idx >= byte {
            break;
        }
        if ch == '\n' {
            line += 1;
            line_start = idx + 1;
        }
    }
    let character = source[line_start..byte]
        .chars()
        .map(|c| c.len_utf16() as u32)
        .sum();
    lsp_types::Position { line, character }
}

/// LSP position (UTF-16 line/character) → byte offset.
fn position_to_byte(source: &str, pos: lsp_types::Position) -> Result<usize, String> {
    let mut line = 0u32;
    let mut offset = 0usize;
    if pos.line > 0 {
        let mut found = false;
        for (idx, ch) in source.char_indices() {
            if ch == '\n' {
                line += 1;
                if line == pos.line {
                    offset = idx + 1;
                    found = true;
                    break;
                }
            }
        }
        if !found {
            return Err(format!("position line {} out of range", pos.line));
        }
    }
    let mut utf16 = 0u32;
    for (idx, ch) in source[offset..].char_indices() {
        if utf16 >= pos.character {
            return Ok(offset + idx);
        }
        if ch == '\n' {
            break;
        }
        utf16 += ch.len_utf16() as u32;
    }
    if utf16 >= pos.character {
        Ok(source.len().min(offset + source[offset..].find('\n').unwrap_or(source.len() - offset)))
    } else {
        Err(format!(
            "position {}:{} out of range",
            pos.line, pos.character
        ))
    }
}

impl LspState {
    /// Ensure the server has the document open with the current disk content.
    async fn ensure_current(
        &self,
        root: &Path,
        abs: &Path,
        source: &str,
        sha: &str,
    ) -> Result<OpenDocument, String> {
        let mut docs = self.docs.lock().await;
        match docs.get_mut(&abs.to_path_buf()) {
            Some(entry) => {
                if entry.last_sha != sha {
                    let version = entry.doc.version + 1;
                    self.pool
                        .apply_change(&mut entry.doc, version, source.to_string())
                        .await
                        .map_err(render_lsp_error)?;
                    entry.last_sha = sha.to_string();
                }
                Ok(entry.doc.clone())
            }
            None => {
                let doc = self
                    .pool
                    .open_document(root, Language::Rust, abs, 1, source.to_string())
                    .await
                    .map_err(render_lsp_error)?;
                docs.insert(
                    abs.to_path_buf(),
                    DocEntry {
                        doc: doc.clone(),
                        last_sha: sha.to_string(),
                    },
                );
                Ok(docs.get(&abs.to_path_buf()).unwrap().doc.clone())
            }
        }
    }
}

fn render_lsp_error(e: bro_lsp::Error) -> String {
    if e.is_lsp_unavailable() {
        format!("lsp_unavailable: {e} — refusing to proceed (RX-V3: no syntax-only downgrade); install/configure rust-analyzer or use code.*/edits.* with explicit spans")
    } else {
        format!("{e}")
    }
}

/// `lsp.rename` — server-authority rename, returning span-shaped changes.
pub struct LspRename(pub Arc<LspState>);

#[derive(Deserialize)]
struct RenameParams {
    span: Span,
    #[serde(rename = "newName", alias = "new_name")]
    new_name: String,
}

#[async_trait]
impl Tool for LspRename {
    fn name(&self) -> &str {
        "lsp.rename"
    }
    fn description(&self) -> &str {
        "Rename the symbol a hash-anchored Span points at, across the workspace, via the language server (rust-analyzer; Rust only for now; warms on first use). Whole-item spans are fine — the binding snaps to the item's name identifier automatically. Returns server-authored changes as hash-anchored {span, new_text} entries — feed them to edits.merge, then edits.apply. Fails closed when the server is unavailable (never downgrades to text matching). Errors with stale_span if the file changed since the Span was minted."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "span": span_schema_pub(),
                "newName": { "type": "string", "description": "The new symbol name." }
            },
            "required": ["span", "newName"]
        })
    }
    fn annotations(&self) -> ToolAnnotations {
        ToolAnnotations {
            read_only: true,
            destructive: false,
        }
    }
    fn namespace_binding(&self) -> Option<(String, String)> {
        Some(("lsp".to_string(), "rename".to_string()))
    }
    async fn call(&self, input: Value, cx: &ToolCx) -> ToolResult {
        let params: RenameParams = match serde_json::from_value(input) {
            Ok(p) => p,
            Err(e) => {
                return err(format!(
                    "lsp.rename: bad input — expected {{ span: Span, newName: string }}; {e}"
                ));
            }
        };
        let span = params.span;
        let abs = match bro_tools::workspace::resolve_in_root(&cx.root, &span.file) {
            Ok(p) => p,
            Err(e) => return err(format!("lsp.rename: {}: {e}", span.file)),
        };
        if abs.extension().and_then(|e| e.to_str()) != Some("rs") {
            return err(format!(
                "lsp.rename: {} — rust only for now (rust-analyzer)",
                span.file
            ));
        }
        let bytes = match tokio::fs::read(&abs).await {
            Ok(b) => b,
            Err(e) => return err(format!("lsp.rename: {}: {e}", span.file)),
        };
        let sha = bbox_refactor::sha256_hex(&bytes);
        if sha != span.content_sha256 {
            return err(format!(
                "lsp.rename: stale_span: {} changed since the span was minted (span hash {}, current {sha}); re-derive the span from fresh facts",
                span.file, span.content_sha256
            ));
        }
        let source = String::from_utf8_lossy(&bytes).to_string();
        let doc = match self.0.ensure_current(&cx.root, &abs, &source, &sha).await {
            Ok(d) => d,
            Err(e) => return err(format!("lsp.rename: {e}")),
        };
        // Snap to the item's name identifier: a whole-item span starts at
        // `pub`/`fn`, which the server refuses with "No references found at
        // position" (probe-lsp-1 burned cells on exactly this).
        let aim = {
            let abs = abs.clone();
            let (start, end) = (span.byte_start, span.byte_end);
            bro_tools::tool::call_blocking(move || {
                let snapped = bbox_refactor::facts::name_span(&abs, start, end)
                    .ok()
                    .flatten()
                    .map(|(name_start, _)| name_start)
                    .unwrap_or(start);
                ToolResult::Json(json!(snapped))
            })
            .await
        };
        let aim_byte = match aim {
            ToolResult::Json(v) => v.as_u64().map(|b| b as usize).unwrap_or(span.byte_start),
            _ => span.byte_start,
        };
        let position = byte_to_position(&source, aim_byte);
        let edit = match self.0.pool.rename(&doc, position, &params.new_name).await {
            Ok(e) => e,
            Err(e) => return err(format!("lsp.rename: {}", render_lsp_error(e))),
        };

        // Flatten the WorkspaceEdit into per-file lsp edits.
        let mut by_uri: Vec<(lsp_types::Url, Vec<lsp_types::TextEdit>)> = Vec::new();
        if let Some(changes) = edit.changes {
            for (uri, edits) in changes {
                by_uri.push((uri, edits));
            }
        }
        if let Some(document_changes) = edit.document_changes {
            match document_changes {
                lsp_types::DocumentChanges::Edits(edits) => {
                    for doc_edit in edits {
                        let flattened = doc_edit
                            .edits
                            .into_iter()
                            .map(|e| match e {
                                lsp_types::OneOf::Left(edit) => edit,
                                lsp_types::OneOf::Right(annotated) => annotated.text_edit,
                            })
                            .collect();
                        by_uri.push((doc_edit.text_document.uri, flattened));
                    }
                }
                lsp_types::DocumentChanges::Operations(_) => {
                    return err(
                        "lsp.rename: server proposed file create/rename/delete operations — not supported by edits.merge yet; rename a symbol that does not move files",
                    );
                }
            }
        }
        if by_uri.is_empty() {
            return err("lsp.rename: server returned an empty WorkspaceEdit");
        }

        // Convert to hash-anchored span changes, reading each touched file
        // once to mint its generation hash.
        let mut changes: Vec<Value> = Vec::new();
        for (uri, edits) in by_uri {
            let path = match uri.to_file_path() {
                Ok(p) => p,
                Err(()) => return err(format!("lsp.rename: non-file uri in WorkspaceEdit: {uri}")),
            };
            let rel = match path.strip_prefix(&cx.root) {
                Ok(r) => r.to_string_lossy().to_string(),
                // resolve_in_root canonicalizes lexically; fall back to the
                // canonicalized root for symlinked tempdirs (macOS /var).
                Err(_) => match cx.root.canonicalize() {
                    Ok(canon) => match path.strip_prefix(&canon) {
                        Ok(r) => r.to_string_lossy().to_string(),
                        Err(_) => {
                            return err(format!(
                                "lsp.rename: WorkspaceEdit touches {} outside the worktree root",
                                path.display()
                            ));
                        }
                    },
                    Err(e) => return err(format!("lsp.rename: {e}")),
                },
            };
            let bytes = match tokio::fs::read(&path).await {
                Ok(b) => b,
                Err(e) => return err(format!("lsp.rename: {rel}: {e}")),
            };
            let file_sha = bbox_refactor::sha256_hex(&bytes);
            let file_source = String::from_utf8_lossy(&bytes).to_string();
            for text_edit in edits {
                let byte_start = match position_to_byte(&file_source, text_edit.range.start) {
                    Ok(b) => b,
                    Err(e) => return err(format!("lsp.rename: {rel}: {e}")),
                };
                let byte_end = match position_to_byte(&file_source, text_edit.range.end) {
                    Ok(b) => b,
                    Err(e) => return err(format!("lsp.rename: {rel}: {e}")),
                };
                changes.push(json!({
                    "span": Span {
                        file: rel.clone(),
                        byte_start,
                        byte_end,
                        content_sha256: file_sha.clone(),
                    },
                    "new_text": text_edit.new_text,
                }));
            }
        }
        let files: std::collections::BTreeSet<String> = changes
            .iter()
            .filter_map(|c| c["span"]["file"].as_str().map(str::to_string))
            .collect();
        ToolResult::Json(json!({
            "changes": changes,
            "files": files,
            "edit_count": changes.len(),
            "authority": "lsp",
            "language": "rust",
        }))
    }
}

/// The `lsp.*` binding set, sharing one session-scoped [`LspState`].
pub fn tools(state: Arc<LspState>) -> Vec<Arc<dyn Tool>> {
    vec![Arc::new(LspRename(state)) as Arc<dyn Tool>]
}

/// Hand-authored namespace documentation + TS declarations (cell-dsl §5.2).
pub fn namespace_description() -> bro_code_mode::ToolNamespaceDescription {
    bro_code_mode::ToolNamespaceDescription {
        name: "lsp".to_string(),
        description: "Language-server authority (rust-analyzer; Rust only for now). Session-backed: the first call in a workspace warms the server (may take a few seconds on a cold crate — the call blocks, no need to poll); later calls are fast. Fails closed when the server is unavailable — there is deliberately no silent fallback to text matching (RX-V3). THE RENAME RECIPE: aim a Span at the symbol (e.g. a code.query name capture, or an item span start), then `const r = await lsp.rename({ span, newName: \"x\" })` → `await edits.merge({ es, changes: r.changes })` → `await edits.apply({ es })` — server-authored edits join the same EditSet artifact as cell-authored ones."
            .to_string(),
        declarations: r#"type SpanChange = { span: Span; new_text: string };
declare const lsp: {
  /** Workspace-wide rename of the symbol the span points at (whole-item spans fine — snaps to the name identifier). Returns hash-anchored server-authored changes for edits.merge. Fails closed if rust-analyzer is unavailable; stale_span on content drift. */
  rename(args: { span: Span; newName: string }): Promise<{ changes: SpanChange[]; files: string[]; edit_count: number; authority: "lsp"; language: string }>;
};"#
            .to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn byte_position_round_trips_with_multibyte() {
        let source = "fn a() {}\n// caf\u{e9} \u{1f980} note\nfn b() {}\n";
        for (byte, _) in source.char_indices() {
            let pos = byte_to_position(source, byte);
            assert_eq!(
                position_to_byte(source, pos).unwrap(),
                byte,
                "round trip at byte {byte}"
            );
        }
    }

    #[test]
    fn byte_to_position_counts_utf16_units() {
        // '🦀' is 4 bytes / 2 UTF-16 units.
        let source = "\u{1f980}x";
        let pos = byte_to_position(source, 4);
        assert_eq!(pos.line, 0);
        assert_eq!(pos.character, 2);
    }
}
