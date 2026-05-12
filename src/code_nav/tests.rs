use super::*;
use crate::projects::ProjectRecord;
use std::collections::BTreeSet;
use std::fs;
use tempfile::TempDir;

fn setup_test_file(dir: &TempDir, name: &str, content: &str) -> String {
    let path = dir.path().join(name);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(&path, content).unwrap();
    path.to_string_lossy().into_owned()
}

/// Build a synthetic `ProjectRecord` for a TempDir so the
/// registered-project gate accepts the dir during code_symbols tests.
/// The canonical_path matches what the gate will canonicalise the tempdir
/// to, so descendant checks pass.
fn registered_for(dir: &TempDir) -> Vec<ProjectRecord> {
    let canonical_path = dir
        .path()
        .canonicalize()
        .unwrap()
        .to_string_lossy()
        .into_owned();
    vec![ProjectRecord {
        project_id: "test-project".to_string(),
        repo_id: None,
        canonical_path,
        registered_at: "2026-01-01T00:00:00Z".to_string(),
        is_git_repo: false,
        languages: BTreeSet::new(),
    }]
}

#[test]
fn test_code_query_rust() {
    let dir = TempDir::new().unwrap();
    let file = setup_test_file(&dir, "test.rs", "fn main() { println!(\"hello\"); }");
    let params = CodeQueryParams {
        file,
        query: "(function_item name: (identifier) @name)".to_string(),
        project_dir: None,
        language: None,
        limit: None,
        include_text: Some(true),
    };
    let response_json = code_query(&params).unwrap();
    let response: CodeQueryResponse = serde_json::from_str(&response_json).unwrap();
    assert_eq!(response.captures.len(), 1);
    assert_eq!(response.captures[0].capture_name, "name");
    assert_eq!(response.captures[0].text.as_deref(), Some("main"));
    assert_eq!(
        response.captures[0]
            .handoff
            .nearest_refactor_item
            .as_ref()
            .and_then(|item| item.name.as_deref()),
        Some("main")
    );
    assert_eq!(
        response.captures[0]
            .handoff
            .refactor_status
            .as_ref()
            .map(|hint| hint.tool.as_str()),
        Some("bbox_refactor_status")
    );
}

#[test]
fn test_code_node_describe_python() {
    let dir = TempDir::new().unwrap();
    let file = setup_test_file(&dir, "test.py", "def foo():\n    pass");
    let params = CodeNodeDescribeParams {
        file,
        line: 1,
        column: 6, // 'foo'
        project_dir: None,
        include_siblings: Some(true),
        include_text: Some(true),
    };
    let response_json = code_node_describe(&params).unwrap();
    let response: CodeNodeDescribeResponse = serde_json::from_str(&response_json).unwrap();
    assert_eq!(response.node_kind, "identifier");
    assert_eq!(response.text.as_deref(), Some("foo"));
    assert!(response
        .parent_chain
        .contains(&"function_definition".to_string()));
    assert_eq!(
        response
            .handoff
            .nearest_refactor_item
            .as_ref()
            .and_then(|item| item.name.as_deref()),
        Some("foo")
    );
}

#[test]
fn test_code_query_java() {
    let dir = TempDir::new().unwrap();
    let file = setup_test_file(&dir, "Test.java", "class Test { void run() {} }");
    let params = CodeQueryParams {
        file,
        query: "(method_declaration name: (identifier) @name)".to_string(),
        project_dir: None,
        language: None,
        limit: None,
        include_text: Some(true),
    };
    let response_json = code_query(&params).unwrap();
    let response: CodeQueryResponse = serde_json::from_str(&response_json).unwrap();
    assert_eq!(response.captures.len(), 1);
    assert_eq!(response.captures[0].text.as_deref(), Some("run"));
}

#[test]
fn test_code_query_javascript() {
    let dir = TempDir::new().unwrap();
    let file = setup_test_file(&dir, "test.js", "function hello() { return 42; }");
    let params = CodeQueryParams {
        file,
        query: "(function_declaration name: (identifier) @name)".to_string(),
        project_dir: None,
        language: None,
        limit: None,
        include_text: Some(true),
    };
    let response_json = code_query(&params).unwrap();
    let response: CodeQueryResponse = serde_json::from_str(&response_json).unwrap();
    assert_eq!(response.captures.len(), 1);
    assert_eq!(response.captures[0].text.as_deref(), Some("hello"));
}

/// CN-T3 (Acceptance Criterion 6): explicit TypeScript coverage on
/// `bbox_code_query` and `bbox_code_node_describe`. Existing tests
/// cover Rust, Python (describe), Java, JavaScript; TS shares the
/// JavaScript curated query but rides a different grammar via
/// tree_sitter_typescript, so coverage here pins the
/// language-pack mapping for `.ts`.
#[test]
fn test_code_query_typescript() {
    let dir = TempDir::new().unwrap();
    let file = setup_test_file(
        &dir,
        "src/m.ts",
        "function greet(name: string): string { return name; }\n",
    );
    let params = CodeQueryParams {
        file,
        query: "(function_declaration name: (identifier) @name)".to_string(),
        project_dir: None,
        language: None,
        limit: None,
        include_text: Some(true),
    };
    let response_json = code_query(&params).unwrap();
    let response: CodeQueryResponse = serde_json::from_str(&response_json).unwrap();
    assert_eq!(response.language, "typescript");
    assert_eq!(response.captures[0].text.as_deref(), Some("greet"));
    assert_eq!(response.semantic_status, SEMANTIC_STATUS_SYNTAX_ONLY);
}

/// CN-T3: TypeScript node-describe parallels the Python case from
/// CN-S2 era — exercise the node-describe path on a TS class
/// method to confirm parent_chain resolution + semantic_status.
#[test]
fn test_code_node_describe_typescript() {
    let dir = TempDir::new().unwrap();
    let file = setup_test_file(
        &dir,
        "src/m.ts",
        "class C {\n  run(): void {\n    return;\n  }\n}\n",
    );
    let params = CodeNodeDescribeParams {
        file,
        line: 2,
        column: 3, // `run`
        project_dir: None,
        include_siblings: Some(false),
        include_text: Some(true),
    };
    let response_json = code_node_describe(&params).unwrap();
    let response: CodeNodeDescribeResponse = serde_json::from_str(&response_json).unwrap();
    assert_eq!(response.language, "typescript");
    assert!(
        response
            .parent_chain
            .iter()
            .any(|kind| kind == "class_declaration"),
        "TS class method describe must include class_declaration in parent_chain; got {:?}",
        response.parent_chain
    );
    assert_eq!(response.semantic_status, SEMANTIC_STATUS_SYNTAX_ONLY);
}

#[test]
fn test_code_query_handoff_suggests_refactor_status_for_java_method() {
    let dir = TempDir::new().unwrap();
    let file = setup_test_file(&dir, "Test.java", "class Test { void run() {} }");
    let params = CodeQueryParams {
        file,
        query: "(method_declaration name: (identifier) @name)".to_string(),
        project_dir: None,
        language: None,
        limit: None,
        include_text: Some(true),
    };
    let response_json = code_query(&params).unwrap();
    let response: CodeQueryResponse = serde_json::from_str(&response_json).unwrap();
    let status_args = &response.captures[0]
        .handoff
        .refactor_status
        .as_ref()
        .unwrap()
        .arguments;
    assert_eq!(status_args.item_names, vec!["run"]);
    assert_eq!(status_args.item_kinds, vec!["method_declaration"]);
}

#[test]
fn test_code_symbols_finds_java_method_line_ranges_without_rg() {
    let dir = TempDir::new().unwrap();
    setup_test_file(
        &dir,
        "src/main/java/Test.java",
        "class Test {\n  void run() {}\n  void stop() {}\n}\n",
    );
    setup_test_file(&dir, "src/main/rust/lib.rs", "fn run() {}\n");
    let params = CodeSymbolSearchParams {
        project_dir: dir.path().to_string_lossy().into_owned(),
        query: Some("run".to_string()),
        languages: Some(vec!["java".to_string()]),
        item_kinds: Some(vec!["method_declaration".to_string()]),
        path_contains: None,
        limit: None,
        file_limit: None,
        include_attributes: Some(false),
        mode: None,
    };
    let response_json = code_symbols(&params, &registered_for(&dir), None).unwrap();
    let response: CodeSymbolSearchResponse = serde_json::from_str(&response_json).unwrap();
    assert_eq!(response.scanned_files, 1);
    assert_eq!(response.matching_items, 1);
    assert_eq!(response.items[0].file, "src/main/java/Test.java");
    assert_eq!(response.items[0].name.as_deref(), Some("run"));
    assert_eq!(response.items[0].line_range, (2, 2));
    assert_eq!(
        response.items[0]
            .handoff
            .refactor_status
            .as_ref()
            .unwrap()
            .arguments
            .item_names,
        vec!["run"]
    );
}

#[test]
fn test_code_query_uses_language_override_for_extensionless_file() {
    let dir = TempDir::new().unwrap();
    let file = setup_test_file(&dir, "script", "fn main() {}");
    let params = CodeQueryParams {
        file,
        query: "(function_item name: (identifier) @name)".to_string(),
        project_dir: None,
        language: Some("rust".to_string()),
        limit: None,
        include_text: Some(true),
    };
    let response_json = code_query(&params).unwrap();
    let response: CodeQueryResponse = serde_json::from_str(&response_json).unwrap();
    assert_eq!(response.language, "rust");
    assert_eq!(response.matching_captures, 1);
    assert_eq!(response.returned_captures, 1);
    assert!(!response.truncated);
    assert_eq!(response.captures[0].text.as_deref(), Some("main"));
    assert_eq!(
        response.captures[0]
            .handoff
            .project_refs
            .arguments
            .query
            .as_deref(),
        Some("main")
    );
}

#[test]
fn test_code_query_language_override_controls_parse_and_query_language() {
    let dir = TempDir::new().unwrap();
    let file = setup_test_file(
        &dir,
        "looks_like_rust.rs",
        "function hello() { return 42; }",
    );
    let params = CodeQueryParams {
        file,
        query: "(function_declaration name: (identifier) @name)".to_string(),
        project_dir: None,
        language: Some("javascript".to_string()),
        limit: None,
        include_text: Some(true),
    };
    let response_json = code_query(&params).unwrap();
    let response: CodeQueryResponse = serde_json::from_str(&response_json).unwrap();
    assert_eq!(response.language, "javascript");
    assert_eq!(response.matching_captures, 1);
    assert_eq!(response.returned_captures, 1);
    assert_eq!(response.captures[0].text.as_deref(), Some("hello"));
}

#[test]
fn test_code_query_uses_language_pack_for_mapped_languages() {
    let dir = TempDir::new().unwrap();
    let file = setup_test_file(&dir, "lib/example.ex", "defmodule Example do\nend\n");
    let params = CodeQueryParams {
        file,
        query: "(_) @node".to_string(),
        project_dir: None,
        language: None,
        limit: Some(1),
        include_text: Some(false),
    };
    let response_json = code_query(&params).unwrap();
    let response: CodeQueryResponse = serde_json::from_str(&response_json).unwrap();
    assert_eq!(response.language, "elixir");
    assert!(response.matching_captures >= 1);
    assert_eq!(response.returned_captures, 1);
    assert!(response.truncated);
}

#[test]
fn test_code_query_handoff_maps_rust_impl_method_to_refactor_status_kind() {
    let dir = TempDir::new().unwrap();
    let file = setup_test_file(
        &dir,
        "test.rs",
        "struct Thing;\nimpl Thing { fn run(&self) {} }",
    );
    let params = CodeQueryParams {
        file,
        query: "(function_item name: (identifier) @name)".to_string(),
        project_dir: None,
        language: None,
        limit: None,
        include_text: Some(true),
    };
    let response_json = code_query(&params).unwrap();
    let response: CodeQueryResponse = serde_json::from_str(&response_json).unwrap();
    let status_args = &response.captures[0]
        .handoff
        .refactor_status
        .as_ref()
        .unwrap()
        .arguments;
    assert_eq!(status_args.item_names, vec!["run"]);
    assert_eq!(status_args.item_kinds, vec!["impl_method"]);
}

#[test]
fn test_code_query_reports_truncation() {
    let dir = TempDir::new().unwrap();
    let file = setup_test_file(&dir, "test.rs", "fn one() {}\nfn two() {}\nfn three() {}");
    let params = CodeQueryParams {
        file,
        query: "(function_item name: (identifier) @name)".to_string(),
        project_dir: None,
        language: None,
        limit: Some(2),
        include_text: Some(true),
    };
    let response_json = code_query(&params).unwrap();
    let response: CodeQueryResponse = serde_json::from_str(&response_json).unwrap();
    assert_eq!(response.matching_captures, 3);
    assert_eq!(response.returned_captures, 2);
    assert!(response.truncated);
    assert_eq!(response.captures.len(), 2);
}

#[test]
fn test_unsupported_language_error() {
    let dir = TempDir::new().unwrap();
    let file = setup_test_file(&dir, "test.unknown", "test");
    let params = CodeQueryParams {
        file,
        query: "(identifier)".to_string(),
        project_dir: None,
        language: None,
        limit: None,
        include_text: None,
    };
    let result = code_query(&params);
    assert!(result.is_err());
    assert!(result
        .unwrap_err()
        .to_string()
        .contains("unsupported source file extension"));
}

/// Every code-nav tool must emit `semantic_status = "syntax_only"` at the
/// top level. The design boundary in
/// `design/proposed/code-nav-symbolic-exploration.md` forbids surfaces that
/// promise binding-aware answers without going through LSP / compiler /
/// graph confidence — this test locks that invariant mechanically so it
/// cannot drift from the doc.
#[test]
fn test_code_nav_tools_always_label_semantic_status_syntax_only() {
    let dir = TempDir::new().unwrap();
    let rs_file = setup_test_file(&dir, "test.rs", "fn main() { println!(\"x\"); }");
    let java_file = setup_test_file(
        &dir,
        "src/main/java/Test.java",
        "class Test {\n  void run() {}\n}\n",
    );

    let query_response_json = code_query(&CodeQueryParams {
        file: rs_file.clone(),
        query: "(function_item name: (identifier) @name)".to_string(),
        project_dir: None,
        language: None,
        limit: None,
        include_text: None,
    })
    .unwrap();
    let query_response: CodeQueryResponse = serde_json::from_str(&query_response_json).unwrap();
    assert_eq!(query_response.semantic_status, SEMANTIC_STATUS_SYNTAX_ONLY);
    assert_eq!(query_response.semantic_status, "syntax_only");

    let describe_response_json = code_node_describe(&CodeNodeDescribeParams {
        file: rs_file,
        line: 1,
        column: 4,
        project_dir: None,
        include_siblings: None,
        include_text: None,
    })
    .unwrap();
    let describe_response: CodeNodeDescribeResponse =
        serde_json::from_str(&describe_response_json).unwrap();
    assert_eq!(
        describe_response.semantic_status,
        SEMANTIC_STATUS_SYNTAX_ONLY
    );
    assert_eq!(describe_response.semantic_status, "syntax_only");

    let _ = java_file;
    let symbols_response_json = code_symbols(
        &CodeSymbolSearchParams {
            project_dir: dir.path().to_string_lossy().into_owned(),
            query: Some("run".to_string()),
            languages: Some(vec!["java".to_string()]),
            item_kinds: None,
            path_contains: None,
            limit: None,
            file_limit: None,
            include_attributes: Some(false),
            mode: None,
        },
        &registered_for(&dir),
        None,
    )
    .unwrap();
    let symbols_response: CodeSymbolSearchResponse =
        serde_json::from_str(&symbols_response_json).unwrap();
    assert_eq!(
        symbols_response.semantic_status,
        SEMANTIC_STATUS_SYNTAX_ONLY
    );
    assert_eq!(symbols_response.semantic_status, "syntax_only");
}

/// CN-T2: refactor_kind_for is the load-bearing function that lets
/// the indexed and live lanes return the same `kind` field on Rust
/// impl methods. New synthesis cases must be added to this function
/// AND mirrored in refactor::status. Lock the current contract here.
#[test]
fn refactor_kind_for_synthesises_rust_impl_method() {
    // The only documented synthesis as of CN-T2 landing.
    assert_eq!(
        refactor_kind_for("rust", "function_item", Some("impl_item")),
        "impl_method"
    );
    // Top-level Rust functions stay function_item.
    assert_eq!(
        refactor_kind_for("rust", "function_item", None),
        "function_item"
    );
    // Other languages: no synthesis today.
    assert_eq!(
        refactor_kind_for("java", "method_declaration", Some("class_declaration")),
        "method_declaration"
    );
    // Unknown language: pass through.
    assert_eq!(refactor_kind_for("haskell", "function", None), "function");
}

/// CN-T2: reverse derivation used by the live lane to fill
/// symbol_kind/parent_kind when only the refactor synthetic kind is
/// available. Asymmetric — only documented synthesis pairs are
/// recovered.
#[test]
fn symbol_kind_from_refactor_recovers_rust_impl_method_parent() {
    assert_eq!(
        symbol_kind_from_refactor("rust", "impl_method"),
        ("function_item".to_string(), Some("impl_item".to_string()))
    );
    // No synthesis → pass through with no parent claim.
    assert_eq!(
        symbol_kind_from_refactor("rust", "struct_item"),
        ("struct_item".to_string(), None)
    );
    assert_eq!(
        symbol_kind_from_refactor("java", "method_declaration"),
        ("method_declaration".to_string(), None)
    );
}

/// CN-T2: live-lane records must carry both `kind` (refactor synth)
/// and `symbol_kind` (raw tree-sitter) for every entry. For Rust impl
/// methods specifically, both must round-trip and parent_kind must
/// resolve to `impl_item`.
#[test]
fn code_symbols_live_lane_populates_symbol_kind_and_parent_kind() {
    let dir = TempDir::new().unwrap();
    setup_test_file(
        &dir,
        "src/lib.rs",
        "pub struct S;\n\nimpl S {\n    pub fn run(&self) -> i32 { 1 }\n}\n",
    );
    let params = CodeSymbolSearchParams {
        project_dir: dir.path().to_string_lossy().into_owned(),
        query: None,
        languages: Some(vec!["rust".to_string()]),
        item_kinds: None,
        path_contains: None,
        limit: None,
        file_limit: None,
        include_attributes: None,
        mode: Some("live".to_string()),
    };
    let json = code_symbols(&params, &registered_for(&dir), None).unwrap();
    let response: CodeSymbolSearchResponse = serde_json::from_str(&json).unwrap();
    assert_eq!(response.status, "ok");
    assert_eq!(response.mode, "live");

    let method = response
        .items
        .iter()
        .find(|it| it.kind == "impl_method")
        .expect("impl_method record present");
    assert_eq!(method.symbol_kind.as_deref(), Some("function_item"));
    assert_eq!(method.parent_kind.as_deref(), Some("impl_item"));
    assert_eq!(method.name.as_deref(), Some("run"));
}

/// CN-T2 fix: invalid `mode` returns a typed `CodeNavErrorResponse`
/// (status="error", code="invalid_code_symbols_mode") — never an
/// anyhow bail. Agents already know how to parse the typed shape from
/// every other recoverable error.
#[test]
fn code_symbols_invalid_mode_returns_typed_error_response() {
    let dir = TempDir::new().unwrap();
    let params = CodeSymbolSearchParams {
        project_dir: dir.path().to_string_lossy().into_owned(),
        query: None,
        languages: None,
        item_kinds: None,
        path_contains: None,
        limit: None,
        file_limit: None,
        include_attributes: None,
        mode: Some("nope".to_string()),
    };
    let json = code_symbols(&params, &registered_for(&dir), None).unwrap();
    let response: CodeNavErrorResponse = serde_json::from_str(&json).unwrap();
    assert_eq!(response.status, "error");
    assert_eq!(response.code, "invalid_code_symbols_mode");
    assert_eq!(response.semantic_status, SEMANTIC_STATUS_SYNTAX_ONLY);
    assert!(response.message.contains("nope"));
    assert!(response.suggestion.contains("indexed"));
    assert!(response.suggestion.contains("live"));
}

/// CN-T2 fix: live lane stats each candidate file and skips ones over
/// MAX_CODE_NAV_FILE_BYTES, surfacing the skip as a typed per-file
/// error in `errors[]` so the caller knows what was dropped.
#[test]
fn code_symbols_live_skips_oversize_files_with_typed_error() {
    let dir = TempDir::new().unwrap();
    setup_test_file(&dir, "src/small.rs", "fn small() {}\n");

    let mut oversize = String::with_capacity(MAX_CODE_NAV_FILE_BYTES as usize + 1024);
    oversize.push_str("fn huge() {}\n");
    while (oversize.len() as u64) <= MAX_CODE_NAV_FILE_BYTES {
        oversize.push_str("// padding to exceed the code-nav file cap\n");
    }
    setup_test_file(&dir, "src/huge.rs", &oversize);

    let params = CodeSymbolSearchParams {
        project_dir: dir.path().to_string_lossy().into_owned(),
        query: None,
        languages: Some(vec!["rust".to_string()]),
        item_kinds: None,
        path_contains: None,
        limit: None,
        file_limit: None,
        include_attributes: None,
        mode: Some("live".to_string()),
    };
    let json = code_symbols(&params, &registered_for(&dir), None).unwrap();
    let response: CodeSymbolSearchResponse = serde_json::from_str(&json).unwrap();
    assert_eq!(response.status, "ok");
    assert!(
        response
            .errors
            .iter()
            .any(|e| e.file.contains("huge.rs")
                && e.error.contains("file_too_large_for_code_nav")),
        "expected huge.rs in errors[] with file_too_large code, got {:?}",
        response.errors
    );
    assert!(
        response
            .items
            .iter()
            .any(|it| it.file.contains("small.rs") && it.name.as_deref() == Some("small")),
        "expected small.rs symbol present, got {:?}",
        response.items
    );
}

/// CN-T2 fix: indexed kind filter must decompose `"impl_method"`
/// (refactor synthetic) into `symbol_kind=function_item AND
/// parent_kind=impl_item` — otherwise the previous Boolean shape
/// (two identical Should probes on symbol_kind) silently returned
/// zero rows for any synthetic-kind filter. Lock the decomposition
/// contract.
#[test]
fn indexed_kind_filter_decomposes_impl_method() {
    let fields = make_test_field_handles();
    let raw_clauses = indexed_kind_filter_for(fields, "function_item");
    assert_eq!(raw_clauses.len(), 1, "raw kind => single probe");

    let synth_clauses = indexed_kind_filter_for(fields, "impl_method");
    assert_eq!(
        synth_clauses.len(),
        2,
        "synthetic kind => raw probe + decomposition (got {})",
        synth_clauses.len()
    );

    let unknown_clauses = indexed_kind_filter_for(fields, "some_future_synthetic");
    assert_eq!(unknown_clauses.len(), 1);
}

/// CN-T2 fix (round 2): end-to-end behaviour assertion. Build a real
/// tantivy index with two project_file docs (one Rust impl method,
/// one top-level Rust fn), then run both `item_kinds=["impl_method"]`
/// and `item_kinds=["function_item"]` queries against the indexed
/// lane. The impl-method query must return only the impl-method doc;
/// the function_item query must return BOTH. Locks the
/// dual-vocabulary contract from CodeSymbolSearchParams::item_kinds
/// docs.
#[test]
fn indexed_lane_item_kinds_matches_both_synthetic_and_raw_for_rust_impl_method() {
    use crate::index::TranscriptIndex;

    let dir = TempDir::new().unwrap();
    let index_path = dir.path().join("index");
    let index = TranscriptIndex::open_or_create(
        &index_path,
        Vec::new(),
        None,
        dir.path().join("projects.json"),
        dir.path().join("knowledge.json"),
        dir.path().join("threads.json"),
        dir.path().join("roadmap.json"),
    )
    .unwrap();
    let project = ProjectRecord {
        project_id: "proj-impl-test".into(),
        repo_id: None,
        canonical_path: dir
            .path()
            .canonicalize()
            .unwrap()
            .to_string_lossy()
            .into_owned(),
        registered_at: "2026-01-01T00:00:00Z".into(),
        is_git_repo: false,
        languages: BTreeSet::new(),
    };

    let method_chunk = crate::chunker::Chunk {
        project_id: "proj-impl-test".into(),
        file_path: std::path::PathBuf::from("src/lib.rs"),
        rel_path_hash: "h1".into(),
        chunk_kind: "code_block".into(),
        chunk_hash: "a".repeat(64),
        occurrence_idx: 0,
        language: Some("rust".into()),
        symbol: Some("S::run".into()),
        symbol_exact: Some("run".into()),
        symbol_kind: Some("function_item".into()),
        parent_kind: Some("impl_item".into()),
        line_start: Some(3),
        line_end: Some(3),
        content: "fn run(&self) {}".into(),
        byte_start: 19,
        byte_end: 35,
    };
    let toplevel_chunk = crate::chunker::Chunk {
        project_id: "proj-impl-test".into(),
        file_path: std::path::PathBuf::from("src/lib.rs"),
        rel_path_hash: "h1".into(),
        chunk_kind: "code_block".into(),
        chunk_hash: "b".repeat(64),
        occurrence_idx: 1,
        language: Some("rust".into()),
        symbol: Some("top".into()),
        symbol_exact: Some("top".into()),
        symbol_kind: Some("function_item".into()),
        parent_kind: None,
        line_start: Some(7),
        line_end: Some(7),
        content: "fn top() {}".into(),
        byte_start: 50,
        byte_end: 61,
    };

    let mut writer = index.index_handle().writer(50_000_000).unwrap();
    let abs_path = dir.path().join("src/lib.rs");
    writer
        .add_document(crate::index::project_files::build_project_file_doc(
            &method_chunk,
            &project,
            &abs_path,
            None,
            index.field_handles(),
        ))
        .unwrap();
    writer
        .add_document(crate::index::project_files::build_project_file_doc(
            &toplevel_chunk,
            &project,
            &abs_path,
            None,
            index.field_handles(),
        ))
        .unwrap();
    writer.commit().unwrap();
    drop(writer);
    // The shared reader is OnCommit; nudging here is enough.
    index.reader_reload_for_test();

    let registered = vec![project.clone()];
    let project_dir = project.canonical_path.clone();

    // Query 1: synthetic kind impl_method → only the method record.
    let impl_method_json = code_symbols(
        &CodeSymbolSearchParams {
            project_dir: project_dir.clone(),
            query: None,
            languages: None,
            item_kinds: Some(vec!["impl_method".to_string()]),
            path_contains: None,
            limit: None,
            file_limit: None,
            include_attributes: None,
            mode: Some("indexed".to_string()),
        },
        &registered,
        Some(&index),
    )
    .unwrap();
    let impl_method_response: CodeSymbolSearchResponse =
        serde_json::from_str(&impl_method_json).unwrap();
    assert_eq!(impl_method_response.status, "ok");
    assert_eq!(impl_method_response.mode, "indexed");
    let names: Vec<&str> = impl_method_response
        .items
        .iter()
        .filter_map(|it| it.name.as_deref())
        .collect();
    assert_eq!(
        names,
        vec!["run"],
        "impl_method query must return only the impl method; got {:?}",
        names
    );
    let method_item = &impl_method_response.items[0];
    assert_eq!(method_item.kind, "impl_method", "refactor kind synthesised");
    assert_eq!(method_item.symbol_kind.as_deref(), Some("function_item"));
    assert_eq!(method_item.parent_kind.as_deref(), Some("impl_item"));

    // Query 2: raw kind function_item → both records.
    let function_json = code_symbols(
        &CodeSymbolSearchParams {
            project_dir,
            query: None,
            languages: None,
            item_kinds: Some(vec!["function_item".to_string()]),
            path_contains: None,
            limit: None,
            file_limit: None,
            include_attributes: None,
            mode: Some("indexed".to_string()),
        },
        &registered,
        Some(&index),
    )
    .unwrap();
    let function_response: CodeSymbolSearchResponse =
        serde_json::from_str(&function_json).unwrap();
    let names: std::collections::HashSet<&str> = function_response
        .items
        .iter()
        .filter_map(|it| it.name.as_deref())
        .collect();
    assert_eq!(
        names,
        ["run", "top"].iter().copied().collect(),
        "function_item query must return both records; got {:?}",
        names
    );
}

fn make_test_field_handles() -> crate::index::FieldHandles {
    let (_schema, fields) = crate::index::build_schema();
    fields
}

// ── CN-T1: bbox_code_refs ────────────────────────────────────────────

/// Rust calls: simple function-call captures with containing_symbol
/// resolution to the enclosing fn.
#[test]
fn code_refs_rust_calls_resolve_containing_symbol() {
    let dir = TempDir::new().unwrap();
    let file = setup_test_file(
        &dir,
        "src/lib.rs",
        "fn outer() { println!(\"hi\"); helper(); }\nfn helper() {}\n",
    );
    let json = code_refs(&CodeRefsParams {
        file,
        project_dir: None,
        kind: "calls".to_string(),
        query: None,
        limit: None,
        include_text: None,
    })
    .unwrap();
    let response: CodeRefsResponse = serde_json::from_str(&json).unwrap();
    assert_eq!(response.status, "ok");
    assert_eq!(response.language, "rust");
    assert_eq!(response.kind_filter, "calls");
    assert_eq!(response.semantic_status, SEMANTIC_STATUS_SYNTAX_ONLY);
    assert!(response.refs.iter().all(|r| r.edge_confidence == "heuristic"));
    let names: std::collections::HashSet<&str> =
        response.refs.iter().map(|r| r.name.as_str()).collect();
    assert!(
        names.contains("helper"),
        "expected `helper` call; got {:?}",
        names
    );
    let helper_ref = response
        .refs
        .iter()
        .find(|r| r.name == "helper")
        .expect("helper ref present");
    assert_eq!(helper_ref.kind, "call");
    assert_eq!(
        helper_ref.containing_symbol.as_deref(),
        Some("outer"),
        "containing_symbol must resolve to the enclosing fn"
    );
}

/// Java imports: the curated query emits one `@import` per
/// import_declaration node.
#[test]
fn code_refs_java_imports() {
    let dir = TempDir::new().unwrap();
    let file = setup_test_file(
        &dir,
        "src/Foo.java",
        "package x;\nimport java.util.List;\nimport java.util.Map;\nclass Foo {}\n",
    );
    let json = code_refs(&CodeRefsParams {
        file,
        project_dir: None,
        kind: "imports".to_string(),
        query: None,
        limit: None,
        include_text: Some(true),
    })
    .unwrap();
    let response: CodeRefsResponse = serde_json::from_str(&json).unwrap();
    assert_eq!(response.status, "ok");
    assert_eq!(response.language, "java");
    assert_eq!(response.matching_refs, 2);
    assert!(response.refs.iter().all(|r| r.kind == "import"));
}

/// Python identifiers: filter by query substring narrows the
/// notoriously-large identifier set to a single symbol.
#[test]
fn code_refs_python_identifiers_with_query_filter() {
    let dir = TempDir::new().unwrap();
    let file = setup_test_file(
        &dir,
        "src/m.py",
        "def alpha():\n    return beta() + gamma\n\ndef beta():\n    return 1\n",
    );
    let json = code_refs(&CodeRefsParams {
        file,
        project_dir: None,
        kind: "identifiers".to_string(),
        query: Some("beta".to_string()),
        limit: None,
        include_text: None,
    })
    .unwrap();
    let response: CodeRefsResponse = serde_json::from_str(&json).unwrap();
    assert_eq!(response.status, "ok");
    assert_eq!(response.language, "python");
    assert!(response.refs.iter().all(|r| r.name.contains("beta")));
    assert!(response.matching_refs >= 2, "expected at least 2 beta identifiers");
}

/// Unsupported language + non-identifier kind returns a typed
/// error response with code `unsupported_language_for_code_refs`.
/// Same kind="identifiers" on that language falls back to the
/// generic walker.
#[test]
fn code_refs_unsupported_language_typed_error_for_non_identifier_kind() {
    let dir = TempDir::new().unwrap();
    let file = setup_test_file(&dir, "x.erl", "foo() -> ok.\n");

    let calls_json = code_refs(&CodeRefsParams {
        file: file.clone(),
        project_dir: None,
        kind: "calls".to_string(),
        query: None,
        limit: None,
        include_text: None,
    })
    .unwrap();
    let calls_err: CodeNavErrorResponse = serde_json::from_str(&calls_json).unwrap();
    assert_eq!(calls_err.status, "error");
    assert_eq!(calls_err.code, "unsupported_language_for_code_refs");
    assert!(calls_err.suggestion.contains("identifiers"));

    let ids_json = code_refs(&CodeRefsParams {
        file,
        project_dir: None,
        kind: "identifiers".to_string(),
        query: None,
        limit: None,
        include_text: None,
    })
    .unwrap();
    let ids_response: CodeRefsResponse = serde_json::from_str(&ids_json).unwrap();
    assert_eq!(ids_response.status, "ok", "identifiers fallback must work");
    // The generic walker emits records for nodes literally named
    // `"identifier"`. Erlang uses different node kinds (`atom`,
    // `variable`, ...), so the count here may be 0 — that's an
    // honest limitation of the fallback. The test asserts the
    // shape, not the count.
    assert_eq!(ids_response.kind_filter, "identifiers");
    assert_eq!(ids_response.semantic_status, SEMANTIC_STATUS_SYNTAX_ONLY);
}

/// CN-S1 file-size gate must apply to code_refs the same way it
/// applies to code_query / code_node_describe / code_symbols.
#[test]
fn code_refs_rejects_oversize_files_with_typed_error() {
    let dir = TempDir::new().unwrap();
    let mut oversize = String::with_capacity(MAX_CODE_NAV_FILE_BYTES as usize + 1024);
    oversize.push_str("fn main() {}\n");
    while (oversize.len() as u64) <= MAX_CODE_NAV_FILE_BYTES {
        oversize.push_str("// padding line\n");
    }
    let file = setup_test_file(&dir, "huge.rs", &oversize);

    let json = code_refs(&CodeRefsParams {
        file,
        project_dir: None,
        kind: "calls".to_string(),
        query: None,
        limit: None,
        include_text: None,
    })
    .unwrap();
    let err: CodeNavErrorResponse = serde_json::from_str(&json).unwrap();
    assert_eq!(err.status, "error");
    assert_eq!(err.code, "file_too_large_for_code_nav");
    assert!(err.file_bytes.unwrap() > MAX_CODE_NAV_FILE_BYTES);
}

/// `kind="all"` returns calls + imports + fields + identifiers
/// merged into one record list, with stable byte-order sorting.
#[test]
fn code_refs_all_kind_returns_union_in_byte_order() {
    let dir = TempDir::new().unwrap();
    let file = setup_test_file(
        &dir,
        "src/lib.rs",
        "use std::fs;\nfn main() { let x = 1; fs::read(\"p\"); x.field; }\n",
    );
    let json = code_refs(&CodeRefsParams {
        file,
        project_dir: None,
        kind: "all".to_string(),
        query: None,
        limit: None,
        include_text: None,
    })
    .unwrap();
    let response: CodeRefsResponse = serde_json::from_str(&json).unwrap();
    let kinds: std::collections::HashSet<&str> =
        response.refs.iter().map(|r| r.kind.as_str()).collect();
    assert!(kinds.contains("identifier"), "expected identifier refs in 'all'");
    assert!(
        kinds.contains("call"),
        "expected call refs in 'all'; got {:?}",
        kinds
    );
    // Stable byte-order: each record's byte_start must be >= prev.
    for window in response.refs.windows(2) {
        assert!(
            window[0].byte_range.0 <= window[1].byte_range.0,
            "refs must be sorted by byte_start; got {:?}",
            response.refs.iter().map(|r| r.byte_range).collect::<Vec<_>>()
        );
    }
}

/// CN-T1 closing fix: invalid `kind` on bbox_code_refs returns a
/// typed CodeNavErrorResponse (code="invalid_code_refs_kind"),
/// matching the shape used by invalid_code_symbols_mode and the
/// other recoverable errors. Was anyhow-bailing before.
#[test]
fn code_refs_invalid_kind_returns_typed_error_response() {
    let dir = TempDir::new().unwrap();
    let file = setup_test_file(&dir, "src/lib.rs", "fn main() {}\n");
    let json = code_refs(&CodeRefsParams {
        file,
        project_dir: None,
        kind: "nope".to_string(),
        query: None,
        limit: None,
        include_text: None,
    })
    .unwrap();
    let err: CodeNavErrorResponse = serde_json::from_str(&json).unwrap();
    assert_eq!(err.status, "error");
    assert_eq!(err.code, "invalid_code_refs_kind");
    assert_eq!(err.semantic_status, SEMANTIC_STATUS_SYNTAX_ONLY);
    assert!(err.message.contains("nope"));
    assert!(err.suggestion.contains("calls"));
    assert!(err.suggestion.contains("identifiers"));
}

/// CN-T1 closing fix: every bbox_code_refs record carries a
/// `handoff` block with pre-filled bbox_refactor_status and
/// bbox_refactor_project_refs argument shapes. The CN-X2 audit
/// claims "handoff blocks on every code-nav response" — this test
/// locks the claim for code_refs.
#[test]
fn code_refs_records_carry_handoff_with_pre_filled_args() {
    let dir = TempDir::new().unwrap();
    let file = setup_test_file(
        &dir,
        "src/lib.rs",
        "fn outer() { helper(); }\nfn helper() {}\n",
    );
    let json = code_refs(&CodeRefsParams {
        file,
        project_dir: None,
        kind: "calls".to_string(),
        query: None,
        limit: None,
        include_text: None,
    })
    .unwrap();
    let response: CodeRefsResponse = serde_json::from_str(&json).unwrap();
    let helper_ref = response
        .refs
        .iter()
        .find(|r| r.name == "helper")
        .expect("helper ref present");
    // handoff carries nearest_refactor_item describing the ref itself.
    let near = helper_ref
        .handoff
        .nearest_refactor_item
        .as_ref()
        .expect("nearest_refactor_item present");
    assert_eq!(near.name.as_deref(), Some("helper"));
    // refactor_status pre-filled with the containing symbol when
    // available, falling back to the ref name; item_kinds empty
    // because we can't infer the defining kind from a ref site.
    let status_hint = helper_ref
        .handoff
        .refactor_status
        .as_ref()
        .expect("refactor_status present");
    assert_eq!(status_hint.tool, "bbox_refactor_status");
    assert_eq!(status_hint.arguments.item_names, vec!["outer"]); // containing symbol
    assert!(status_hint.arguments.item_kinds.is_empty());
    // project_refs pre-filled with the same query name so callers
    // can ground via project_file entity refs.
    assert_eq!(
        helper_ref.handoff.project_refs.tool,
        "bbox_refactor_project_refs"
    );
    assert_eq!(
        helper_ref.handoff.project_refs.arguments.query.as_deref(),
        Some("outer")
    );
}

/// `limit` truncates the returned set; `matching_refs` reflects the
/// pre-truncation total; `truncation_reason="limit_reached"` fires.
#[test]
fn code_refs_truncation_reports_limit_reached() {
    let dir = TempDir::new().unwrap();
    let file = setup_test_file(
        &dir,
        "src/lib.rs",
        "fn main() { a; b; c; d; e; }\n",
    );
    let json = code_refs(&CodeRefsParams {
        file,
        project_dir: None,
        kind: "identifiers".to_string(),
        query: None,
        limit: Some(2),
        include_text: None,
    })
    .unwrap();
    let response: CodeRefsResponse = serde_json::from_str(&json).unwrap();
    assert_eq!(response.returned_refs, 2);
    assert!(response.matching_refs > response.returned_refs);
    assert!(response.truncated);
    assert_eq!(response.truncation_reason.as_deref(), Some("limit_reached"));
}

/// CN-T2: when no index is provided, the dispatcher must default to
/// mode="live" rather than failing. Tests historically don't pass an
/// index; this guards that contract.
#[test]
fn code_symbols_dispatch_defaults_to_live_when_no_index() {
    let dir = TempDir::new().unwrap();
    setup_test_file(&dir, "src/lib.rs", "fn main() {}\n");
    let params = CodeSymbolSearchParams {
        project_dir: dir.path().to_string_lossy().into_owned(),
        query: None,
        languages: None,
        item_kinds: None,
        path_contains: None,
        limit: None,
        file_limit: None,
        include_attributes: None,
        mode: None, // unset
    };
    let json = code_symbols(&params, &registered_for(&dir), None).unwrap();
    let response: CodeSymbolSearchResponse = serde_json::from_str(&json).unwrap();
    assert_eq!(response.mode, "live");
}

/// `bbox_code_symbols` must refuse a `project_dir` that is neither a
/// registered project root nor a descendant of one. The error response
/// must be a typed JSON object (not an anyhow bail) so the agent can
/// recover programmatically.
#[test]
fn test_code_symbols_rejects_unregistered_project_dir() {
    let dir = TempDir::new().unwrap();
    setup_test_file(&dir, "src/main/java/Test.java", "class Test { void run() {} }\n");
    let params = CodeSymbolSearchParams {
        project_dir: dir.path().to_string_lossy().into_owned(),
        query: None,
        languages: None,
        item_kinds: None,
        path_contains: None,
        limit: None,
        file_limit: None,
        include_attributes: None,
        mode: None,
    };
    // Empty registry — dir is not registered nor a descendant.
    let response_json = code_symbols(&params, &[], None).unwrap();
    let response: CodeNavErrorResponse = serde_json::from_str(&response_json).unwrap();
    assert_eq!(response.status, "error");
    assert_eq!(response.code, "project_not_registered");
    assert_eq!(response.semantic_status, SEMANTIC_STATUS_SYNTAX_ONLY);
    assert!(response.project_dir.is_some());
    assert!(response.suggestion.contains("bbox_project_register"));
}

/// A descendant of a registered project root is accepted (worktrees,
/// subdirectories, etc.). The walker still runs and returns results.
#[test]
fn test_code_symbols_accepts_descendant_of_registered_root() {
    let dir = TempDir::new().unwrap();
    setup_test_file(&dir, "src/main/java/Test.java", "class Test { void run() {} }\n");
    let params = CodeSymbolSearchParams {
        project_dir: dir
            .path()
            .join("src")
            .canonicalize()
            .unwrap()
            .to_string_lossy()
            .into_owned(),
        query: None,
        languages: Some(vec!["java".to_string()]),
        item_kinds: None,
        path_contains: None,
        limit: None,
        file_limit: None,
        include_attributes: None,
        mode: None,
    };
    let response_json = code_symbols(&params, &registered_for(&dir), None).unwrap();
    let response: CodeSymbolSearchResponse = serde_json::from_str(&response_json).unwrap();
    assert_eq!(response.status, "ok");
    assert!(response.matching_items >= 1);
}

/// Files larger than MAX_CODE_NAV_FILE_BYTES must be rejected by every
/// single-file code-nav tool with a typed JSON error that names the
/// observed size and the cap. The agent should see the same shape from
/// `bbox_code_query` and `bbox_code_node_describe`.
#[test]
fn test_code_nav_single_file_tools_reject_oversize() {
    let dir = TempDir::new().unwrap();
    // Build a file just over the cap with valid Rust so the gate
    // (not the parser) is what rejects.
    let mut oversize = String::with_capacity(MAX_CODE_NAV_FILE_BYTES as usize + 1024);
    oversize.push_str("fn main() {}\n");
    while (oversize.len() as u64) <= MAX_CODE_NAV_FILE_BYTES {
        oversize.push_str("// padding line to exceed the code-nav file-size cap\n");
    }
    let file = setup_test_file(&dir, "huge.rs", &oversize);

    let query_response_json = code_query(&CodeQueryParams {
        file: file.clone(),
        query: "(function_item) @f".to_string(),
        project_dir: None,
        language: None,
        limit: None,
        include_text: None,
    })
    .unwrap();
    let query_error: CodeNavErrorResponse = serde_json::from_str(&query_response_json).unwrap();
    assert_eq!(query_error.status, "error");
    assert_eq!(query_error.code, "file_too_large_for_code_nav");
    assert!(query_error.file_bytes.unwrap() > MAX_CODE_NAV_FILE_BYTES);
    assert_eq!(query_error.max_bytes, Some(MAX_CODE_NAV_FILE_BYTES));

    let describe_response_json = code_node_describe(&CodeNodeDescribeParams {
        file,
        line: 1,
        column: 4,
        project_dir: None,
        include_siblings: None,
        include_text: None,
    })
    .unwrap();
    let describe_error: CodeNavErrorResponse =
        serde_json::from_str(&describe_response_json).unwrap();
    assert_eq!(describe_error.status, "error");
    assert_eq!(describe_error.code, "file_too_large_for_code_nav");
    assert_eq!(describe_error.semantic_status, SEMANTIC_STATUS_SYNTAX_ONLY);
}
