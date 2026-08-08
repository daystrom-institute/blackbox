//! Phase 3 milestone P3-D gate: the pre-replacement history materializer,
//! the immutable generation store, and the rebuild manifest.
//!
//! Two fixture families, because the two proof surfaces have different
//! prerequisites:
//!
//! - a MIGRATED root produced through the real migration facade rehearsal.
//!   Only a genuinely migrated store carries the persisted
//!   `LegacyCommitNamespaceInventoryAssetV1`, so the proved /
//!   commitment-mismatch / missing-asset rows live here;
//! - a FRESH v2 store built through `initialize_empty` plus `transact`.
//!   A fresh store has no Phase 1 evidence by construction, which is exactly
//!   the plan section 4.4 drift case, so the ambiguous / unclaimed /
//!   empty-index / transact-advancement / GC / recovery rows live here.
//!
//! Every test canonicalizes its tempdir root before deriving paths (macOS
//! resolves `/var/folders` to `/private/var/folders` under the code being
//! tested) and touches no real HOME or XDG state.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;

use bbox_corpus_core::git::GitCommit;

use bbox_code_source_store::CodeSourceStore;
use bbox_config::config::{self, Config, LoadOptions};
use bbox_corpus_core::project_catalog::{
    AmbiguousNamespaceRecord, AmbiguousNamespaceStatus, CommitNamespace, ProjectId,
    RepoHistoryAuthority, RepoHistoryId, RepoHistoryMaterialization,
    RepoHistoryQuarantineMaterialization, RepoHistoryRecord,
};
use bbox_corpus_index::index::history_generations::{
    HistoryCommitDocumentV1, HistoryFaultPoint, HistoryGenerationError, HistoryGenerationIo,
    HistoryGenerationStore, HistoryProofModeV1, HistoryScanLimitsV1, RepoHistoryRebuildCommittedV1,
    RepoHistoryRebuildRecoveryV1, scan_commit_documents,
};
use bbox_corpus_index::index::{TranscriptIndex, register_code_tokenizer};
use bbox_edge_sidecar::manifest::ManifestIndex;
use bbox_indexing::index::consolidated_history::{
    ConsolidatedWalkOutcomeV1, RepoHistoryCursorStoreV1, RepoHistoryIngestGroupV1,
};
use bbox_indexing::index::history_materializer::{
    HistoryMaterializerRequestV1, NamespaceClassificationV1, classify_rebuild_recovery,
    history_generation_gc_roots, materialize_history_generations,
    materialize_history_generations_with_io, plan_history_generation_gc, prepare_rebuild_manifest,
};
use bbox_indexing::index::history_refresh::refresh_repo_history_generation;
use bbox_indexing::project_catalog_inventory::{
    ProjectCatalogMigrationStatusV1, decode_migration_report_v1,
};
use bbox_indexing::project_catalog_migration::{
    ProjectCatalogMigrationApplyOutcomeV1, ProjectCatalogMigrationApplyRequestV1,
    ProjectCatalogMigrationFacadeV1, ProjectCatalogMigrationLayoutOverridesV1,
    ProjectCatalogMigrationPreflightRequestV1, ProjectCatalogMigrationResolvedLayoutV1,
    load_legacy_commit_namespace_inventory_asset, project_catalog_migration_store_limits,
};
use bbox_indexing::project_catalog_store::ProjectCatalogStore;
use bbox_vectors::VectorStore;
use sha2::{Digest, Sha256};
use tantivy::{Index, TantivyDocument};
use tempfile::tempdir;

// ---------------------------------------------------------------------------
// Shared fixture helpers
// ---------------------------------------------------------------------------

/// These fixtures resolve every root below one state directory, so the
/// fixture's vector root is the state directory's `vectors`. Production reads
/// `paths.vectors_path` instead, which is why the request carries the root
/// rather than deriving it (R33F1).
fn fixture_vector_root(projects_path: &Path) -> PathBuf {
    projects_path
        .parent()
        .expect("the fixture projects path has a parent")
        .join("vectors")
}

fn write(path: &Path, bytes: &[u8]) {
    fs::create_dir_all(path.parent().expect("fixture path has a parent")).unwrap();
    fs::write(path, bytes).unwrap();
}

fn git(root: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .env("GIT_AUTHOR_NAME", "History Fixture")
        .env("GIT_AUTHOR_EMAIL", "fixture@example.invalid")
        .env("GIT_COMMITTER_NAME", "History Fixture")
        .env("GIT_COMMITTER_EMAIL", "fixture@example.invalid")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {:?} failed: {}",
        args,
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim().to_string()
}

fn initialize_empty_provenance_ref(root: &Path, config: &Config) {
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .arg("mktree")
        .env("GIT_AUTHOR_NAME", "History Fixture")
        .env("GIT_AUTHOR_EMAIL", "fixture@example.invalid")
        .env("GIT_COMMITTER_NAME", "History Fixture")
        .env("GIT_COMMITTER_EMAIL", "fixture@example.invalid")
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(output.status.success());
    let empty_tree = String::from_utf8(output.stdout).unwrap();
    let notes_commit = git(
        root,
        &[
            "commit-tree",
            empty_tree.trim(),
            "-m",
            "initialize empty provenance notes",
        ],
    );
    let notes_ref = format!(
        "refs/notes/{}/provenance",
        config.provenance.git_notes_namespace
    );
    git(root, &["update-ref", &notes_ref, &notes_commit]);
}

fn initialize_empty_owner_state(root: &Path) {
    let state = root.join("state");
    let index_path = state.join("index");
    let index = TranscriptIndex::open_or_create_with_records(
        &index_path,
        Vec::new(),
        None,
        state.join("projects.json"),
        state.join("blackbox-knowledge.json"),
        state.join("blackbox-threads.json"),
        state.join("blackbox-roadmap.json"),
        std::sync::Arc::new(bbox_corpus_index::index::StaticProjectRecordsProvider::empty()),
    )
    .unwrap();
    drop(index);
    write(
        &index_path.join("_meta.json"),
        br#"{"version":2,"rows":{}}"#,
    );

    VectorStore::open(state.join("vectors")).unwrap();
    ManifestIndex::new()
        .write_atomic(&state.join("edges"))
        .unwrap();
    fs::create_dir_all(state.join("git_meta")).unwrap();

    for (name, body) in [
        ("blackbox-knowledge.json", r#"{"version":1,"entries":[]}"#),
        ("blackbox-gaps.json", r#"{"version":1,"gaps":[]}"#),
        ("blackbox-threads.json", r#"{"version":1,"threads":[]}"#),
        ("blackbox-notes.json", r#"{"version":1,"notes":[]}"#),
        ("blackbox-pins.json", r#"{"version":1,"pins":[]}"#),
        (
            "blackbox-roadmap.json",
            r#"{"version":1,"items":[],"edges":[]}"#,
        ),
    ] {
        write(&state.join(name), body.as_bytes());
    }
    for directory in [
        state.join("packets"),
        state.join("artifacts"),
        state.join("bro/badgey/proposals"),
        state.join("bro/whiteboards"),
    ] {
        fs::create_dir_all(directory).unwrap();
    }
    write(&state.join("bro/tasks.json"), b"[]");
    write(
        &state.join("bro/slack-channel-bindings.json"),
        br#"{"bindings":{}}"#,
    );
    write(
        &state.join("bro/slack-proposal-links.json"),
        br#"{"order":[],"links":{},"by_proposal":{}}"#,
    );
}

fn config(root: &Path) -> Config {
    let _guard = bbox_util::util::test_env_lock();
    let config_path = root.join("config.toml");
    write(
        &config_path,
        // `vectors_dir` is explicit: the vector root defaults to the PLATFORM
        // state directory (R33F1), and a fixture that omitted it would inventory
        // the host's real vector store.
        format!(
            "[paths]\nstate_dir = {:?}\nvectors_dir = {:?}\n",
            root.join("protected"),
            root.join("protected").join("vectors")
        )
        .as_bytes(),
    );
    config::load_with(LoadOptions {
        config_path: Some(config_path),
        ..Default::default()
    })
    .unwrap()
}

fn commit_sha(seed: u8) -> String {
    let mut sha = format!("{seed:02x}");
    while sha.len() < 40 {
        sha.push('0');
    }
    sha
}

/// Append commit documents to an existing index, mirroring the exact stored
/// field set `git_history::build_commit_doc` writes, including the two
/// path-bearing fields the generation format deliberately drops.
fn write_commit_documents(index_path: &Path, namespace: &str, messages: &[(&str, &str)]) {
    let index = Index::open_in_dir(index_path).unwrap();
    // The schema's `path_tokens` field names the custom code tokenizer, so a
    // writer cannot be built until it is registered on this Index handle.
    register_code_tokenizer(&index);
    let schema = index.schema();
    let mut writer: tantivy::IndexWriter = index.writer(15_000_000).unwrap();
    let field = |name: &str| schema.get_field(name).unwrap();
    for (sha, message) in messages {
        let mut doc = TantivyDocument::new();
        doc.add_text(field("doc_type"), "commit");
        doc.add_text(field("chunk_kind"), "git_message");
        doc.add_text(field("entity_id"), format!("commit:{namespace}:{sha}"));
        doc.add_text(field("content"), *message);
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
        doc.add_text(field("commit_sha"), *sha);
        doc.add_text(field("commit_author_name"), "History Fixture");
        doc.add_text(field("commit_author_email"), "fixture@example.invalid");
        doc.add_text(field("session_id"), "");
        doc.add_text(field("account"), "git");
        // The two path-bearing fields: a raw host path and the per-project
        // git source key. Neither may appear in a generation.
        doc.add_text(field("project"), "/host/path/that/must/not/travel");
        doc.add_text(field("file_path"), "git:some-project");
        doc.add_text(field("role"), "commit");
        doc.add_u64(field("byte_offset"), 0);
        doc.add_u64(field("is_subagent"), 0);
        writer.add_document(doc).unwrap();
    }
    writer.commit().unwrap();
    drop(writer);
}

/// Delete commit documents by entity id, so a fixture can model history LOSS
/// (a shrunk or vanished namespace) rather than only growth.
fn delete_commit_documents(index_path: &Path, namespace: &str, shas: &[&str]) {
    let index = Index::open_in_dir(index_path).unwrap();
    register_code_tokenizer(&index);
    let schema = index.schema();
    let entity_id = schema.get_field("entity_id").unwrap();
    let mut writer: tantivy::IndexWriter = index.writer(15_000_000).unwrap();
    for sha in shas {
        writer.delete_term(tantivy::Term::from_field_text(
            entity_id,
            &format!("commit:{namespace}:{sha}"),
        ));
    }
    writer.commit().unwrap();
    drop(writer);
}

// ---------------------------------------------------------------------------
// Fresh v2 fixture
// ---------------------------------------------------------------------------

struct FreshFixture {
    _directory: tempfile::TempDir,
    root: PathBuf,
    index_path: PathBuf,
    projects_path: PathBuf,
    store: ProjectCatalogStore,
}

impl FreshFixture {
    fn request(&self) -> HistoryMaterializerRequestV1 {
        HistoryMaterializerRequestV1 {
            index_path: self.index_path.clone(),
            projects_path: self.projects_path.clone(),
            vector_root: fixture_vector_root(&self.projects_path),
            scan_limits: HistoryScanLimitsV1::default(),
        }
    }
}

fn history_id(seed: u8) -> RepoHistoryId {
    let mut hex = format!("{seed:02x}");
    while hex.len() < 32 {
        hex.push('0');
    }
    RepoHistoryId::parse(format!("rh_{hex}")).unwrap()
}

/// A fresh v2 store whose catalog names the given owned namespaces and,
/// optionally, one ambiguous namespace with two candidate histories (the
/// minimum `validate_catalog` accepts).
fn fresh_fixture(owned: &[(&str, u8)], ambiguous: Option<&str>) -> FreshFixture {
    let directory = tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap();
    let state = root.join("state");
    fs::create_dir_all(&state).unwrap();
    let index_path = state.join("index");
    let index = TranscriptIndex::open_or_create_with_records(
        &index_path,
        Vec::new(),
        None,
        state.join("legacy-projects.json"),
        state.join("blackbox-knowledge.json"),
        state.join("blackbox-threads.json"),
        state.join("blackbox-roadmap.json"),
        std::sync::Arc::new(bbox_corpus_index::index::StaticProjectRecordsProvider::empty()),
    )
    .unwrap();
    drop(index);

    let projects_path = state.join("projects.json");
    let store = ProjectCatalogStore::initialize_empty(&projects_path).unwrap();
    let epoch = store.snapshot().unwrap().epoch();
    let owned = owned.to_vec();
    let ambiguous = ambiguous.map(str::to_string);
    store
        .transact(epoch, |catalog, _attachments| {
            for (namespace, seed) in &owned {
                let id = history_id(*seed);
                catalog.repo_histories.insert(
                    id.clone(),
                    RepoHistoryRecord {
                        repo_history_id: id,
                        membership_generation: 0,
                        authority: RepoHistoryAuthority::LegacyNamespace(
                            CommitNamespace::parse(namespace.to_string()).unwrap(),
                        ),
                        primary_namespace: CommitNamespace::parse(namespace.to_string()).unwrap(),
                        compatibility_namespaces: BTreeSet::new(),
                        materialization: RepoHistoryMaterialization::NotBuilt,
                    },
                );
            }
            if let Some(namespace) = &ambiguous {
                let namespace = CommitNamespace::parse(namespace.clone()).unwrap();
                catalog.ambiguous_namespaces.insert(
                    namespace.clone(),
                    AmbiguousNamespaceRecord {
                        namespace,
                        candidate_repo_history_ids: owned
                            .iter()
                            .map(|(_, seed)| history_id(*seed))
                            .collect(),
                        status: AmbiguousNamespaceStatus::Quarantined,
                        materialization: RepoHistoryQuarantineMaterialization::NotBuilt,
                    },
                );
            }
            Ok(())
        })
        .unwrap();

    FreshFixture {
        _directory: directory,
        root,
        index_path,
        projects_path,
        store,
    }
}

// ---------------------------------------------------------------------------
// Fresh-store matrix rows
// ---------------------------------------------------------------------------

#[test]
fn an_empty_index_produces_no_generations_and_stays_not_built() {
    let fixture = fresh_fixture(&[("owned-ns", 1)], None);
    let outcome = materialize_history_generations(&fixture.store, &fixture.request()).unwrap();
    assert!(outcome.namespaces.is_empty());
    assert_eq!(outcome.catalog_epoch_after, None);
    let catalog = fixture.store.snapshot().unwrap();
    assert_eq!(
        catalog.catalog().repo_histories[&history_id(1)].materialization,
        RepoHistoryMaterialization::NotBuilt
    );
}

#[test]
fn an_absent_index_is_an_empty_scan_not_a_refusal() {
    let fixture = fresh_fixture(&[("owned-ns", 1)], None);
    let mut request = fixture.request();
    request.index_path = fixture.root.join("state").join("no-such-index");
    let outcome = materialize_history_generations(&fixture.store, &request).unwrap();
    assert!(outcome.namespaces.is_empty());
}

#[test]
fn an_owned_namespace_materializes_and_advances_the_catalog() {
    let fixture = fresh_fixture(&[("owned-ns", 1)], None);
    write_commit_documents(
        &fixture.index_path,
        "owned-ns",
        &[
            (commit_sha(1).as_str(), "first commit"),
            (commit_sha(2).as_str(), "second commit"),
        ],
    );
    let before = fixture.store.snapshot().unwrap().epoch();
    let outcome = materialize_history_generations(&fixture.store, &fixture.request()).unwrap();

    assert_eq!(outcome.namespaces.len(), 1);
    let entry = &outcome.namespaces[0];
    assert!(matches!(
        entry.classification,
        NamespaceClassificationV1::Owned { .. }
    ));
    assert!(entry.generation.id.as_str().starts_with("rhg_"));
    assert_eq!(entry.generation.manifest.body.commit_document_count, 2);

    // The path-bearing fields never enter the generation: a serialized
    // generation must not carry the host path or the git source key the
    // source documents held.
    let serialized = serde_json::to_string(&entry.generation.commit_documents).unwrap();
    assert!(!serialized.contains("/host/path/that/must/not/travel"));
    assert!(!serialized.contains("git:some-project"));

    assert_eq!(outcome.catalog_epoch_after, Some(before + 1));
    let state = fixture.store.snapshot().unwrap();
    let record = &state.catalog().repo_histories[&history_id(1)];
    match &record.materialization {
        RepoHistoryMaterialization::Ready { generation_id } => {
            assert_eq!(generation_id.as_str(), entry.generation.id.as_str());
        }
        other => panic!("expected Ready, got {other:?}"),
    }
    // The advanced catalog still satisfies every validation clause, which is
    // what the P3-A `Ready`-shape clause exists to guarantee.
    state.catalog().validate().unwrap();
}

#[test]
fn a_pre_bump_schema_marker_still_scans_and_is_recorded_verbatim() {
    // The materializer exists to run BEFORE a destructive schema
    // replacement, so it must read an index whose marker is the OUTGOING
    // version while the binary already carries the incoming one. If this
    // ever starts refusing, the P3-E replacement boundary becomes
    // unreachable and history is lost at the very bump it exists to survive.
    let fixture = fresh_fixture(&[("owned-ns", 1)], None);
    write_commit_documents(
        &fixture.index_path,
        "owned-ns",
        &[(commit_sha(16).as_str(), "pre-bump commit")],
    );
    fs::write(
        fixture.index_path.join("schema_version.txt"),
        b"agentic-corpus-previous-schema",
    )
    .unwrap();

    let outcome = materialize_history_generations(&fixture.store, &fixture.request()).unwrap();
    assert_eq!(outcome.namespaces.len(), 1);
    assert_eq!(
        outcome.namespaces[0]
            .generation
            .manifest
            .body
            .source_schema_version,
        "agentic-corpus-previous-schema"
    );
}

#[test]
fn an_ambiguous_namespace_materializes_a_quarantine_generation() {
    let fixture = fresh_fixture(&[("owned-a", 1), ("owned-b", 2)], Some("ambiguous-ns"));
    write_commit_documents(
        &fixture.index_path,
        "ambiguous-ns",
        &[(commit_sha(3).as_str(), "ambiguous commit")],
    );
    let outcome = materialize_history_generations(&fixture.store, &fixture.request()).unwrap();
    let entry = &outcome.namespaces[0];
    assert!(matches!(
        entry.classification,
        NamespaceClassificationV1::Ambiguous { .. }
    ));
    assert!(entry.generation.id.as_str().starts_with("rhq_"));

    let state = fixture.store.snapshot().unwrap();
    let namespace = CommitNamespace::parse("ambiguous-ns").unwrap();
    match &state.catalog().ambiguous_namespaces[&namespace].materialization {
        RepoHistoryQuarantineMaterialization::Ready { generation_id } => {
            assert_eq!(generation_id.as_str(), entry.generation.id.as_str());
        }
        other => panic!("expected Ready, got {other:?}"),
    }
    state.catalog().validate().unwrap();
}

#[test]
fn an_unclaimed_namespace_is_manifest_owned_and_never_touches_the_catalog() {
    let fixture = fresh_fixture(&[("owned-ns", 1)], None);
    write_commit_documents(
        &fixture.index_path,
        "drifted-ns",
        &[(commit_sha(4).as_str(), "orphan commit")],
    );
    let before = fixture.store.snapshot().unwrap().epoch();
    let outcome = materialize_history_generations(&fixture.store, &fixture.request()).unwrap();

    let entry = &outcome.namespaces[0];
    assert!(matches!(
        entry.classification,
        NamespaceClassificationV1::Unclaimed { .. }
    ));
    assert!(entry.generation.id.as_str().starts_with("rhq_"));
    // No catalog mutation at all: an unclaimed namespace is unrepresentable
    // in `CatalogSnapshotV2` (an ambiguous record needs >= 2 existing
    // candidates), so the rebuild manifest is its only durable owner.
    assert_eq!(outcome.catalog_epoch_after, None);
    let state = fixture.store.snapshot().unwrap();
    assert_eq!(state.epoch(), before);
    assert!(state.catalog().ambiguous_namespaces.is_empty());

    let scan = scan_commit_documents(&fixture.index_path, HistoryScanLimitsV1::default())
        .unwrap()
        .unwrap();
    let prepared =
        prepare_rebuild_manifest(&scan, &outcome, before, "lexical-next", "vector-next").unwrap();
    assert_eq!(prepared.unclaimed_generation_ids.len(), 1);
    assert!(
        prepared
            .unclaimed_generation_ids
            .contains(entry.generation.id.as_str())
    );
    assert_eq!(prepared.namespace_inventory.len(), 1);
}

#[test]
fn a_re_run_is_idempotent_and_yields_byte_identical_generation_ids() {
    let fixture = fresh_fixture(&[("owned-ns", 1)], None);
    write_commit_documents(
        &fixture.index_path,
        "owned-ns",
        &[(commit_sha(5).as_str(), "only commit")],
    );
    let first = materialize_history_generations(&fixture.store, &fixture.request()).unwrap();
    let epoch_after_first = fixture.store.snapshot().unwrap().epoch();
    let second = materialize_history_generations(&fixture.store, &fixture.request()).unwrap();

    assert_eq!(first.generation_ids(), second.generation_ids());
    assert_eq!(
        first.namespaces[0].generation.manifest,
        second.namespaces[0].generation.manifest
    );
    // The second pass finds every record already `Ready` at the same
    // content-addressed id, so it advances nothing.
    assert_eq!(second.catalog_epoch_after, None);
    assert_eq!(fixture.store.snapshot().unwrap().epoch(), epoch_after_first);

    let generations = fixture.root.join("state").join("history-generations");
    assert!(generations.is_dir());
    // Sibling of the index, never inside it: the destructive reset removes
    // `index_path` recursively.
    assert!(!generations.starts_with(&fixture.index_path));
}

#[test]
fn an_unclaimed_re_run_is_idempotent_across_a_changed_catalog_epoch() {
    // An unclaimed generation's id hashes its `inventory_diagnostic`. If that
    // diagnostic ever carries the catalog epoch, a wall-clock value, or any
    // host detail, every pass mints a fresh id for identical history and the
    // rebuild manifest's pins go stale silently. This row is the guard.
    let fixture = fresh_fixture(&[("owned-ns", 1), ("second-ns", 2)], None);
    write_commit_documents(
        &fixture.index_path,
        "drifted-ns",
        &[(commit_sha(17).as_str(), "orphan commit")],
    );
    let first = materialize_history_generations(&fixture.store, &fixture.request()).unwrap();

    // Move the catalog epoch without touching the drifted namespace.
    let epoch = fixture.store.snapshot().unwrap().epoch();
    fixture
        .store
        .transact(epoch, |catalog, _attachments| {
            catalog
                .repo_histories
                .get_mut(&history_id(2))
                .unwrap()
                .compatibility_namespaces
                .insert(CommitNamespace::parse("second-compat").unwrap());
            Ok(())
        })
        .unwrap();
    assert_ne!(fixture.store.snapshot().unwrap().epoch(), epoch);

    let second = materialize_history_generations(&fixture.store, &fixture.request()).unwrap();
    assert_eq!(first.generation_ids(), second.generation_ids());
    assert_eq!(
        first.namespaces[0].generation.manifest,
        second.namespaces[0].generation.manifest
    );
}

#[test]
fn a_generation_carries_one_vector_input_per_commit_document() {
    let fixture = fresh_fixture(&[("owned-ns", 1)], None);
    write_commit_documents(
        &fixture.index_path,
        "owned-ns",
        &[
            (commit_sha(6).as_str(), "alpha"),
            (commit_sha(7).as_str(), "beta"),
            (commit_sha(8).as_str(), "gamma"),
        ],
    );
    let outcome = materialize_history_generations(&fixture.store, &fixture.request()).unwrap();
    let generation = &outcome.namespaces[0].generation;
    assert_eq!(generation.manifest.body.commit_document_count, 3);
    assert_eq!(generation.manifest.body.vector_input_count, 3);
    let document_entities = generation
        .commit_documents
        .iter()
        .map(|document| document.entity_id.clone())
        .collect::<BTreeSet<_>>();
    let input_entities = generation
        .vector_inputs
        .iter()
        .map(|input| input.entity_id.clone())
        .collect::<BTreeSet<_>>();
    assert_eq!(document_entities, input_entities);
    // No truncated messages here, so nothing was skipped; the raw-vs-truncated
    // hash divergence carve-out is exercised by the migrated matrix below.
    assert_eq!(generation.manifest.body.truncated_message_count, 0);
    assert!(outcome.namespaces_with_truncated_messages.is_empty());
}

#[test]
fn gc_roots_pin_catalog_and_manifest_generations_and_refuse_their_removal() {
    let fixture = fresh_fixture(&[("owned-ns", 1)], None);
    write_commit_documents(
        &fixture.index_path,
        "owned-ns",
        &[(commit_sha(9).as_str(), "owned commit")],
    );
    write_commit_documents(
        &fixture.index_path,
        "drifted-ns",
        &[(commit_sha(10).as_str(), "orphan commit")],
    );
    let epoch = fixture.store.snapshot().unwrap().epoch();
    let outcome = materialize_history_generations(&fixture.store, &fixture.request()).unwrap();
    let scan = scan_commit_documents(&fixture.index_path, HistoryScanLimitsV1::default())
        .unwrap()
        .unwrap();
    let generation_store = HistoryGenerationStore::open_for_index(&fixture.index_path).unwrap();
    let manifest = generation_store
        .write_prepared_rebuild_manifest(
            prepare_rebuild_manifest(&scan, &outcome, epoch, "lexical-next", "vector-next")
                .unwrap(),
        )
        .unwrap();

    let state = fixture.store.snapshot().unwrap();
    let roots = history_generation_gc_roots(state.catalog(), std::slice::from_ref(&manifest));
    // Both generations are roots: the owned one through the catalog record,
    // the unclaimed one only through the manifest.
    assert_eq!(roots.len(), 2);
    assert!(
        plan_history_generation_gc(&generation_store, &roots)
            .unwrap()
            .is_empty()
    );
    for id in generation_store.list().unwrap() {
        let error = generation_store
            .remove_unreferenced(&id, &roots)
            .unwrap_err();
        assert_eq!(error.code(), "error.history_generation_referenced");
    }

    // Drop the manifest and the unclaimed generation loses its only root.
    generation_store.roll_back_rebuild_manifest().unwrap();
    let roots = history_generation_gc_roots(state.catalog(), &[]);
    assert_eq!(roots.len(), 1);
    let sweepable = plan_history_generation_gc(&generation_store, &roots).unwrap();
    assert_eq!(sweepable.len(), 1);
    generation_store
        .remove_unreferenced(&sweepable[0], &roots)
        .unwrap();
    assert_eq!(generation_store.list().unwrap().len(), 1);
}

// ---------------------------------------------------------------------------
// Manifest recovery matrix
// ---------------------------------------------------------------------------

/// Fails at exactly one fault point, so a crash can be positioned instead of
/// simulated by killing a process.
#[derive(Debug)]
struct FailAt(HistoryFaultPoint);

impl HistoryGenerationIo for FailAt {
    fn checkpoint(&self, point: HistoryFaultPoint) -> Result<(), HistoryGenerationError> {
        if point == self.0 {
            return Err(HistoryGenerationError::new(
                "error.history_generation_injected_fault",
                format!("injected fault at {point:?}"),
            ));
        }
        Ok(())
    }
}

/// Row 1: crash BEFORE the prepared manifest is written.
///
/// Arm: neither. There is no manifest to classify, the source index is
/// untouched, and the last-good lexical and vector views stay selected.
#[test]
fn recovery_row_crash_before_prepared_has_nothing_to_recover() {
    let fixture = fresh_fixture(&[("owned-ns", 1)], None);
    write_commit_documents(
        &fixture.index_path,
        "owned-ns",
        &[(commit_sha(11).as_str(), "commit")],
    );
    let io = Arc::new(FailAt(HistoryFaultPoint::GenerationManifestWrite));
    let error = materialize_history_generations_with_io(&fixture.store, &fixture.request(), io)
        .unwrap_err();
    assert_eq!(error.code(), "error.history_generation_injected_fault");

    let generation_store = HistoryGenerationStore::open_for_index(&fixture.index_path).unwrap();
    assert_eq!(
        classify_rebuild_recovery(&generation_store, &fixture.index_path).unwrap(),
        RepoHistoryRebuildRecoveryV1::NoManifest
    );
    // Nothing was advanced, so the catalog is untouched.
    assert_eq!(
        fixture.store.snapshot().unwrap().catalog().repo_histories[&history_id(1)].materialization,
        RepoHistoryMaterialization::NotBuilt
    );
}

/// Row 2: crash BETWEEN prepared and committed, with the source index still
/// intact.
///
/// Arm: ROLL BACK. The destructive drop never ran, so a last-good index
/// exists to return to; recovery deletes the manifest and leaves it selected.
#[test]
fn recovery_row_crash_between_prepared_and_committed_with_index_intact_rolls_back() {
    let fixture = fresh_fixture(&[("owned-ns", 1)], None);
    write_commit_documents(
        &fixture.index_path,
        "owned-ns",
        &[(commit_sha(12).as_str(), "commit")],
    );
    let epoch = fixture.store.snapshot().unwrap().epoch();
    let outcome = materialize_history_generations(&fixture.store, &fixture.request()).unwrap();
    let scan = scan_commit_documents(&fixture.index_path, HistoryScanLimitsV1::default())
        .unwrap()
        .unwrap();
    let generation_store = HistoryGenerationStore::open_for_index(&fixture.index_path).unwrap();
    generation_store
        .write_prepared_rebuild_manifest(
            prepare_rebuild_manifest(&scan, &outcome, epoch, "lexical-next", "vector-next")
                .unwrap(),
        )
        .unwrap();

    let recovery = classify_rebuild_recovery(&generation_store, &fixture.index_path).unwrap();
    let RepoHistoryRebuildRecoveryV1::RollBackPrepared { manifest } = recovery else {
        panic!("intact index must classify as roll back, got {recovery:?}");
    };
    assert!(!manifest.pinned_generation_ids().is_empty());
    generation_store.roll_back_rebuild_manifest().unwrap();
    assert!(generation_store.read_rebuild_manifest().unwrap().is_none());
    // The generations survive the rollback: they are immutable and
    // content-addressed, so re-preparing re-derives the same ids.
    assert_eq!(generation_store.list().unwrap().len(), 1);
}

/// Row 3: crash BETWEEN prepared and committed, AFTER the destructive drop.
///
/// Arm: RESUME. There is no last-good index left to roll back to, so the
/// replacement must be re-executed from the manifest's pinned generations.
#[test]
fn recovery_row_crash_between_prepared_and_committed_after_the_drop_resumes() {
    let fixture = fresh_fixture(&[("owned-ns", 1)], None);
    write_commit_documents(
        &fixture.index_path,
        "owned-ns",
        &[(commit_sha(13).as_str(), "commit")],
    );
    let epoch = fixture.store.snapshot().unwrap().epoch();
    let outcome = materialize_history_generations(&fixture.store, &fixture.request()).unwrap();
    let scan = scan_commit_documents(&fixture.index_path, HistoryScanLimitsV1::default())
        .unwrap()
        .unwrap();
    let generation_store = HistoryGenerationStore::open_for_index(&fixture.index_path).unwrap();
    generation_store
        .write_prepared_rebuild_manifest(
            prepare_rebuild_manifest(&scan, &outcome, epoch, "lexical-next", "vector-next")
                .unwrap(),
        )
        .unwrap();

    // The destructive step: exactly what `reset_index_on_schema_mismatch`
    // does. The generations root is a sibling, so it survives.
    fs::remove_dir_all(&fixture.index_path).unwrap();
    assert!(generation_store.root().is_dir());

    let recovery = classify_rebuild_recovery(&generation_store, &fixture.index_path).unwrap();
    let RepoHistoryRebuildRecoveryV1::ResumePrepared { manifest } = recovery else {
        panic!("dropped index must classify as resume, got {recovery:?}");
    };
    assert_eq!(manifest.pinned_generation_ids().len(), 1);
    // The pinned generation is still readable, which is what makes resume
    // possible without a checkout.
    let id = generation_store.list().unwrap().into_iter().next().unwrap();
    assert_eq!(
        generation_store
            .load(&id)
            .unwrap()
            .manifest
            .body
            .commit_document_count,
        1
    );
}

/// Row 3b: a fresh index under a DIFFERENT schema marker also reads as
/// post-drop, because the reset recreates the directory before the rebuild
/// finishes.
///
/// Arm: RESUME.
#[test]
fn recovery_row_prepared_with_a_replaced_index_marker_resumes() {
    let fixture = fresh_fixture(&[("owned-ns", 1)], None);
    write_commit_documents(
        &fixture.index_path,
        "owned-ns",
        &[(commit_sha(14).as_str(), "commit")],
    );
    let epoch = fixture.store.snapshot().unwrap().epoch();
    let outcome = materialize_history_generations(&fixture.store, &fixture.request()).unwrap();
    let scan = scan_commit_documents(&fixture.index_path, HistoryScanLimitsV1::default())
        .unwrap()
        .unwrap();
    let generation_store = HistoryGenerationStore::open_for_index(&fixture.index_path).unwrap();
    generation_store
        .write_prepared_rebuild_manifest(
            prepare_rebuild_manifest(&scan, &outcome, epoch, "lexical-next", "vector-next")
                .unwrap(),
        )
        .unwrap();
    fs::write(
        fixture.index_path.join("schema_version.txt"),
        b"some-future-schema",
    )
    .unwrap();

    assert!(matches!(
        classify_rebuild_recovery(&generation_store, &fixture.index_path).unwrap(),
        RepoHistoryRebuildRecoveryV1::ResumePrepared { .. }
    ));
}

/// Row 4: crash AFTER the committed manifest is written.
///
/// Arm: neither rollback nor resume. The replacement is done and its views
/// are verified; recovery is a no-op that keeps the generations pinned.
#[test]
fn recovery_row_crash_after_committed_is_a_no_op() {
    let fixture = fresh_fixture(&[("owned-ns", 1)], None);
    write_commit_documents(
        &fixture.index_path,
        "owned-ns",
        &[(commit_sha(15).as_str(), "commit")],
    );
    let epoch = fixture.store.snapshot().unwrap().epoch();
    let outcome = materialize_history_generations(&fixture.store, &fixture.request()).unwrap();
    let scan = scan_commit_documents(&fixture.index_path, HistoryScanLimitsV1::default())
        .unwrap()
        .unwrap();
    let generation_store = HistoryGenerationStore::open_for_index(&fixture.index_path).unwrap();
    generation_store
        .write_prepared_rebuild_manifest(
            prepare_rebuild_manifest(&scan, &outcome, epoch, "lexical-next", "vector-next")
                .unwrap(),
        )
        .unwrap();
    generation_store
        .commit_rebuild_manifest(RepoHistoryRebuildCommittedV1 {
            verified_lexical_view: "lexical-next".to_string(),
            verified_vector_view: "vector-next".to_string(),
            resulting_catalog_epoch: epoch + 1,
            vector_inventory: Vec::new(),
        })
        .unwrap();
    fs::remove_dir_all(&fixture.index_path).unwrap();

    let recovery = classify_rebuild_recovery(&generation_store, &fixture.index_path).unwrap();
    let RepoHistoryRebuildRecoveryV1::AlreadyCommitted { manifest } = recovery else {
        panic!("committed manifest must classify as already committed, got {recovery:?}");
    };
    assert_eq!(
        manifest.committed.as_ref().unwrap().resulting_catalog_epoch,
        epoch + 1
    );
    let state = fixture.store.snapshot().unwrap();
    assert_eq!(
        history_generation_gc_roots(state.catalog(), std::slice::from_ref(&manifest)).len(),
        1
    );
}

// ---------------------------------------------------------------------------
// Migrated-root matrix: proof against the persisted Phase 1 asset
// ---------------------------------------------------------------------------

struct MigratedFixture {
    _directory: tempfile::TempDir,
    rehearsal_root: PathBuf,
    namespace: String,
    store: ProjectCatalogStore,
}

impl MigratedFixture {
    fn index_path(&self) -> PathBuf {
        self.rehearsal_root.join("state").join("index")
    }

    fn projects_path(&self) -> PathBuf {
        self.rehearsal_root.join("state").join("projects.json")
    }

    fn request(&self) -> HistoryMaterializerRequestV1 {
        HistoryMaterializerRequestV1 {
            index_path: self.index_path(),
            projects_path: self.projects_path(),
            vector_root: fixture_vector_root(&self.projects_path()),
            scan_limits: HistoryScanLimitsV1::default(),
        }
    }
}

/// A genuinely migrated v2 root, produced by the real facade rehearsal, with
/// one project, one proved commit namespace, and the persisted
/// `LegacyCommitNamespaceInventoryAssetV1` the materializer proves against.
fn migrated_fixture(commits: &[(&str, &str)]) -> MigratedFixture {
    let directory = tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap();
    let config = config(&root);
    let rehearsal_root = root.join("rehearsal");
    fs::create_dir_all(&rehearsal_root).unwrap();

    let namespace = "neutral-history-repository".to_string();
    let checkout = rehearsal_root.join("checkouts").join("history-checkout");
    fs::create_dir_all(&checkout).unwrap();
    git(&checkout, &["init", "-q"]);
    git(&checkout, &["checkout", "-qb", "main"]);
    write(
        &checkout.join(".bbox/config.toml"),
        format!("[project]\nrepo_id = \"{namespace}\"\n").as_bytes(),
    );
    git(&checkout, &["add", ".bbox"]);
    git(&checkout, &["commit", "-qm", "seed history fixture"]);
    initialize_empty_provenance_ref(&checkout, &config);

    initialize_empty_owner_state(&rehearsal_root);
    let state = rehearsal_root.join("state");
    // The inventory adapters refuse a missing code-source store
    // (`code_source_store_missing`); this fixture needs the store to exist,
    // not to hold any collected generation.
    CodeSourceStore::open(
        state.join("code-sources"),
        project_catalog_migration_store_limits(&config),
    )
    .unwrap();
    write_commit_documents(&state.join("index"), &namespace, commits);

    let project = ProjectId::parse("neutral-history-project").unwrap();
    write(
        &state.join("projects.json"),
        serde_json::to_vec_pretty(&serde_json::json!({
            "version": 1,
            "projects": [{
                "project_id": project,
                "repo_id": namespace,
                "canonical_path": checkout,
                "registered_at": "2026-01-02T03:04:05Z",
                "is_git_repo": true,
                "languages": [],
                "aliases": []
            }]
        }))
        .unwrap()
        .as_slice(),
    );

    let rehearsal =
        ProjectCatalogMigrationResolvedLayoutV1::from_rehearsal_root(&rehearsal_root, &config)
            .unwrap();
    let protected_root = root.join("protected");
    fs::create_dir_all(&protected_root).unwrap();
    let protected = ProjectCatalogMigrationResolvedLayoutV1::from_config(
        &config,
        ProjectCatalogMigrationLayoutOverridesV1 {
            projects_path: Some(protected_root.join("projects.json")),
            state_dir: Some(protected_root),
        },
    )
    .unwrap();
    let review = rehearsal_root.join("review");
    let report_path = review.join("report.json");
    let resolution_path = review.join("resolution.json");
    let preflight =
        ProjectCatalogMigrationFacadeV1::preflight(ProjectCatalogMigrationPreflightRequestV1 {
            layout: rehearsal.clone(),
            report_path: report_path.clone(),
            resolution_path: resolution_path.clone(),
            sensitive_report_path: None,
        })
        .unwrap();
    assert_eq!(
        preflight.receipt.status,
        ProjectCatalogMigrationStatusV1::Clean,
        "single-project history fixture must preflight clean: {:?}",
        decode_migration_report_v1(&fs::read(&report_path).unwrap())
            .unwrap()
            .refusals
    );
    let applied =
        ProjectCatalogMigrationFacadeV1::apply_rehearsal(ProjectCatalogMigrationApplyRequestV1 {
            rehearsal_layout: rehearsal,
            protected_layout: protected,
            report_path,
            resolution_path,
        })
        .unwrap();
    assert_eq!(
        applied.receipt.outcome,
        ProjectCatalogMigrationApplyOutcomeV1::Applied
    );

    let store = ProjectCatalogStore::open_existing(state.join("projects.json")).unwrap();
    MigratedFixture {
        _directory: directory,
        rehearsal_root,
        namespace,
        store,
    }
}

/// A migrated root whose repo-history record owns a POPULATED COMPATIBILITY
/// namespace in addition to its primary one.
///
/// How this shape is reached, and why it is built this way:
///
/// The v1 importer cannot emit it. `inventoried_group_namespaces` only
/// admits namespaces the inventory ATTRIBUTED, and attribution is `Proved`
/// only for a namespace equal to some project's committed authority; but a
/// history group holding two distinct published authorities is refused
/// outright with `conflicting_published_authorities`
/// (`project_catalog_migration.rs`, authority selection). So a migrated group
/// has exactly one attributed namespace and `planned_compatibility_namespaces`
/// stays empty. A genuine two-project monorepo fixture was tried first and
/// refuses at preflight for exactly that reason.
///
/// Compatibility namespaces are nonetheless a designed, REACHABLE state: the
/// runtime admin path adds them (relpath move / authority change), the
/// governing design's namespace-resolution op will add them, and
/// `validate_catalog` accepts them. This fixture therefore reaches the shape
/// the way production does - a real migrated root plus a catalog transaction -
/// and then gives the compatibility namespace its own commit documents.
///
/// The resulting pass runs in DRIFT proof mode (the index moved after the
/// asset was captured). That is not a limitation of the row: the
/// primary-versus-compatibility routing in `advance_catalog_materialization`
/// never consults the proof mode, so the assertions below hold identically in
/// either mode.
fn migrated_fixture_with_compatibility_namespace() -> (MigratedFixture, RepoHistoryId, String) {
    let fixture = migrated_fixture(&[(commit_sha(61).as_str(), "primary namespace commit")]);
    let compatibility = "neutral-legacy-compatibility".to_string();
    let state = fixture.store.snapshot().unwrap();
    let history_id = state
        .catalog()
        .repo_histories
        .keys()
        .next()
        .cloned()
        .expect("the migrated fixture has one repo history record");
    let epoch = state.epoch();
    let namespace_to_add = compatibility.clone();
    let target = history_id.clone();
    fixture
        .store
        .transact(epoch, move |catalog, _attachments| {
            catalog
                .repo_histories
                .get_mut(&target)
                .unwrap()
                .compatibility_namespaces
                .insert(CommitNamespace::parse(namespace_to_add).unwrap());
            Ok(())
        })
        .unwrap();
    write_commit_documents(
        &fixture.index_path(),
        &compatibility,
        &[(commit_sha(62).as_str(), "compatibility namespace commit")],
    );
    (fixture, history_id, compatibility)
}

#[test]
fn a_record_with_a_compatibility_namespace_advances_only_its_primary() {
    let (fixture, history_id, compatibility) = migrated_fixture_with_compatibility_namespace();
    let epoch = fixture.store.snapshot().unwrap().epoch();
    let outcome = materialize_history_generations(&fixture.store, &fixture.request()).unwrap();
    assert_eq!(outcome.namespaces.len(), 2);

    let entry_for = |name: &str| {
        outcome
            .namespaces
            .iter()
            .find(|entry| entry.namespace.as_str() == name)
            .unwrap_or_else(|| panic!("{name} materializes"))
    };
    let primary_entry = entry_for(&fixture.namespace);
    let compatibility_entry = entry_for(&compatibility);
    assert!(matches!(
        primary_entry.classification,
        NamespaceClassificationV1::Owned { .. }
    ));
    assert!(matches!(
        compatibility_entry.classification,
        NamespaceClassificationV1::OwnedCompatibility { .. }
    ));
    // Both are genuinely owned history, so both carry `rhg_` ids; a
    // compatibility namespace is not quarantined and must not look like it.
    assert!(primary_entry.generation.id.as_str().starts_with("rhg_"));
    assert!(
        compatibility_entry
            .generation
            .id
            .as_str()
            .starts_with("rhg_")
    );
    compatibility_entry.generation.validate().unwrap();
    assert_eq!(
        compatibility_entry
            .generation
            .manifest
            .body
            .commit_document_count,
        1
    );

    // Exactly ONE Ready id in the whole catalog, and it is the primary's.
    let after = fixture.store.snapshot().unwrap();
    after.catalog().validate().unwrap();
    let ready = after
        .catalog()
        .repo_histories
        .values()
        .filter_map(|record| match &record.materialization {
            RepoHistoryMaterialization::Ready { generation_id } => Some(generation_id.clone()),
            RepoHistoryMaterialization::NotBuilt => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(ready.len(), 1);
    assert_eq!(ready[0].as_str(), primary_entry.generation.id.as_str());
    assert!(matches!(
        &after.catalog().repo_histories[&history_id].materialization,
        RepoHistoryMaterialization::Ready { generation_id }
            if generation_id.as_str() == primary_entry.generation.id.as_str()
    ));

    // The compatibility generation's ONLY durable owner is the manifest.
    let scan = scan_commit_documents(&fixture.index_path(), HistoryScanLimitsV1::default())
        .unwrap()
        .unwrap();
    let prepared =
        prepare_rebuild_manifest(&scan, &outcome, epoch, "lexical-next", "vector-next").unwrap();
    assert_eq!(
        prepared.owned_generation_ids,
        [primary_entry.generation.id.as_str().to_string()]
            .into_iter()
            .collect()
    );
    assert_eq!(
        prepared.compatibility_generation_ids,
        [compatibility_entry.generation.id.as_str().to_string()]
            .into_iter()
            .collect()
    );
    let generation_store = HistoryGenerationStore::open_for_index(&fixture.index_path()).unwrap();
    let manifest = generation_store
        .write_prepared_rebuild_manifest(prepared)
        .unwrap();

    // GC: both are roots, the compatibility one SOLELY through the manifest.
    // Dropping the manifest leaves the catalog naming only the primary, which
    // is precisely why the manifest must pin the other.
    let roots = history_generation_gc_roots(after.catalog(), std::slice::from_ref(&manifest));
    assert_eq!(roots.len(), 2);
    assert!(roots.contains(compatibility_entry.generation.id.as_str()));
    assert!(
        plan_history_generation_gc(&generation_store, &roots)
            .unwrap()
            .is_empty()
    );
    let catalog_only = history_generation_gc_roots(after.catalog(), &[]);
    assert_eq!(catalog_only.len(), 1);
    assert!(!catalog_only.contains(compatibility_entry.generation.id.as_str()));
}

#[test]
fn a_compatibility_namespace_re_run_is_idempotent_and_byte_identical() {
    let (fixture, _, _) = migrated_fixture_with_compatibility_namespace();
    let first = materialize_history_generations(&fixture.store, &fixture.request()).unwrap();
    let epoch = fixture.store.snapshot().unwrap().epoch();
    let second = materialize_history_generations(&fixture.store, &fixture.request()).unwrap();
    assert_eq!(first.generation_ids(), second.generation_ids());
    assert_eq!(first.generation_ids().len(), 2);
    for (left, right) in first.namespaces.iter().zip(second.namespaces.iter()) {
        assert_eq!(left.generation.manifest, right.generation.manifest);
        assert_eq!(left.classification, right.classification);
    }
    // The compatibility namespace advances nothing and the primary is already
    // Ready at the same content-addressed id, so the second pass is a no-op.
    assert_eq!(second.catalog_epoch_after, None);
    assert_eq!(fixture.store.snapshot().unwrap().epoch(), epoch);
}

fn asset_path(fixture: &MigratedFixture) -> PathBuf {
    let assets = fixture
        .rehearsal_root
        .join("state")
        .join("project-catalog-migration-assets");
    let transaction_id = match &fixture.store.snapshot().unwrap().catalog().origin {
        bbox_corpus_core::project_catalog::CatalogOriginV2::MigratedV1 { transaction_id } => {
            transaction_id.clone()
        }
        other => panic!("fixture must be migrated, got {other:?}"),
    };
    let asset =
        load_legacy_commit_namespace_inventory_asset(&fixture.projects_path(), &transaction_id)
            .unwrap()
            .expect("migrated fixture installs the namespace inventory asset");
    assert!(
        asset
            .rows
            .iter()
            .any(|row| row.namespace.as_str() == fixture.namespace),
        "asset must record the fixture namespace"
    );
    fs::read_dir(&assets)
        .unwrap()
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .find(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .map(|name| {
                    name.ends_with(".immutable")
                        && fs::read(path)
                            .map(|bytes| {
                                serde_json::from_slice::<serde_json::Value>(&bytes)
                                    .ok()
                                    .and_then(|value| value.get("rows").cloned())
                                    .is_some()
                            })
                            .unwrap_or(false)
                })
                .unwrap_or(false)
        })
        .expect("namespace inventory asset file exists")
}

#[test]
fn a_migrated_root_proves_its_namespace_against_the_persisted_asset() {
    let fixture = migrated_fixture(&[
        (commit_sha(21).as_str(), "migrated first"),
        (commit_sha(22).as_str(), "migrated second"),
    ]);
    let before = fixture.store.snapshot().unwrap().epoch();
    let outcome = materialize_history_generations(&fixture.store, &fixture.request()).unwrap();

    // Equality mode must be genuinely REACHED here, not merely satisfied:
    // an unchanged-since-migration index is the one shape where the asset is
    // an exact description, and Phase 6's offline rebuild depends on this
    // mode being reachable at all. Drift mode would also pass the assertions
    // below, so the mode itself is asserted first.
    assert_eq!(outcome.proof_mode, HistoryProofModeV1::Equality);
    assert_eq!(
        outcome.recorded_source_index_fingerprint,
        outcome.observed_source_index_fingerprint
    );
    assert!(outcome.recorded_source_index_fingerprint.is_some());
    assert_eq!(outcome.namespaces.len(), 1);
    let entry = &outcome.namespaces[0];
    assert_eq!(entry.namespace.as_str(), fixture.namespace);
    assert!(matches!(
        entry.classification,
        NamespaceClassificationV1::Owned { .. }
    ));
    assert_eq!(entry.generation.manifest.body.commit_document_count, 2);
    // An unchanged-since-migration index derives exactly the recorded vector
    // keys, so the structured residue report is empty, not merely unlogged.
    assert!(outcome.dropped_vector_residue.is_empty());
    assert_eq!(outcome.catalog_epoch_after, Some(before + 1));

    let state = fixture.store.snapshot().unwrap();
    state.catalog().validate().unwrap();
    let ready = state
        .catalog()
        .repo_histories
        .values()
        .filter_map(|record| match &record.materialization {
            RepoHistoryMaterialization::Ready { generation_id } => Some(generation_id.clone()),
            RepoHistoryMaterialization::NotBuilt => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(ready.len(), 1);
    assert_eq!(ready[0].as_str(), entry.generation.id.as_str());
}

// --- drift mode -----------------------------------------------------------
//
// The persisted asset is a POINT-IN-TIME migration record. A root that has
// been live-indexed since migration legitimately outgrows it, so equality is
// only the right contract while the index is unchanged. These rows replace
// two earlier ones that asserted refusal on exactly the growth a live root
// produces; that assertion was what the live forced-replacement smoke hit.

#[test]
fn drift_mode_a_recorded_namespace_that_grew_after_capture_materializes() {
    let fixture = migrated_fixture(&[(commit_sha(23).as_str(), "migrated first")]);
    write_commit_documents(
        &fixture.index_path(),
        &fixture.namespace,
        &[(commit_sha(24).as_str(), "commit ingested after migration")],
    );
    let outcome = materialize_history_generations(&fixture.store, &fixture.request()).unwrap();
    assert_eq!(outcome.proof_mode, HistoryProofModeV1::Drift);
    assert_ne!(
        outcome.recorded_source_index_fingerprint,
        outcome.observed_source_index_fingerprint
    );
    assert_eq!(outcome.namespaces.len(), 1);
    // Append-only growth carries the full observed set, not the recorded one.
    assert_eq!(
        outcome.namespaces[0]
            .generation
            .manifest
            .body
            .commit_document_count,
        2
    );
    let state = fixture.store.snapshot().unwrap();
    state.catalog().validate().unwrap();
}

#[test]
fn drift_mode_a_namespace_minted_after_migration_classifies_with_no_asset_constraint() {
    // The live-smoke shape: a `local_` history minted BY the migration whose
    // commit documents were ingested afterwards by live walks. It is
    // catalog-OWNED and absent from the asset, and it must materialize.
    let fixture = migrated_fixture(&[(commit_sha(25).as_str(), "migrated first")]);
    let minted = "local_8889982e025a4390a22acd07fc69e00d";
    // Modelled as its OWN repo-history record whose PRIMARY namespace is the
    // minted one, which is how the migration mints a local history: one
    // record, one namespace. (Hanging it off an existing record as a
    // COMPATIBILITY namespace is a different and currently unsupported
    // shape - see the note in the report - and is deliberately not what this
    // row exercises.)
    let epoch = fixture.store.snapshot().unwrap().epoch();
    fixture
        .store
        .transact(epoch, |catalog, _attachments| {
            let id = RepoHistoryId::parse("rh_000000000000000000000000000000aa").unwrap();
            catalog.repo_histories.insert(
                id.clone(),
                RepoHistoryRecord {
                    repo_history_id: id,
                    membership_generation: 0,
                    authority: RepoHistoryAuthority::LegacyNamespace(
                        CommitNamespace::parse(minted).unwrap(),
                    ),
                    primary_namespace: CommitNamespace::parse(minted).unwrap(),
                    compatibility_namespaces: BTreeSet::new(),
                    materialization: RepoHistoryMaterialization::NotBuilt,
                },
            );
            Ok(())
        })
        .unwrap();
    write_commit_documents(
        &fixture.index_path(),
        minted,
        &[(commit_sha(26).as_str(), "ingested after migration")],
    );

    let outcome = materialize_history_generations(&fixture.store, &fixture.request()).unwrap();
    assert_eq!(outcome.proof_mode, HistoryProofModeV1::Drift);
    let minted_entry = outcome
        .namespaces
        .iter()
        .find(|entry| entry.namespace.as_str() == minted)
        .expect("the post-migration namespace materializes");
    assert!(matches!(
        minted_entry.classification,
        NamespaceClassificationV1::Owned { .. }
    ));
    fixture
        .store
        .snapshot()
        .unwrap()
        .catalog()
        .validate()
        .unwrap();
}

#[test]
fn drift_mode_a_recorded_namespace_that_shrank_refuses() {
    let fixture = migrated_fixture(&[
        (commit_sha(41).as_str(), "migrated first"),
        (commit_sha(42).as_str(), "migrated second"),
        (commit_sha(43).as_str(), "migrated third"),
    ]);
    delete_commit_documents(
        &fixture.index_path(),
        &fixture.namespace,
        &[commit_sha(43).as_str()],
    );
    let error = materialize_history_generations(&fixture.store, &fixture.request()).unwrap_err();
    assert_eq!(error.code(), "error.history_commitment_mismatch");
    assert!(
        error.message().contains("cannot shrink"),
        "unexpected message: {}",
        error.message()
    );
    // Loss evidence keeps the replacement refused with last-good intact.
    let state = fixture.store.snapshot().unwrap();
    assert!(
        state
            .catalog()
            .repo_histories
            .values()
            .all(|record| { record.materialization == RepoHistoryMaterialization::NotBuilt })
    );
}

#[test]
fn drift_mode_a_recorded_namespace_that_vanished_refuses() {
    let fixture = migrated_fixture(&[
        (commit_sha(44).as_str(), "migrated first"),
        (commit_sha(45).as_str(), "migrated second"),
    ]);
    delete_commit_documents(
        &fixture.index_path(),
        &fixture.namespace,
        &[commit_sha(44).as_str(), commit_sha(45).as_str()],
    );
    let error = materialize_history_generations(&fixture.store, &fixture.request()).unwrap_err();
    assert_eq!(error.code(), "error.history_commitment_mismatch");
    assert!(
        error.message().contains("absent from the index"),
        "unexpected message: {}",
        error.message()
    );
}

#[test]
fn the_prepared_manifest_records_the_proof_mode_and_both_fingerprints() {
    // Drift arm.
    let fixture = migrated_fixture(&[(commit_sha(46).as_str(), "migrated first")]);
    write_commit_documents(
        &fixture.index_path(),
        &fixture.namespace,
        &[(commit_sha(47).as_str(), "ingested after migration")],
    );
    let epoch = fixture.store.snapshot().unwrap().epoch();
    let outcome = materialize_history_generations(&fixture.store, &fixture.request()).unwrap();
    let scan = scan_commit_documents(&fixture.index_path(), HistoryScanLimitsV1::default())
        .unwrap()
        .unwrap();
    let prepared =
        prepare_rebuild_manifest(&scan, &outcome, epoch, "lexical-next", "vector-next").unwrap();
    assert_eq!(prepared.proof_mode, HistoryProofModeV1::Drift);
    assert!(prepared.recorded_source_index_fingerprint.is_some());
    assert!(prepared.observed_source_index_fingerprint.is_some());
    assert_ne!(
        prepared.recorded_source_index_fingerprint,
        prepared.observed_source_index_fingerprint
    );
    let generation_store = HistoryGenerationStore::open_for_index(&fixture.index_path()).unwrap();
    let manifest = generation_store
        .write_prepared_rebuild_manifest(prepared)
        .unwrap();
    let reloaded = generation_store.read_rebuild_manifest().unwrap().unwrap();
    assert_eq!(reloaded, manifest);
    assert_eq!(reloaded.prepared.proof_mode, HistoryProofModeV1::Drift);

    // Equality arm.
    let untouched = migrated_fixture(&[(commit_sha(48).as_str(), "migrated first")]);
    let epoch = untouched.store.snapshot().unwrap().epoch();
    let outcome = materialize_history_generations(&untouched.store, &untouched.request()).unwrap();
    let scan = scan_commit_documents(&untouched.index_path(), HistoryScanLimitsV1::default())
        .unwrap()
        .unwrap();
    let prepared =
        prepare_rebuild_manifest(&scan, &outcome, epoch, "lexical-next", "vector-next").unwrap();
    assert_eq!(prepared.proof_mode, HistoryProofModeV1::Equality);
    assert_eq!(
        prepared.recorded_source_index_fingerprint,
        prepared.observed_source_index_fingerprint
    );
    assert!(prepared.recorded_source_index_fingerprint.is_some());
}

#[test]
fn a_migrated_root_without_its_asset_refuses_with_inventory_missing() {
    let fixture = migrated_fixture(&[(commit_sha(27).as_str(), "migrated first")]);
    let asset = asset_path(&fixture);
    fs::remove_file(&asset).unwrap();
    let error = materialize_history_generations(&fixture.store, &fixture.request()).unwrap_err();
    assert_eq!(error.code(), "error.history_inventory_missing");
    let state = fixture.store.snapshot().unwrap();
    assert!(
        state
            .catalog()
            .repo_histories
            .values()
            .all(|record| { record.materialization == RepoHistoryMaterialization::NotBuilt })
    );
}

#[test]
fn a_migrated_root_refuses_an_asset_that_disagrees_with_its_content_hash() {
    let fixture = migrated_fixture(&[(commit_sha(28).as_str(), "migrated first")]);
    let asset = asset_path(&fixture);
    let mut bytes = fs::read(&asset).unwrap();
    bytes.push(b' ');
    fs::write(&asset, bytes).unwrap();
    let error = materialize_history_generations(&fixture.store, &fixture.request()).unwrap_err();
    assert_eq!(
        error.code(),
        "error.project_catalog_migration_artifact_identity"
    );
}

#[test]
fn a_migrated_root_materializes_idempotently_across_two_passes() {
    let fixture = migrated_fixture(&[
        (commit_sha(29).as_str(), "migrated first"),
        (commit_sha(30).as_str(), "migrated second"),
    ]);
    let first = materialize_history_generations(&fixture.store, &fixture.request()).unwrap();
    let epoch = fixture.store.snapshot().unwrap().epoch();
    let second = materialize_history_generations(&fixture.store, &fixture.request()).unwrap();
    assert_eq!(first.generation_ids(), second.generation_ids());
    assert_eq!(second.catalog_epoch_after, None);
    assert_eq!(fixture.store.snapshot().unwrap().epoch(), epoch);
}

// ---------------------------------------------------------------------------
// Identity across schema generations (D-039)
// ---------------------------------------------------------------------------

/// Append commit documents whose stored fields mirror EXACT generation rows,
/// so a scan of this index reconstructs byte-identical
/// `HistoryCommitDocumentV1` values. Used by the composition rows below to
/// model an index whose content equals an already-materialized generation.
fn write_commit_documents_from_rows(index_path: &Path, rows: &[HistoryCommitDocumentV1]) {
    let index = Index::open_in_dir(index_path).unwrap();
    register_code_tokenizer(&index);
    let schema = index.schema();
    let mut writer: tantivy::IndexWriter = index.writer(15_000_000).unwrap();
    let field = |name: &str| schema.get_field(name).unwrap();
    for row in rows {
        let mut doc = TantivyDocument::new();
        doc.add_text(field("doc_type"), &row.doc_type);
        doc.add_text(field("chunk_kind"), &row.chunk_kind);
        doc.add_text(field("entity_id"), &row.entity_id);
        doc.add_text(field("content"), &row.content);
        doc.add_text(field("path_tokens"), &row.path_tokens);
        doc.add_text(field("chunk_hash"), &row.content_hash);
        doc.add_text(field("parser_version"), &row.parser_version);
        doc.add_text(field("repo_id"), &row.repo_id);
        doc.add_text(field("commit_sha"), &row.commit_sha);
        doc.add_text(field("commit_author_name"), &row.commit_author_name);
        doc.add_text(field("commit_author_email"), &row.commit_author_email);
        doc.add_text(field("session_id"), &row.session_id);
        doc.add_text(field("account"), &row.account);
        doc.add_text(field("project"), "/host/path/that/must/not/travel");
        doc.add_text(field("file_path"), "git:some-project");
        doc.add_text(field("role"), &row.role);
        doc.add_u64(field("byte_offset"), row.byte_offset);
        doc.add_u64(field("is_subagent"), row.is_subagent);
        writer.add_document(doc).unwrap();
    }
    writer.commit().unwrap();
    drop(writer);
}

#[test]
fn a_second_replacement_at_a_new_marker_re_derives_the_same_id_and_advances_nothing() {
    // D-039 regression row: the SECOND schema replacement scans the same
    // carried content under a DIFFERENT outgoing marker. Source evidence is
    // outside the id preimage, so the re-derived id must equal the recorded
    // one and the strict no-remint advance must see nothing to do. Before
    // D-039 this refused with `history_commitment_mismatch` and wedged every
    // catalog-mode store at its next schema bump.
    let fixture = fresh_fixture(&[("owned-ns", 1)], None);
    write_commit_documents(
        &fixture.index_path,
        "owned-ns",
        &[(commit_sha(40).as_str(), "carried across bumps")],
    );
    fs::write(
        fixture.index_path.join("schema_version.txt"),
        b"outgoing-schema-a",
    )
    .unwrap();
    let first = materialize_history_generations(&fixture.store, &fixture.request()).unwrap();
    let epoch_after_first = fixture.store.snapshot().unwrap().epoch();

    // The next bump: identical content, new outgoing marker (and therefore a
    // new schema fingerprint recorded by the scan).
    fs::write(
        fixture.index_path.join("schema_version.txt"),
        b"outgoing-schema-b",
    )
    .unwrap();
    let second = materialize_history_generations(&fixture.store, &fixture.request()).unwrap();

    assert_eq!(first.generation_ids(), second.generation_ids());
    assert_eq!(second.catalog_epoch_after, None);
    assert_eq!(fixture.store.snapshot().unwrap().epoch(), epoch_after_first);
    // Provenance stays the first writer's: the on-disk generation still
    // records the evidence it was actually observed under.
    assert_eq!(
        second.namespaces[0]
            .generation
            .manifest
            .body
            .source_schema_version,
        "outgoing-schema-a"
    );
}

#[test]
fn a_live_refresh_then_a_replacement_scan_agree_on_generation_identity() {
    // D-039 composition row: a live refresh advances `Ready` with the
    // constant live-refresh marker in its evidence slot; the next
    // replacement's scan observes the same content with scan evidence. The
    // two construction sites must re-derive the SAME id, so the advance is a
    // no-op instead of a permanent open-time refusal.
    let fixture = fresh_fixture(&[("owned-ns", 1)], None);
    let generation_store = HistoryGenerationStore::open_for_index(&fixture.index_path).unwrap();
    let cursors = RepoHistoryCursorStoreV1::new(fixture.root.join("state").join("git_meta"));
    let group = RepoHistoryIngestGroupV1 {
        repo_history_id: history_id(1),
        primary_namespace: CommitNamespace::parse("owned-ns").unwrap(),
        members: BTreeMap::new(),
    };
    let walk = ConsolidatedWalkOutcomeV1 {
        commits: vec![GitCommit {
            sha: commit_sha(41),
            parent_shas: Vec::new(),
            author_name: "History Fixture".to_string(),
            author_email: "fixture@example.invalid".to_string(),
            message: "refreshed commit".to_string(),
        }],
        head: commit_sha(41),
        ..Default::default()
    };
    let refreshed =
        refresh_repo_history_generation(&fixture.store, &generation_store, &cursors, &group, &walk)
            .unwrap();
    assert!(refreshed.catalog_epoch_after.is_some());

    // The next schema replacement: an index carrying exactly the refreshed
    // generation's rows, scanned under an outgoing marker.
    write_commit_documents_from_rows(&fixture.index_path, &refreshed.generation.commit_documents);
    fs::write(
        fixture.index_path.join("schema_version.txt"),
        b"outgoing-schema-next",
    )
    .unwrap();
    let outcome = materialize_history_generations(&fixture.store, &fixture.request()).unwrap();

    assert_eq!(outcome.namespaces.len(), 1);
    assert_eq!(outcome.namespaces[0].generation.id, refreshed.generation.id);
    assert_eq!(outcome.catalog_epoch_after, None);
    // The on-disk evidence remains the refresh's, provenance only.
    assert_eq!(
        outcome.namespaces[0]
            .generation
            .manifest
            .body
            .source_index_fingerprint_sha256,
        "blackbox.repo-history-generation.live-refresh.v1"
    );
}

#[test]
fn generation_documents_and_vector_inputs_survive_the_source_index_drop() {
    let fixture = migrated_fixture(&[(commit_sha(31).as_str(), "history that must survive")]);
    let outcome = materialize_history_generations(&fixture.store, &fixture.request()).unwrap();
    let id = outcome.namespaces[0].generation.id.clone();
    let generation_store = HistoryGenerationStore::open_for_index(&fixture.index_path()).unwrap();

    // The destructive replacement, verbatim.
    fs::remove_dir_all(fixture.index_path()).unwrap();

    let record = generation_store.load(&id).unwrap();
    assert_eq!(record.commit_documents.len(), 1);
    assert_eq!(record.vector_inputs.len(), 1);
    assert_eq!(
        record.commit_documents[0].content,
        "history that must survive"
    );
    record.validate().unwrap();
}
