//! Phase 3 milestone P3-E gate: the guarded replacement boundary.
//!
//! The document-side rows live in
//! `bbox-corpus-index/tests/path_free_schema_cut.rs`. This file covers the rows
//! that need the catalog transact, and therefore this crate:
//!
//! - the refusal matrix (missing generation, corrupt manifest, commitment
//!   mismatch), each of which must leave the outgoing index readable;
//! - count and content equality for commit documents carried across the
//!   guarded replacement, plus the invariant that re-emission touches nothing
//!   else;
//! - the bridge guard's spill, written before the drop.
//!
//! Every test canonicalizes its tempdir root before deriving paths (macOS
//! resolves `/var/folders` to `/private/var/folders` inside the code under
//! test) and touches no real HOME or XDG state.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use bbox_corpus_core::project_catalog::{
    CommitNamespace, RepoHistoryAuthority, RepoHistoryGenerationId, RepoHistoryId,
    RepoHistoryMaterialization, RepoHistoryRecord,
};
use bbox_corpus_core::project_record::{
    ProjectRecord, ProjectRecordsProvider, ProjectRecordsSnapshot,
};
use bbox_corpus_index::index::history_generations::{
    HistoryGenerationIdV1, HistoryGenerationStore, HistoryScanLimitsV1, RepoHistoryRebuildStateV1,
    scan_commit_documents,
};
use bbox_corpus_index::index::schema_replacement::{
    CommitDocumentOwnerV1, SchemaReplacementRequest, read_commit_spill,
};
use bbox_corpus_index::index::{TranscriptIndex, first_text, register_code_tokenizer};
use bbox_indexing::index::schema_rebuild::{
    bridge_schema_replacement_guard, catalog_schema_replacement_guard,
    commit_prepared_rebuild_manifest, reemit_prepared_history_generations,
};
use bbox_indexing::project_catalog_store::ProjectCatalogStore;
use sha2::{Digest, Sha256};
use tantivy::collector::{Count, TopDocs};
use tantivy::query::TermQuery;
use tantivy::schema::IndexRecordOption;
use tantivy::{Index, TantivyDocument, Term};
use tempfile::tempdir;

const OUTGOING_SCHEMA: &str = "outgoing-test-schema";
/// Host path deliberately baked into the outgoing index's commit documents.
/// The re-emitted documents must not reproduce it.
const OUTGOING_HOST_PATH: &str = "/host-checkouts/outgoing-fixture";

struct Fixture {
    _directory: tempfile::TempDir,
    index_path: PathBuf,
    projects_path: PathBuf,
    store: ProjectCatalogStore,
}

impl Fixture {
    /// Put the CURRENT schema marker back, so the pre-drop index can be
    /// reopened for staging without tripping the mismatch trigger.
    fn restamp_current(&self) {
        fs::write(
            self.index_path.join("schema_version.txt"),
            format!("{}\n", bbox_corpus_index::index::INDEX_SCHEMA_VERSION),
        )
        .unwrap();
    }
}

fn history_id(seed: u8) -> RepoHistoryId {
    let mut hex = format!("{seed:02x}");
    while hex.len() < 32 {
        hex.push('0');
    }
    RepoHistoryId::parse(format!("rh_{hex}")).unwrap()
}

fn commit_sha(seed: u8) -> String {
    let mut sha = format!("{seed:02x}");
    while sha.len() < 40 {
        sha.push('0');
    }
    sha
}

/// Append commit documents carrying BOTH path-bearing fields, mirroring what a
/// pre-cut `build_commit_doc` wrote.
fn write_commit_documents(index_path: &Path, namespace: &str, messages: &[(String, String)]) {
    let index = Index::open_in_dir(index_path).unwrap();
    register_code_tokenizer(&index);
    let schema = index.schema();
    let mut writer: tantivy::IndexWriter = index.writer(15_000_000).unwrap();
    let field = |name: &str| schema.get_field(name).unwrap();
    for (sha, message) in messages {
        let mut doc = TantivyDocument::new();
        doc.add_text(field("doc_type"), "commit");
        doc.add_text(field("chunk_kind"), "git_message");
        doc.add_text(field("entity_id"), format!("commit:{namespace}:{sha}"));
        doc.add_text(field("content"), message);
        doc.add_text(
            field("path_tokens"),
            message.lines().next().unwrap_or_default(),
        );
        doc.add_text(
            field("chunk_hash"),
            hex::encode(Sha256::digest(message.as_bytes())),
        );
        doc.add_text(field("parser_version"), "test-parser");
        doc.add_text(field("repo_id"), namespace);
        doc.add_text(field("commit_sha"), sha);
        doc.add_text(field("commit_author_name"), "History Fixture");
        doc.add_text(field("commit_author_email"), "fixture@example.invalid");
        doc.add_text(field("session_id"), "");
        doc.add_text(field("account"), "git");
        doc.add_text(field("project"), OUTGOING_HOST_PATH);
        doc.add_text(field("file_path"), "git:proj-outgoing");
        doc.add_text(field("role"), "commit");
        doc.add_u64(field("byte_offset"), 0);
        doc.add_u64(field("is_subagent"), 0);
        writer.add_document(doc).unwrap();
    }
    writer.commit().unwrap();
}

/// A non-commit document, so the re-emission's blast radius can be asserted.
fn write_project_file_document(index_path: &Path, project_id: &str) {
    let index = Index::open_in_dir(index_path).unwrap();
    register_code_tokenizer(&index);
    let schema = index.schema();
    let mut writer: tantivy::IndexWriter = index.writer(15_000_000).unwrap();
    let field = |name: &str| schema.get_field(name).unwrap();
    let mut doc = TantivyDocument::new();
    doc.add_text(field("doc_type"), "project_file");
    doc.add_text(field("project_id"), project_id);
    doc.add_text(field("entity_id"), "project_file:boundary:fixture");
    doc.add_text(field("content"), "boundary fixture body");
    doc.add_text(field("relative_path"), "src/boundary.rs");
    doc.add_text(field("file_path"), "src/boundary.rs");
    writer.add_document(doc).unwrap();
    writer.commit().unwrap();
}

fn stamp_outgoing_marker(index_path: &Path) {
    fs::write(
        index_path.join("schema_version.txt"),
        format!("{OUTGOING_SCHEMA}\n"),
    )
    .unwrap();
}

/// `fixture` without the outgoing marker, so a caller can stage real documents
/// into the pre-drop index before stamping it.
fn fixture_unstamped(namespace: &str, commits: usize) -> Fixture {
    let fixture = fixture(namespace, commits);
    // Restore the CURRENT marker: `fixture` stamped the outgoing one, and an
    // index whose marker mismatches cannot be reopened without triggering the
    // reset this suite is testing.
    fixture.restamp_current();
    fixture
}

/// A fresh v2 catalog whose repo-history record owns `namespace`, with an index
/// stamped at an OUTGOING schema marker plus commit documents in it.
fn fixture(namespace: &str, commits: usize) -> Fixture {
    let directory = tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap();
    let state = root.join("state");
    fs::create_dir_all(&state).unwrap();
    let index_path = state.join("index");
    {
        let index = TranscriptIndex::open_or_create(
            &index_path,
            Vec::new(),
            None,
            state.join("legacy-projects.json"),
            state.join("knowledge.json"),
            state.join("threads.json"),
            state.join("roadmap.json"),
        )
        .unwrap();
        index.complete_schema_migration().unwrap();
    }
    let messages = (0..commits)
        .map(|index| {
            (
                commit_sha(index as u8 + 1),
                format!("carried subject {index}\n\nbody {index}"),
            )
        })
        .collect::<Vec<_>>();
    write_commit_documents(&index_path, namespace, &messages);

    let projects_path = state.join("projects.json");
    let store = ProjectCatalogStore::initialize_empty(&projects_path).unwrap();
    let epoch = store.snapshot().unwrap().epoch();
    let namespace = namespace.to_string();
    store
        .transact(epoch, |catalog, _attachments| {
            let id = history_id(1);
            let parsed = CommitNamespace::parse(namespace.clone()).unwrap();
            catalog.repo_histories.insert(
                id.clone(),
                RepoHistoryRecord {
                    repo_history_id: id,
                    authority: RepoHistoryAuthority::LegacyNamespace(parsed.clone()),
                    primary_namespace: parsed,
                    compatibility_namespaces: BTreeSet::new(),
                    materialization: RepoHistoryMaterialization::NotBuilt,
                },
            );
            Ok(())
        })
        .unwrap();

    // Stamp the outgoing marker AFTER every pre-drop write so the scan records
    // it and the recovery classifier can compare against it. A caller that must
    // stage documents into the pre-drop index first uses
    // `fixture_unstamped` + `stamp_outgoing_marker`: reopening a
    // marker-mismatched index would trigger the very reset under test.
    stamp_outgoing_marker(&index_path);

    Fixture {
        _directory: directory,
        index_path,
        projects_path,
        store,
    }
}

fn request(fixture: &Fixture) -> SchemaReplacementRequest<'_> {
    SchemaReplacementRequest {
        index_path: &fixture.index_path,
        projects_path: &fixture.projects_path,
        code_source_store_path: &fixture.projects_path,
        observed_schema_version: Some(OUTGOING_SCHEMA.to_string()),
        target_schema_version: "incoming-test-schema",
    }
}

fn commit_rows(index_path: &Path) -> Vec<(String, String, String, String)> {
    let index = Index::open_in_dir(index_path).unwrap();
    register_code_tokenizer(&index);
    let schema = index.schema();
    let field = |name: &str| schema.get_field(name).unwrap();
    let reader = index.reader().unwrap();
    let searcher = reader.searcher();
    let query = TermQuery::new(
        Term::from_field_text(field("doc_type"), "commit"),
        IndexRecordOption::Basic,
    );
    let count = searcher.search(&query, &Count).unwrap();
    if count == 0 {
        return Vec::new();
    }
    let mut rows = searcher
        .search(&query, &TopDocs::with_limit(count))
        .unwrap()
        .into_iter()
        .map(|(_, address)| {
            let doc: TantivyDocument = searcher.doc(address).unwrap();
            (
                first_text(&doc, field("entity_id")),
                first_text(&doc, field("commit_sha")),
                first_text(&doc, field("chunk_hash")),
                first_text(&doc, field("content")),
            )
        })
        .collect::<Vec<_>>();
    rows.sort();
    rows
}

fn doc_count(index_path: &Path, doc_type: &str) -> usize {
    let index = Index::open_in_dir(index_path).unwrap();
    register_code_tokenizer(&index);
    let schema = index.schema();
    let reader = index.reader().unwrap();
    reader
        .searcher()
        .search(
            &TermQuery::new(
                Term::from_field_text(schema.get_field("doc_type").unwrap(), doc_type),
                IndexRecordOption::Basic,
            ),
            &Count,
        )
        .unwrap()
}

// ---------------------------------------------------------------------------
// Guarded replacement: count and content equality
// ---------------------------------------------------------------------------

#[test]
fn the_catalog_guard_prepares_a_manifest_and_reemission_preserves_every_commit() {
    let fixture = fixture("repo-outgoing", 4);
    let before = commit_rows(&fixture.index_path);
    assert_eq!(before.len(), 4);

    // 1. The guard authorizes the replacement and leaves a PREPARED manifest.
    let guard = catalog_schema_replacement_guard(
        Arc::new(ProjectCatalogStore::open_existing(&fixture.projects_path).unwrap()),
        HistoryScanLimitsV1::default(),
        fixture.projects_path.parent().unwrap().join("vectors"),
    );
    let authorization = guard(&request(&fixture)).expect("the guard authorizes");
    assert!(
        authorization.authorized_by.contains("rebuild manifest"),
        "{}",
        authorization.authorized_by
    );
    let generation_store = HistoryGenerationStore::open_for_index(&fixture.index_path).unwrap();
    let manifest = generation_store
        .read_rebuild_manifest()
        .unwrap()
        .expect("a prepared manifest");
    assert_eq!(manifest.state, RepoHistoryRebuildStateV1::Prepared);
    assert_eq!(manifest.prepared.namespace_inventory.len(), 1);
    assert_eq!(
        manifest.prepared.namespace_inventory[0].commit_document_count,
        4
    );
    // The catalog advanced to Ready, which is what makes the generation a GC
    // root independent of the manifest. Read through a FRESH handle: the guard
    // advanced the catalog through its own store handle, and this fixture's
    // handle holds its own snapshot.
    let reopened = ProjectCatalogStore::open_existing(&fixture.projects_path).unwrap();
    let catalog = reopened.snapshot().unwrap();
    assert!(matches!(
        catalog.catalog().repo_histories[&history_id(1)].materialization,
        RepoHistoryMaterialization::Ready { .. }
    ));

    // 2. The destructive step, then a fresh index under the incoming schema.
    fs::remove_dir_all(&fixture.index_path).unwrap();
    let index = TranscriptIndex::open_or_create(
        &fixture.index_path,
        Vec::new(),
        None,
        fixture.projects_path.parent().unwrap().join("legacy.json"),
        fixture
            .projects_path
            .parent()
            .unwrap()
            .join("knowledge.json"),
        fixture.projects_path.parent().unwrap().join("threads.json"),
        fixture.projects_path.parent().unwrap().join("roadmap.json"),
    )
    .unwrap();
    assert!(commit_rows(&fixture.index_path).is_empty());

    // 3. Re-emission from the pinned generations, as the rebuild pass runs it.
    let owners = BTreeMap::from([(
        "repo-outgoing".to_string(),
        CommitDocumentOwnerV1 {
            project_id: Some("proj-outgoing".into()),
            project_display: "acme-service".into(),
        },
    )]);
    let writer: tantivy::IndexWriter = index.index_handle().writer(15_000_000).unwrap();
    let outcome = reemit_prepared_history_generations(
        &fixture.index_path,
        &writer,
        index.field_handles(),
        &owners,
    )
    .unwrap()
    .expect("a prepared manifest drives re-emission");
    let mut writer = writer;
    writer.commit().unwrap();

    assert_eq!(outcome.namespaces, 1);
    assert_eq!(outcome.commit_documents, 4);
    // No vector probe is registered in this test process, so every row is
    // conservatively re-enqueued rather than claimed covered.
    assert_eq!(outcome.vectors_reenqueued, 4);
    assert_eq!(outcome.vectors_verified, 0);

    // Count AND content equality on identity, sha, content hash, and body.
    let after = commit_rows(&fixture.index_path);
    assert_eq!(after, before, "the carried commit set must be identical");

    // And the re-emitted documents carry the display value, not the host path.
    let index = Index::open_in_dir(&fixture.index_path).unwrap();
    register_code_tokenizer(&index);
    let schema = index.schema();
    let reader = index.reader().unwrap();
    let searcher = reader.searcher();
    let query = TermQuery::new(
        Term::from_field_text(schema.get_field("doc_type").unwrap(), "commit"),
        IndexRecordOption::Basic,
    );
    for (_, address) in searcher.search(&query, &TopDocs::with_limit(10)).unwrap() {
        let doc: TantivyDocument = searcher.doc(address).unwrap();
        assert_eq!(
            first_text(&doc, schema.get_field("project").unwrap()),
            "acme-service"
        );
        assert_eq!(
            first_text(&doc, schema.get_field("file_path").unwrap()),
            "git:proj-outgoing"
        );
    }

    // 4. The manifest is promoted only after the population is durable.
    let committed = commit_prepared_rebuild_manifest(
        &fixture.index_path,
        "lexical:incoming-test-schema",
        "vector:agentic-corpus-e3",
        reopened.snapshot().unwrap().epoch(),
        outcome.vector_inventory.clone(),
    )
    .unwrap()
    .expect("a manifest to commit");
    assert_eq!(committed.state, RepoHistoryRebuildStateV1::Committed);
    let evidence = committed.committed.expect("committed evidence");
    assert_eq!(
        evidence.verified_lexical_view,
        "lexical:incoming-test-schema"
    );
    assert_eq!(evidence.verified_vector_view, "vector:agentic-corpus-e3");
    // The verified-or-re-enqueued inventory is durable evidence, per generation.
    assert_eq!(evidence.vector_inventory.len(), 1);
    assert_eq!(evidence.vector_inventory[0].vector_inputs_verified, 0);
    assert_eq!(evidence.vector_inventory[0].vector_inputs_reenqueued, 4);
}

#[test]
fn reemission_touches_only_commit_documents() {
    let fixture = fixture("repo-outgoing", 2);
    write_project_file_document(&fixture.index_path, "proj-outgoing");
    assert_eq!(doc_count(&fixture.index_path, "project_file"), 1);

    let guard = catalog_schema_replacement_guard(
        Arc::new(ProjectCatalogStore::open_existing(&fixture.projects_path).unwrap()),
        HistoryScanLimitsV1::default(),
        fixture.projects_path.parent().unwrap().join("vectors"),
    );
    guard(&request(&fixture)).unwrap();

    // Re-emit into the SAME index rather than a replaced one: the point is the
    // blast radius, and delete-term-then-add per commit entity id must leave
    // every other document kind alone.
    let index = Index::open_in_dir(&fixture.index_path).unwrap();
    register_code_tokenizer(&index);
    let (_schema, fields) = bbox_corpus_index::index::build_schema();
    let writer: tantivy::IndexWriter = index.writer(15_000_000).unwrap();
    reemit_prepared_history_generations(
        &fixture.index_path,
        &writer,
        fields,
        &BTreeMap::from([(
            "repo-outgoing".to_string(),
            CommitDocumentOwnerV1 {
                project_id: Some("proj-outgoing".into()),
                project_display: "acme-service".into(),
            },
        )]),
    )
    .unwrap()
    .unwrap();
    let mut writer = writer;
    writer.commit().unwrap();

    assert_eq!(
        doc_count(&fixture.index_path, "commit"),
        2,
        "the idempotent re-add must not duplicate the commit set"
    );
    assert_eq!(
        doc_count(&fixture.index_path, "project_file"),
        1,
        "re-emission must not touch project-file documents"
    );
}

// ---------------------------------------------------------------------------
// Refusal matrix
// ---------------------------------------------------------------------------

/// Shared assertion for every refusal row: the outgoing index is still there,
/// still carries its own marker, and its commit population is still readable.
fn assert_outgoing_index_readable(fixture: &Fixture, expected_commits: usize) {
    assert!(fixture.index_path.is_dir());
    assert_eq!(
        fs::read_to_string(fixture.index_path.join("schema_version.txt"))
            .unwrap()
            .trim(),
        OUTGOING_SCHEMA
    );
    assert_eq!(commit_rows(&fixture.index_path).len(), expected_commits);
}

#[test]
fn refusal_commitment_mismatch_keeps_the_old_index_readable() {
    let fixture = fixture("repo-outgoing", 3);
    // Pin the record Ready at a generation id the index cannot re-derive. The
    // materializer refuses the whole pass rather than partially advancing.
    let epoch = fixture.store.snapshot().unwrap().epoch();
    fixture
        .store
        .transact(epoch, |catalog, _attachments| {
            let record = catalog.repo_histories.get_mut(&history_id(1)).unwrap();
            record.materialization = RepoHistoryMaterialization::Ready {
                generation_id: RepoHistoryGenerationId::parse(format!("rhg_{}", "0".repeat(64)))
                    .unwrap(),
            };
            Ok(())
        })
        .unwrap();

    let guard = catalog_schema_replacement_guard(
        Arc::new(ProjectCatalogStore::open_existing(&fixture.projects_path).unwrap()),
        HistoryScanLimitsV1::default(),
        fixture.projects_path.parent().unwrap().join("vectors"),
    );
    let error = guard(&request(&fixture)).err().expect("the guard refuses");
    assert!(
        format!("{error:#}").contains("history_commitment_mismatch"),
        "{error:#}"
    );
    assert_outgoing_index_readable(&fixture, 3);
    assert!(
        HistoryGenerationStore::open_for_index(&fixture.index_path)
            .unwrap()
            .read_rebuild_manifest()
            .unwrap()
            .is_none(),
        "a refused pass must not leave a manifest authorizing anything"
    );
}

#[test]
fn refusal_corrupt_manifest_keeps_the_old_index_readable() {
    let fixture = fixture("repo-outgoing", 3);
    let store = HistoryGenerationStore::open_for_index(&fixture.index_path).unwrap();
    fs::write(store.root().join("rebuild-manifest.json"), b"{ not json").unwrap();

    let error = store
        .read_rebuild_manifest()
        .err()
        .expect("a corrupt manifest must not decode");
    assert_eq!(error.code(), "error.history_generation_corrupt");
    // The recovery classification the daemon runs before opening the index
    // propagates the same refusal, so boot stops with everything intact.
    let recovery = bbox_indexing::index::schema_rebuild::recover_rebuild_manifest_before_open(
        &fixture.index_path,
    );
    assert!(recovery.is_err(), "recovery must refuse a corrupt manifest");
    assert_outgoing_index_readable(&fixture, 3);
}

#[test]
fn refusal_missing_generation_keeps_the_old_index_readable() {
    let fixture = fixture("repo-outgoing", 3);
    let guard = catalog_schema_replacement_guard(
        Arc::new(ProjectCatalogStore::open_existing(&fixture.projects_path).unwrap()),
        HistoryScanLimitsV1::default(),
        fixture.projects_path.parent().unwrap().join("vectors"),
    );
    guard(&request(&fixture)).unwrap();

    // Remove the generation the manifest pins, then attempt re-emission. It
    // must fail BEFORE the rebuild pass could reach `delete_all_documents`.
    let store = HistoryGenerationStore::open_for_index(&fixture.index_path).unwrap();
    let manifest = store.read_rebuild_manifest().unwrap().unwrap();
    let generation_id = &manifest.prepared.namespace_inventory[0].generation_id;
    let parsed = HistoryGenerationIdV1::parse(generation_id).unwrap();
    assert!(store.load(&parsed).is_ok());
    fs::remove_dir_all(store.root().join(generation_id)).unwrap();

    let index = Index::open_in_dir(&fixture.index_path).unwrap();
    register_code_tokenizer(&index);
    let (_schema, fields) = bbox_corpus_index::index::build_schema();
    let writer: tantivy::IndexWriter = index.writer(15_000_000).unwrap();
    let error =
        reemit_prepared_history_generations(&fixture.index_path, &writer, fields, &BTreeMap::new())
            .err()
            .expect("a manifest naming a missing generation must refuse");
    assert!(
        format!("{error:#}").contains(generation_id),
        "the refusal must name the generation: {error:#}"
    );
    drop(writer);
    assert_outgoing_index_readable(&fixture, 3);
}

// ---------------------------------------------------------------------------
// Bridge guard
// ---------------------------------------------------------------------------

#[derive(Debug)]
struct StaticRecords(ProjectRecordsSnapshot);

impl ProjectRecordsProvider for StaticRecords {
    fn records_snapshot(&self) -> ProjectRecordsSnapshot {
        self.0.clone()
    }
}

#[test]
fn the_bridge_guard_spills_the_commit_set_before_the_drop() {
    let fixture = fixture("repo-outgoing", 3);
    let records = ProjectRecordsSnapshot::from_bridge_records(
        vec![ProjectRecord {
            project_id: "proj-outgoing".into(),
            repo_id: Some("repo-outgoing".into()),
            canonical_path: OUTGOING_HOST_PATH.into(),
            registered_at: "2026-07-25T00:00:00Z".into(),
            is_git_repo: true,
            languages: Default::default(),
            aliases: ["acme-service".to_string()].into_iter().collect(),
        }],
        1,
    );
    let guard = bridge_schema_replacement_guard(Arc::new(StaticRecords(records)));
    let authorization = guard(&request(&fixture)).expect("the bridge guard authorizes");
    assert!(
        authorization.authorized_by.contains("bridge-spill"),
        "{}",
        authorization.authorized_by
    );

    let spill = read_commit_spill(&fixture.index_path)
        .unwrap()
        .expect("a spill was written before the drop");
    assert_eq!(spill.commit_document_count(), 3);
    assert_eq!(spill.namespaces.len(), 1);
    assert_eq!(spill.namespaces[0].namespace, "repo-outgoing");
    assert_eq!(
        spill.namespaces[0].owner.project_display, "acme-service",
        "the spilled owner is the display name, never the checkout path"
    );
    assert_eq!(
        spill.source_schema_version.as_deref(),
        Some(OUTGOING_SCHEMA)
    );
    for row in &spill.namespaces[0].commit_documents {
        let encoded = serde_json::to_string(row).unwrap();
        assert!(
            !encoded.contains(OUTGOING_HOST_PATH),
            "a spilled row must carry no host path: {encoded}"
        );
    }
    // The guard does not drop anything itself; the open path does, after it.
    assert_outgoing_index_readable(&fixture, 3);
}

/// Exit-gate shape (Phase 3 plan section 1 item 1 + section 9): a store with an
/// ACTIVE collected generation boots through the whole guarded replacement after
/// the paired version bump, and the project is searchable afterwards.
///
/// The pre-drop collected state is built with REAL documents under the CURRENT
/// materialization, so `document_count` and `entity_inventory_sha256` on the
/// activation record are TRUE; only the selector suffix, the snapshot id, and the
/// activation's copies of those two are rewritten to the synthetic outgoing form.
/// That reproduces the live shape exactly - an agreeing activation with a nonzero
/// count against a post-reset EMPTY index - which is what makes the pass's
/// collected preservation gate meaningful rather than vacuous. An earlier
/// version of this fixture declared `document_count: 0`, which made the gate
/// trivially satisfied and hid the wedge the live smoke then hit.
#[test]
fn a_collected_project_survives_the_guarded_replacement_and_stays_searchable() {
    let fixture = fixture_unstamped("repo-outgoing", 3);
    let state = fixture.index_path.parent().unwrap().to_path_buf();
    let index_projects_path = state.join("legacy-projects.json");
    let before_commits = commit_rows(&fixture.index_path);
    assert_eq!(before_commits.len(), 3);

    // A real collected generation in the store the reindex pass will read.
    let bytes = b"pub fn collected() {}\n";
    let content_hash = hex::encode(Sha256::digest(bytes));
    let entries = vec![bbox_code_source::ManifestEntry {
        relative_path: "src/lib.rs".into(),
        content_sha256: content_hash.clone(),
        size: bytes.len() as u64,
    }];
    let head_commit = "b".repeat(40);
    let descriptor = bbox_code_source::GenerationDescriptor {
        schema_version: bbox_code_source::SCHEMA_VERSION,
        walker_policy_version: bbox_code_source::WALKER_POLICY_VERSION.into(),
        scope: bbox_corpus_core::identity::PublishedScope::try_new("repo-family", ".").unwrap(),
        head_commit: head_commit.clone(),
        dirty_fingerprint: bbox_code_source::dirty_fingerprint(&head_commit, &entries),
        manifest_sha256: bbox_code_source::manifest_sha256(&entries),
        file_count: entries.len() as u64,
        logical_bytes: bytes.len() as u64,
    };
    let store = Arc::new(
        bbox_code_source_store::CodeSourceStore::open(
            state.join("code-sources"),
            bbox_code_source_store::StoreLimits::default(),
        )
        .unwrap(),
    );
    let upload = store.begin_upload("host-a", descriptor.clone()).unwrap();
    store
        .put_manifest_page("host-a", &upload.upload_id, 0, &entries)
        .unwrap();
    store
        .complete_manifest("host-a", &upload.upload_id)
        .unwrap();
    store
        .install_blob(
            "host-a",
            &upload.upload_id,
            &content_hash,
            bytes.len() as u64,
            &bytes[..],
        )
        .unwrap();
    let generation_id = store
        .finalize_upload("host-a", &upload.upload_id)
        .unwrap()
        .generation_id;

    let project_id = "collected-project";
    let record = ProjectRecord {
        project_id: project_id.into(),
        repo_id: Some("repo-family".into()),
        canonical_path: "/unavailable/remote/project".into(),
        registered_at: "2026-07-25T00:00:00Z".into(),
        is_git_repo: true,
        languages: Default::default(),
        aliases: ["acme-service".to_string()].into_iter().collect(),
    };
    fs::write(
        &index_projects_path,
        serde_json::json!({
            "projects": [{
                "project_id": project_id,
                "repo_id": "repo-family",
                "canonical_path": "/unavailable/remote/project",
                "registered_at": "2026-07-25T00:00:00Z",
                "is_git_repo": true,
                "aliases": ["acme-service"],
            }]
        })
        .to_string(),
    )
    .unwrap();
    let records: Arc<dyn ProjectRecordsProvider> = Arc::new(StaticRecords(
        ProjectRecordsSnapshot::from_bridge_records(vec![record.clone()], 1),
    ));
    let broker = Arc::new(bbox_indexing::checkout_access::CheckoutAccessBroker::new(
        Arc::new(bbox_indexing::checkout_access::DenyCheckoutAccess),
        bbox_indexing::checkout_access::CheckoutAccessObservations::in_memory(),
    ));

    // Stage the generation into the PRE-DROP index so its count and inventory
    // are real. This is the state a pre-bump daemon actually left behind.
    let (true_document_count, true_inventory) = {
        let index = TranscriptIndex::open_or_create(
            &fixture.index_path,
            Vec::new(),
            None,
            index_projects_path.clone(),
            state.join("knowledge.json"),
            state.join("threads.json"),
            state.join("roadmap.json"),
        )
        .unwrap();
        let actor = bbox_indexing::index::IndexWriterActor::spawn_for_with_checkout_access(
            &index,
            records.clone(),
            broker.clone(),
        );
        let staged = actor
            .stage_collected_generation(
                bbox_corpus_core::code_project_identity::CodeProjectIdentity::from_bridge_record(
                    &record,
                )
                .unwrap(),
                descriptor.clone(),
                generation_id.clone(),
                entries.clone(),
                store.clone(),
            )
            .unwrap();
        let counts = (
            staged.document_count,
            staged.entity_inventory_sha256.clone(),
        );
        assert!(counts.0 > 0, "the fixture must stage real documents");
        drop(staged);
        drop(actor);
        counts
    };

    // Rewrite ONLY the selector suffix, the snapshot id, and the activation's
    // copies of them. Count and inventory stay TRUE.
    let outgoing_selector = format!(
        "{}:m{}",
        bbox_code_source::source_selector(project_id, &generation_id),
        "0123456789abcdef"
    );
    let outgoing_snapshot = format!("collected-{}", "9".repeat(32));
    let edges_dir =
        bbox_edge_sidecar::edge_sidecar::edges_dir_from_projects_path(&index_projects_path);
    let outgoing_snapshot_dir =
        bbox_edge_sidecar::snapshot::snapshot_dir(&edges_dir, project_id, &outgoing_snapshot);
    fs::create_dir_all(&outgoing_snapshot_dir).unwrap();
    fs::write(outgoing_snapshot_dir.join("project.jsonl"), b"").unwrap();
    store
        .record_materialization(
            &descriptor.scope,
            &generation_id,
            true_document_count,
            true_inventory.clone(),
        )
        .unwrap();
    store
        .save_activation(&bbox_code_source_store::ActivationRecord {
            version: 1,
            project_id: project_id.to_string(),
            generation_id: generation_id.clone(),
            selector: outgoing_selector.clone(),
            snapshot_id: outgoing_snapshot.clone(),
            document_count: true_document_count,
            entity_inventory_sha256: true_inventory,
            current_chunk_targets: Default::default(),
            activated_unix_secs: 1,
            cutback_pending: false,
            diagnostic: None,
        })
        .unwrap();
    bbox_edge_sidecar::snapshot::activate_collected_snapshot_with(
        &edges_dir,
        project_id,
        descriptor.scope.repo_id(),
        &descriptor.head_commit,
        &generation_id,
        &outgoing_selector,
        &outgoing_snapshot,
        || Ok(()),
    )
    .unwrap();
    stamp_outgoing_marker(&fixture.index_path);

    // 1. The guard authorizes and prepares the manifest.
    catalog_schema_replacement_guard(
        Arc::new(ProjectCatalogStore::open_existing(&fixture.projects_path).unwrap()),
        HistoryScanLimitsV1::default(),
        fixture.projects_path.parent().unwrap().join("vectors"),
    )(&request(&fixture))
    .expect("the catalog guard authorizes");

    // 2. The destructive drop.
    fs::remove_dir_all(&fixture.index_path).unwrap();

    // 3. Reopen under the incoming schema: the index is now EMPTY while the
    //    activation record still promises `true_document_count` documents.
    let index = TranscriptIndex::open_or_create(
        &fixture.index_path,
        Vec::new(),
        None,
        index_projects_path.clone(),
        state.join("knowledge.json"),
        state.join("threads.json"),
        state.join("roadmap.json"),
    )
    .unwrap();
    let fields = index.field_handles();
    let actor = bbox_indexing::index::IndexWriterActor::spawn_for_with_checkout_access(
        &index,
        records,
        broker.clone(),
    );

    // 4. The synchronous schema-migration rebuild. An ORDINARY full pass would
    //    fail here on the collected preservation gate ("expected N, observed 0")
    //    because it verifies against the just-emptied index; the schema-migration
    //    cause skips that gate and lets re-staging be the authority.
    actor
        .run_reindex_pass_for_schema_migration()
        .expect("the post-replacement rebuild must converge, not refuse");
    index.reader_reload_for_test();

    // The carried commit set is intact.
    let after_commits = commit_rows(&fixture.index_path);
    assert_eq!(
        after_commits.len(),
        before_commits.len(),
        "every commit document must survive the replacement"
    );
    assert_eq!(
        after_commits.iter().map(|row| &row.0).collect::<Vec<_>>(),
        before_commits.iter().map(|row| &row.0).collect::<Vec<_>>(),
        "commit identity must be byte-stable across the replacement"
    );

    // The collected project migrated and is searchable under the CURRENT selector.
    let expected_selector =
        bbox_corpus_index::index::project_files::collected_materialization_selector(
            project_id,
            &generation_id,
        );
    assert_ne!(expected_selector, outgoing_selector);
    let searcher = index.searcher();
    let live = |selector: &str| {
        searcher
            .search(
                &TermQuery::new(
                    Term::from_field_text(fields.code_source_selector, selector),
                    IndexRecordOption::Basic,
                ),
                &Count,
            )
            .unwrap()
    };
    assert_eq!(
        live(&expected_selector) as u64,
        true_document_count,
        "the re-staged generation must be complete under the current selector"
    );
    assert_eq!(live(&outgoing_selector), 0);
    let activation = store.load_activation(project_id).unwrap().unwrap();
    assert_eq!(activation.selector, expected_selector);
    assert_eq!(activation.document_count, true_document_count);
    assert!(store.retirement_pending(&outgoing_selector).unwrap());

    // The pinned selector map, refreshed the way boot refreshes it, names the
    // migrated selector - otherwise a reader would filter the new documents out.
    assert_eq!(
        index
            .refresh_active_code_selectors()
            .unwrap()
            .get(project_id)
            .map(String::as_str),
        Some(expected_selector.as_str())
    );

    // The rebuild committed the manifest.
    let manifest = HistoryGenerationStore::open_for_index(&fixture.index_path)
        .unwrap()
        .read_rebuild_manifest()
        .unwrap()
        .expect("a manifest");
    assert_eq!(manifest.state, RepoHistoryRebuildStateV1::Committed);
}

/// F3 (parity): a PRE-MARKER index carries. `schema_version.txt` absent plus a
/// non-empty directory is a must-replace state for
/// `reset_index_on_schema_mismatch`, so before the scan admitted marker-less
/// indexes its commit documents dropped with no generation, no spill, and no
/// manifest - the silent loss this boundary exists to end, surviving in one
/// narrow arm.
///
/// Both guards are asserted from ONE fixture so the two lanes cannot drift on
/// the same input.
#[test]
fn a_pre_marker_index_carries_its_commit_documents_through_both_guards() {
    // --- catalog lane: carried as drift-mode generations -------------------
    let fixture = fixture_unstamped("repo-premarker", 3);
    // The pre-marker state: no marker at all, index files intact.
    fs::remove_file(fixture.index_path.join("schema_version.txt")).unwrap();
    assert!(
        !fixture.index_path.join("schema_version.txt").exists(),
        "the fixture must be marker-less"
    );
    assert!(
        fixture.index_path.join("meta.json").is_file(),
        "a pre-marker index still holds a tantivy index; the not-an-index arm          must not swallow it"
    );
    let before = commit_rows(&fixture.index_path);
    assert_eq!(before.len(), 3);

    // The guard observes NO marker, which is exactly the request shape
    // `reset_index_on_schema_mismatch` builds for this state.
    let pre_marker_request = SchemaReplacementRequest {
        index_path: &fixture.index_path,
        projects_path: &fixture.projects_path,
        code_source_store_path: &fixture.projects_path,
        observed_schema_version: None,
        target_schema_version: "incoming-test-schema",
    };
    let authorization = catalog_schema_replacement_guard(
        Arc::new(ProjectCatalogStore::open_existing(&fixture.projects_path).unwrap()),
        HistoryScanLimitsV1::default(),
        fixture.projects_path.parent().unwrap().join("vectors"),
    )(&pre_marker_request)
    .expect("a pre-marker index must be authorized, not refused");
    assert!(
        authorization.authorized_by.contains("rebuild manifest"),
        "authorized-with-carry, not authorized-empty: {}",
        authorization.authorized_by
    );

    let generation_store = HistoryGenerationStore::open_for_index(&fixture.index_path).unwrap();
    let manifest = generation_store
        .read_rebuild_manifest()
        .unwrap()
        .expect("a pre-marker replacement must still prepare a manifest");
    assert_eq!(manifest.state, RepoHistoryRebuildStateV1::Prepared);
    assert_eq!(
        manifest.prepared.source_schema_version,
        bbox_corpus_index::index::history_generations::PRE_MARKER_SOURCE_SCHEMA,
        "the manifest records the reserved sentinel for a marker-less source"
    );
    assert_eq!(manifest.prepared.namespace_inventory.len(), 1);
    assert_eq!(
        manifest.prepared.namespace_inventory[0].commit_document_count, 3,
        "every commit document must be pinned by the manifest"
    );

    // Survives the replacement: drop, reopen empty, re-emit from the pinned
    // generation, and the carried set is identical.
    fs::remove_dir_all(&fixture.index_path).unwrap();
    let state = fixture.index_path.parent().unwrap().to_path_buf();
    let index = TranscriptIndex::open_or_create(
        &fixture.index_path,
        Vec::new(),
        None,
        state.join("legacy-projects.json"),
        state.join("knowledge.json"),
        state.join("threads.json"),
        state.join("roadmap.json"),
    )
    .unwrap();
    assert!(commit_rows(&fixture.index_path).is_empty());
    let writer: tantivy::IndexWriter = index.index_handle().writer(15_000_000).unwrap();
    let outcome = reemit_prepared_history_generations(
        &fixture.index_path,
        &writer,
        index.field_handles(),
        &BTreeMap::from([(
            "repo-premarker".to_string(),
            CommitDocumentOwnerV1 {
                project_id: Some("proj-premarker".into()),
                project_display: "acme-service".into(),
            },
        )]),
    )
    .unwrap()
    .expect("the pinned manifest drives re-emission");
    let mut writer = writer;
    writer.commit().unwrap();
    assert_eq!(outcome.commit_documents, 3);
    assert_eq!(
        commit_rows(&fixture.index_path),
        before,
        "the pre-marker commit set must survive the replacement byte-identically"
    );

    // --- bridge lane: carried by the spill --------------------------------
    let bridge = fixture_unstamped("repo-premarker", 3);
    fs::remove_file(bridge.index_path.join("schema_version.txt")).unwrap();
    let bridge_before = commit_rows(&bridge.index_path);
    assert_eq!(bridge_before.len(), 3);
    let bridge_request = SchemaReplacementRequest {
        index_path: &bridge.index_path,
        projects_path: &bridge.projects_path,
        code_source_store_path: &bridge.projects_path,
        observed_schema_version: None,
        target_schema_version: "incoming-test-schema",
    };
    let authorization = bridge_schema_replacement_guard(Arc::new(StaticRecords(
        ProjectRecordsSnapshot::from_bridge_records(
            vec![ProjectRecord {
                project_id: "proj-premarker".into(),
                repo_id: Some("repo-premarker".into()),
                canonical_path: OUTGOING_HOST_PATH.into(),
                registered_at: "2026-07-25T00:00:00Z".into(),
                is_git_repo: true,
                languages: Default::default(),
                aliases: ["acme-service".to_string()].into_iter().collect(),
            }],
            1,
        ),
    )))(&bridge_request)
    .expect("a pre-marker index must be authorized, not refused");
    assert!(
        authorization.authorized_by.contains("3 commit documents"),
        "authorized-with-carry, not authorized-empty: {}",
        authorization.authorized_by
    );
    let spill = read_commit_spill(&bridge.index_path)
        .unwrap()
        .expect("a pre-marker index must spill before the drop");
    assert_eq!(spill.commit_document_count(), 3);
    assert_eq!(
        spill.source_schema_version, None,
        "the spill records the absent marker verbatim; consumption is \
         unconditional so this is diagnostics only"
    );

    // The spill survives the drop and is consumed at the next open, which is
    // the property that makes the pre-marker lane whole rather than merely
    // authorized.
    fs::remove_dir_all(&bridge.index_path).unwrap();
    let bridge_state = bridge.index_path.parent().unwrap().to_path_buf();
    let reopened = TranscriptIndex::open_or_create(
        &bridge.index_path,
        Vec::new(),
        None,
        bridge_state.join("legacy-projects.json"),
        bridge_state.join("knowledge.json"),
        bridge_state.join("threads.json"),
        bridge_state.join("roadmap.json"),
    )
    .unwrap();
    reopened.reader_reload_for_test();
    assert_eq!(
        commit_rows(&bridge.index_path),
        bridge_before,
        "the spilled pre-marker commit set must be restored at the next open"
    );
    assert!(read_commit_spill(&bridge.index_path).unwrap().is_none());
}

/// The other half of the F3 pair: an EMPTY pre-marker directory is genuinely
/// nothing to carry, and both guards authorize with nothing carried. This is
/// the row the scan's not-an-index discriminator protects - without it the
/// exposed `Index::open_in_dir` would error and both guards would refuse,
/// blocking boot for a directory holding one stray file and no marker.
#[test]
fn an_empty_pre_marker_directory_authorizes_with_nothing_carried() {
    let directory = tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap();
    let state = root.join("state");
    let index_path = state.join("index");
    fs::create_dir_all(&index_path).unwrap();
    // Non-empty (so `reset_index_on_schema_mismatch` would trigger) but not a
    // tantivy index and carrying no marker.
    fs::write(index_path.join("stray-file"), b"not an index").unwrap();
    assert!(!index_path.join("meta.json").exists());
    assert!(!index_path.join("schema_version.txt").exists());

    let projects_path = state.join("projects.json");
    let store = ProjectCatalogStore::initialize_empty(&projects_path).unwrap();
    let request = SchemaReplacementRequest {
        index_path: &index_path,
        projects_path: &projects_path,
        code_source_store_path: &projects_path,
        observed_schema_version: None,
        target_schema_version: "incoming-test-schema",
    };

    let catalog = catalog_schema_replacement_guard(
        Arc::new(ProjectCatalogStore::open_existing(&projects_path).unwrap()),
        HistoryScanLimitsV1::default(),
        projects_path.parent().unwrap().join("vectors"),
    )(&request)
    .expect("an empty pre-marker directory authorizes");
    assert!(
        catalog.authorized_by.contains("no legacy history observed"),
        "{}",
        catalog.authorized_by
    );
    drop(store);

    let bridge = bridge_schema_replacement_guard(Arc::new(StaticRecords(
        ProjectRecordsSnapshot::from_bridge_records(Vec::new(), 1),
    )))(&request)
    .expect("an empty pre-marker directory authorizes");
    assert!(
        bridge.authorized_by.contains("no legacy history observed"),
        "{}",
        bridge.authorized_by
    );

    // Nothing was carried and nothing was fabricated.
    assert!(read_commit_spill(&index_path).unwrap().is_none());
    assert!(
        HistoryGenerationStore::open_for_index(&index_path)
            .unwrap()
            .read_rebuild_manifest()
            .unwrap()
            .is_none(),
        "no manifest may authorize anything for an empty directory"
    );
    assert!(
        index_path.join("stray-file").is_file(),
        "the guard must not mutate the directory it declined to carry from"
    );
}

#[test]
fn an_index_with_no_history_still_authorizes_both_guards() {
    let directory = tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap();
    let state = root.join("state");
    fs::create_dir_all(&state).unwrap();
    let index_path = state.join("index");
    {
        let index = TranscriptIndex::open_or_create(
            &index_path,
            Vec::new(),
            None,
            state.join("legacy-projects.json"),
            state.join("knowledge.json"),
            state.join("threads.json"),
            state.join("roadmap.json"),
        )
        .unwrap();
        index.complete_schema_migration().unwrap();
    }
    let projects_path = state.join("projects.json");
    let store = ProjectCatalogStore::initialize_empty(&projects_path).unwrap();
    let request = SchemaReplacementRequest {
        index_path: &index_path,
        projects_path: &projects_path,
        code_source_store_path: &projects_path,
        observed_schema_version: Some(OUTGOING_SCHEMA.to_string()),
        target_schema_version: "incoming-test-schema",
    };
    assert!(
        scan_commit_documents(&index_path, HistoryScanLimitsV1::default())
            .unwrap()
            .unwrap()
            .namespaces
            .is_empty()
    );
    catalog_schema_replacement_guard(
        Arc::new(ProjectCatalogStore::open_existing(&projects_path).unwrap()),
        HistoryScanLimitsV1::default(),
        projects_path.parent().unwrap().join("vectors"),
    )(&request)
    .expect("a no-history catalog open authorizes");
    drop(store);
    bridge_schema_replacement_guard(Arc::new(StaticRecords(
        ProjectRecordsSnapshot::from_bridge_records(Vec::new(), 1),
    )))(&request)
    .expect("a no-history bridge open authorizes");
    assert!(read_commit_spill(&index_path).unwrap().is_none());
}
