//! `edits.*` — the pure edit algebra and its `apply` choke point.
//!
//! The mutation half of the refactor cell DSL
//! (design/bro-harness/refactor-tools-v2.md §3.2–§3.3): cells accumulate
//! edits against hash-anchored [`Span`]s into a **host-backed EditSet**
//! (host-backed so the host watches composition — the provenance ledger
//! lands on this same seam later), then `edits.apply` is the ONLY binding
//! that writes.
//!
//! Trust model (v2 §3.3): **no confirm flag.** A clean EditSet applies; the
//! gate is detection, not ceremony. Detected conditions — stale spans,
//! overlapping or out-of-bounds edits, create-target collisions, parse
//! errors after the write — return `applied: false` with structured
//! findings (the bounce), rolling back any writes already made. Every
//! finding carries enough to repair without re-running discovery.
//!
//! All edits are cell-/syntax-authored today, so applied results stamp
//! `semantic_status: "syntax_only"`; lineage-computed status arrives with
//! the ledger (code-mode-cell-dsl.md §4).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use bbox_refactor::{TextEdit, apply_text_edits, sha256_hex, write_atomic, write_atomic_noclobber};
use bro_tools::{Tool, ToolAnnotations, ToolCx, ToolResult};
use serde::Deserialize;
use serde_json::{Value, json};

use super::code_facts::{Span, read_file_bytes};

/// Accumulated edits for one file, pinned to the content hash of the first
/// Span seen for that file.
#[derive(Debug, Clone, Default)]
struct FileAccum {
    expected_sha256: String,
    edits: Vec<TextEdit>,
}

/// One pending file creation.
#[derive(Debug, Clone)]
struct CreateFile {
    path: String,
    content: String,
}

/// A host-side EditSet under construction.
#[derive(Debug, Clone, Default)]
struct EditSetState {
    files: BTreeMap<String, FileAccum>,
    creates: Vec<CreateFile>,
}

/// Session-scoped store of live EditSets. One store is constructed per
/// session by [`super::binding_tools`]; sets survive across cells (the id is
/// a plain string a cell can `store()`/`load()`), and are consumed by a
/// successful `edits.apply`.
#[derive(Debug, Default)]
pub struct EditStore {
    sets: Mutex<(u64, BTreeMap<String, EditSetState>)>,
}

impl EditStore {
    fn begin(&self) -> String {
        let mut guard = self.sets.lock().expect("edit store poisoned");
        guard.0 += 1;
        let id = format!("es-{}", guard.0);
        guard.1.insert(id.clone(), EditSetState::default());
        id
    }

    fn with_set<T>(
        &self,
        id: &str,
        f: impl FnOnce(&mut EditSetState) -> Result<T, String>,
    ) -> Result<T, String> {
        let mut guard = self.sets.lock().expect("edit store poisoned");
        let set = guard.1.get_mut(id).ok_or_else(|| {
            format!("unknown EditSet `{id}` (consumed by a prior apply, or never begun)")
        })?;
        f(set)
    }

    fn snapshot(&self, id: &str) -> Result<EditSetState, String> {
        self.with_set(id, |set| Ok(set.clone()))
    }

    fn consume(&self, id: &str) {
        let mut guard = self.sets.lock().expect("edit store poisoned");
        guard.1.remove(id);
    }
}

fn err(msg: impl std::fmt::Display) -> ToolResult {
    ToolResult::Error(msg.to_string())
}

/// Record a span-addressed edit into the set, enforcing the one-hash-per-file
/// rule: every Span for a file must carry the hash the first Span pinned.
fn push_span_edit(
    store: &EditStore,
    es: &str,
    span: &Span,
    edit: TextEdit,
) -> Result<usize, String> {
    store.with_set(es, |set| {
        let accum = set.files.entry(span.file.clone()).or_default();
        if accum.expected_sha256.is_empty() {
            accum.expected_sha256 = span.content_sha256.clone();
        } else if accum.expected_sha256 != span.content_sha256 {
            return Err(format!(
                "span hash conflict for {}: EditSet `{es}` already holds edits against {}, this span carries {} — re-derive all spans for a file from the same read",
                span.file, accum.expected_sha256, span.content_sha256
            ));
        }
        accum.edits.push(edit);
        Ok(accum.edits.len())
    })
}

/// `edits.begin` — open a fresh host-backed EditSet.
pub struct EditsBegin(pub Arc<EditStore>);

#[async_trait]
impl Tool for EditsBegin {
    fn name(&self) -> &str {
        "edits.begin"
    }
    fn description(&self) -> &str {
        "Open a fresh EditSet and return its id. The EditSet lives host-side for the session (pass the id through store()/load() across cells); a successful edits.apply consumes it."
    }
    fn input_schema(&self) -> Value {
        json!({ "type": "object", "properties": {} })
    }
    fn annotations(&self) -> ToolAnnotations {
        ToolAnnotations {
            read_only: true,
            destructive: false,
        }
    }
    fn namespace_binding(&self) -> Option<(String, String)> {
        Some(("edits".to_string(), "begin".to_string()))
    }
    async fn call(&self, _input: Value, _cx: &ToolCx) -> ToolResult {
        // A bare string, deliberately: `const es = await edits.begin()` is the
        // natural cell idiom (probe-edits-1 burned ~6 turns passing a wrapper
        // object's `{es}` into later calls when this returned an object).
        ToolResult::Json(Value::String(self.0.begin()))
    }
}

/// Lenient input normalization for the algebra's two recurring cell
/// mistakes (probe-edits-1): `es` passed as the begin() result object, and
/// `span` passed as a JSON-encoded string. Canonical shapes stay documented;
/// these just absorb the obvious slips instead of bouncing the cell on a
/// field-nameless serde error.
fn normalize_algebra_input(mut input: Value) -> Value {
    if let Some(es) = input.get("es") {
        if let Some(inner) = es.get("es").and_then(Value::as_str) {
            let inner = inner.to_string();
            input["es"] = Value::String(inner);
        }
    }
    if let Some(span) = input.get("span")
        && let Some(raw) = span.as_str()
        && let Ok(parsed) = serde_json::from_str::<Value>(raw)
        && parsed.is_object()
    {
        input["span"] = parsed;
    }
    input
}

/// Decode tool params with an error that names the expected shape — serde's
/// bare "invalid type: map, expected a string" cost probe-edits-1 several
/// diagnostic cells because it names neither field nor fix.
fn decode<T: serde::de::DeserializeOwned>(
    tool: &str,
    shape: &str,
    input: Value,
) -> Result<T, ToolResult> {
    serde_json::from_value(normalize_algebra_input(input))
        .map_err(|e| err(format!("{tool}: bad input — expected {shape}; {e}")))
}

#[derive(Deserialize)]
struct SpanEditParams {
    es: String,
    span: Span,
    #[serde(default)]
    text: Option<String>,
}

macro_rules! span_edit_tool {
    ($tool:ident, $name:literal, $method:literal, $desc:literal, $needs_text:literal, $to_edit:expr) => {
        pub struct $tool(pub Arc<EditStore>);

        #[async_trait]
        impl Tool for $tool {
            fn name(&self) -> &str {
                $name
            }
            fn description(&self) -> &str {
                $desc
            }
            fn input_schema(&self) -> Value {
                let mut properties = json!({
                    "es": { "type": "string", "description": "EditSet id from edits.begin." },
                    "span": super::code_facts::span_schema_pub()
                });
                if $needs_text {
                    properties["text"] =
                        json!({ "type": "string", "description": "Replacement / inserted text." });
                }
                let required: Vec<&str> = if $needs_text {
                    vec!["es", "span", "text"]
                } else {
                    vec!["es", "span"]
                };
                json!({ "type": "object", "properties": properties, "required": required })
            }
            fn namespace_binding(&self) -> Option<(String, String)> {
                Some(("edits".to_string(), $method.to_string()))
            }
            async fn call(&self, input: Value, _cx: &ToolCx) -> ToolResult {
                let params: SpanEditParams = match decode($name, "{ es: string, span: Span, text?: string }", input) {
                    Ok(p) => p,
                    Err(e) => return e,
                };
                if $needs_text && params.text.is_none() {
                    return err(format!("{}: `text` is required", $name));
                }
                #[allow(clippy::redundant_closure_call)]
                let edit: TextEdit = ($to_edit)(&params.span, params.text.unwrap_or_default());
                match push_span_edit(&self.0, &params.es, &params.span, edit) {
                    Ok(count) => ToolResult::Json(json!({
                        "es": params.es,
                        "file": params.span.file,
                        "edit_count": count,
                    })),
                    Err(e) => err(format!("{}: {e}", $name)),
                }
            }
        }
    };
}

span_edit_tool!(
    EditsReplace,
    "edits.replace",
    "replace",
    "Queue replacement of a Span's exact bytes with new text (pure; builds, never writes).",
    true,
    |span: &Span, text: String| TextEdit {
        byte_start: span.byte_start,
        byte_end: span.byte_end,
        replacement: text,
    }
);

span_edit_tool!(
    EditsInsertAfter,
    "edits.insertAfter",
    "insertAfter",
    "Queue insertion of text immediately after a Span's end (pure; builds, never writes).",
    true,
    |span: &Span, text: String| TextEdit {
        byte_start: span.byte_end,
        byte_end: span.byte_end,
        replacement: text,
    }
);

span_edit_tool!(
    EditsInsertBefore,
    "edits.insertBefore",
    "insertBefore",
    "Queue insertion of text immediately before a Span's start (pure; builds, never writes). Use an item's span to place text directly above it — no byte arithmetic.",
    true,
    |span: &Span, text: String| TextEdit {
        byte_start: span.byte_start,
        byte_end: span.byte_start,
        replacement: text,
    }
);

span_edit_tool!(
    EditsDelete,
    "edits.delete",
    "delete",
    "Queue deletion of a Span's exact bytes (pure; builds, never writes).",
    false,
    |span: &Span, _text: String| TextEdit {
        byte_start: span.byte_start,
        byte_end: span.byte_end,
        replacement: String::new(),
    }
);

/// `edits.createFile` — queue creation of a new file.
pub struct EditsCreateFile(pub Arc<EditStore>);

#[derive(Deserialize)]
struct CreateFileParams {
    es: String,
    path: String,
    content: String,
}

#[async_trait]
impl Tool for EditsCreateFile {
    fn name(&self) -> &str {
        "edits.createFile"
    }
    fn description(&self) -> &str {
        "Queue creation of a new file with the given content (pure; builds, never writes). Apply bounces with `create_exists` if the path exists at apply time."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "es": { "type": "string", "description": "EditSet id from edits.begin." },
                "path": { "type": "string", "description": "Workspace-relative path that must not yet exist." },
                "content": { "type": "string" }
            },
            "required": ["es", "path", "content"]
        })
    }
    fn namespace_binding(&self) -> Option<(String, String)> {
        Some(("edits".to_string(), "createFile".to_string()))
    }
    async fn call(&self, input: Value, _cx: &ToolCx) -> ToolResult {
        let params: CreateFileParams = match decode("edits.createFile", "{ es: string, path: string, content: string }", input) {
            Ok(p) => p,
            Err(e) => return e,
        };
        let result = self.0.with_set(&params.es, |set| {
            if set.creates.iter().any(|c| c.path == params.path)
                || set.files.contains_key(&params.path)
            {
                return Err(format!(
                    "EditSet `{}` already touches {}",
                    params.es, params.path
                ));
            }
            set.creates.push(CreateFile {
                path: params.path.clone(),
                content: params.content.clone(),
            });
            Ok(set.creates.len())
        });
        match result {
            Ok(creates) => ToolResult::Json(json!({ "es": params.es, "creates": creates })),
            Err(e) => err(format!("edits.createFile: {e}")),
        }
    }
}

/// `edits.merge` — fold span-shaped changes (e.g. lsp.rename output) into
/// an EditSet: server-authored and cell-authored edits compose into ONE
/// artifact (refactor-tools-v2 §3.2).
pub struct EditsMerge(pub Arc<EditStore>);

#[derive(Deserialize)]
struct MergeParams {
    es: String,
    changes: Vec<MergeChange>,
}

#[derive(Deserialize)]
struct MergeChange {
    span: Span,
    new_text: String,
}

#[async_trait]
impl Tool for EditsMerge {
    fn name(&self) -> &str {
        "edits.merge"
    }
    fn description(&self) -> &str {
        "Fold span-shaped changes — e.g. the `changes` array from lsp.rename — into an EditSet (pure; builds, never writes). Each change is {span, new_text}; spans stay hash-pinned per file."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "es": { "type": "string", "description": "EditSet id from edits.begin." },
                "changes": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "properties": {
                            "span": super::code_facts::span_schema_pub(),
                            "new_text": { "type": "string" }
                        },
                        "required": ["span", "new_text"]
                    }
                }
            },
            "required": ["es", "changes"]
        })
    }
    fn namespace_binding(&self) -> Option<(String, String)> {
        Some(("edits".to_string(), "merge".to_string()))
    }
    async fn call(&self, input: Value, _cx: &ToolCx) -> ToolResult {
        let params: MergeParams = match decode(
            "edits.merge",
            "{ es: string, changes: { span: Span, new_text: string }[] }",
            input,
        ) {
            Ok(p) => p,
            Err(e) => return e,
        };
        if params.changes.is_empty() {
            return err("edits.merge: `changes` must be non-empty");
        }
        let mut total = 0usize;
        for change in &params.changes {
            let edit = TextEdit {
                byte_start: change.span.byte_start,
                byte_end: change.span.byte_end,
                replacement: change.new_text.clone(),
            };
            match push_span_edit(&self.0, &params.es, &change.span, edit) {
                Ok(_) => total += 1,
                Err(e) => return err(format!("edits.merge: {e}")),
            }
        }
        ToolResult::Json(json!({ "es": params.es, "merged": total }))
    }
}

/// One structured bounce finding — span + classification + repair hint, so a
/// repair cell acts without re-running discovery (v2 §4).
fn finding(kind: &str, file: &str, detail: String, hint: &str) -> Value {
    json!({ "kind": kind, "file": file, "detail": detail, "resolution_hint": hint })
}

/// `edits.apply` — the choke point: the only binding that writes.
pub struct EditsApply(pub Arc<EditStore>);

#[derive(Deserialize)]
struct ApplyParams {
    es: String,
    #[serde(default)]
    validations: Option<Vec<String>>,
}

#[async_trait]
impl Tool for EditsApply {
    fn name(&self) -> &str {
        "edits.apply"
    }
    fn description(&self) -> &str {
        "Apply an EditSet to the worktree — the ONLY mutating binding. No confirm flag: a clean set applies; detected conditions (stale_span, overlapping/invalid edits, create_exists, post-write parse errors) bounce with `applied: false` and structured findings, rolling back any writes. Validations default to [\"tree_sitter_no_errors\"]; run compiler checks yourself after a successful apply. A successful apply consumes the EditSet."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "es": { "type": "string", "description": "EditSet id from edits.begin." },
                "validations": {
                    "type": "array",
                    "items": { "type": "string", "enum": ["tree_sitter_no_errors"] },
                    "description": "Post-write validations (default: [\"tree_sitter_no_errors\"])."
                }
            },
            "required": ["es"]
        })
    }
    fn annotations(&self) -> ToolAnnotations {
        ToolAnnotations {
            read_only: false,
            destructive: true,
        }
    }
    fn namespace_binding(&self) -> Option<(String, String)> {
        Some(("edits".to_string(), "apply".to_string()))
    }
    async fn call(&self, input: Value, cx: &ToolCx) -> ToolResult {
        let params: ApplyParams = match decode("edits.apply", "{ es: string, validations?: string[] }", input) {
            Ok(p) => p,
            Err(e) => return e,
        };
        let validations = params
            .validations
            .unwrap_or_else(|| vec!["tree_sitter_no_errors".to_string()]);
        for v in &validations {
            if v != "tree_sitter_no_errors" {
                return err(format!(
                    "edits.apply: unknown validation `{v}` (supported: tree_sitter_no_errors)"
                ));
            }
        }
        let set = match self.0.snapshot(&params.es) {
            Ok(s) => s,
            Err(e) => return err(format!("edits.apply: {e}")),
        };
        if set.files.is_empty() && set.creates.is_empty() {
            return err(format!("edits.apply: EditSet `{}` is empty", params.es));
        }

        // Resolve every touched path inside the root before any work.
        let mut resolved_edits: Vec<(String, PathBuf, FileAccum)> = Vec::new();
        for (file, accum) in &set.files {
            match bro_tools::workspace::resolve_in_root(&cx.root, file) {
                Ok(path) => resolved_edits.push((file.clone(), path, accum.clone())),
                Err(e) => return err(format!("edits.apply: {file}: {e}")),
            }
        }
        let mut resolved_creates: Vec<(String, PathBuf, String)> = Vec::new();
        for create in &set.creates {
            match bro_tools::workspace::resolve_in_root(&cx.root, &create.path) {
                Ok(path) => resolved_creates.push((create.path.clone(), path, create.content.clone())),
                Err(e) => return err(format!("edits.apply: {}: {e}", create.path)),
            }
        }

        let store = Arc::clone(&self.0);
        let es_id = params.es.clone();
        let run_validation = validations.contains(&"tree_sitter_no_errors".to_string());
        let edit_sink = Arc::clone(&cx.edits);

        bro_tools::tool::call_blocking(move || {
            // ---- Detection pass (no writes) ----
            let mut findings: Vec<Value> = Vec::new();
            let mut planned: Vec<(String, PathBuf, Vec<u8>, String)> = Vec::new(); // (file, path, pre_image, new_text)
            for (file, path, accum) in &resolved_edits {
                let bytes = match read_file_bytes(path) {
                    Ok(b) => b,
                    Err(e) => {
                        findings.push(finding(
                            "stale_span",
                            file,
                            format!("file unreadable: {e}"),
                            "re-derive spans from fresh facts (code.items / code.query)",
                        ));
                        continue;
                    }
                };
                let current = sha256_hex(&bytes);
                if current != accum.expected_sha256 {
                    findings.push(finding(
                        "stale_span",
                        file,
                        format!(
                            "content changed since spans were minted (edits hash {}, current {current})",
                            accum.expected_sha256
                        ),
                        "re-derive spans from fresh facts and rebuild the edits for this file",
                    ));
                    continue;
                }
                let source = String::from_utf8_lossy(&bytes).to_string();
                match apply_text_edits(&source, &accum.edits) {
                    Ok(new_text) => planned.push((file.clone(), path.clone(), bytes, new_text)),
                    Err(e) => findings.push(finding(
                        "invalid_edits",
                        file,
                        format!("{e:#}"),
                        "overlapping or out-of-bounds edits — rebuild this file's edits from disjoint spans",
                    )),
                }
            }
            for (file, path, _content) in &resolved_creates {
                if path.exists() {
                    findings.push(finding(
                        "create_exists",
                        file,
                        "create target already exists".to_string(),
                        "pick a different path, or express the change as edits to the existing file",
                    ));
                }
            }
            if !findings.is_empty() {
                // EditSet retained so a repair cell can inspect/rebuild.
                return ToolResult::Json(json!({
                    "applied": false,
                    "es": es_id,
                    "findings": findings,
                    "semantic_status": "syntax_only",
                }));
            }

            // ---- Write pass (snapshot first, rollback on any failure) ----
            let mut written: Vec<(String, PathBuf, Vec<u8>, usize)> = Vec::new(); // pre_image kept for rollback
            let mut created: Vec<(String, PathBuf)> = Vec::new();
            let rollback = |written: &[(String, PathBuf, Vec<u8>, usize)],
                            created: &[(String, PathBuf)]|
             -> Vec<String> {
                let mut errors = Vec::new();
                for (file, path, pre_image, _) in written {
                    if let Err(e) = write_atomic(path, pre_image) {
                        errors.push(format!("{file}: restore failed: {e:#}"));
                    }
                }
                for (file, path) in created {
                    if let Err(e) = remove_created(path) {
                        errors.push(format!("{file}: created-file removal failed: {e:#}"));
                    }
                }
                errors
            };

            for (file, path, pre_image, new_text) in &planned {
                if let Err(e) = write_atomic(path, new_text.as_bytes()) {
                    let rollback_errors = rollback(&written, &created);
                    return ToolResult::Json(json!({
                        "applied": false,
                        "es": es_id,
                        "findings": [finding("write_failed", file, format!("{e:#}"), "filesystem error — inspect and retry")],
                        "rolled_back": true,
                        "rollback_errors": rollback_errors,
                        "semantic_status": "syntax_only",
                    }));
                }
                written.push((file.clone(), path.clone(), pre_image.clone(), new_text.len()));
            }
            for (file, path, content) in &resolved_creates {
                if let Err(e) = write_atomic_noclobber(path, content.as_bytes()) {
                    let rollback_errors = rollback(&written, &created);
                    return ToolResult::Json(json!({
                        "applied": false,
                        "es": es_id,
                        "findings": [finding("create_exists", file, format!("{e:#}"), "the path appeared between detection and write — pick a different path")],
                        "rolled_back": true,
                        "rollback_errors": rollback_errors,
                        "semantic_status": "syntax_only",
                    }));
                }
                created.push((file.clone(), path.clone()));
            }

            // ---- Post-write validation ----
            if run_validation {
                let mut parse_findings = Vec::new();
                for (file, path) in written
                    .iter()
                    .map(|(f, p, _, _)| (f, p))
                    .chain(created.iter().map(|(f, p)| (f, p)))
                {
                    match bbox_refactor::facts::parse_check(path) {
                        Ok(check) if check.error_nodes > 0 || check.missing_nodes > 0 => {
                            parse_findings.push(finding(
                                "parse_error_after_apply",
                                file,
                                format!(
                                    "{} error node(s), {} missing node(s) after applying edits",
                                    check.error_nodes, check.missing_nodes
                                ),
                                "the edits produced syntactically broken code; rebuild with corrected spans/text",
                            ));
                        }
                        _ => {} // healthy, or unsupported extension (skip)
                    }
                }
                if !parse_findings.is_empty() {
                    let rollback_errors = rollback(&written, &created);
                    return ToolResult::Json(json!({
                        "applied": false,
                        "es": es_id,
                        "findings": parse_findings,
                        "rolled_back": true,
                        "rollback_errors": rollback_errors,
                        "semantic_status": "syntax_only",
                    }));
                }
            }

            // ---- Success: edit-sink events, consume, summarize ----
            // Post-state hashes computed once, used for both the edit sink
            // and the summary: the Applied result carries the NEW generation's
            // content_sha256 per file, so follow-up cells can mint fresh Spans
            // without re-calling code.items (probe-edits-2 retro ask).
            let written_posts: Vec<(String, PathBuf, Vec<u8>, usize, String)> = written
                .iter()
                .map(|(file, path, pre_image, new_len)| {
                    let post = read_file_bytes(path).unwrap_or_default();
                    let post_sha = sha256_hex(&post);
                    (file.clone(), path.clone(), pre_image.clone(), *new_len, post_sha)
                })
                .collect();
            let created_posts: Vec<(String, PathBuf, String)> = created
                .iter()
                .map(|(file, path)| {
                    let post = read_file_bytes(path).unwrap_or_default();
                    (file.clone(), path.clone(), sha256_hex(&post))
                })
                .collect();
            if let Ok(mut sink) = edit_sink.lock() {
                for (_file, path, pre_image, _, post_sha) in &written_posts {
                    sink.push(bro_tools::edits::EditEvent {
                        path: path.clone(),
                        pre_image: pre_image.clone(),
                        pre_sha256: sha256_hex(pre_image),
                        post_sha256: post_sha.clone(),
                    });
                }
                for (_file, path, post_sha) in &created_posts {
                    sink.push(bro_tools::edits::EditEvent {
                        path: path.clone(),
                        pre_image: Vec::new(),
                        pre_sha256: sha256_hex(&[]),
                        post_sha256: post_sha.clone(),
                    });
                }
            }
            store.consume(&es_id);

            let summary: Vec<Value> = written_posts
                .iter()
                .map(|(file, _path, pre_image, new_len, post_sha)| {
                    json!({
                        "file": file,
                        "edits": set.files.get(file).map(|a| a.edits.len()).unwrap_or(0),
                        "bytes_before": pre_image.len(),
                        "bytes_after": new_len,
                        "content_sha256": post_sha,
                    })
                })
                .chain(created_posts.iter().map(|(file, _, post_sha)| {
                    json!({ "file": file, "created": true, "content_sha256": post_sha })
                }))
                .collect();
            ToolResult::Json(json!({
                "applied": true,
                "es": es_id,
                "summary": summary,
                "semantic_status": "syntax_only",
                "validations": if run_validation {
                    json!([{ "validation": "tree_sitter_no_errors", "status": "passed", "files_checked": written.len() + created.len() }])
                } else {
                    json!([])
                },
            }))
        })
        .await
    }
}

// Called from edits.apply's call_blocking closure (blocking pool).
#[allow(clippy::disallowed_methods)]
fn remove_created(path: &Path) -> std::io::Result<()> {
    std::fs::remove_file(path)
}

/// The `edits.*` binding set, sharing one session-scoped [`EditStore`].
pub fn tools(store: Arc<EditStore>) -> Vec<Arc<dyn Tool>> {
    vec![
        Arc::new(EditsBegin(Arc::clone(&store))) as Arc<dyn Tool>,
        Arc::new(EditsReplace(Arc::clone(&store))) as Arc<dyn Tool>,
        Arc::new(EditsInsertAfter(Arc::clone(&store))) as Arc<dyn Tool>,
        Arc::new(EditsInsertBefore(Arc::clone(&store))) as Arc<dyn Tool>,
        Arc::new(EditsDelete(Arc::clone(&store))) as Arc<dyn Tool>,
        Arc::new(EditsCreateFile(Arc::clone(&store))) as Arc<dyn Tool>,
        Arc::new(EditsMerge(Arc::clone(&store))) as Arc<dyn Tool>,
        Arc::new(EditsApply(store)) as Arc<dyn Tool>,
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::collections::BTreeMap as StdBTreeMap;
    use std::sync::Mutex as StdMutex;

    fn cx_in(dir: &Path) -> ToolCx {
        ToolCx {
            root: dir.to_path_buf(),
            safety: Arc::new(bro_tools::SafetyPolicy::new()),
            http: reqwest::Client::new(),
            todos: Arc::new(StdMutex::new(bro_tools::TodoList::default())),
            shell_sessions: Arc::new(StdMutex::new(bro_tools::ShellSessions::default())),
            edits: Arc::new(StdMutex::new(bro_tools::EditSink::default())),
            session_env: Arc::new(StdBTreeMap::new()),
            tool_arg_defaults: Arc::new(bro_tools::ToolArgDefaults::default()),
            shell_env: Arc::new(Default::default()),
        }
    }

    fn json_of(result: ToolResult) -> Value {
        match result {
            ToolResult::Json(v) => v,
            other => panic!("expected json, got {other:?}"),
        }
    }

    const FIXTURE: &str = "pub struct Alpha;\n\npub fn beta() -> u8 {\n    7\n}\n";

    async fn beta_span(root: &Path, cx: &ToolCx) -> (Value, Value) {
        let items = json_of(
            super::super::code_facts::CodeItems
                .call(json!({ "file": "probe.rs" }), cx)
                .await,
        );
        let _ = root;
        let beta = items["items"]
            .as_array()
            .unwrap()
            .iter()
            .find(|i| i["name"] == "beta")
            .unwrap()
            .clone();
        (items, beta)
    }

    fn set_up(dir: &Path) -> (Arc<EditStore>, ToolCx) {
        std::fs::write(dir.join("probe.rs"), FIXTURE).unwrap();
        (Arc::new(EditStore::default()), cx_in(dir))
    }

    #[tokio::test]
    async fn replace_then_apply_mutates_and_consumes() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let (store, cx) = set_up(&root);
        let (_, beta) = beta_span(&root, &cx).await;

        let es = json_of(EditsBegin(store.clone()).call(json!({}), &cx).await)
            .as_str()
            .unwrap()
            .to_string();
        json_of(
            EditsReplace(store.clone())
                .call(
                    json!({ "es": es, "span": beta["span"], "text": "pub fn beta() -> u8 {\n    8\n}" }),
                    &cx,
                )
                .await,
        );
        let result = json_of(
            EditsApply(store.clone())
                .call(json!({ "es": es }), &cx)
                .await,
        );
        assert_eq!(result["applied"], true, "{result}");
        assert_eq!(result["semantic_status"], "syntax_only");
        let on_disk = std::fs::read_to_string(root.join("probe.rs")).unwrap();
        assert!(on_disk.contains("    8\n"), "{on_disk}");
        // Edit-sink event recorded.
        assert_eq!(cx.edits.lock().unwrap().events().len(), 1);
        // Consumed: a second apply must fail with unknown EditSet.
        let again = EditsApply(store).call(json!({ "es": es }), &cx).await;
        assert!(matches!(again, ToolResult::Error(e) if e.contains("unknown EditSet")));
    }

    #[tokio::test]
    async fn insert_before_places_text_above_item_and_reports_new_hash() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let (store, cx) = set_up(&root);
        let (_, beta) = beta_span(&root, &cx).await;

        let es = json_of(EditsBegin(store.clone()).call(json!({}), &cx).await)
            .as_str()
            .unwrap()
            .to_string();
        json_of(
            EditsInsertBefore(store.clone())
                .call(
                    json!({ "es": es, "span": beta["span"], "text": "/// The answer precursor.\n" }),
                    &cx,
                )
                .await,
        );
        let result = json_of(EditsApply(store).call(json!({ "es": es }), &cx).await);
        assert_eq!(result["applied"], true, "{result}");
        let on_disk = std::fs::read(root.join("probe.rs")).unwrap();
        assert!(
            String::from_utf8_lossy(&on_disk).contains("/// The answer precursor.\npub fn beta"),
            "{}",
            String::from_utf8_lossy(&on_disk)
        );
        // Applied summary carries the NEW generation hash.
        assert_eq!(
            result["summary"][0]["content_sha256"],
            bbox_refactor::sha256_hex(&on_disk),
            "{result}"
        );
    }

    #[tokio::test]
    async fn lenient_inputs_absorb_wrapper_es_and_stringified_span() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let (store, cx) = set_up(&root);
        let (_, beta) = beta_span(&root, &cx).await;

        let es = json_of(EditsBegin(store.clone()).call(json!({}), &cx).await)
            .as_str()
            .unwrap()
            .to_string();
        // The two probe-edits-1 slips: es as the begin() wrapper object shape,
        // span as a JSON-encoded string.
        let span_str = serde_json::to_string(&beta["span"]).unwrap();
        let result = EditsDelete(store)
            .call(
                json!({ "es": { "es": es }, "span": span_str }),
                &cx,
            )
            .await;
        let out = json_of(result);
        assert_eq!(out["edit_count"], 1, "{out}");
    }

    #[tokio::test]
    async fn stale_span_bounces_without_writing() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let (store, cx) = set_up(&root);
        let (_, beta) = beta_span(&root, &cx).await;

        let es = json_of(EditsBegin(store.clone()).call(json!({}), &cx).await)
            .as_str()
            .unwrap()
            .to_string();
        json_of(
            EditsReplace(store.clone())
                .call(json!({ "es": es, "span": beta["span"], "text": "fn x() {}" }), &cx)
                .await,
        );
        std::fs::write(root.join("probe.rs"), "pub fn drifted() {}\n").unwrap();
        let result = json_of(
            EditsApply(store.clone())
                .call(json!({ "es": es }), &cx)
                .await,
        );
        assert_eq!(result["applied"], false, "{result}");
        assert_eq!(result["findings"][0]["kind"], "stale_span");
        assert!(
            std::fs::read_to_string(root.join("probe.rs"))
                .unwrap()
                .contains("drifted")
        );
        // Bounced set is retained for repair.
        assert!(store.snapshot(&es).is_ok());
    }

    #[tokio::test]
    async fn parse_breakage_rolls_back_and_bounces() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let (store, cx) = set_up(&root);
        let (_, beta) = beta_span(&root, &cx).await;

        let es = json_of(EditsBegin(store.clone()).call(json!({}), &cx).await)
            .as_str()
            .unwrap()
            .to_string();
        json_of(
            EditsReplace(store.clone())
                .call(
                    json!({ "es": es, "span": beta["span"], "text": "pub fn broken( {" }),
                    &cx,
                )
                .await,
        );
        let result = json_of(EditsApply(store).call(json!({ "es": es }), &cx).await);
        assert_eq!(result["applied"], false, "{result}");
        assert_eq!(result["findings"][0]["kind"], "parse_error_after_apply");
        assert_eq!(result["rolled_back"], true);
        assert_eq!(
            std::fs::read_to_string(root.join("probe.rs")).unwrap(),
            FIXTURE,
            "rollback must restore the original bytes"
        );
    }

    #[tokio::test]
    async fn create_file_applies_and_collision_bounces() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let (store, cx) = set_up(&root);

        let es = json_of(EditsBegin(store.clone()).call(json!({}), &cx).await)
            .as_str()
            .unwrap()
            .to_string();
        json_of(
            EditsCreateFile(store.clone())
                .call(json!({ "es": es, "path": "newmod.rs", "content": "pub fn fresh() {}\n" }), &cx)
                .await,
        );
        let result = json_of(EditsApply(store.clone()).call(json!({ "es": es }), &cx).await);
        assert_eq!(result["applied"], true, "{result}");
        assert!(root.join("newmod.rs").exists());

        let es2 = json_of(EditsBegin(store.clone()).call(json!({}), &cx).await)
            .as_str()
            .unwrap()
            .to_string();
        json_of(
            EditsCreateFile(store.clone())
                .call(json!({ "es": es2, "path": "newmod.rs", "content": "x" }), &cx)
                .await,
        );
        let result = json_of(EditsApply(store).call(json!({ "es": es2 }), &cx).await);
        assert_eq!(result["applied"], false, "{result}");
        assert_eq!(result["findings"][0]["kind"], "create_exists");
    }

    #[tokio::test]
    async fn span_hash_conflict_fails_at_algebra_time() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let (store, cx) = set_up(&root);
        let (_, beta) = beta_span(&root, &cx).await;

        let es = json_of(EditsBegin(store.clone()).call(json!({}), &cx).await)
            .as_str()
            .unwrap()
            .to_string();
        json_of(
            EditsDelete(store.clone())
                .call(json!({ "es": es, "span": beta["span"] }), &cx)
                .await,
        );
        let mut forged = beta["span"].clone();
        forged["content_sha256"] = json!("0000000000000000000000000000000000000000000000000000000000000000");
        let result = EditsDelete(store)
            .call(json!({ "es": es, "span": forged }), &cx)
            .await;
        assert!(
            matches!(result, ToolResult::Error(ref e) if e.contains("span hash conflict")),
            "got: {result:?}"
        );
    }
}

/// Hand-authored namespace documentation + TS declarations (cell-dsl §5.2).
pub fn namespace_description() -> bro_code_mode::ToolNamespaceDescription {
    bro_code_mode::ToolNamespaceDescription {
        name: "edits".to_string(),
        description: "The edit algebra and its apply choke point — the ONLY mutation path for source edits; prefer it over file_write/shell for span-shaped changes. Build: begin() → replace/insertBefore/insertAfter/delete (consume hash-anchored Spans from code.*) / createFile / merge (fold lsp.rename changes in) → apply(). NO confirm flag: a clean EditSet applies; detected conditions bounce with `applied: false` and findings [{kind, file, detail, resolution_hint}] (kinds: stale_span, invalid_edits, create_exists, parse_error_after_apply, write_failed) — repair by re-deriving fresh facts, rebuilding the edits, and applying again. Writes are atomic with snapshot/rollback; tree_sitter_no_errors validation runs by default and rolls back broken syntax. All spans for one file must carry the SAME content_sha256 (one read generation). After a successful apply every Span minted before it is stale — re-derive facts before further edits. Run `cargo check`/tests via shell AFTER a successful apply; apply itself only guarantees parseability. semantic_status is syntax_only for cell-authored edits."
            .to_string(),
        declarations: r#"type Finding = { kind: "stale_span" | "invalid_edits" | "create_exists" | "parse_error_after_apply" | "write_failed"; file: string; detail: string; resolution_hint: string };
type Applied = { applied: true; es: string; summary: { file: string; edits?: number; bytes_before?: number; bytes_after?: number; created?: boolean; content_sha256: string }[]; semantic_status: "syntax_only"; validations: { validation: string; status: string; files_checked: number }[] };  // content_sha256 is the NEW generation — mint fresh Spans from it without re-calling code.items
type Bounced = { applied: false; es: string; findings: Finding[]; rolled_back?: boolean; semantic_status: "syntax_only" };
declare const edits: {
  /** Open a fresh EditSet (host-side, session-scoped; survives across cells by id). */
  begin(args?: {}): Promise<string>;  // returns the EditSet id directly
  /** Queue replacement of a Span's exact bytes (pure; never writes). */
  replace(args: { es: string; span: Span; text: string }): Promise<{ es: string; file: string; edit_count: number }>;
  /** Queue insertion immediately after a Span's end (pure; never writes). */
  insertAfter(args: { es: string; span: Span; text: string }): Promise<{ es: string; file: string; edit_count: number }>;
  /** Queue insertion immediately before a Span's start — place text directly above an item without byte arithmetic (pure; never writes). */
  insertBefore(args: { es: string; span: Span; text: string }): Promise<{ es: string; file: string; edit_count: number }>;
  /** Queue deletion of a Span's exact bytes (pure; never writes). */
  delete(args: { es: string; span: Span }): Promise<{ es: string; file: string; edit_count: number }>;
  /** Queue creation of a new file (pure; never writes; bounces if it exists at apply). */
  createFile(args: { es: string; path: string; content: string }): Promise<{ es: string; creates: number }>;
  /** Fold span-shaped changes (e.g. lsp.rename().changes) into the set — server-authored edits join the same artifact (pure; never writes). */
  merge(args: { es: string; changes: { span: Span; new_text: string }[] }): Promise<{ es: string; merged: number }>;
  /** THE choke point: apply the EditSet. Clean → Applied (EditSet consumed). Detected condition → Bounced with findings (EditSet retained for repair; writes rolled back). */
  apply(args: { es: string; validations?: "tree_sitter_no_errors"[] }): Promise<Applied | Bounced>;
};"#
            .to_string(),
    }
}
