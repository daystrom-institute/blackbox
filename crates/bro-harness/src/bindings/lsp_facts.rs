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
//! algebra consumes (`edits.merge`). Every returned change is recorded in
//! the provenance [`ledger`](super::ledger) at `lsp_verified`, so an
//! EditSet assembled purely from these changes applies with
//! `semantic_status: "lsp_verified"` — lineage is recomputed host-side at
//! the choke point, never read from cell-supplied tags.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use bro_lsp::{Language, LspConfig, OpenDocument, SessionPool};
use bro_tools::{Tool, ToolAnnotations, ToolCx, ToolResult};
use serde::Deserialize;
use serde_json::{Value, json};

use super::code_facts::{Span, span_schema_pub};
use super::ledger::{AuthorityTier, ProvenanceLedger};

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

/// Route a source file to its language-server backend by extension. Today
/// Rust (rust-analyzer) and Java (JDTLS) are wired; everything else is an
/// explicit error so a cell can't accidentally warm the wrong backend.
fn language_for_file(abs: &Path) -> Result<Language, String> {
    match abs.extension().and_then(|e| e.to_str()) {
        Some("rs") => Ok(Language::Rust),
        Some("java") => Ok(Language::Java),
        Some(ext) => Err(format!(
            "lsp: unsupported file extension `{ext}` — rust-analyzer (.rs) and jdtls (.java) are wired"
        )),
        None => Err("lsp: file has no extension".to_string()),
    }
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
        Ok(source
            .len()
            .min(offset + source[offset..].find('\n').unwrap_or(source.len() - offset)))
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
        language: Language,
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
                    .open_document(root, language, abs, 1, source.to_string())
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

    pub(super) async fn will_rename_files(
        &self,
        root: &Path,
        language: Language,
        renames: Vec<(PathBuf, PathBuf)>,
    ) -> Result<Option<lsp_types::WorkspaceEdit>, String> {
        self.pool
            .will_rename_files(root, language, renames)
            .await
            .map_err(render_lsp_error)
    }

    pub(super) async fn ensure_file_current(
        &self,
        root: &Path,
        abs: &Path,
        language: Language,
    ) -> Result<OpenDocument, String> {
        let bytes = tokio::fs::read(abs)
            .await
            .map_err(|e| format!("lsp: read {}: {e}", abs.display()))?;
        let sha = bbox_refactor::sha256_hex(&bytes);
        let source = String::from_utf8_lossy(&bytes).to_string();
        self.ensure_current(root, abs, language, &source, &sha)
            .await
    }

    async fn execute_command(
        &self,
        root: &Path,
        language: Language,
        command: String,
        arguments: Vec<Value>,
    ) -> Result<Option<Value>, String> {
        self.pool
            .execute_command(root, language, command, arguments)
            .await
            .map_err(render_lsp_error)
    }

    pub(super) async fn request_raw(
        &self,
        root: &Path,
        language: Language,
        method: &str,
        params: Value,
    ) -> Result<Value, String> {
        self.pool
            .request_raw(root, language, method.to_string(), params)
            .await
            .map_err(render_lsp_error)
    }
}

fn render_lsp_error(e: bro_lsp::Error) -> String {
    if e.is_lsp_unavailable() {
        format!(
            "lsp_unavailable: {e} — refusing to proceed (no syntax-only downgrade); install/configure the language server or use code.*/edits.* with explicit spans"
        )
    } else {
        format!("{e}")
    }
}

pub(super) struct FlattenedWorkspaceEdit {
    pub(super) text_edits: Vec<(lsp_types::Url, Vec<lsp_types::TextEdit>)>,
    pub(super) resource_ops: Vec<Value>,
}

pub(super) fn flatten_workspace_edit(edit: lsp_types::WorkspaceEdit) -> FlattenedWorkspaceEdit {
    let mut text_edits = Vec::new();
    let mut resource_ops = Vec::new();
    if let Some(changes) = edit.changes {
        for (uri, edits) in changes {
            text_edits.push((uri, edits));
        }
    }
    if let Some(document_changes) = edit.document_changes {
        match document_changes {
            lsp_types::DocumentChanges::Edits(edits) => {
                for doc_edit in edits {
                    text_edits.push((
                        doc_edit.text_document.uri,
                        flatten_doc_edits(doc_edit.edits),
                    ));
                }
            }
            lsp_types::DocumentChanges::Operations(ops) => {
                for op in ops {
                    match op {
                        lsp_types::DocumentChangeOperation::Edit(doc_edit) => {
                            text_edits.push((
                                doc_edit.text_document.uri,
                                flatten_doc_edits(doc_edit.edits),
                            ));
                        }
                        lsp_types::DocumentChangeOperation::Op(resource_op) => {
                            resource_ops.push(resource_op_to_value(resource_op));
                        }
                    }
                }
            }
        }
    }
    FlattenedWorkspaceEdit {
        text_edits,
        resource_ops,
    }
}

fn flatten_doc_edits(
    edits: Vec<lsp_types::OneOf<lsp_types::TextEdit, lsp_types::AnnotatedTextEdit>>,
) -> Vec<lsp_types::TextEdit> {
    edits
        .into_iter()
        .map(|e| match e {
            lsp_types::OneOf::Left(edit) => edit,
            lsp_types::OneOf::Right(annotated) => annotated.text_edit,
        })
        .collect()
}

fn resource_op_to_value(op: lsp_types::ResourceOp) -> Value {
    match op {
        lsp_types::ResourceOp::Create(create) => {
            json!({ "kind": "create", "uri": create.uri.to_string() })
        }
        lsp_types::ResourceOp::Rename(rename) => json!({
            "kind": "rename",
            "old_uri": rename.old_uri.to_string(),
            "new_uri": rename.new_uri.to_string(),
        }),
        lsp_types::ResourceOp::Delete(delete) => {
            json!({ "kind": "delete", "uri": delete.uri.to_string() })
        }
    }
}

pub(super) async fn text_edits_to_changes(
    root: &Path,
    context: &str,
    by_uri: Vec<(lsp_types::Url, Vec<lsp_types::TextEdit>)>,
) -> Result<Vec<(Span, String)>, String> {
    let mut changes: Vec<(Span, String)> = Vec::new();
    for (uri, edits) in by_uri {
        let path = match uri.to_file_path() {
            Ok(p) => p,
            Err(()) => return Err(format!("{context}: non-file uri in WorkspaceEdit: {uri}")),
        };
        let rel = match path.strip_prefix(root) {
            Ok(r) => r.to_string_lossy().to_string(),
            Err(_) => match root.canonicalize() {
                Ok(canon) => match path.strip_prefix(&canon) {
                    Ok(r) => r.to_string_lossy().to_string(),
                    Err(_) => {
                        return Err(format!(
                            "{context}: WorkspaceEdit touches {} outside the worktree root",
                            path.display()
                        ));
                    }
                },
                Err(e) => return Err(format!("{context}: {e}")),
            },
        };
        let bytes = tokio::fs::read(&path)
            .await
            .map_err(|e| format!("{context}: {rel}: {e}"))?;
        let file_sha = bbox_refactor::sha256_hex(&bytes);
        let file_source = String::from_utf8_lossy(&bytes).to_string();
        for text_edit in edits {
            let byte_start = position_to_byte(&file_source, text_edit.range.start)
                .map_err(|e| format!("{context}: {rel}: {e}"))?;
            let byte_end = position_to_byte(&file_source, text_edit.range.end)
                .map_err(|e| format!("{context}: {rel}: {e}"))?;
            changes.push((
                Span {
                    file: rel.clone(),
                    byte_start,
                    byte_end,
                    content_sha256: file_sha.clone(),
                },
                text_edit.new_text,
            ));
        }
    }
    Ok(changes)
}

pub(super) fn apply_text_edits_to_source(
    source: &str,
    edits: &[lsp_types::TextEdit],
    context: &str,
) -> Result<String, String> {
    let mut ranges = Vec::new();
    for edit in edits {
        let start = position_to_byte(source, edit.range.start)
            .map_err(|e| format!("{context}: moved file: {e}"))?;
        let end = position_to_byte(source, edit.range.end)
            .map_err(|e| format!("{context}: moved file: {e}"))?;
        if start > end {
            return Err(format!("{context}: moved file edit has inverted range"));
        }
        ranges.push((start, end, edit.new_text.as_str()));
    }
    ranges.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| b.1.cmp(&a.1)));
    let mut out = source.to_string();
    let mut previous_start = source.len() + 1;
    for (start, end, replacement) in ranges {
        if end > previous_start {
            return Err(format!("{context}: moved file edits overlap"));
        }
        out.replace_range(start..end, replacement);
        previous_start = start;
    }
    Ok(out)
}

/// `lsp.rename` — server-authority rename, returning span-shaped changes
/// recorded in the provenance ledger at `lsp_verified`.
pub struct LspRename(pub Arc<LspState>, pub Arc<ProvenanceLedger>);

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
        "Rename the symbol a hash-anchored Span points at, across the workspace, via the language server (rust-analyzer for Rust, JDTLS for Java; warms on first use). Whole-item spans are fine — the binding snaps to the item's name identifier automatically. Returns server-authored changes as hash-anchored {span, new_text} entries — feed them to edits.merge, then edits.apply. Fails closed when the server is unavailable (never downgrades to text matching). Errors with stale_span if the file changed since the Span was minted."
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
        let language = match language_for_file(&abs) {
            Ok(l) => l,
            Err(e) => return err(format!("lsp.rename: {e}")),
        };
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
        let doc = match self
            .0
            .ensure_current(&cx.root, &abs, language, &source, &sha)
            .await
        {
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

        let flattened = flatten_workspace_edit(edit);
        if flattened.text_edits.is_empty() && flattened.resource_ops.is_empty() {
            return err("lsp.rename: server returned an empty WorkspaceEdit");
        }

        // Convert to hash-anchored span changes, reading each touched file
        // once to mint its generation hash.
        let changes =
            match text_edits_to_changes(&cx.root, "lsp.rename", flattened.text_edits).await {
                Ok(changes) => changes,
                Err(e) => return err(e),
            };
        // Server-authored changes enter the ledger; edits.merge recognizes
        // them by content digest, so the EditSet's lineage carries
        // lsp_verified through to apply.
        let issuance = self.1.record_changes(
            "lsp.rename",
            AuthorityTier::LspVerified,
            changes.iter().map(|(span, text)| (span, text.as_str())),
        );
        let files: std::collections::BTreeSet<&str> =
            changes.iter().map(|(span, _)| span.file.as_str()).collect();
        ToolResult::Json(json!({
            "changes": changes
                .iter()
                .map(|(span, new_text)| json!({ "span": span, "new_text": new_text }))
                .collect::<Vec<Value>>(),
            "files": files,
            "edit_count": changes.len(),
            "authority": "lsp",
            "language": language.language_id(),
            "resource_ops": flattened.resource_ops,
            "issuance": issuance,
            "provenance": AuthorityTier::LspVerified.as_str(),
        }))
    }
}

/// `lsp.willRenameFiles` — ask the language server for edits that belong
/// before a client-side file move/rename.
pub struct LspWillRenameFiles(pub Arc<LspState>, pub Arc<ProvenanceLedger>);

#[derive(Deserialize)]
struct WillRenameFilesParams {
    renames: Vec<FileRenameParam>,
    #[serde(default)]
    language: Option<String>,
}

#[derive(Deserialize)]
struct FileRenameParam {
    #[serde(rename = "oldFile", alias = "old_file")]
    old_file: String,
    #[serde(rename = "newFile", alias = "new_file")]
    new_file: String,
}

#[async_trait]
impl Tool for LspWillRenameFiles {
    fn name(&self) -> &str {
        "lsp.willRenameFiles"
    }
    fn description(&self) -> &str {
        "Ask the language server for workspace edits that must be applied before client-side file renames: provide oldFile/newFile pairs, get hash-anchored edits for edits.merge plus any resource ops the server reports. Fails closed when the server is unavailable."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "renames": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "properties": {
                            "oldFile": { "type": "string" },
                            "newFile": { "type": "string" }
                        },
                        "required": ["oldFile", "newFile"]
                    }
                },
                "language": { "type": "string", "enum": ["java", "rust"], "description": "Defaults to java." }
            },
            "required": ["renames"]
        })
    }
    fn annotations(&self) -> ToolAnnotations {
        ToolAnnotations {
            read_only: true,
            destructive: false,
        }
    }
    fn namespace_binding(&self) -> Option<(String, String)> {
        Some(("lsp".to_string(), "willRenameFiles".to_string()))
    }
    async fn call(&self, input: Value, cx: &ToolCx) -> ToolResult {
        let params: WillRenameFilesParams = match serde_json::from_value(input) {
            Ok(p) => p,
            Err(e) => {
                return err(format!(
                    "lsp.willRenameFiles: bad input — expected {{ renames: [{{oldFile, newFile}}], language? }}; {e}"
                ));
            }
        };
        if params.renames.is_empty() {
            return err("lsp.willRenameFiles: renames must not be empty");
        }
        let language = match params.language.as_deref().unwrap_or("java") {
            "java" => Language::Java,
            "rust" => Language::Rust,
            other => {
                return err(format!(
                    "lsp.willRenameFiles: unsupported language `{other}`"
                ));
            }
        };
        let mut renames = Vec::new();
        for rename in &params.renames {
            let old_abs = match bro_tools::workspace::resolve_in_root(&cx.root, &rename.old_file) {
                Ok(p) => p,
                Err(e) => return err(format!("lsp.willRenameFiles: {}: {e}", rename.old_file)),
            };
            let new_abs = match resolve_target_in_root(&cx.root, &rename.new_file) {
                Ok(p) => p,
                Err(e) => return err(format!("lsp.willRenameFiles: {e}")),
            };
            renames.push((old_abs, new_abs));
        }
        let edit = match self.0.will_rename_files(&cx.root, language, renames).await {
            Ok(Some(edit)) => edit,
            Ok(None) => return err("lsp.willRenameFiles: server returned no WorkspaceEdit"),
            Err(e) => return err(format!("lsp.willRenameFiles: {e}")),
        };
        let flattened = flatten_workspace_edit(edit);
        if flattened.text_edits.is_empty() && flattened.resource_ops.is_empty() {
            return err("lsp.willRenameFiles: server returned an empty WorkspaceEdit");
        }
        let changes = match text_edits_to_changes(
            &cx.root,
            "lsp.willRenameFiles",
            flattened.text_edits,
        )
        .await
        {
            Ok(changes) => changes,
            Err(e) => return err(e),
        };
        let issuance = self.1.record_changes(
            "lsp.willRenameFiles",
            AuthorityTier::LspVerified,
            changes.iter().map(|(span, text)| (span, text.as_str())),
        );
        let files: std::collections::BTreeSet<&str> =
            changes.iter().map(|(span, _)| span.file.as_str()).collect();
        ToolResult::Json(json!({
            "changes": changes
                .iter()
                .map(|(span, new_text)| json!({ "span": span, "new_text": new_text }))
                .collect::<Vec<Value>>(),
            "files": files,
            "edit_count": changes.len(),
            "resource_ops": flattened.resource_ops,
            "authority": "lsp",
            "language": language.language_id(),
            "issuance": issuance,
            "provenance": AuthorityTier::LspVerified.as_str(),
        }))
    }
}

fn resolve_target_in_root(root: &Path, file: &str) -> Result<PathBuf, String> {
    let rel = Path::new(file);
    if rel.is_absolute()
        || rel
            .components()
            .any(|component| matches!(component, std::path::Component::ParentDir))
    {
        return Err(format!(
            "target file must be a workspace-relative path without `..`: {file}"
        ));
    }
    Ok(root.join(rel))
}

/// `lsp.executeCommand` — generic language-server command seam for
/// server-specific refactor commands.
pub struct LspExecuteCommand(pub Arc<LspState>);

#[derive(Deserialize)]
struct ExecuteCommandBindingParams {
    command: String,
    #[serde(default)]
    arguments: Vec<Value>,
    #[serde(default)]
    language: Option<String>,
}

#[async_trait]
impl Tool for LspExecuteCommand {
    fn name(&self) -> &str {
        "lsp.executeCommand"
    }
    fn description(&self) -> &str {
        "Execute a language-server workspace command through workspace/executeCommand. Defaults to Java/JDTLS. This is the generic harness seam for server-specific refactor commands that are not modeled as standard LSP requests."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "command": { "type": "string" },
                "arguments": { "type": "array", "items": {} },
                "language": { "type": "string", "enum": ["java", "rust"], "description": "Defaults to java." }
            },
            "required": ["command"]
        })
    }
    fn annotations(&self) -> ToolAnnotations {
        ToolAnnotations {
            read_only: true,
            destructive: false,
        }
    }
    fn namespace_binding(&self) -> Option<(String, String)> {
        Some(("lsp".to_string(), "executeCommand".to_string()))
    }
    async fn call(&self, input: Value, cx: &ToolCx) -> ToolResult {
        let params: ExecuteCommandBindingParams = match serde_json::from_value(input) {
            Ok(p) => p,
            Err(e) => {
                return err(format!(
                    "lsp.executeCommand: bad input — expected {{ command, arguments?, language? }}; {e}"
                ));
            }
        };
        let language = match params.language.as_deref().unwrap_or("java") {
            "java" => Language::Java,
            "rust" => Language::Rust,
            other => {
                return err(format!(
                    "lsp.executeCommand: unsupported language `{other}`"
                ));
            }
        };
        let result = match self
            .0
            .execute_command(&cx.root, language, params.command.clone(), params.arguments)
            .await
        {
            Ok(result) => result,
            Err(e) => return err(format!("lsp.executeCommand: {e}")),
        };
        ToolResult::Json(json!({
            "command": params.command,
            "language": language.language_id(),
            "result": result,
        }))
    }
}

/// The `lsp.*` binding set, sharing one session-scoped [`LspState`] and the
/// session's provenance ledger.
pub fn tools(state: Arc<LspState>, ledger: Arc<ProvenanceLedger>) -> Vec<Arc<dyn Tool>> {
    vec![
        Arc::new(LspRename(state.clone(), Arc::clone(&ledger))) as Arc<dyn Tool>,
        Arc::new(LspWillRenameFiles(state.clone(), ledger)) as Arc<dyn Tool>,
        Arc::new(LspExecuteCommand(state.clone())) as Arc<dyn Tool>,
        Arc::new(LspHover(state)) as Arc<dyn Tool>,
    ]
}

/// Flatten LSP `Hover` contents (any of the three `HoverContents` shapes)
/// into a single string for the caller. MarkupContent value is taken
/// verbatim (it already carries markdown fences); `MarkedString` language
/// blocks are joined with blank lines.
fn hover_text(contents: lsp_types::HoverContents) -> String {
    use lsp_types::HoverContents;
    match contents {
        HoverContents::Scalar(s) => marked_string_to_string(s),
        HoverContents::Array(arr) => arr
            .into_iter()
            .map(marked_string_to_string)
            .collect::<Vec<_>>()
            .join("\n\n"),
        HoverContents::Markup(m) => m.value,
    }
}

fn marked_string_to_string(s: lsp_types::MarkedString) -> String {
    match s {
        lsp_types::MarkedString::String(s) => s,
        lsp_types::MarkedString::LanguageString(ls) => ls.value,
    }
}

/// `lsp.hover` — authoritative resolved type/info for the symbol a
/// hash-anchored Span points at, via the language server (rust-analyzer for
/// Rust, JDTLS for Java; warms on first use). THE Java `var` SEAM: point a
/// Span at a `var x = ...` declarator and JDTLS returns the resolved type —
/// cross-file receiver return types and generic parameters (e.g. jOOQ
/// `Table<R>.newRecord()` → `R`) that the pure-bytes facts module cannot
/// derive. Read-only: returns the raw hover contents for the caller to
/// interpret (no provenance ledger — hover produces no edits). `null`
/// contents when the server has no info at the position. Fails closed when
/// the server is unavailable (never downgrades to text matching); stale_span
/// on content drift.
pub struct LspHover(pub Arc<LspState>);

#[derive(Deserialize)]
struct HoverBindingParams {
    span: Span,
}

#[async_trait]
impl Tool for LspHover {
    fn name(&self) -> &str {
        "lsp.hover"
    }
    fn description(&self) -> &str {
        "Resolve the type/info for the symbol a hash-anchored Span points at, via the language server (rust-analyzer for Rust, JDTLS for Java; warms on first use). Whole-item spans are fine — the binding snaps to the item's name identifier like lsp.rename. THE Java var SEAM: point a Span at a `var x = ...` declarator (or any symbol) and get the server's authoritative resolved type — JDTLS resolves cross-file receiver return types and generic parameters (e.g. jOOQ Table<R>.newRecord() -> R) that pure-bytes facts cannot. Returns { contents, language, position }; contents is null when the server has no info there. Fails closed when the server is unavailable; stale_span if the file changed since the Span was minted."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "span": span_schema_pub(),
            },
            "required": ["span"]
        })
    }
    fn annotations(&self) -> ToolAnnotations {
        ToolAnnotations {
            read_only: true,
            destructive: false,
        }
    }
    fn namespace_binding(&self) -> Option<(String, String)> {
        Some(("lsp".to_string(), "hover".to_string()))
    }
    async fn call(&self, input: Value, cx: &ToolCx) -> ToolResult {
        let params: HoverBindingParams = match serde_json::from_value(input) {
            Ok(p) => p,
            Err(e) => {
                return err(format!(
                    "lsp.hover: bad input — expected {{ span: Span }}; {e}"
                ));
            }
        };
        let span = params.span;
        let abs = match bro_tools::workspace::resolve_in_root(&cx.root, &span.file) {
            Ok(p) => p,
            Err(e) => return err(format!("lsp.hover: {}: {e}", span.file)),
        };
        let language = match language_for_file(&abs) {
            Ok(l) => l,
            Err(e) => return err(format!("lsp.hover: {e}")),
        };
        let bytes = match tokio::fs::read(&abs).await {
            Ok(b) => b,
            Err(e) => return err(format!("lsp.hover: {}: {e}", span.file)),
        };
        let sha = bbox_refactor::sha256_hex(&bytes);
        if sha != span.content_sha256 {
            return err(format!(
                "lsp.hover: stale_span: {} changed since the span was minted (span hash {}, current {sha}); re-derive the span from fresh facts",
                span.file, span.content_sha256
            ));
        }
        let source = String::from_utf8_lossy(&bytes).to_string();
        let doc = match self
            .0
            .ensure_current(&cx.root, &abs, language, &source, &sha)
            .await
        {
            Ok(d) => d,
            Err(e) => return err(format!("lsp.hover: {e}")),
        };
        // gap-e5023a54: same snap as lsp.rename — a whole-item span starts at
        // annotations/modifiers, where the server hovers to nothing.
        let aim_byte = {
            let abs = abs.clone();
            let (start, end) = (span.byte_start, span.byte_end);
            let snapped = bro_tools::tool::call_blocking(move || {
                let snapped = bbox_refactor::facts::name_span(&abs, start, end)
                    .ok()
                    .flatten()
                    .map(|(name_start, _)| name_start)
                    .unwrap_or(start);
                ToolResult::Json(json!(snapped))
            })
            .await;
            match snapped {
                ToolResult::Json(v) => v.as_u64().map(|b| b as usize).unwrap_or(span.byte_start),
                _ => span.byte_start,
            }
        };
        let position = byte_to_position(&source, aim_byte);
        let hover = match self.0.pool.hover(&doc, position).await {
            Ok(h) => h,
            Err(e) => return err(format!("lsp.hover: {}", render_lsp_error(e))),
        };
        let contents = hover.map(|h| hover_text(h.contents));
        ToolResult::Json(json!({
            "contents": contents,
            "language": language.language_id(),
            "position": position,
        }))
    }
}

/// Hand-authored namespace documentation + TS declarations (cell-dsl §5.2).
pub fn namespace_description() -> bro_code_mode::ToolNamespaceDescription {
    bro_code_mode::ToolNamespaceDescription {
        name: "lsp".to_string(),
        description: "Language-server authority. Session-backed: the first call in a workspace warms the backend for that language (may take a few seconds cold — rust-analyzer indexes the crate, JDTLS imports the gradle/maven workspace; the call blocks, no need to poll); later calls are fast. Backends: rust-analyzer (Rust, .rs) and JDTLS (Java, .java). Fails closed when the server is unavailable — there is deliberately no silent fallback to text matching. Verbs: `lsp.rename` (rust + java), `lsp.willRenameFiles` (standard file move/rename preflight edits), `lsp.executeCommand` (server-specific command seam), and `lsp.hover` (rust + java). THE RENAME RECIPE: aim a Span at the symbol, then `const r = await lsp.rename({ span, newName: \"x\" })` → `await edits.merge({ es, changes: r.changes })` → `await edits.apply({ es })` — server-authored edits join the same EditSet artifact as cell-authored ones; the host ledgers them at lsp_verified, so pass them through UNMODIFIED (filtering is fine, rewriting a change's bytes floors it at syntax_only). THE JAVA MOVE SEAM: use `java.moveClass` / `java.movePackage`; those tools call JDTLS `java/getMoveDestinations` and `java/move` directly instead of the standard file-operation preflight when JDTLS does not supply edits there. THE Java var SEAM: `lsp.hover` resolves a `var x = ...` declarator's type authoritatively — JDTLS resolves cross-file receiver return types and generic parameters (jOOQ Table<R>.newRecord() -> R) that `analysis.methodRegions` (pure-bytes facts) leaves as resolved_type:null. Recipe shape: for each unresolved var, `const h = await lsp.hover({ span: varDeclaratorSpan });` and pass the parsed type as an explicit parameter to `java.extractMethodCodeBlock`."
            .to_string(),
        declarations: r#"type SpanChange = { span: Span; new_text: string };
type HoverResult = { contents: string | null; language: string; position: { line: number; character: number } };
declare const lsp: {
  /** Workspace-wide rename of the symbol the span points at (whole-item spans fine — snaps to the name identifier). Rust and Java. Returns hash-anchored server-authored changes for edits.merge; the host ledgers them at lsp_verified, so an EditSet built purely from them applies with semantic_status "lsp_verified" (hand-editing a change drops it to syntax_only). Fails closed if the language server is unavailable; stale_span on content drift. */
  rename(args: { span: Span; newName: string }): Promise<{ changes: SpanChange[]; files: string[]; edit_count: number; resource_ops: unknown[]; authority: "lsp"; language: string; issuance: string; provenance: "lsp_verified" }>;
  /** Ask the language server for edits that must be applied before standard file renames. */
  willRenameFiles(args: { renames: Array<{ oldFile: string; newFile: string }>; language?: "java" | "rust" }): Promise<{ changes: SpanChange[]; files: string[]; edit_count: number; resource_ops: unknown[]; authority: "lsp"; language: string; issuance: string; provenance: "lsp_verified" }>;
  /** Execute a language-server workspace command. Defaults to Java/JDTLS. Use for server-specific refactor commands not modeled as standard LSP requests. */
  executeCommand(args: { command: string; arguments?: unknown[]; language?: "java" | "rust" }): Promise<{ command: string; language: string; result: unknown }>;
  /** Resolve the type/info for the symbol a Span points at (rust-analyzer for .rs, JDTLS for .java; warms on first use). Whole-item spans are fine — snaps to the item's name identifier like lsp.rename. THE Java var SEAM: point a Span at a `var x = ...` declarator and JDTLS returns the authoritative resolved type — cross-file receiver returns and generic params (jOOQ Table<R>.newRecord() -> R) that pure-bytes facts cannot derive. contents is the raw hover text (markdown/code blocks) for the caller to interpret, or null when the server has no info there. Read-only (no provenance ledger). Fails closed if the server is unavailable; stale_span on content drift. */
  hover(args: { span: Span }): Promise<HoverResult>;
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
