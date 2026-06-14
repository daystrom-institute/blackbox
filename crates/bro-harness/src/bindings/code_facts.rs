//! `code.*` — pure syntax facts over the working set, projected into cells.
//!
//! The v0 fact bindings from design/bro-harness/refactor-v2-pressure-test.md
//! §5: `code.items`, `code.query`, `code.read`, `code.spanUnion`. All pure
//! functions of the file bytes (tree-sitter via the bbox-refactor facts
//! substrate); provenance tier `syntax_only`. Every binding returns
//! hash-anchored [`Span`]s — the hash is captured where the bytes are read,
//! so drift-guarding is a property of the address, not a discipline
//! (code-mode-cell-dsl.md §3).
//!
//! Paths are normalized by the same [`bro_tools::workspace::resolve_in_root`]
//! helper the flat file tools use: relative paths join the session worktree
//! root, absolute paths are accepted as-is — there is no containment check
//! (gap-e0ae3e7d). Zero daemon reach-back (decision af3c4783).

use std::path::Path;
use std::sync::Arc;

use async_trait::async_trait;
use bbox_refactor::facts;
use bro_code_mode::ToolNamespaceDescription;
use bro_tools::{Tool, ToolAnnotations, ToolCx, ToolResult};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

/// Hash-anchored byte span — the cell DSL's composability quantum.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Span {
    /// File path the span was cut from. May be relative to the session
    /// worktree root or absolute (e.g. when a span is minted from a file
    /// outside the worktree).
    pub file: String,
    pub byte_start: usize,
    pub byte_end: usize,
    /// sha256 of the full file content the span was cut from.
    pub content_sha256: String,
}

fn span_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "file": { "type": "string", "description": "File path. May be relative to the session worktree root or absolute." },
            "byte_start": { "type": "integer", "minimum": 0 },
            "byte_end": { "type": "integer", "minimum": 0 },
            "content_sha256": { "type": "string", "description": "sha256 of the full file content the span was cut from." }
        },
        "required": ["file", "byte_start", "byte_end", "content_sha256"]
    })
}

/// Span JSON schema for sibling binding modules (the edits algebra consumes
/// the same hash-anchored Spans the facts mint).
pub(super) fn span_schema_pub() -> Value {
    span_schema()
}

fn err(msg: impl std::fmt::Display) -> ToolResult {
    ToolResult::Error(msg.to_string())
}

/// Resolve a path through the same normalizer the flat file tools use —
/// relative paths join the effective session root, absolute paths are
/// accepted as-is. There is no containment boundary here.
fn resolve(root: &Path, file: &str) -> anyhow::Result<std::path::PathBuf> {
    bro_tools::workspace::resolve_in_root(root, file)
}

// Called from binding call_blocking closures (blocking pool); shared with
// the edits algebra for hash checks and snapshots.
#[allow(clippy::disallowed_methods)]
pub(super) fn read_file_bytes(path: &Path) -> std::io::Result<Vec<u8>> {
    std::fs::read(path)
}

/// One-of `file` / `files` target selection, shared by the multi-file fact
/// bindings (gap-3ec052ea: per-crate fan-out is the host's job, not a cell
/// for-loop).
#[derive(Deserialize)]
struct FactTargets {
    #[serde(default)]
    file: Option<String>,
    #[serde(default)]
    files: Option<Vec<String>>,
}

impl FactTargets {
    /// `(targets, multi)` — `multi` preserves the single-file return shape
    /// for `file:` callers while `files:` callers get the batched shape.
    fn resolve(self, tool: &str) -> Result<(Vec<String>, bool), ToolResult> {
        match (self.file, self.files) {
            (Some(f), None) => Ok((vec![f], false)),
            (None, Some(fs)) if !fs.is_empty() => Ok((fs, true)),
            (None, Some(_)) => Err(err(format!("{tool}: `files` must be non-empty"))),
            (Some(_), Some(_)) => Err(err(format!(
                "{tool}: pass `file` (single) OR `files` (batch), not both"
            ))),
            (None, None) => Err(err(format!("{tool}: `file` or `files` is required"))),
        }
    }
}

/// `code.items` — top-level syntax-item inventory with Spans.
pub struct CodeItems;

fn items_payload(file: &str, found: &facts::FileItemsFacts) -> Value {
    let items: Vec<Value> = found
        .items
        .iter()
        .map(|fact| {
            let item = &fact.item;
            json!({
                "name": item.name,
                "kind": item.kind,
                "visibility": fact.visibility,
                "span": Span {
                    file: file.to_string(),
                    byte_start: item.byte_start,
                    byte_end: item.byte_end,
                    content_sha256: found.content_sha256.clone(),
                },
                "trivia_span": Span {
                    file: file.to_string(),
                    byte_start: item.leading_trivia_start,
                    byte_end: item.trailing_trivia_end,
                    content_sha256: found.content_sha256.clone(),
                },
                "line_start": item.line_start,
                "line_end": item.line_end,
                "attributes": item.attributes,
            })
        })
        .collect();
    json!({
        "file": file,
        "language": found.language,
        "content_sha256": found.content_sha256,
        "source_len": found.source_len,
        "items": items,
    })
}

#[async_trait]
impl Tool for CodeItems {
    fn name(&self) -> &str {
        "code.items"
    }
    fn description(&self) -> &str {
        "Inventory the top-level syntax items of source files (tree-sitter; pure; syntax_only tier). Returns hash-anchored Spans for every item. Pass `file` for one file (flat result) or `files` for a host-side batch (per-file results; a bad file becomes an `error` entry, not a failed call)."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "file": { "type": "string", "description": "Source file path. Relative paths resolve against the session root; absolute paths are accepted as-is (single-file shape)." },
                "files": { "type": "array", "items": { "type": "string" }, "description": "Batch of paths (relative paths resolve against the session root, absolute paths are accepted as-is); the host fans out (use instead of a cell for-loop)." }
            }
        })
    }
    fn annotations(&self) -> ToolAnnotations {
        ToolAnnotations {
            read_only: true,
            destructive: false,
        }
    }
    fn namespace_binding(&self) -> Option<(String, String)> {
        Some(("code".to_string(), "items".to_string()))
    }
    async fn call(&self, input: Value, cx: &ToolCx) -> ToolResult {
        let params: FactTargets = match serde_json::from_value(input) {
            Ok(p) => p,
            Err(e) => return err(format!("code.items: {e}")),
        };
        let (targets, multi) = match params.resolve("code.items") {
            Ok(t) => t,
            Err(e) => return e,
        };
        let mut resolved = Vec::with_capacity(targets.len());
        for file in targets {
            match resolve(&cx.root, &file) {
                Ok(p) => resolved.push((file, p)),
                Err(e) => return err(format!("code.items: {file}: {e}")),
            }
        }
        bro_tools::tool::call_blocking(move || {
            if !multi {
                let (file, path) = &resolved[0];
                return match facts::file_items(path) {
                    Ok(found) => ToolResult::Json(items_payload(file, &found)),
                    Err(e) => err(format!("code.items: {e:#}")),
                };
            }
            let files: Vec<Value> = resolved
                .iter()
                .map(|(file, path)| match facts::file_items(path) {
                    Ok(found) => items_payload(file, &found),
                    Err(e) => json!({ "file": file, "error": format!("{e:#}") }),
                })
                .collect();
            ToolResult::Json(json!({ "files": files }))
        })
        .await
    }
}

/// `code.fields` — Java field declaration inventory with declaration spans.
pub struct CodeFields;

#[derive(Deserialize)]
struct CodeFieldsParams {
    file: String,
    #[serde(default, rename = "className", alias = "class_name")]
    class_name: Option<String>,
}

fn fields_payload(file: &str, found: &facts::FileJavaFieldsFacts) -> Value {
    let fields: Vec<Value> = found
        .fields
        .iter()
        .map(|field| {
            json!({
                "name": field.name,
                "type": field.type_text,
                "owner_class": field.owner_class,
                "visibility": field.visibility,
                "modifiers": field.modifiers,
                "annotations": field.annotations,
                "is_static": field.is_static,
                "is_final": field.is_final,
                "is_static_final": field.is_static && field.is_final,
                "is_mutable_instance": !field.is_static && !field.is_final,
                "span": Span {
                    file: file.to_string(),
                    byte_start: field.byte_start,
                    byte_end: field.byte_end,
                    content_sha256: found.content_sha256.clone(),
                },
                "name_span": Span {
                    file: file.to_string(),
                    byte_start: field.name_byte_start,
                    byte_end: field.name_byte_end,
                    content_sha256: found.content_sha256.clone(),
                },
            })
        })
        .collect();
    json!({
        "file": file,
        "language": found.language,
        "content_sha256": found.content_sha256,
        "source_len": found.source_len,
        "fields": fields,
    })
}

#[async_trait]
impl Tool for CodeFields {
    fn name(&self) -> &str {
        "code.fields"
    }
    fn description(&self) -> &str {
        "Inventory Java field declarations in one source file, returning name/type/modifiers/annotations/owner_class plus hash-anchored declaration and name spans. Optional className restricts to one owner class. Pure; syntax_only tier."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "file": { "type": "string", "description": "Workspace-relative .java source file." },
                "className": { "type": "string", "description": "Optional Java class name to restrict field results to one owner class." }
            },
            "required": ["file"]
        })
    }
    fn annotations(&self) -> ToolAnnotations {
        ToolAnnotations {
            read_only: true,
            destructive: false,
        }
    }
    fn namespace_binding(&self) -> Option<(String, String)> {
        Some(("code".to_string(), "fields".to_string()))
    }
    async fn call(&self, input: Value, cx: &ToolCx) -> ToolResult {
        let params: CodeFieldsParams = match serde_json::from_value(input) {
            Ok(p) => p,
            Err(e) => return err(format!("code.fields: {e}")),
        };
        let path = match resolve(&cx.root, &params.file) {
            Ok(p) => p,
            Err(e) => return err(format!("code.fields: {}: {e}", params.file)),
        };
        let file = params.file;
        let class_name = params.class_name;
        bro_tools::tool::call_blocking(move || {
            match facts::java_fields(&path, class_name.as_deref()) {
                Ok(found) => ToolResult::Json(fields_payload(&file, &found)),
                Err(e) => err(format!("code.fields: {e:#}")),
            }
        })
        .await
    }
}

/// `code.files` — enumerate the tree-sitter-supported source files of the
/// working set (gap-3ec052ea's original ask: fact pipelines self-contained,
/// no tools.glob bootstrap).
pub struct CodeFiles;

#[derive(Deserialize)]
struct CodeFilesParams {
    #[serde(default)]
    dir: Option<String>,
    #[serde(default)]
    language: Option<String>,
}

#[async_trait]
impl Tool for CodeFiles {
    fn name(&self) -> &str {
        "code.files"
    }
    fn description(&self) -> &str {
        "Enumerate the source files the code.* facts can parse (pure walk; skips dot-dirs and target/node_modules/build/dist/vendor). Optional `dir` narrows to a subdirectory, `language` to one language (e.g. \"rust\", \"java\"). Feed the result straight into code.items({files}) / code.query({files})."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "dir": { "type": "string", "description": "Subdirectory to enumerate. Relative paths resolve against the session worktree root; absolute paths are accepted as-is. Default: the whole workspace." },
                "language": { "type": "string", "description": "Restrict to one language name, as reported by code.items (e.g. \"rust\")." }
            }
        })
    }
    fn annotations(&self) -> ToolAnnotations {
        ToolAnnotations {
            read_only: true,
            destructive: false,
        }
    }
    fn namespace_binding(&self) -> Option<(String, String)> {
        Some(("code".to_string(), "files".to_string()))
    }
    async fn call(&self, input: Value, cx: &ToolCx) -> ToolResult {
        let params: CodeFilesParams = match serde_json::from_value(input) {
            Ok(p) => p,
            Err(e) => return err(format!("code.files: {e}")),
        };
        // Confine `dir` exactly like file targets.
        if let Some(dir) = &params.dir
            && let Err(e) = resolve(&cx.root, dir)
        {
            return err(format!("code.files: {dir}: {e}"));
        }
        let root = cx.root.clone();
        bro_tools::tool::call_blocking(move || {
            match facts::source_files(&root, params.dir.as_deref(), params.language.as_deref()) {
                Ok((files, truncated)) => {
                    let entries: Vec<Value> = files
                        .iter()
                        .map(|f| json!({ "file": f.file, "language": f.language }))
                        .collect();
                    ToolResult::Json(json!({
                        "files": entries,
                        "count": entries.len(),
                        "truncated": truncated,
                    }))
                }
                Err(e) => err(format!("code.files: {e:#}")),
            }
        })
        .await
    }
}

/// `code.query` — run a tree-sitter query, returning captures with Spans.
pub struct CodeQuery;

#[derive(Deserialize)]
struct CodeQueryParams {
    #[serde(flatten)]
    targets: FactTargets,
    query: String,
    #[serde(default)]
    within: Option<WithinRange>,
}

#[derive(Deserialize)]
struct WithinRange {
    byte_start: usize,
    byte_end: usize,
}

#[async_trait]
impl Tool for CodeQuery {
    fn name(&self) -> &str {
        "code.query"
    }
    fn description(&self) -> &str {
        "Run a tree-sitter query over source files (pure; syntax_only tier). Captures carry hash-anchored Spans. Pass `file` for one file, or `files` for a host-side batch — cross-file symbol search is ONE call (captures from every file in a flat array; each span names its file). `within` restricts to a byte range (single-file only)."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "file": { "type": "string", "description": "Source file path. Relative paths resolve against the session root; absolute paths are accepted as-is (single-file shape)." },
                "files": { "type": "array", "items": { "type": "string" }, "description": "Batch of paths (relative paths resolve against the session root, absolute paths are accepted as-is); the host fans out and returns a flat captures array." },
                "query": { "type": "string", "description": "Tree-sitter query source, e.g. \"(function_item name: (identifier) @fn_name)\"." },
                "within": {
                    "type": "object",
                    "description": "Optional byte range; only matches intersecting it are returned (single-file only).",
                    "properties": {
                        "byte_start": { "type": "integer", "minimum": 0 },
                        "byte_end": { "type": "integer", "minimum": 0 }
                    },
                    "required": ["byte_start", "byte_end"]
                }
            },
            "required": ["query"]
        })
    }
    fn annotations(&self) -> ToolAnnotations {
        ToolAnnotations {
            read_only: true,
            destructive: false,
        }
    }
    fn namespace_binding(&self) -> Option<(String, String)> {
        Some(("code".to_string(), "query".to_string()))
    }
    async fn call(&self, input: Value, cx: &ToolCx) -> ToolResult {
        let params: CodeQueryParams = match serde_json::from_value(input) {
            Ok(p) => p,
            Err(e) => return err(format!("code.query: {e}")),
        };
        let (targets, multi) = match params.targets.resolve("code.query") {
            Ok(t) => t,
            Err(e) => return e,
        };
        if multi && params.within.is_some() {
            return err("code.query: `within` is single-file only — pass `file`, not `files`");
        }
        let mut resolved = Vec::with_capacity(targets.len());
        for file in targets {
            match resolve(&cx.root, &file) {
                Ok(p) => resolved.push((file, p)),
                Err(e) => return err(format!("code.query: {file}: {e}")),
            }
        }
        let query = params.query.clone();
        let within = params.within.map(|w| (w.byte_start, w.byte_end));
        bro_tools::tool::call_blocking(move || {
            let captures_of = |file: &str, found: &facts::FileQueryFacts| -> Vec<Value> {
                found
                    .captures
                    .iter()
                    .map(|capture| {
                        json!({
                            "capture": capture.capture,
                            "kind": capture.kind,
                            "text": capture.text,
                            "span": Span {
                                file: file.to_string(),
                                byte_start: capture.byte_start,
                                byte_end: capture.byte_end,
                                content_sha256: found.content_sha256.clone(),
                            },
                        })
                    })
                    .collect()
            };
            if !multi {
                let (file, path) = &resolved[0];
                return match facts::file_query(path, &query, within) {
                    Ok(found) => {
                        let truncated = found.captures.len() >= facts::MAX_QUERY_CAPTURES;
                        ToolResult::Json(json!({
                            "file": file,
                            "language": found.language,
                            "content_sha256": found.content_sha256,
                            "captures": captures_of(file, &found),
                            "truncated": truncated,
                        }))
                    }
                    Err(e) => err(format!("code.query: {e:#}")),
                };
            }
            // Batch: flat captures (spans name their file), per-file roll-up.
            // Aggregate-capped: a broad query over a whole repo would
            // otherwise flatten to a multi-MB array that OOMs the isolate
            // (probe-dash-1, ~1,700 files). Once the flat array reaches the
            // ceiling, stop accumulating, mark truncated, and report how far
            // the scan got so the caller can narrow.
            let mut all_captures: Vec<Value> = Vec::new();
            let mut file_summaries: Vec<Value> = Vec::new();
            let mut truncated = false;
            let mut files_scanned = 0usize;
            let total_files = resolved.len();
            for (file, path) in &resolved {
                if all_captures.len() >= facts::MAX_AGGREGATE_QUERY_CAPTURES {
                    truncated = true;
                    break;
                }
                files_scanned += 1;
                match facts::file_query(path, &query, None) {
                    Ok(found) => {
                        truncated |= found.captures.len() >= facts::MAX_QUERY_CAPTURES;
                        let captures = captures_of(file, &found);
                        file_summaries.push(json!({
                            "file": file,
                            "language": found.language,
                            "content_sha256": found.content_sha256,
                            "captures": captures.len(),
                        }));
                        all_captures.extend(captures);
                    }
                    Err(e) => {
                        file_summaries.push(json!({ "file": file, "error": format!("{e:#}") }));
                    }
                }
            }
            // Every file erroring is one upstream mistake (usually a
            // malformed query) — fail the call so the cell sees it once,
            // instead of an empty-but-plausible captures array.
            if !file_summaries.is_empty() && file_summaries.iter().all(|f| f.get("error").is_some()) {
                let first = file_summaries[0]["error"].as_str().unwrap_or("unknown");
                return err(format!(
                    "code.query: all {} file(s) failed; first error: {first}",
                    file_summaries.len()
                ));
            }
            let mut payload = json!({
                "captures": all_captures,
                "files": file_summaries,
                "truncated": truncated,
            });
            if truncated && files_scanned < total_files {
                // Aggregate ceiling hit before the file set was exhausted —
                // tell the caller exactly how to bound the next call.
                payload["aggregate_capped"] = json!(true);
                payload["files_scanned"] = json!(files_scanned);
                payload["files_total"] = json!(total_files);
                payload["hint"] = json!(format!(
                    "stopped at {} captures across {files_scanned}/{total_files} files (aggregate cap). Narrow the query (more specific node/predicate) or the file set (a subdir via code.files({{dir}})), then re-run.",
                    all_captures.len()
                ));
            }
            ToolResult::Json(payload)
        })
        .await
    }
}

/// `code.read` — read the exact bytes of a Span, verifying its content hash.
pub struct CodeRead;

#[derive(Deserialize)]
struct CodeReadParams {
    span: Span,
}

#[async_trait]
impl Tool for CodeRead {
    fn name(&self) -> &str {
        "code.read"
    }
    fn description(&self) -> &str {
        "Read the exact source text of a hash-anchored Span (pure). Errors with `stale_span` if the file content no longer matches the Span's content_sha256."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": { "span": span_schema() },
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
        Some(("code".to_string(), "read".to_string()))
    }
    async fn call(&self, input: Value, cx: &ToolCx) -> ToolResult {
        let params: CodeReadParams = match serde_json::from_value(input) {
            Ok(p) => p,
            Err(e) => return err(format!("code.read: {e}")),
        };
        let path = match resolve(&cx.root, &params.span.file) {
            Ok(p) => p,
            Err(e) => return err(format!("code.read: {e}")),
        };
        let span = params.span;
        bro_tools::tool::call_blocking(move || {
            let bytes = match read_file_bytes(&path) {
                Ok(b) => b,
                Err(e) => return err(format!("code.read: {}: {e}", span.file)),
            };
            let current = bbox_refactor::sha256_hex(&bytes);
            if current != span.content_sha256 {
                return err(format!(
                    "code.read: stale_span: {} changed since the span was minted (span hash {}, current {current}); re-derive the span from fresh facts",
                    span.file, span.content_sha256
                ));
            }
            if span.byte_start > span.byte_end || span.byte_end > bytes.len() {
                return err(format!(
                    "code.read: span {}..{} out of bounds for {} ({} bytes)",
                    span.byte_start,
                    span.byte_end,
                    span.file,
                    bytes.len()
                ));
            }
            let text = String::from_utf8_lossy(&bytes[span.byte_start..span.byte_end]).to_string();
            let byte_length = span.byte_end.saturating_sub(span.byte_start);
            let char_length = text.chars().count();
            ToolResult::Json(json!({
                "text": text,
                "span": span,
                "byte_length": byte_length,
                "char_length": char_length,
                "truncated": false
            }))
        })
        .await
    }
}

/// `code.signature` — extract a callable declaration signature from the AST at a Span.
pub struct CodeSignature;

#[derive(Deserialize)]
struct CodeSignatureParams {
    span: Span,
}

#[async_trait]
impl Tool for CodeSignature {
    fn name(&self) -> &str {
        "code.signature"
    }
    fn description(&self) -> &str {
        "Extract the language-shaped signature of the callable declaration at (or enclosing) a hash-anchored Span. Rust returns function_item facts; Java returns method_declaration/constructor_declaration facts including params_span for formatting checks (pure; syntax_only tier). Errors with `stale_span` on content drift."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": { "span": span_schema() },
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
        Some(("code".to_string(), "signature".to_string()))
    }
    async fn call(&self, input: Value, cx: &ToolCx) -> ToolResult {
        let params: CodeSignatureParams = match serde_json::from_value(input) {
            Ok(p) => p,
            Err(e) => return err(format!("code.signature: {e}")),
        };
        let path = match resolve(&cx.root, &params.span.file) {
            Ok(p) => p,
            Err(e) => return err(format!("code.signature: {e}")),
        };
        let span = params.span;
        bro_tools::tool::call_blocking(move || {
            match facts::callable_signature(
                &path,
                span.byte_start,
                span.byte_end,
                Some(&span.content_sha256),
            ) {
                Ok(sig) => signature_payload(&span.file, sig),
                Err(e) => err(format!("code.signature: {e:#}")),
            }
        })
        .await
    }
}

fn range_payload(file: &str, range: &facts::SignatureRangeFact) -> Span {
    Span {
        file: file.to_string(),
        byte_start: range.byte_start,
        byte_end: range.byte_end,
        content_sha256: range.content_sha256.clone(),
    }
}

fn signature_payload(file: &str, sig: facts::SignatureFacts) -> ToolResult {
    match sig {
        facts::SignatureFacts::Rust(sig) => {
            let params_json: Vec<Value> = sig
                .params
                .iter()
                .map(|p| json!({ "pattern": p.pattern, "type": p.type_text }))
                .collect();
            ToolResult::Json(json!({
                "language": sig.language,
                "kind": sig.kind,
                "name": sig.name,
                "visibility": sig.visibility,
                "is_async": sig.is_async,
                "params": params_json,
                "return_type": sig.return_type,
                "generics": sig.generics,
                "span": Span {
                    file: file.to_string(),
                    byte_start: sig.byte_start,
                    byte_end: sig.byte_end,
                    content_sha256: sig.content_sha256,
                },
                "signature_span": range_payload(file, &sig.signature_span),
                "params_span": sig.params_span.as_ref().map(|range| range_payload(file, range)),
            }))
        }
        facts::SignatureFacts::Java(sig) => {
            let params_json: Vec<Value> = sig
                .params
                .iter()
                .map(|p| {
                    json!({
                        "name": p.name,
                        "type": p.type_text,
                        "modifiers": p.modifiers,
                        "annotations": p.annotations,
                        "varargs": p.varargs,
                        "span": Span {
                            file: file.to_string(),
                            byte_start: p.byte_start,
                            byte_end: p.byte_end,
                            content_sha256: sig.content_sha256.clone(),
                        },
                    })
                })
                .collect();
            ToolResult::Json(json!({
                "language": sig.language,
                "kind": sig.kind,
                "name": sig.name,
                "visibility": sig.visibility,
                "modifiers": sig.modifiers,
                "annotations": sig.annotations,
                "params": params_json,
                "return_type": sig.return_type,
                "type_parameters": sig.type_parameters,
                "throws": sig.throws,
                "throws_text": sig.throws_text,
                "span": Span {
                    file: file.to_string(),
                    byte_start: sig.byte_start,
                    byte_end: sig.byte_end,
                    content_sha256: sig.content_sha256,
                },
                "signature_span": range_payload(file, &sig.signature_span),
                "params_span": sig.params_span.as_ref().map(|range| range_payload(file, range)),
            }))
        }
    }
}

/// `code.spanUnion` — union same-file Spans into one covering Span (pure).
pub struct CodeSpanUnion;

#[derive(Deserialize)]
struct CodeSpanUnionParams {
    spans: Vec<Span>,
}

#[async_trait]
impl Tool for CodeSpanUnion {
    fn name(&self) -> &str {
        "code.spanUnion"
    }
    fn description(&self) -> &str {
        "Union hash-anchored Spans from one file into a single covering Span (pure; no I/O). All spans must share the same file and content_sha256."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "spans": { "type": "array", "items": span_schema(), "minItems": 1 }
            },
            "required": ["spans"]
        })
    }
    fn annotations(&self) -> ToolAnnotations {
        ToolAnnotations {
            read_only: true,
            destructive: false,
        }
    }
    fn namespace_binding(&self) -> Option<(String, String)> {
        Some(("code".to_string(), "spanUnion".to_string()))
    }
    async fn call(&self, input: Value, _cx: &ToolCx) -> ToolResult {
        let params: CodeSpanUnionParams = match serde_json::from_value(input) {
            Ok(p) => p,
            Err(e) => return err(format!("code.spanUnion: {e}")),
        };
        let Some(first) = params.spans.first() else {
            return err("code.spanUnion: `spans` must be non-empty");
        };
        for span in &params.spans {
            if span.file != first.file {
                return err(format!(
                    "code.spanUnion: spans cross files ({} vs {})",
                    first.file, span.file
                ));
            }
            if span.content_sha256 != first.content_sha256 {
                return err(format!(
                    "code.spanUnion: spans carry different content hashes for {} — re-derive from fresh facts",
                    first.file
                ));
            }
        }
        let union = Span {
            file: first.file.clone(),
            byte_start: params.spans.iter().map(|s| s.byte_start).min().unwrap_or(0),
            byte_end: params.spans.iter().map(|s| s.byte_end).max().unwrap_or(0),
            content_sha256: first.content_sha256.clone(),
        };
        ToolResult::Json(json!({ "span": union }))
    }
}

/// The `code.*` binding set.
pub fn tools() -> Vec<Arc<dyn Tool>> {
    vec![
        Arc::new(CodeFiles) as Arc<dyn Tool>,
        Arc::new(CodeItems) as Arc<dyn Tool>,
        Arc::new(CodeFields) as Arc<dyn Tool>,
        Arc::new(CodeQuery) as Arc<dyn Tool>,
        Arc::new(CodeRead) as Arc<dyn Tool>,
        Arc::new(CodeSignature) as Arc<dyn Tool>,
        Arc::new(CodeSpanUnion) as Arc<dyn Tool>,
    ]
}

/// Hand-authored namespace documentation + TS declarations (cell-dsl §5.2):
/// the cross-binding value type (`Span`) and the four signatures, curated
/// rather than schema-rendered.
pub fn namespace_description() -> ToolNamespaceDescription {
    ToolNamespaceDescription {
        name: "code".to_string(),
        description: "Pure syntax facts over the working set (tree-sitter). FOR ANY span/hash/structure work on source files, prefer `code.*` over raw file reads, shell, or regex — it is the canonical syntax-fact surface. Provenance tier: syntax_only. Spans are hash-anchored at read time — a Span from stale file content fails closed at consumption, so re-derive facts after any write to the file. The seven methods below are the complete `code` surface. Cross-file work is self-contained and ONE call deep: `code.files({ language: \"rust\" })` enumerates the working set, and `code.items`/`code.query` accept `files: string[]` so the host fans out — \"find symbol X in the crate\" is `code.query({ files, query })`, never a cell for-loop. Use `code.fields` for Java field declarations; do not hand-roll field_declaration queries just to learn modifiers/type/name. Keep intermediate facts in cell variables or `store()` — `text()` only the derived result, not raw inventories. THE RECIPE for signature predicates (\"public Rust fns returning Result\" or \"Java constructors with multiline params\"): `code.items` → filter callable kinds (`function_item`, `method_declaration`, `constructor_declaration`) → `Promise.all(items.map(i => code.signature({ span: i.span })))` → branch on `language`/`kind`. For Java formatting checks, read `params_span` rather than the whole file or raw regexing the constructor. Whole-file read: `code.read({ span: { file, byte_start: 0, byte_end: inv.source_len, content_sha256: inv.content_sha256 } })`. Query authoring: use real tree-sitter node kinds (the `kind` values returned by `code.items`/`code.query` are exactly those names) — e.g. Rust public functions are `(function_item (visibility_modifier)) @pub_fn`, function names `(function_item name: (identifier) @fn_name)`; Java callables are `method_declaration` / `constructor_declaration`. An `Invalid node type` error means the node name does not exist in that grammar, while an EMPTY `captures` array means the query is valid but matched nothing — do not read empty results as the surface being broken."
            .to_string(),
        declarations: r#"type Span = { file: string; byte_start: number; byte_end: number; content_sha256: string };
type SyntaxItemFact = { name?: string; kind: string; visibility?: string; span: Span; trivia_span: Span; line_start: number; line_end: number; attributes: string[] };
type JavaFieldFact = { name: string; type: string; owner_class?: string; visibility?: string; modifiers: string[]; annotations: string[]; is_static: boolean; is_final: boolean; is_static_final: boolean; is_mutable_instance: boolean; span: Span; name_span: Span };
type QueryCapture = { capture: string; kind: string; text: string; span: Span };
type FileItems = { file: string; language: string; content_sha256: string; source_len: number; items: SyntaxItemFact[] };
type FileFields = { file: string; language: "java"; content_sha256: string; source_len: number; fields: JavaFieldFact[] };
type RustSignature = { language: "rust"; kind: "function_item"; name?: string; visibility?: string; is_async: boolean; params: { pattern: string; type?: string }[]; return_type?: string; generics?: string; span: Span; signature_span: Span; params_span?: Span };
type JavaSignature = { language: "java"; kind: "method_declaration" | "constructor_declaration"; name?: string; visibility?: string; modifiers: string[]; annotations: string[]; params: { name?: string; type?: string; modifiers: string[]; annotations: string[]; varargs: boolean; span: Span }[]; return_type?: string; type_parameters?: string; throws: string[]; throws_text?: string; span: Span; signature_span: Span; params_span?: Span };
declare const code: {
  /** Enumerate parseable source files (skips dot-dirs, target, node_modules, build, dist, vendor). Feed straight into items({files})/query({files}). */
  files(args?: { dir?: string; language?: string }): Promise<{ files: { file: string; language: string }[]; count: number; truncated: boolean }>;
  /** Inventory top-level syntax items. visibility is "pub"/"public"/... or undefined = private. source_len enables whole-file Spans. `file` → flat shape; `files` → host-side batch ({ files: (FileItems | { file; error })[] }). */
  items(args: { file: string } | { files: string[] }): Promise<FileItems | { files: (FileItems | { file: string; error: string })[] }>;
  /** Inventory Java field declarations with type/modifiers/annotations/owner and hash-anchored declaration/name spans. Use this instead of raw field_declaration queries. */
  fields(args: { file: string; className?: string }): Promise<FileFields>;
  /** Tree-sitter query; captures carry hash-anchored Spans. `file` → per-file shape (within allowed); `files` → batch: flat captures across all files (each span names its file) + per-file roll-up. The batch is aggregate-capped (~20k captures): a broad query over a large repo sets aggregate_capped + files_scanned/files_total + hint — narrow the query or the file set and re-run rather than widening blindly. */
  query(args: { file: string; query: string; within?: { byte_start: number; byte_end: number } } | { files: string[]; query: string }): Promise<{ file: string; language: string; content_sha256: string; captures: QueryCapture[]; truncated: boolean } | { captures: QueryCapture[]; files: ({ file: string; language: string; content_sha256: string; captures: number } | { file: string; error: string })[]; truncated: boolean; aggregate_capped?: boolean; files_scanned?: number; files_total?: number; hint?: string }>;
  /** Read the exact text of a Span; errors with stale_span on content drift. `truncated` is always false for successful reads; UI/tool display may still elide long text. */
  read(args: { span: Span }): Promise<{ text: string; span: Span; byte_length: number; char_length: number; truncated: false }>;
  /** Language-shaped callable signature at/enclosing a Span. Rust returns function facts; Java returns method/constructor facts. `span` covers the whole item; `signature_span` covers the declaration header; `params_span` covers the raw parameter list for formatting checks. Errors with stale_span on drift. */
  signature(args: { span: Span }): Promise<RustSignature | JavaSignature>;
  /** Union same-file Spans into one covering Span (pure; no I/O). */
  spanUnion(args: { spans: Span[] }): Promise<{ span: Span }>;
};"#
            .to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::sync::Mutex;

    fn cx_in(dir: &Path) -> ToolCx {
        ToolCx {
            root: dir.to_path_buf(),
            safety: Arc::new(bro_tools::SafetyPolicy::new()),
            http: reqwest::Client::new(),
            todos: Arc::new(Mutex::new(bro_tools::TodoList::default())),
            shell_sessions: Arc::new(Mutex::new(bro_tools::ShellSessions::default())),
            edits: Arc::new(Mutex::new(bro_tools::EditSink::default())),
            session_env: Arc::new(BTreeMap::new()),
            tool_arg_defaults: Arc::new(bro_tools::ToolArgDefaults::default()),
            shell_env: Arc::new(Default::default()),
        }
    }

    fn fixture(dir: &Path) -> &'static str {
        std::fs::write(
            dir.join("probe.rs"),
            "pub struct Alpha;\n\npub fn beta() -> u8 {\n    7\n}\n",
        )
        .unwrap();
        "probe.rs"
    }

    fn java_fixture(dir: &Path) -> &'static str {
        std::fs::write(
            dir.join("Probe.java"),
            r#"package com.acme;

class Probe {
    @Inject
    private final Repo repo;
    private int count;
    static final String KIND = "probe";

    @Inject
    public Probe(
            final Repo repo,
            @Named("primary") Provider<Foo> fooProvider
    ) throws IOException {
    }

    public static <T> List<T> load(final String name, int count) throws IOException, SQLException {
        return List.of();
    }

    static class Inner {
        private String ignored;
    }
}
"#,
        )
        .unwrap();
        "Probe.java"
    }

    fn json_of(result: ToolResult) -> Value {
        match result {
            ToolResult::Json(v) => v,
            other => panic!("expected json, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn items_returns_spans_with_file_hash() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let file = fixture(&root);
        let out = json_of(CodeItems.call(json!({ "file": file }), &cx_in(&root)).await);
        assert_eq!(out["language"], "rust");
        let items = out["items"].as_array().unwrap();
        assert!(items.iter().any(|i| i["name"] == "beta"), "{items:?}");
        let span = &items[0]["span"];
        assert_eq!(span["file"], file);
        assert_eq!(span["content_sha256"], out["content_sha256"]);
    }

    #[tokio::test]
    async fn query_then_read_round_trips_span_text() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let file = fixture(&root);
        let out = json_of(
            CodeQuery
                .call(
                    json!({ "file": file, "query": "(function_item name: (identifier) @fn_name)" }),
                    &cx_in(&root),
                )
                .await,
        );
        let captures = out["captures"].as_array().unwrap();
        assert_eq!(captures.len(), 1);
        assert_eq!(captures[0]["text"], "beta");
        let span = captures[0]["span"].clone();
        let read = json_of(CodeRead.call(json!({ "span": span }), &cx_in(&root)).await);
        assert_eq!(read["text"], "beta");
        assert_eq!(read["byte_length"], 4);
        assert_eq!(read["char_length"], 4);
        assert_eq!(read["truncated"], false);
    }

    #[tokio::test]
    async fn fields_returns_java_field_declaration_facts() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let file = java_fixture(&root);
        let out = json_of(
            CodeFields
                .call(json!({ "file": file }), &cx_in(&root))
                .await,
        );
        assert_eq!(out["language"], "java");
        let fields = out["fields"].as_array().unwrap();
        let names: Vec<&str> = fields
            .iter()
            .map(|field| field["name"].as_str().unwrap())
            .collect();
        assert_eq!(names, vec!["repo", "count", "KIND", "ignored"], "{out}");
        let repo = fields.iter().find(|field| field["name"] == "repo").unwrap();
        assert_eq!(repo["type"], "Repo");
        assert_eq!(repo["owner_class"], "Probe");
        assert_eq!(repo["visibility"], "private");
        assert_eq!(repo["modifiers"], json!(["private", "final"]));
        assert_eq!(repo["annotations"], json!(["@Inject"]));
        assert_eq!(repo["is_static_final"], false);
        assert_eq!(repo["span"]["content_sha256"], out["content_sha256"]);
        assert_eq!(repo["name_span"]["content_sha256"], out["content_sha256"]);

        let filtered = json_of(
            CodeFields
                .call(json!({ "file": file, "className": "Probe" }), &cx_in(&root))
                .await,
        );
        let filtered_names: Vec<&str> = filtered["fields"]
            .as_array()
            .unwrap()
            .iter()
            .map(|field| field["name"].as_str().unwrap())
            .collect();
        assert_eq!(filtered_names, vec!["repo", "count", "KIND"]);
    }

    #[tokio::test]
    async fn signature_extracts_visibility_and_return_type() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let file = fixture(&root);
        let items = json_of(CodeItems.call(json!({ "file": file }), &cx_in(&root)).await);
        let beta_span = items["items"]
            .as_array()
            .unwrap()
            .iter()
            .find(|i| i["name"] == "beta")
            .unwrap()["span"]
            .clone();
        let sig = json_of(
            CodeSignature
                .call(json!({ "span": beta_span }), &cx_in(&root))
                .await,
        );
        assert_eq!(sig["language"], "rust");
        assert_eq!(sig["kind"], "function_item");
        assert_eq!(sig["name"], "beta");
        assert_eq!(sig["visibility"], "pub");
        assert_eq!(sig["return_type"], "u8");
        assert_eq!(sig["is_async"], false);
        assert_eq!(sig["span"]["content_sha256"], items["content_sha256"]);
        assert_eq!(
            sig["signature_span"]["content_sha256"],
            items["content_sha256"]
        );
        assert_eq!(
            sig["params_span"]["content_sha256"],
            items["content_sha256"]
        );
    }

    #[tokio::test]
    async fn signature_extracts_java_constructor_params_span() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let file = java_fixture(&root);
        let items = json_of(CodeItems.call(json!({ "file": file }), &cx_in(&root)).await);
        let ctor_span = items["items"]
            .as_array()
            .unwrap()
            .iter()
            .find(|i| i["kind"] == "constructor_declaration")
            .unwrap()["span"]
            .clone();
        let sig = json_of(
            CodeSignature
                .call(json!({ "span": ctor_span }), &cx_in(&root))
                .await,
        );
        assert_eq!(sig["language"], "java");
        assert_eq!(sig["kind"], "constructor_declaration");
        assert_eq!(sig["name"], "Probe");
        assert_eq!(sig["visibility"], "public");
        assert_eq!(sig["return_type"], Value::Null);
        assert_eq!(sig["modifiers"], json!(["public"]));
        assert_eq!(sig["annotations"], json!(["@Inject"]));
        assert_eq!(sig["throws"], json!(["IOException"]));
        assert_eq!(sig["params"].as_array().unwrap().len(), 2);
        assert_eq!(sig["params"][0]["name"], "repo");
        assert_eq!(sig["params"][0]["type"], "Repo");
        assert_eq!(sig["params"][0]["modifiers"], json!(["final"]));
        assert_eq!(
            sig["params"][1]["annotations"],
            json!([r#"@Named("primary")"#])
        );
        let params_text = json_of(
            CodeRead
                .call(json!({ "span": sig["params_span"].clone() }), &cx_in(&root))
                .await,
        );
        assert!(
            params_text["text"].as_str().unwrap().contains('\n'),
            "{params_text:?}"
        );
    }

    #[tokio::test]
    async fn signature_extracts_java_method_shape() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let file = java_fixture(&root);
        let items = json_of(CodeItems.call(json!({ "file": file }), &cx_in(&root)).await);
        let method_span = items["items"]
            .as_array()
            .unwrap()
            .iter()
            .find(|i| i["kind"] == "method_declaration" && i["name"] == "load")
            .unwrap()["span"]
            .clone();
        let sig = json_of(
            CodeSignature
                .call(json!({ "span": method_span }), &cx_in(&root))
                .await,
        );
        assert_eq!(sig["language"], "java");
        assert_eq!(sig["kind"], "method_declaration");
        assert_eq!(sig["name"], "load");
        assert_eq!(sig["visibility"], "public");
        assert_eq!(sig["modifiers"], json!(["public", "static"]));
        assert_eq!(sig["type_parameters"], "<T>");
        assert_eq!(sig["return_type"], "List<T>");
        assert_eq!(sig["throws"], json!(["IOException", "SQLException"]));
        assert_eq!(sig["params"][0]["name"], "name");
        assert_eq!(sig["params"][0]["type"], "String");
        assert_eq!(sig["params"][1]["name"], "count");
        assert_eq!(sig["params"][1]["type"], "int");
    }

    #[tokio::test]
    async fn signature_fails_closed_on_stale_span() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let file = fixture(&root);
        let items = json_of(CodeItems.call(json!({ "file": file }), &cx_in(&root)).await);
        let span = items["items"][1]["span"].clone();
        std::fs::write(root.join(file), "pub fn mutated() -> u8 { 1 }\n").unwrap();
        let result = CodeSignature
            .call(json!({ "span": span }), &cx_in(&root))
            .await;
        match result {
            ToolResult::Error(e) => assert!(e.contains("stale_span"), "got: {e}"),
            other => panic!("expected stale_span error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn read_fails_closed_on_stale_span() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let file = fixture(&root);
        let out = json_of(CodeItems.call(json!({ "file": file }), &cx_in(&root)).await);
        let span = out["items"][0]["span"].clone();
        std::fs::write(root.join(file), "pub struct Mutated;\n").unwrap();
        let result = CodeRead.call(json!({ "span": span }), &cx_in(&root)).await;
        match result {
            ToolResult::Error(e) => assert!(e.contains("stale_span"), "got: {e}"),
            other => panic!("expected stale_span error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn span_union_rejects_cross_file_and_unions_in_file() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let a = Span {
            file: "a.rs".into(),
            byte_start: 10,
            byte_end: 20,
            content_sha256: "h1".into(),
        };
        let b = Span {
            byte_start: 2,
            byte_end: 12,
            ..a.clone()
        };
        let out = json_of(
            CodeSpanUnion
                .call(json!({ "spans": [a.clone(), b] }), &cx_in(&root))
                .await,
        );
        assert_eq!(out["span"]["byte_start"], 2);
        assert_eq!(out["span"]["byte_end"], 20);
        let cross = Span {
            file: "b.rs".into(),
            ..a.clone()
        };
        let result = CodeSpanUnion
            .call(json!({ "spans": [a, cross] }), &cx_in(&root))
            .await;
        assert!(matches!(result, ToolResult::Error(_)));
    }

    #[tokio::test]
    async fn files_enumerates_and_filters_by_language_and_dir() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::create_dir_all(root.join("target/debug")).unwrap();
        std::fs::create_dir_all(root.join(".hidden")).unwrap();
        std::fs::write(root.join("src/a.rs"), "pub fn a() {}\n").unwrap();
        std::fs::write(root.join("src/B.java"), "class B {}\n").unwrap();
        std::fs::write(root.join("src/notes.txt"), "not source\n").unwrap();
        std::fs::write(root.join("target/debug/gen.rs"), "fn skipped() {}\n").unwrap();
        std::fs::write(root.join(".hidden/h.rs"), "fn hidden() {}\n").unwrap();

        let all = json_of(CodeFiles.call(json!({}), &cx_in(&root)).await);
        let files: Vec<&str> = all["files"]
            .as_array()
            .unwrap()
            .iter()
            .map(|f| f["file"].as_str().unwrap())
            .collect();
        assert_eq!(files, vec!["src/B.java", "src/a.rs"], "{all}");
        assert_eq!(all["truncated"], false);

        let rust_only = json_of(
            CodeFiles
                .call(json!({ "language": "rust", "dir": "src" }), &cx_in(&root))
                .await,
        );
        assert_eq!(rust_only["count"], 1, "{rust_only}");
        assert_eq!(rust_only["files"][0]["file"], "src/a.rs");
    }

    #[tokio::test]
    async fn items_batches_files_with_per_file_errors() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        std::fs::write(root.join("a.rs"), "pub fn a() {}\n").unwrap();
        std::fs::write(root.join("b.rs"), "pub fn b() {}\n").unwrap();

        let out = json_of(
            CodeItems
                .call(
                    json!({ "files": ["a.rs", "b.rs", "missing.rs"] }),
                    &cx_in(&root),
                )
                .await,
        );
        let files = out["files"].as_array().unwrap();
        assert_eq!(files.len(), 3, "{out}");
        assert_eq!(files[0]["items"][0]["name"], "a");
        assert_eq!(files[1]["items"][0]["name"], "b");
        assert!(files[2]["error"].as_str().is_some(), "{out}");
        // Single-file shape unchanged.
        let single = json_of(
            CodeItems
                .call(json!({ "file": "a.rs" }), &cx_in(&root))
                .await,
        );
        assert_eq!(single["file"], "a.rs");
        assert!(single["items"].is_array());
    }

    #[tokio::test]
    async fn query_batch_returns_flat_cross_file_captures() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        std::fs::write(root.join("a.rs"), "pub fn alpha() {}\n").unwrap();
        std::fs::write(
            root.join("b.rs"),
            "pub fn beta() {}\nfn alpha_caller() { alpha(); }\n",
        )
        .unwrap();

        let out = json_of(
            CodeQuery
                .call(
                    json!({ "files": ["a.rs", "b.rs"], "query": "(function_item name: (identifier) @fn)" }),
                    &cx_in(&root),
                )
                .await,
        );
        let captures = out["captures"].as_array().unwrap();
        let names: Vec<&str> = captures
            .iter()
            .map(|c| c["text"].as_str().unwrap())
            .collect();
        assert_eq!(names, vec!["alpha", "beta", "alpha_caller"], "{out}");
        // Spans name their files and carry per-file hashes.
        assert_eq!(captures[0]["span"]["file"], "a.rs");
        assert_eq!(captures[1]["span"]["file"], "b.rs");
        assert_ne!(
            captures[0]["span"]["content_sha256"],
            captures[1]["span"]["content_sha256"]
        );

        // within + files is an explicit error.
        let bad = CodeQuery
            .call(
                json!({ "files": ["a.rs"], "query": "(function_item) @f", "within": { "byte_start": 0, "byte_end": 5 } }),
                &cx_in(&root),
            )
            .await;
        assert!(
            matches!(bad, ToolResult::Error(ref e) if e.contains("single-file")),
            "{bad:?}"
        );

        // A query failing on every file fails the call once.
        let broken = CodeQuery
            .call(
                json!({ "files": ["a.rs", "b.rs"], "query": "(nonexistent_node) @x" }),
                &cx_in(&root),
            )
            .await;
        assert!(
            matches!(broken, ToolResult::Error(ref e) if e.contains("all 2 file(s) failed")),
            "{broken:?}"
        );
    }

    #[tokio::test]
    async fn query_batch_aggregate_caps_to_protect_isolate_heap() {
        // probe-dash-1: a broad query over a large file set flattened to a
        // multi-MB array and OOM'd the V8 isolate. The aggregate cap bounds
        // the payload and tells the caller how to narrow.
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        // Many files, each with several identifier captures, so the flat
        // array crosses MAX_AGGREGATE_QUERY_CAPTURES well before the file
        // set is exhausted.
        let per_file = "pub fn a(){} pub fn b(){} pub fn c(){} pub fn d(){} pub fn e(){}\n";
        let n_files = (bbox_refactor::facts::MAX_AGGREGATE_QUERY_CAPTURES / 5) + 200;
        let mut files = Vec::with_capacity(n_files);
        for i in 0..n_files {
            let name = format!("f{i}.rs");
            std::fs::write(root.join(&name), per_file).unwrap();
            files.push(name);
        }
        let out = json_of(
            CodeQuery
                .call(
                    json!({ "files": files, "query": "(function_item name: (identifier) @fn)" }),
                    &cx_in(&root),
                )
                .await,
        );
        let cap = bbox_refactor::facts::MAX_AGGREGATE_QUERY_CAPTURES;
        let n = out["captures"].as_array().unwrap().len();
        assert!(n <= cap + 5, "captures {n} exceeded aggregate cap {cap}");
        assert!(n >= cap, "expected the cap to actually bite, got {n}");
        assert_eq!(out["aggregate_capped"], true, "{}", out["hint"]);
        assert!(out["files_scanned"].as_u64().unwrap() < n_files as u64);
        assert!(out["hint"].as_str().unwrap().contains("Narrow"));
    }

    #[tokio::test]
    async fn dotdot_path_errors_because_target_is_absent() {
        // `../outside.rs` resolves to a file under the parent of the
        // session root. The test exercises the *error surface*, not a
        // confinement rejection: tree-sitter fails to parse a missing
        // file. The previous test of this shape (called
        // `paths_are_confined_to_root`) passed for the same reason but
        // misled the model into thinking containment still applied.
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let result = CodeItems
            .call(json!({ "file": "../outside.rs" }), &cx_in(&root))
            .await;
        assert!(matches!(result, ToolResult::Error(_)));
    }

    #[tokio::test]
    async fn absolute_path_outside_root_is_accepted() {
        // `code.*` accepts absolute paths. Create a real source file in a
        // sibling tempdir, point `code.items` at it via its absolute path,
        // and confirm it parses successfully.
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let outside_dir = tempfile::tempdir().unwrap();
        let outside = outside_dir.path().canonicalize().unwrap();
        let target = outside.join("external.rs");
        std::fs::write(&target, "pub fn hello() {}\n").unwrap();
        let result = CodeItems
            .call(
                json!({ "file": target.display().to_string() }),
                &cx_in(&root),
            )
            .await;
        let value = match result {
            ToolResult::Json(v) => v,
            ToolResult::Text(t) => serde_json::from_str(&t)
                .unwrap_or_else(|e| panic!("text not JSON: {e}: {t}")),
            other => panic!("expected json/text, got {other:?}"),
        };
        assert_eq!(
            value["file"].as_str(),
            Some(target.display().to_string().as_str()),
            "{value}"
        );
        let names: Vec<&str> = value["items"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|i| i["name"].as_str())
            .collect();
        assert!(names.contains(&"hello"), "names: {names:?}");
    }
}
