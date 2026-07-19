//! `rust_references` — syntactic Rust symbol reference counting.
//!
//! Fast syntactic count lane for Rust: walks .rs files under a project root,
//! counts tree-sitter-grounded references to simple symbol names, and returns
//! per-symbol counts, file lists, and up to five example sites per symbol.
//!
//! Deliberately returns no hash-anchored spans and no full usages array; the
//! isolate-heap discipline (§2 of bindings AGENTS.md) caps the payload. This is
//! the bounded answer to "where is this referenced?" for Rust.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use anyhow::{Result, anyhow};
use serde::{Deserialize, Serialize};
use tree_sitter::Node;
use walkdir::WalkDir;

use super::parse_rust_file;

/// Cap on examples per symbol returned in the summary.
const MAX_EXAMPLES_PER_SYMBOL: usize = 5;
/// Cap on total usages before truncation.
const MAX_TOTAL_USAGES: usize = 5000;
/// Cap on files scanned.
const MAX_FILES_SCANNED: usize = 500;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RustReferenceExample {
    pub path: String,
    pub line: usize,
    pub column: usize,
    pub context: String,
    pub is_test_site: bool,
    pub usage_kind: String,
    pub matched_name: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct RustReferenceSummary {
    pub symbols: Vec<String>,
    pub total_usages: usize,
    pub unique_files: usize,
    pub production_sites: usize,
    pub test_sites: usize,
    pub counts_by_symbol: BTreeMap<String, usize>,
    pub files_by_symbol: BTreeMap<String, Vec<String>>,
    pub examples_by_symbol: BTreeMap<String, Vec<RustReferenceExample>>,
    pub truncated: bool,
}

/// Count Rust references to `symbols` across .rs files under `project_dir`.
pub fn count_rust_references(
    project_dir: &Path,
    symbols: &[String],
) -> Result<RustReferenceSummary> {
    if symbols.is_empty() {
        return Err(anyhow!("symbols must be non-empty"));
    }
    let symbol_set: BTreeSet<&str> = symbols.iter().map(String::as_str).collect();

    let mut counts_by_symbol: BTreeMap<String, usize> = BTreeMap::new();
    let mut files_by_symbol: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut examples_by_symbol: BTreeMap<String, Vec<RustReferenceExample>> = BTreeMap::new();
    let mut unique_files: BTreeSet<String> = BTreeSet::new();
    let mut production_sites: usize = 0;
    let mut test_sites: usize = 0;
    let mut total_usages: usize = 0;
    let mut file_count: usize = 0;

    for entry in WalkDir::new(project_dir).into_iter().filter_entry(|entry| {
        let name = entry.file_name().to_string_lossy();
        !matches!(
            name.as_ref(),
            ".git" | "target" | "node_modules" | ".claude" | ".bro"
        )
    }) {
        let entry = entry?;
        if !entry.file_type().is_file()
            || entry.path().extension().and_then(|e| e.to_str()) != Some("rs")
        {
            continue;
        }
        if file_count >= MAX_FILES_SCANNED {
            break;
        }
        file_count += 1;

        let path = entry.path();
        let is_test = path.to_string_lossy().contains("/tests/")
            || path
                .file_name()
                .map(|n| n.to_string_lossy().ends_with("_test.rs"))
                .unwrap_or(false)
            || path.to_string_lossy().contains("/test/")
            || path
                .file_name()
                .map(|n| {
                    let s = n.to_string_lossy();
                    s.starts_with("test_") || s.ends_with("_tests.rs")
                })
                .unwrap_or(false);

        let parsed = match parse_rust_file(path) {
            Ok(p) => p,
            Err(_) => continue, // skip unparseable files
        };

        let rel_path = path_string(path);
        let per_file = count_in_file(&parsed, &symbol_set, &rel_path, is_test);

        if !per_file.is_empty() {
            unique_files.insert(rel_path.clone());
            for (sym, (count, examples)) in per_file {
                *counts_by_symbol.entry(sym.clone()).or_default() += count;
                if is_test {
                    test_sites += count;
                } else {
                    production_sites += count;
                }
                total_usages += count;

                files_by_symbol
                    .entry(sym.clone())
                    .or_default()
                    .push(rel_path.clone());

                let existing_examples = examples_by_symbol.entry(sym).or_default();
                for ex in examples {
                    if existing_examples.len() < MAX_EXAMPLES_PER_SYMBOL {
                        existing_examples.push(ex);
                    }
                }
            }
        }

        if total_usages >= MAX_TOTAL_USAGES {
            break;
        }
    }

    let truncated = total_usages >= MAX_TOTAL_USAGES || file_count >= MAX_FILES_SCANNED;

    Ok(RustReferenceSummary {
        symbols: symbols.to_vec(),
        total_usages,
        unique_files: unique_files.len(),
        production_sites,
        test_sites,
        counts_by_symbol,
        files_by_symbol,
        examples_by_symbol,
        truncated,
    })
}

fn count_in_file(
    parsed: &super::ParsedSource,
    symbols: &BTreeSet<&str>,
    rel_path: &str,
    is_test: bool,
) -> BTreeMap<String, (usize, Vec<RustReferenceExample>)> {
    let mut out: BTreeMap<String, (usize, Vec<RustReferenceExample>)> = BTreeMap::new();
    let root = parsed.tree.root_node();
    let source = &parsed.source;
    walk_node(root, source, symbols, rel_path, is_test, &mut out);
    out
}

fn walk_node(
    node: Node,
    source: &str,
    symbols: &BTreeSet<&str>,
    rel_path: &str,
    is_test: bool,
    out: &mut BTreeMap<String, (usize, Vec<RustReferenceExample>)>,
) {
    match node.kind() {
        "identifier" | "type_identifier" => {
            if let Ok(text) = node.utf8_text(source.as_bytes())
                && symbols.contains(text)
            {
                let kind = classify_usage(&node, source);
                record_hit(out, text, rel_path, node, source, is_test, kind);
            }
        }
        "macro_invocation" => {
            // Check the macro name (first child identifier)
            if let Some(name_node) = node.child_by_field_name("macro")
                && let Ok(text) = name_node.utf8_text(source.as_bytes())
                && symbols.contains(text)
            {
                record_hit(out, text, rel_path, name_node, source, is_test, "macro_use");
            }
            // Macro arguments are token trees, not parsed AST; skip
            // recursion to avoid double-counting the macro name as
            // path_ref when the walker re-encounters it as a child
            // identifier.
            return;
        }
        _ => {}
    }

    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        walk_node(child, source, symbols, rel_path, is_test, out);
    }
}

fn classify_usage(node: &Node, _source: &str) -> &'static str {
    let parent = node.parent();
    let Some(parent) = parent else {
        return "path_ref";
    };

    match parent.kind() {
        "call_expression" => {
            // If this identifier is the function of a call expression
            if parent.child_by_field_name("function").map(|f| f.id()) == Some(node.id()) {
                return "call";
            }
            "path_ref"
        }
        "type_identifier" => "type_ref",
        "scoped_type_identifier" => "type_ref",
        "generic_type" => "type_ref",
        "use_declaration" | "use_as_clause" | "use_list" => "path_ref",
        "scoped_identifier" => "path_ref",
        _ => "path_ref",
    }
}

fn record_hit(
    out: &mut BTreeMap<String, (usize, Vec<RustReferenceExample>)>,
    name: &str,
    rel_path: &str,
    node: Node,
    source: &str,
    is_test: bool,
    usage_kind: &str,
) {
    let entry = out.entry(name.to_string()).or_default();
    entry.0 += 1;
    if entry.1.len() < MAX_EXAMPLES_PER_SYMBOL {
        let start = node.start_position();
        let line = start.row + 1;
        let column = start.column + 1;
        let line_text = source
            .lines()
            .nth(start.row)
            .unwrap_or("")
            .trim()
            .chars()
            .take(240)
            .collect::<String>();
        entry.1.push(RustReferenceExample {
            path: rel_path.to_string(),
            line,
            column,
            context: line_text,
            is_test_site: is_test,
            usage_kind: usage_kind.to_string(),
            matched_name: name.to_string(),
        });
    }
}

fn path_string(path: &Path) -> String {
    path.to_string_lossy().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn write_fixture(dir: &Path, name: &str, content: &str) {
        let path = dir.join(name);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(&path, content).unwrap();
    }

    #[test]
    fn counts_symbol_references() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();

        write_fixture(
            root,
            "src/lib.rs",
            r#"pub fn hello() -> String { "hello".into() }

pub fn greet() {
    let _ = hello();           // call
    let x: Option<String> = None;  // type_ref
    println!("{}", hello());   // call
}

pub struct Greeter {
    name: String,
}

impl Greeter {
    pub fn new(name: String) -> Greeter {
        Greeter { name }
    }
    pub fn say(&self) -> String {
        hello()
    }
}
"#,
        );

        write_fixture(
            root,
            "tests/test_lib.rs",
            r#"use crate::hello;

#[test]
fn test_hello() {
    let s = hello();
    assert_eq!(s, "hello");
}

fn helper() -> Greeter {
    Greeter::new("test".into())
}
"#,
        );

        let summary = count_rust_references(
            root,
            &[
                "hello".to_string(),
                "Greeter".to_string(),
                "Option".to_string(),
            ],
        )
        .unwrap();

        assert!(!summary.truncated);
        // "hello" appears in both files as call targets
        assert!(
            *summary.counts_by_symbol.get("hello").unwrap_or(&0) >= 3,
            "hello should have >=3 refs: {summary:?}"
        );
        // "Greeter" appears as type_ref
        assert!(
            *summary.counts_by_symbol.get("Greeter").unwrap_or(&0) >= 2,
            "Greeter should have >=2 refs: {summary:?}"
        );
        // Options should have at least 1
        assert!(
            *summary.counts_by_symbol.get("Option").unwrap_or(&0) >= 1,
            "Option should have >=1 refs: {summary:?}"
        );
        assert!(summary.production_sites > 0);
        assert!(summary.test_sites > 0);
    }

    #[test]
    fn reports_truncation_flag() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        // Write many files with references
        for i in 0..10 {
            write_fixture(
                root,
                &format!("src/mod{i}.rs"),
                &format!("fn f{i}() {{ let _ = sym(); }}"),
            );
        }

        let summary = count_rust_references(root, &["sym".to_string()]).unwrap();
        // small fixture, no truncation expected
        assert!(!summary.truncated);
        assert_eq!(*summary.counts_by_symbol.get("sym").unwrap_or(&0), 10);
    }

    #[test]
    fn empty_symbols_is_error() {
        let dir = tempfile::tempdir().unwrap();
        let err = count_rust_references(dir.path(), &[]).unwrap_err();
        assert!(err.to_string().contains("non-empty"));
    }

    #[test]
    fn macro_usage_detected() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write_fixture(
            root,
            "src/lib.rs",
            "fn f() { println!(\"hi\"); println!(\"there\"); }",
        );

        let summary = count_rust_references(root, &["println".to_string()]).unwrap();
        assert_eq!(
            *summary.counts_by_symbol.get("println").unwrap_or(&0),
            2,
            "{summary:?}"
        );
        // Examples should show macro_use kind
        if let Some(examples) = summary.examples_by_symbol.get("println") {
            assert!(
                examples.iter().any(|e| e.usage_kind == "macro_use"),
                "{examples:?}"
            );
        }
    }
}
