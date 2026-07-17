use super::*;
use crate::projects::ProjectRecord;
use std::collections::BTreeSet;
use tempfile::TempDir;

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
        aliases: Default::default(),
        ..Default::default()
    };

    let method_chunk = bbox_chunker::Chunk {
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
        visual_payload: None,
    };
    let toplevel_chunk = bbox_chunker::Chunk {
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
        visual_payload: None,
    };

    let mut writer = index.index_handle().writer(50_000_000).unwrap();
    let abs_path = dir.path().join("src/lib.rs");
    writer
        .add_document(crate::index::project_files::build_project_file_doc(
            &method_chunk,
            &project,
            &abs_path,
            None,
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
            None,
            index.field_handles(),
        ))
        .unwrap();
    writer.commit().unwrap();
    drop(writer);
    index.reader_reload_for_test();

    let registered = vec![project.clone()];
    let project_dir = project.canonical_path.clone();

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
    let function_response: CodeSymbolSearchResponse = serde_json::from_str(&function_json).unwrap();
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
