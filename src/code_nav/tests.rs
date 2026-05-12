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
    };
    let response_json = code_symbols(&params, &registered_for(&dir)).unwrap();
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
        },
        &registered_for(&dir),
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
    };
    // Empty registry — dir is not registered nor a descendant.
    let response_json = code_symbols(&params, &[]).unwrap();
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
    };
    let response_json = code_symbols(&params, &registered_for(&dir)).unwrap();
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
