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
//! Paths are root-relative and confined to the session worktree root via the
//! same [`bro_tools::workspace::resolve_in_root`] guard the flat file tools
//! use. Zero daemon reach-back (decision af3c4783).

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
    /// Workspace-relative file path.
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
            "file": { "type": "string", "description": "Workspace-relative file path." },
            "byte_start": { "type": "integer", "minimum": 0 },
            "byte_end": { "type": "integer", "minimum": 0 },
            "content_sha256": { "type": "string", "description": "sha256 of the full file content the span was cut from." }
        },
        "required": ["file", "byte_start", "byte_end", "content_sha256"]
    })
}

fn err(msg: impl std::fmt::Display) -> ToolResult {
    ToolResult::Error(msg.to_string())
}

/// Resolve a workspace-relative path inside the session root, reading the
/// guard from the same confinement the flat file tools use.
fn resolve(root: &Path, file: &str) -> anyhow::Result<std::path::PathBuf> {
    bro_tools::workspace::resolve_in_root(root, file)
}

// Called from code.read's call_blocking closure (blocking pool).
#[allow(clippy::disallowed_methods)]
fn read_file_bytes(path: &Path) -> std::io::Result<Vec<u8>> {
    std::fs::read(path)
}

/// `code.items` — top-level syntax-item inventory with Spans.
pub struct CodeItems;

#[derive(Deserialize)]
struct CodeItemsParams {
    file: String,
}

#[async_trait]
impl Tool for CodeItems {
    fn name(&self) -> &str {
        "code.items"
    }
    fn description(&self) -> &str {
        "Inventory the top-level syntax items of one source file (tree-sitter; pure; syntax_only tier). Returns hash-anchored Spans for every item."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "file": { "type": "string", "description": "Workspace-relative source file path." }
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
        Some(("code".to_string(), "items".to_string()))
    }
    async fn call(&self, input: Value, cx: &ToolCx) -> ToolResult {
        let params: CodeItemsParams = match serde_json::from_value(input) {
            Ok(p) => p,
            Err(e) => return err(format!("code.items: {e}")),
        };
        let path = match resolve(&cx.root, &params.file) {
            Ok(p) => p,
            Err(e) => return err(format!("code.items: {e}")),
        };
        let file = params.file.clone();
        bro_tools::tool::call_blocking(move || match facts::file_items(&path) {
            Ok(found) => {
                let items: Vec<Value> = found
                    .items
                    .iter()
                    .map(|item| {
                        json!({
                            "name": item.name,
                            "kind": item.kind,
                            "span": Span {
                                file: file.clone(),
                                byte_start: item.byte_start,
                                byte_end: item.byte_end,
                                content_sha256: found.content_sha256.clone(),
                            },
                            "trivia_span": Span {
                                file: file.clone(),
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
                ToolResult::Json(json!({
                    "file": file,
                    "language": found.language,
                    "content_sha256": found.content_sha256,
                    "items": items,
                }))
            }
            Err(e) => err(format!("code.items: {e:#}")),
        })
        .await
    }
}

/// `code.query` — run a tree-sitter query, returning captures with Spans.
pub struct CodeQuery;

#[derive(Deserialize)]
struct CodeQueryParams {
    file: String,
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
        "Run a tree-sitter query over one source file (pure; syntax_only tier). Captures carry hash-anchored Spans. Optionally restrict to matches intersecting a `within` byte range."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "file": { "type": "string", "description": "Workspace-relative source file path." },
                "query": { "type": "string", "description": "Tree-sitter query source, e.g. \"(function_item name: (identifier) @fn_name)\"." },
                "within": {
                    "type": "object",
                    "description": "Optional byte range; only matches intersecting it are returned.",
                    "properties": {
                        "byte_start": { "type": "integer", "minimum": 0 },
                        "byte_end": { "type": "integer", "minimum": 0 }
                    },
                    "required": ["byte_start", "byte_end"]
                }
            },
            "required": ["file", "query"]
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
        let path = match resolve(&cx.root, &params.file) {
            Ok(p) => p,
            Err(e) => return err(format!("code.query: {e}")),
        };
        let file = params.file.clone();
        let query = params.query.clone();
        let within = params.within.map(|w| (w.byte_start, w.byte_end));
        bro_tools::tool::call_blocking(move || {
            match facts::file_query(&path, &query, within) {
                Ok(found) => {
                    let truncated = found.captures.len() >= facts::MAX_QUERY_CAPTURES;
                    let captures: Vec<Value> = found
                        .captures
                        .iter()
                        .map(|capture| {
                            json!({
                                "capture": capture.capture,
                                "kind": capture.kind,
                                "text": capture.text,
                                "span": Span {
                                    file: file.clone(),
                                    byte_start: capture.byte_start,
                                    byte_end: capture.byte_end,
                                    content_sha256: found.content_sha256.clone(),
                                },
                            })
                        })
                        .collect();
                    ToolResult::Json(json!({
                        "file": file,
                        "language": found.language,
                        "content_sha256": found.content_sha256,
                        "captures": captures,
                        "truncated": truncated,
                    }))
                }
                Err(e) => err(format!("code.query: {e:#}")),
            }
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
            ToolResult::Json(json!({ "text": text, "span": span }))
        })
        .await
    }
}

/// `code.signature` — extract a function signature from the AST at a Span.
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
        "Extract the signature of the function item at (or enclosing) a hash-anchored Span: name, visibility, params, return type, generics, async (pure; syntax_only tier; Rust only for now). Errors with `stale_span` on content drift."
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
            match facts::fn_signature(
                &path,
                span.byte_start,
                span.byte_end,
                Some(&span.content_sha256),
            ) {
                Ok(sig) => {
                    let params_json: Vec<Value> = sig
                        .params
                        .iter()
                        .map(|p| json!({ "pattern": p.pattern, "type": p.type_text }))
                        .collect();
                    ToolResult::Json(json!({
                        "name": sig.name,
                        "visibility": sig.visibility,
                        "is_async": sig.is_async,
                        "params": params_json,
                        "return_type": sig.return_type,
                        "generics": sig.generics,
                        "span": Span {
                            file: span.file.clone(),
                            byte_start: sig.byte_start,
                            byte_end: sig.byte_end,
                            content_sha256: sig.content_sha256,
                        },
                    }))
                }
                Err(e) => err(format!("code.signature: {e:#}")),
            }
        })
        .await
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
        Arc::new(CodeItems) as Arc<dyn Tool>,
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
        description: "Pure syntax facts over the working set (tree-sitter). Provenance tier: syntax_only. Spans are hash-anchored at read time — a Span from stale file content fails closed at consumption, so re-derive facts after any write to the file. The five methods below are the complete `code` surface. Independent calls are safe to batch with `Promise.all`. Keep intermediate facts in cell variables or `store()` — `text()` only the derived result, not raw inventories. Signature predicates (\"public fns returning Result\") compose as: `code.items` → filter `kind === \"function_item\"` → `Promise.all(fns.map(f => code.signature({ span: f.span })))` → filter on `visibility`/`return_type` — prefer that over hand-writing queries. Query authoring: use real tree-sitter node kinds (the `kind` values returned by `code.items`/`code.query` are exactly those names) — e.g. Rust public functions are `(function_item (visibility_modifier)) @pub_fn`, function names `(function_item name: (identifier) @fn_name)`; an `Invalid node type` error means the node name does not exist in that language's grammar."
            .to_string(),
        declarations: r#"type Span = { file: string; byte_start: number; byte_end: number; content_sha256: string };
type SyntaxItemFact = { name?: string; kind: string; span: Span; trivia_span: Span; line_start: number; line_end: number; attributes: string[] };
declare const code: {
  /** Inventory the top-level syntax items of one source file. */
  items(args: { file: string }): Promise<{ file: string; language: string; content_sha256: string; items: SyntaxItemFact[] }>;
  /** Run a tree-sitter query; captures carry hash-anchored Spans. */
  query(args: { file: string; query: string; within?: { byte_start: number; byte_end: number } }): Promise<{ file: string; language: string; content_sha256: string; captures: { capture: string; kind: string; text: string; span: Span }[]; truncated: boolean }>;
  /** Read the exact text of a Span; errors with stale_span on content drift. */
  read(args: { span: Span }): Promise<{ text: string; span: Span }>;
  /** Signature of the function item at/enclosing a Span (Rust only for now): name, visibility (null = private), params, return_type (null = unit), generics, is_async. Returned span covers the whole function item. Errors with stale_span on drift. */
  signature(args: { span: Span }): Promise<{ name?: string; visibility?: string; is_async: boolean; params: { pattern: string; type?: string }[]; return_type?: string; generics?: string; span: Span }>;
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
        let out = json_of(
            CodeItems
                .call(json!({ "file": file }), &cx_in(&root))
                .await,
        );
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
    }

    #[tokio::test]
    async fn signature_extracts_visibility_and_return_type() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let file = fixture(&root);
        let items = json_of(
            CodeItems
                .call(json!({ "file": file }), &cx_in(&root))
                .await,
        );
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
        assert_eq!(sig["name"], "beta");
        assert_eq!(sig["visibility"], "pub");
        assert_eq!(sig["return_type"], "u8");
        assert_eq!(sig["is_async"], false);
        assert_eq!(sig["span"]["content_sha256"], items["content_sha256"]);
    }

    #[tokio::test]
    async fn signature_fails_closed_on_stale_span() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let file = fixture(&root);
        let items = json_of(
            CodeItems
                .call(json!({ "file": file }), &cx_in(&root))
                .await,
        );
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
        let out = json_of(
            CodeItems
                .call(json!({ "file": file }), &cx_in(&root))
                .await,
        );
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
    async fn paths_are_confined_to_root() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let result = CodeItems
            .call(json!({ "file": "../outside.rs" }), &cx_in(&root))
            .await;
        assert!(matches!(result, ToolResult::Error(_)));
    }
}
