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

/// Top-level syntax-item inventory of one source file — the same per-language
/// item walk `bbox_refactor_status` uses, with the source hash captured at
/// read time so callers can mint drift-guarded spans.
#[derive(Debug, Clone)]
pub struct FileItemsFacts {
    pub language: &'static str,
    pub content_sha256: String,
    pub source_len: usize,
    pub items: Vec<SyntaxItem>,
}

/// Inventory the top-level syntax items of `path`.
pub fn file_items(path: &Path) -> Result<FileItemsFacts> {
    let parsed = super::parse_source_file(path)?;
    let items = match parsed.language {
        "rust" => super::rust_status_items(&parsed),
        "java" => super::java_status_items(&parsed),
        _ => super::generic_top_level_items(&parsed),
    };
    Ok(FileItemsFacts {
        language: parsed.language,
        content_sha256: sha256_hex(parsed.source.as_bytes()),
        source_len: parsed.source.len(),
        items,
    })
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
        let names: Vec<_> = facts
            .items
            .iter()
            .filter_map(|i| i.name.as_deref())
            .collect();
        assert!(names.contains(&"Alpha"), "items: {names:?}");
        assert!(names.contains(&"beta"), "items: {names:?}");
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
    fn file_query_rejects_invalid_query() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let path = fixture(&root);
        let err = file_query(&path, "(nonsense_node_kind) @x", None).unwrap_err();
        assert!(err.to_string().contains("query"), "got: {err}");
    }
}
