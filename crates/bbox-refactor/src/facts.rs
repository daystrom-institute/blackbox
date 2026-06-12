//! Read-only "facts" surface for harness-native bindings.
//!
//! The code-mode cell DSL (design/bro-harness/refactor-tools-v2.md §3.1)
//! projects fact bindings like `code.items` / `code.query` into the V8 cell.
//! Those bindings live in `bro-harness` and call into this module — the same
//! tree-sitter machinery the v1 plan kinds use, exposed as plain functions
//! returning data instead of MCP-shaped JSON strings. Pure functions of the
//! file bytes: no LSP, no daemon state, no writes (the harness-native
//! invariant — decision af3c4783 — depends on this module staying that way).

use std::path::Path;

use anyhow::{Result, anyhow};

use crate::chunker;
use crate::{SyntaxItem, sha256_hex};

/// One inventoried item plus the facts the inventory walk doesn't carry.
///
/// `visibility` exists because its absence actively misleads: probe
/// `probe-code-facts-2` read an empty `attributes` array on a `pub fn` as
/// "this surface doesn't understand visibility" and abandoned the namespace.
#[derive(Debug, Clone)]
pub struct ItemFact {
    pub item: SyntaxItem,
    /// Visibility modifier text (`pub`, `pub(crate)`, `public`, …);
    /// `None` = private/default visibility (or not derivable for the
    /// language).
    pub visibility: Option<String>,
}

/// Top-level syntax-item inventory of one source file — the same per-language
/// item walk `bbox_refactor_status` uses, with the source hash captured at
/// read time so callers can mint drift-guarded spans.
#[derive(Debug, Clone)]
pub struct FileItemsFacts {
    pub language: &'static str,
    pub content_sha256: String,
    pub source_len: usize,
    pub items: Vec<ItemFact>,
}

/// Inventory the top-level syntax items of `path`.
pub fn file_items(path: &Path) -> Result<FileItemsFacts> {
    let parsed = super::parse_source_file(path)?;
    let items = match parsed.language {
        "rust" => super::rust_status_items(&parsed),
        "java" => super::java_status_items(&parsed),
        _ => super::generic_top_level_items(&parsed),
    };
    let root = parsed.tree.root_node();
    let items = items
        .into_iter()
        .map(|item| {
            let visibility = root
                .named_descendant_for_byte_range(item.byte_start, item.byte_end)
                .and_then(|node| item_visibility(node, parsed.language, &parsed.source));
            ItemFact { item, visibility }
        })
        .collect();
    Ok(FileItemsFacts {
        language: parsed.language,
        content_sha256: sha256_hex(parsed.source.as_bytes()),
        source_len: parsed.source.len(),
        items,
    })
}

/// Visibility modifier text of an item node, per language. Rust reads the
/// `visibility_modifier` child; Java reads `public`/`protected`/`private`
/// out of the `modifiers` child. Other languages return `None`.
fn item_visibility(
    node: tree_sitter::Node<'_>,
    language: &str,
    source: &str,
) -> Option<String> {
    let text_of = |n: tree_sitter::Node<'_>| source.get(n.start_byte()..n.end_byte());
    let mut cursor = node.walk();
    match language {
        "rust" => node
            .named_children(&mut cursor)
            .find(|child| child.kind() == "visibility_modifier")
            .and_then(text_of)
            .map(str::to_string),
        "java" => node
            .named_children(&mut cursor)
            .find(|child| child.kind() == "modifiers")
            .and_then(text_of)
            .and_then(|modifiers| {
                ["public", "protected", "private"]
                    .iter()
                    .find(|keyword| modifiers.split_whitespace().any(|m| m == **keyword))
                    .map(|keyword| keyword.to_string())
            }),
        _ => None,
    }
}

/// One capture from a tree-sitter query run.
#[derive(Debug, Clone)]
pub struct QueryCaptureFact {
    /// Capture name from the query (without `@`).
    pub capture: String,
    /// Node kind of the captured node.
    pub kind: String,
    pub byte_start: usize,
    pub byte_end: usize,
    /// Source text of the captured node.
    pub text: String,
}

/// Result of running a tree-sitter query over one file.
#[derive(Debug, Clone)]
pub struct FileQueryFacts {
    pub language: &'static str,
    pub content_sha256: String,
    pub captures: Vec<QueryCaptureFact>,
}

/// Hard ceiling on captures returned by one query run, so a pathological
/// query (e.g. `(identifier) @x` over a generated file) stays bounded.
pub const MAX_QUERY_CAPTURES: usize = 5_000;

/// Run a tree-sitter query over `path`, optionally restricted to matches
/// intersecting the `within` byte range.
pub fn file_query(
    path: &Path,
    query_src: &str,
    within: Option<(usize, usize)>,
) -> Result<FileQueryFacts> {
    let parsed = super::parse_source_file(path)?;
    let ts_language = chunker::code::ts_language_for_name(parsed.language)?;
    let query = tree_sitter::Query::new(&ts_language, query_src).map_err(|e| {
        anyhow!(
            "invalid tree-sitter query for language {}: {e}",
            parsed.language
        )
    })?;
    let capture_names = query.capture_names();

    let mut cursor = tree_sitter::QueryCursor::new();
    if let Some((start, end)) = within {
        if start > end || end > parsed.source.len() {
            return Err(anyhow!(
                "within range {start}..{end} is out of bounds for {} ({} bytes)",
                path.display(),
                parsed.source.len()
            ));
        }
        cursor.set_byte_range(start..end);
    }

    let mut captures = Vec::new();
    let mut matches = cursor.matches(
        &query,
        parsed.tree.root_node(),
        parsed.source.as_bytes(),
    );
    'outer: while let Some(found) = streaming_iterator::StreamingIterator::next(&mut matches) {
        for capture in found.captures {
            if captures.len() >= MAX_QUERY_CAPTURES {
                break 'outer;
            }
            let node = capture.node;
            captures.push(QueryCaptureFact {
                capture: capture_names
                    .get(capture.index as usize)
                    .map(|name| name.to_string())
                    .unwrap_or_default(),
                kind: node.kind().to_string(),
                byte_start: node.start_byte(),
                byte_end: node.end_byte(),
                text: parsed
                    .source
                    .get(node.start_byte()..node.end_byte())
                    .unwrap_or_default()
                    .to_string(),
            });
        }
    }

    Ok(FileQueryFacts {
        language: parsed.language,
        content_sha256: sha256_hex(parsed.source.as_bytes()),
        captures,
    })
}

/// Byte range of the `name` identifier of the item at (or enclosing) the
/// given byte range — e.g. the `spawn_worker` of `pub fn spawn_worker(...)`.
/// Lets position-sensitive consumers (LSP rename) accept whole-item spans:
/// aiming at an item's `byte_start` hits the `pub` keyword, which
/// rust-analyzer refuses with "No references found at position".
pub fn name_span(path: &Path, byte_start: usize, byte_end: usize) -> Result<Option<(usize, usize)>> {
    let parsed = super::parse_source_file(path)?;
    let len = parsed.source.len();
    let (start, end) = (byte_start.min(len), byte_end.min(len).max(byte_start.min(len)));
    let mut node = match parsed.tree.root_node().named_descendant_for_byte_range(start, end) {
        Some(node) => node,
        None => return Ok(None),
    };
    loop {
        if node.kind() == "identifier" {
            return Ok(Some((node.start_byte(), node.end_byte())));
        }
        if let Some(name) = node.child_by_field_name("name") {
            return Ok(Some((name.start_byte(), name.end_byte())));
        }
        match node.parent() {
            Some(parent) => node = parent,
            None => return Ok(None),
        }
    }
}

/// Parse health of one source file.
#[derive(Debug, Clone)]
pub struct ParseCheckFacts {
    pub language: &'static str,
    pub error_nodes: usize,
    pub missing_nodes: usize,
}

/// Tree-sitter parse check for `path` — the post-apply validation primitive.
/// Errors for unsupported extensions; callers skip validation for those.
pub fn parse_check(path: &Path) -> Result<ParseCheckFacts> {
    let parsed = super::parse_source_file(path)?;
    let report = super::parse_report(parsed.tree.root_node());
    Ok(ParseCheckFacts {
        language: parsed.language,
        error_nodes: report.error_nodes,
        missing_nodes: report.missing_nodes,
    })
}

/// One parameter of a function signature.
#[derive(Debug, Clone)]
pub struct FnParamFact {
    /// Binding pattern text (`raw`, `mut workers`, `&self`, …).
    pub pattern: String,
    /// Declared type text; `None` for `self` parameters.
    pub type_text: Option<String>,
}

/// Signature facts for one function item, extracted from the AST.
#[derive(Debug, Clone)]
pub struct FnSignatureFacts {
    pub name: Option<String>,
    /// Visibility modifier text (`pub`, `pub(crate)`, …); `None` = private.
    pub visibility: Option<String>,
    pub is_async: bool,
    pub params: Vec<FnParamFact>,
    /// Return type text without the `->`; `None` = unit.
    pub return_type: Option<String>,
    /// Generic parameter list text (`<T: Clone>`); `None` when absent.
    pub generics: Option<String>,
    /// Byte range of the resolved function item (may widen a narrower input
    /// span, e.g. a name identifier, to the whole item).
    pub byte_start: usize,
    pub byte_end: usize,
    pub content_sha256: String,
}

/// Extract the signature of the function item at (or enclosing) the given
/// byte range. Rust only for now — other languages fail closed with a clear
/// error rather than guessing at grammar shapes.
///
/// When `expected_content_sha256` is set, the file hash is verified BEFORE
/// the byte range is interpreted — a drifted file must fail as `stale_span`,
/// never as a confusing "no function_item at range" against the new tree.
pub fn fn_signature(
    path: &Path,
    byte_start: usize,
    byte_end: usize,
    expected_content_sha256: Option<&str>,
) -> Result<FnSignatureFacts> {
    let parsed = super::parse_source_file(path)?;
    if let Some(expected) = expected_content_sha256 {
        let current = sha256_hex(parsed.source.as_bytes());
        if current != expected {
            return Err(anyhow!(
                "stale_span: {} changed since the span was minted (span hash {expected}, current {current}); re-derive the span from fresh facts",
                path.display()
            ));
        }
    }
    if parsed.language != "rust" {
        return Err(anyhow!(
            "fn_signature supports rust only for now (got {})",
            parsed.language
        ));
    }
    let len = parsed.source.len();
    let (start, end) = (byte_start.min(len), byte_end.min(len).max(byte_start.min(len)));
    let root = parsed.tree.root_node();
    let mut node = root
        .named_descendant_for_byte_range(start, end)
        .ok_or_else(|| anyhow!("no syntax node at byte range {start}..{end}"))?;
    while node.kind() != "function_item" {
        let Some(parent) = node.parent() else {
            return Err(anyhow!(
                "no function_item at or enclosing byte range {start}..{end} (innermost node kind: {})",
                parsed
                    .tree
                    .root_node()
                    .named_descendant_for_byte_range(start, end)
                    .map(|n| n.kind())
                    .unwrap_or("?")
            ));
        };
        node = parent;
    }

    let text_of = |n: tree_sitter::Node<'_>| -> String {
        parsed
            .source
            .get(n.start_byte()..n.end_byte())
            .unwrap_or_default()
            .to_string()
    };

    let mut visibility = None;
    let mut is_async = false;
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        match child.kind() {
            "visibility_modifier" => visibility = Some(text_of(child)),
            "function_modifiers" => is_async = text_of(child).contains("async"),
            _ => {}
        }
    }

    let mut params = Vec::new();
    if let Some(parameters) = node.child_by_field_name("parameters") {
        let mut cursor = parameters.walk();
        for parameter in parameters.named_children(&mut cursor) {
            match parameter.kind() {
                "parameter" => params.push(FnParamFact {
                    pattern: parameter
                        .child_by_field_name("pattern")
                        .map(text_of)
                        .unwrap_or_else(|| text_of(parameter)),
                    type_text: parameter.child_by_field_name("type").map(text_of),
                }),
                "self_parameter" => params.push(FnParamFact {
                    pattern: text_of(parameter),
                    type_text: None,
                }),
                "attribute_item" | "line_comment" | "block_comment" => {}
                _ => params.push(FnParamFact {
                    pattern: text_of(parameter),
                    type_text: None,
                }),
            }
        }
    }

    Ok(FnSignatureFacts {
        name: node.child_by_field_name("name").map(text_of),
        visibility,
        is_async,
        params,
        return_type: node.child_by_field_name("return_type").map(text_of),
        generics: node.child_by_field_name("type_parameters").map(text_of),
        byte_start: node.start_byte(),
        byte_end: node.end_byte(),
        content_sha256: sha256_hex(parsed.source.as_bytes()),
    })
}

#[cfg(test)]
mod facts_tests {
    use super::*;
    use std::fs;

    fn fixture(dir: &Path) -> std::path::PathBuf {
        let path = dir.join("probe.rs");
        fs::write(
            &path,
            "pub struct Alpha;\n\npub fn beta() -> u8 {\n    7\n}\n\nfn gamma() {}\n",
        )
        .unwrap();
        path
    }

    #[test]
    fn file_items_inventories_rust_top_level_items() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let path = fixture(&root);
        let facts = file_items(&path).unwrap();
        assert_eq!(facts.language, "rust");
        assert_eq!(facts.content_sha256.len(), 64);
        assert_eq!(facts.source_len, fs::read(&path).unwrap().len());
        let names: Vec<_> = facts
            .items
            .iter()
            .filter_map(|i| i.item.name.as_deref())
            .collect();
        assert!(names.contains(&"Alpha"), "items: {names:?}");
        assert!(names.contains(&"beta"), "items: {names:?}");
        let beta = facts
            .items
            .iter()
            .find(|i| i.item.name.as_deref() == Some("beta"))
            .unwrap();
        assert_eq!(beta.visibility.as_deref(), Some("pub"));
        let gamma = facts
            .items
            .iter()
            .find(|i| i.item.name.as_deref() == Some("gamma"))
            .unwrap();
        assert_eq!(gamma.visibility, None);
    }

    #[test]
    fn file_query_returns_named_captures_with_spans() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let path = fixture(&root);
        let facts =
            file_query(&path, "(function_item name: (identifier) @fn_name)", None).unwrap();
        let names: Vec<_> = facts.captures.iter().map(|c| c.text.as_str()).collect();
        assert_eq!(names, vec!["beta", "gamma"]);
        let beta = &facts.captures[0];
        assert_eq!(beta.capture, "fn_name");
        assert_eq!(beta.kind, "identifier");
        let source = fs::read_to_string(&path).unwrap();
        assert_eq!(&source[beta.byte_start..beta.byte_end], "beta");
    }

    #[test]
    fn file_query_within_restricts_matches() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let path = fixture(&root);
        let source = fs::read_to_string(&path).unwrap();
        let gamma_at = source.find("fn gamma").unwrap();
        let facts = file_query(
            &path,
            "(function_item name: (identifier) @fn_name)",
            Some((gamma_at, source.len())),
        )
        .unwrap();
        let names: Vec<_> = facts.captures.iter().map(|c| c.text.as_str()).collect();
        assert_eq!(names, vec!["gamma"]);
    }

    #[test]
    fn fn_signature_extracts_pub_fn_with_result_return() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let path = root.join("sig.rs");
        fs::write(
            &path,
            "pub async fn fetch<T: Clone>(id: u32, name: &str) -> Result<T, String> {\n    todo!()\n}\n\nfn private_unit(x: u8) {}\n\npub struct S;\nimpl S {\n    pub fn method(&self, n: usize) -> usize { n }\n}\n",
        )
        .unwrap();
        let source = fs::read_to_string(&path).unwrap();

        let at = source.find("fetch").unwrap();
        let sig = fn_signature(&path, at, at + 5, None).unwrap();
        assert_eq!(sig.name.as_deref(), Some("fetch"));
        assert_eq!(sig.visibility.as_deref(), Some("pub"));
        assert!(sig.is_async);
        assert_eq!(sig.generics.as_deref(), Some("<T: Clone>"));
        assert_eq!(sig.return_type.as_deref(), Some("Result<T, String>"));
        assert_eq!(sig.params.len(), 2);
        assert_eq!(sig.params[0].pattern, "id");
        assert_eq!(sig.params[0].type_text.as_deref(), Some("u32"));
        assert_eq!(sig.params[1].type_text.as_deref(), Some("&str"));
        assert_eq!(&source[sig.byte_start..sig.byte_end].split('(').next().unwrap(), &"pub async fn fetch<T: Clone>");

        let at = source.find("private_unit").unwrap();
        let sig = fn_signature(&path, at, at, None).unwrap();
        assert_eq!(sig.visibility, None);
        assert_eq!(sig.return_type, None);
        assert!(!sig.is_async);

        let at = source.find("method").unwrap();
        let sig = fn_signature(&path, at, at + 6, None).unwrap();
        assert_eq!(sig.name.as_deref(), Some("method"));
        assert_eq!(sig.params[0].pattern, "&self");
        assert_eq!(sig.params[0].type_text, None);
        assert_eq!(sig.return_type.as_deref(), Some("usize"));
    }

    #[test]
    fn fn_signature_rejects_non_function_span() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let path = fixture(&root);
        let source = fs::read_to_string(&path).unwrap();
        let at = source.find("struct Alpha").unwrap();
        let err = fn_signature(&path, at, at + 5, None).unwrap_err();
        assert!(err.to_string().contains("no function_item"), "got: {err}");
    }

    #[test]
    fn file_query_rejects_invalid_query() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let path = fixture(&root);
        let err = file_query(&path, "(nonsense_node_kind) @x", None).unwrap_err();
        assert!(err.to_string().contains("query"), "got: {err}");
    }
}
