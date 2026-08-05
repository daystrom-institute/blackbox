//! Phase 3 milestone P3-E gate: the path-free schema cut.
//!
//! Covers the document-side rows of the plan section 9 gate: the no-host-path
//! sweep over every code-plane document kind, the `source_uri` response round
//! trip, the `display_path` fallback order, `FileMeta` composite rekeying and
//! its version marker, the fail-closed replacement boundary, and the bridge
//! spill lane including its crash lifecycle.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use bbox_chunker::Chunk;
use bbox_corpus_core::project_record::ProjectRecord;
use bbox_corpus_index::index::history_generations::HistoryCommitDocumentV1;
use bbox_corpus_index::index::schema_replacement::{
    CommitDocumentOwnerV1, CommitSpillFileV1, CommitSpillNamespaceV1,
    SchemaReplacementAuthorization, SchemaReplacementGuard, build_commit_doc_from_row,
    read_commit_spill, write_commit_spill,
};
use bbox_corpus_index::index::{
    FieldHandles, FileMeta, FileMetaSource, INDEX_SCHEMA_VERSION, TranscriptIndex, build_schema,
    first_text, git_history, optional_text, project_files, render_display_path,
    render_selected_attachment_display_path,
};
use tantivy::TantivyDocument;
use tantivy::schema::Schema;

/// A host root no path-free document may ever mention. Distinctive enough that
/// a grep-shaped assertion cannot pass by accident.
const FIXTURE_HOST_ROOT: &str = "/host-checkouts/p3e-fixture-root";

fn authorizing_guard() -> SchemaReplacementGuard {
    Arc::new(|_request| Ok(SchemaReplacementAuthorization::new("p3e-test-guard")))
}

fn open_index(root: &Path, guard: Option<SchemaReplacementGuard>) -> TranscriptIndex {
    TranscriptIndex::open_or_create_guarded(
        &root.join("index"),
        Vec::new(),
        None,
        root.join("projects.json"),
        root.join("knowledge.json"),
        root.join("threads.json"),
        root.join("roadmap.json"),
        std::sync::Arc::new(bbox_corpus_index::index::StaticProjectRecordsProvider::empty()),
        guard,
    )
    .expect("index opens")
}

fn chunk(relative_path: &str) -> Chunk {
    Chunk {
        project_id: "proj1234".into(),
        file_path: PathBuf::from(relative_path),
        rel_path_hash: "abcd1234".into(),
        chunk_kind: "code_block".into(),
        chunk_hash: "f".repeat(64),
        occurrence_idx: 0,
        language: Some("rust".into()),
        symbol: Some("Helper".into()),
        symbol_exact: Some("crate::Helper".into()),
        symbol_kind: Some("struct_item".into()),
        parent_kind: None,
        line_start: Some(1),
        line_end: Some(3),
        content: "pub struct Helper;".into(),
        byte_start: 0,
        byte_end: 18,
        visual_payload: None,
    }
}

fn bridge_record() -> ProjectRecord {
    ProjectRecord {
        project_id: "proj1234".into(),
        repo_id: Some("repo1234".into()),
        // Deliberately the fixture host root: the record still carries it (the
        // v1 compatibility view is path-bearing by construction), and the point
        // of the sweep is that no DOCUMENT picks it up.
        canonical_path: FIXTURE_HOST_ROOT.into(),
        registered_at: "2026-07-25T00:00:00Z".into(),
        is_git_repo: true,
        languages: Default::default(),
        aliases: ["acme-service".to_string()].into_iter().collect(),
    }
}

fn commit_row(namespace: &str, sha: &str) -> HistoryCommitDocumentV1 {
    HistoryCommitDocumentV1 {
        entity_id: format!("commit:{namespace}:{sha}"),
        doc_type: "commit".into(),
        chunk_kind: "git_message".into(),
        repo_id: namespace.into(),
        commit_sha: sha.into(),
        content: format!("carried subject {sha}"),
        content_hash: format!("{}{}", "0".repeat(56), &sha[..8]),
        path_tokens: format!("carried subject {sha}"),
        parser_version: "pv1".into(),
        commit_author_name: "Author".into(),
        commit_author_email: "author@example.test".into(),
        session_id: String::new(),
        account: "git".into(),
        role: "commit".into(),
        byte_offset: 0,
        is_subagent: 0,
    }
}

/// Every stored text value on a document, as `(field name, value)` pairs. The
/// sweep needs the whole stored surface, not a hand-listed subset: a future
/// field that reintroduced a host path would otherwise pass unnoticed.
fn stored_text(schema: &Schema, doc: &TantivyDocument) -> Vec<(String, String)> {
    let mut values = Vec::new();
    for (field, entry) in schema.fields() {
        for value in doc.get_all(field) {
            if let tantivy::schema::OwnedValue::Str(text) = value {
                values.push((entry.name().to_string(), text.clone()));
            }
        }
    }
    values
}

fn assert_no_host_root(schema: &Schema, doc: &TantivyDocument, label: &str) {
    for (name, value) in stored_text(schema, doc) {
        assert!(
            !value.contains(FIXTURE_HOST_ROOT),
            "{label} document leaks the fixture host root in `{name}`: {value}"
        );
        assert!(
            !value.starts_with('/'),
            "{label} document stores an absolute path in `{name}`: {value}"
        );
    }
}

// ---------------------------------------------------------------------------
// No-host-path sweep
// ---------------------------------------------------------------------------

#[test]
fn project_file_documents_store_no_host_path_on_either_source_lane() {
    let (schema, fields) = build_schema();
    let chunk = chunk("src/helper.rs");

    let local = project_files::build_project_file_doc_for_source(
        &chunk,
        "proj1234",
        Some("repo1234"),
        "src/helper.rs",
        "acme-service",
        None,
        Some("head-repo1234-0123456789ab"),
        "local:proj1234",
        "local",
        "entry-key",
        fields,
    );
    let collected = project_files::build_project_file_doc_for_source(
        &chunk,
        "proj1234",
        Some("repo1234"),
        "src/helper.rs",
        "acme-service",
        None,
        Some("collected-0123456789abcdef"),
        "collected:proj1234:gen",
        "gen",
        "entry-key",
        fields,
    );
    assert_no_host_root(&schema, &local, "local project_file");
    assert_no_host_root(&schema, &collected, "collected project_file");
}

#[test]
fn the_bridge_local_reindex_lane_stores_no_host_path() {
    let (schema, fields) = build_schema();
    let record = bridge_record();
    let doc = project_files::build_project_file_doc(
        &chunk("src/helper.rs"),
        &record,
        // The bridge fallback from P3-A item 1: first alias, else project id.
        record.aliases.iter().next().unwrap(),
        None,
        None,
        fields,
    );
    assert_no_host_root(&schema, &doc, "bridge local project_file");
    assert_eq!(first_text(&doc, fields.project), "acme-service");
    assert_eq!(first_text(&doc, fields.file_path), "src/helper.rs");
    assert_eq!(first_text(&doc, fields.relative_path), "src/helper.rs");
}

#[test]
fn commit_documents_store_no_host_path_and_keep_namespace_identity() {
    let (schema, fields) = build_schema();
    let commit = bbox_corpus_core::git::GitCommit {
        sha: "a".repeat(40),
        parent_shas: vec!["b".repeat(40)],
        author_name: "Author".into(),
        author_email: "author@example.test".into(),
        message: "P3-E: cut the project-file schema".into(),
    };
    let doc =
        git_history::build_commit_doc(&commit, "repo1234", "proj1234", "acme-service", fields);
    // `file_path` on a commit document is the `git:<project_id>` source key,
    // which is not a filesystem path; the sweep's absolute-path rule holds.
    assert_no_host_root(&schema, &doc, "commit");
    assert_eq!(first_text(&doc, fields.project), "acme-service");
    assert_eq!(first_text(&doc, fields.file_path), "git:proj1234");
    assert_eq!(first_text(&doc, fields.repo_id), "repo1234");
    assert_eq!(first_text(&doc, fields.commit_sha), commit.sha);
}

#[test]
fn reemitted_commit_documents_store_no_host_path_and_are_byte_stable() {
    let (schema, fields) = build_schema();
    let row = commit_row("repo1234", "abcdef0123456789");
    let owner = CommitDocumentOwnerV1 {
        project_id: Some("proj1234".into()),
        project_display: "acme-service".into(),
    };
    let doc = build_commit_doc_from_row(&row, &owner, fields);
    assert_no_host_root(&schema, &doc, "re-emitted commit");
    // Identity and content hash are reproduced byte-for-byte: this is what
    // lets commit vectors ride through a replacement untouched.
    assert_eq!(first_text(&doc, fields.entity_id), row.entity_id);
    assert_eq!(first_text(&doc, fields.repo_id), row.repo_id);
    assert_eq!(first_text(&doc, fields.commit_sha), row.commit_sha);
    assert_eq!(first_text(&doc, fields.chunk_hash), row.content_hash);
    assert_eq!(first_text(&doc, fields.content), row.content);
}

#[test]
fn an_unclaimed_namespace_reemits_without_a_project_path_lane() {
    let (_schema, fields) = build_schema();
    let row = commit_row("orphan-namespace", "abcdef0123456789");
    let doc = build_commit_doc_from_row(
        &row,
        &CommitDocumentOwnerV1::unclaimed("orphan-namespace"),
        fields,
    );
    assert_eq!(first_text(&doc, fields.project), "orphan-namespace");
    assert_eq!(
        optional_text(&doc, fields.file_path),
        None,
        "an unclaimed namespace has no owning project, therefore no git: source key"
    );
}

// ---------------------------------------------------------------------------
// source_uri round trips and display_path
// ---------------------------------------------------------------------------

#[test]
fn source_uri_round_trips_from_the_stored_field() {
    let (_schema, fields) = build_schema();
    for relative_path in [
        "src/helper.rs",
        "docs/a b c.md",
        "docs/100%25.md",
        "docs/what#is?this.md",
        "docs/ünïcode/файл.md",
    ] {
        let doc = project_files::build_project_file_doc_for_source(
            &chunk(relative_path),
            "proj1234",
            None,
            relative_path,
            "acme-service",
            None,
            None,
            "local:proj1234",
            "local",
            "entry-key",
            fields,
        );
        let stored = first_text(&doc, fields.source_uri);
        assert!(
            !stored.is_empty(),
            "no source_uri stored for {relative_path}"
        );
        let (project_id, decoded) =
            bbox_code_source::decode_source_uri(&stored).expect("stored source_uri decodes");
        assert_eq!(project_id, "proj1234");
        assert_eq!(decoded, relative_path);
        assert_eq!(first_text(&doc, fields.relative_path), relative_path);
    }
}

#[test]
fn display_path_tier_three_is_the_display_name_plus_the_relative_path() {
    let properties = BTreeMap::from([
        ("project".to_string(), "acme-service".to_string()),
        ("relative_path".to_string(), "src/helper.rs".to_string()),
    ]);
    assert_eq!(
        render_display_path(&properties).as_deref(),
        Some("acme-service/src/helper.rs")
    );
}

#[test]
fn display_path_is_absent_without_a_relative_path() {
    // Tier 1 (session workspace mapping) does not exist this phase and is
    // deliberately not faked from anything else.
    let properties = BTreeMap::from([("project".to_string(), "acme-service".to_string())]);
    assert_eq!(render_display_path(&properties), None);
}

#[test]
fn display_path_tier_two_joins_a_selected_attachment_without_opening_it() {
    let rendered =
        render_selected_attachment_display_path(Path::new(FIXTURE_HOST_ROOT), "src/helper.rs");
    assert_eq!(rendered, format!("{FIXTURE_HOST_ROOT}/src/helper.rs"));
    // The root does not exist on disk. Tier 2 is rendered, never opened, so a
    // detached or unavailable checkout cannot make it fail.
    assert!(!Path::new(FIXTURE_HOST_ROOT).exists());
}

// ---------------------------------------------------------------------------
// FileMeta composite rekey and version marker
// ---------------------------------------------------------------------------

#[test]
fn project_file_meta_keys_round_trip_and_stay_disjoint_from_other_lanes() {
    let key = bbox_code_source::project_file_meta_key(
        "proj1234",
        bbox_code_source::SOURCE_KIND_LOCAL,
        "src/helper.rs",
    );
    assert_eq!(
        bbox_code_source::parse_project_file_meta_key(&key),
        Some(("proj1234", "local", "src/helper.rs"))
    );
    // The lanes that legitimately key by an absolute path or by the history
    // source key must not parse as project rows.
    for foreign in [
        "/host/sessions/claude/abc.jsonl",
        "git:proj1234",
        FIXTURE_HOST_ROOT,
    ] {
        assert_eq!(
            bbox_code_source::parse_project_file_meta_key(foreign),
            None,
            "{foreign} must not parse as a project-file freshness key"
        );
    }
}

#[test]
fn source_kind_is_derived_from_the_selector_not_stored_twice() {
    assert_eq!(
        bbox_code_source::source_kind_for_selector(&bbox_code_source::local_selector("p")),
        "local"
    );
    assert_eq!(
        bbox_code_source::source_kind_for_selector(&bbox_code_source::source_selector("p", "g")),
        "collected"
    );
    assert_eq!(
        bbox_code_source::source_kind_for_selector("something-else"),
        "unknown"
    );
}

#[test]
fn unversioned_freshness_rows_are_discarded_at_load() {
    let dir = tempfile::tempdir().unwrap();
    let meta_path = dir.path().join("_meta.json");

    // The pre-P3-E format: a bare, absolute-keyed map with no version marker.
    // Keeping those rows would be worse than dropping them - their project rows
    // would look like foreign absolute-path rows and be purged BY PATH.
    let legacy = HashMap::from([(
        format!("{FIXTURE_HOST_ROOT}/src/helper.rs"),
        FileMeta {
            mtime: 1,
            size: 2,
            mat_version: Some("old".into()),
            source: FileMetaSource::LegacyFilesystem,
        },
    )]);
    std::fs::write(&meta_path, serde_json::to_string(&legacy).unwrap()).unwrap();
    assert!(
        bbox_corpus_index::index::passes::load_meta(&meta_path)
            .unwrap()
            .is_empty(),
        "unversioned rows must be discarded"
    );

    // A round trip under the current version keeps every row.
    let current = HashMap::from([(
        bbox_code_source::project_file_meta_key("proj1234", "local", "src/helper.rs"),
        FileMeta {
            mtime: 7,
            size: 9,
            mat_version: Some("current".into()),
            source: FileMetaSource::LocalProjectFile {
                project_id: "proj1234".into(),
                selector: "local:proj1234".into(),
                relative_path: "src/helper.rs".into(),
                entry_key: "entry".into(),
            },
        },
    )]);
    bbox_corpus_index::index::passes::save_meta(&meta_path, &current).unwrap();
    assert_eq!(
        bbox_corpus_index::index::passes::load_meta(&meta_path).unwrap(),
        current
    );
}

#[test]
fn a_future_meta_version_is_discarded_rather_than_misread() {
    let dir = tempfile::tempdir().unwrap();
    let meta_path = dir.path().join("_meta.json");
    std::fs::write(
        &meta_path,
        serde_json::json!({ "version": 9_999, "rows": {} }).to_string(),
    )
    .unwrap();
    assert!(
        bbox_corpus_index::index::passes::load_meta(&meta_path)
            .unwrap()
            .is_empty()
    );
}

// ---------------------------------------------------------------------------
// Bridge spill lane: equality and crash lifecycle
// ---------------------------------------------------------------------------

fn spill_for(namespace: &str, shas: &[&str]) -> CommitSpillFileV1 {
    CommitSpillFileV1::new(
        Some("outgoing-schema".into()),
        vec![CommitSpillNamespaceV1 {
            namespace: namespace.to_string(),
            owner: CommitDocumentOwnerV1 {
                project_id: Some("proj1234".into()),
                project_display: "acme-service".into(),
            },
            commit_documents: shas.iter().map(|sha| commit_row(namespace, sha)).collect(),
        }],
    )
}

fn commit_entity_ids(index: &TranscriptIndex, fields: FieldHandles) -> Vec<String> {
    use tantivy::collector::{Count, TopDocs};
    use tantivy::query::TermQuery;
    use tantivy::schema::IndexRecordOption;

    let searcher = index.searcher();
    let query = TermQuery::new(
        tantivy::Term::from_field_text(fields.doc_type, "commit"),
        IndexRecordOption::Basic,
    );
    let count = searcher.search(&query, &Count).unwrap();
    if count == 0 {
        return Vec::new();
    }
    let mut ids = searcher
        .search(&query, &TopDocs::with_limit(count))
        .unwrap()
        .into_iter()
        .map(|(_, address)| {
            let doc: TantivyDocument = searcher.doc(address).unwrap();
            first_text(&doc, fields.entity_id)
        })
        .collect::<Vec<_>>();
    ids.sort();
    ids
}

/// Equality row: the carried population is exactly what the spill promised,
/// with the display value and the source key re-derived from the owner.
#[test]
fn the_spill_lane_carries_the_complete_commit_set_across_a_replacement() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let index_path = root.join("index");

    // Stand up an out-of-schema index so the mismatch trigger fires.
    std::fs::create_dir_all(&index_path).unwrap();
    std::fs::write(index_path.join("schema_version.txt"), "old-schema\n").unwrap();
    std::fs::write(index_path.join("stale"), "stale").unwrap();

    let spill = spill_for("repo1234", &["aaaaaaaaaaaaaaaa", "bbbbbbbbbbbbbbbb"]);
    // The real bridge guard writes this before the drop; here the guard just
    // authorizes and the spill is pre-staged, which is the same on-disk state.
    write_commit_spill(&index_path, &spill).unwrap();

    let index = open_index(&root, Some(authorizing_guard()));
    let fields = index.field_handles();
    assert!(!index_path.join("stale").exists(), "the drop happened");
    assert_eq!(
        commit_entity_ids(&index, fields),
        vec![
            "commit:repo1234:aaaaaaaaaaaaaaaa".to_string(),
            "commit:repo1234:bbbbbbbbbbbbbbbb".to_string(),
        ]
    );
    assert!(
        read_commit_spill(&index_path).unwrap().is_none(),
        "the spill is deleted only after the re-add commits, and it did"
    );
}

/// Fault row: crash AFTER the drop, BEFORE the rebuild. There is no schema
/// mismatch left to detect (the marker went with the index), so a
/// mismatch-gated consumer would lose the population. Consumption at every
/// open recovers it.
#[test]
fn a_crash_after_the_drop_recovers_the_complete_commit_set_at_the_next_open() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let index_path = root.join("index");
    let spill = spill_for("repo1234", &["aaaaaaaaaaaaaaaa", "bbbbbbbbbbbbbbbb"]);
    write_commit_spill(&index_path, &spill).unwrap();
    // The index directory does not exist: the drop completed and the process
    // died before anything was rebuilt.
    assert!(!index_path.exists());

    let index = open_index(&root, None);
    assert_eq!(commit_entity_ids(&index, index.field_handles()).len(), 2);
    assert!(read_commit_spill(&index_path).unwrap().is_none());
}

/// Fault row: crash DURING the rebuild, with part of the spill already
/// committed. The re-add is delete-term-then-add per commit entity id, so
/// replaying the whole spill cannot duplicate anything.
#[test]
fn a_crash_during_the_rebuild_replays_the_spill_without_duplicating() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let index_path = root.join("index");
    let spill = spill_for("repo1234", &["aaaaaaaaaaaaaaaa", "bbbbbbbbbbbbbbbb"]);

    // First open: half the spill is applied and committed, then the process
    // "dies" with the spill still on disk.
    write_commit_spill(&index_path, &spill).unwrap();
    {
        let index = open_index(&root, None);
        let fields = index.field_handles();
        let mut writer = index.index_handle().writer(15_000_000).unwrap();
        let row = &spill.namespaces[0].commit_documents[0];
        writer
            .add_document(build_commit_doc_from_row(
                row,
                &spill.namespaces[0].owner,
                fields,
            ))
            .unwrap();
        writer.commit().unwrap();
    }
    // The first open already consumed and deleted the spill, so re-stage it to
    // model the crash-before-delete position.
    write_commit_spill(&index_path, &spill).unwrap();

    let index = open_index(&root, None);
    let ids = commit_entity_ids(&index, index.field_handles());
    assert_eq!(
        ids,
        vec![
            "commit:repo1234:aaaaaaaaaaaaaaaa".to_string(),
            "commit:repo1234:bbbbbbbbbbbbbbbb".to_string(),
        ],
        "replay must converge on the exact set, with no duplicate for the row \
         that was already applied"
    );
}

/// Fault row: crash AFTER the rebuild but BEFORE the re-add commits. The
/// uncommitted adds are lost with the writer, and because the spill is deleted
/// only after the commit it is still on disk to replay.
#[test]
fn a_crash_before_the_readd_commits_recovers_from_the_surviving_spill() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let index_path = root.join("index");
    let spill = spill_for("repo1234", &["aaaaaaaaaaaaaaaa"]);

    // Model the lost writer: an index exists at the CURRENT schema with no
    // commit documents in it, and the spill survives.
    {
        let index = open_index(&root, None);
        index.complete_schema_migration().unwrap();
        assert!(commit_entity_ids(&index, index.field_handles()).is_empty());
    }
    write_commit_spill(&index_path, &spill).unwrap();

    let index = open_index(&root, None);
    assert_eq!(commit_entity_ids(&index, index.field_handles()).len(), 1);
    assert!(read_commit_spill(&index_path).unwrap().is_none());
}

/// Fault row: a leftover spill is consumed on an open with NO schema mismatch.
/// This is the property that makes the whole lane crash-safe, since the
/// mismatch trigger fires exactly once.
#[test]
fn a_leftover_spill_is_consumed_on_an_open_with_no_schema_mismatch() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let index_path = root.join("index");
    {
        let index = open_index(&root, None);
        index.complete_schema_migration().unwrap();
    }
    assert_eq!(
        std::fs::read_to_string(index_path.join("schema_version.txt"))
            .unwrap()
            .trim(),
        INDEX_SCHEMA_VERSION,
        "the index is at the current schema: no mismatch will be detected"
    );

    write_commit_spill(&index_path, &spill_for("repo1234", &["cccccccccccccccc"])).unwrap();
    let index = open_index(&root, None);
    assert_eq!(
        commit_entity_ids(&index, index.field_handles()),
        vec!["commit:repo1234:cccccccccccccccc".to_string()]
    );
    assert!(read_commit_spill(&index_path).unwrap().is_none());
}
