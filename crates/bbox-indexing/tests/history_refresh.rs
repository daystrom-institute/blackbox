//! Closing-review round-2 coverage row N2: the live repo-history refresh
//! (`refresh_repo_history_generation`), the sole production path that
//! advances `Ready` after P3-F activation.
//!
//! Pinned here: supersede semantics (advance + retain the outgoing
//! generation), the no-change no-op (no epoch bump, no supersession), the
//! cursor-after-publication ordering, the foreign-namespace refusal, and the
//! primary-namespace guard. The composition with the pre-replacement
//! materializer (same content, same id across the two construction sites)
//! lives in `history_materializer.rs` beside the scan fixtures.
//!
//! Every test canonicalizes its tempdir root before deriving paths and
//! touches no real HOME or XDG state.

use std::collections::{BTreeMap, BTreeSet};

use bbox_corpus_core::git::GitCommit;
use bbox_corpus_core::project_catalog::{
    CommitNamespace, RepoHistoryAuthority, RepoHistoryId, RepoHistoryMaterialization,
    RepoHistoryRecord,
};
use bbox_corpus_index::index::history_generations::{
    HistoryGenerationInputV1, HistoryGenerationOwnerV1, HistoryGenerationStore,
};
use bbox_indexing::index::consolidated_history::{
    ConsolidatedWalkOutcomeV1, RepoHistoryCursorStoreV1, RepoHistoryIngestGroupV1,
};
use bbox_indexing::index::history_refresh::refresh_repo_history_generation;
use bbox_indexing::project_catalog_store::ProjectCatalogStore;
use tempfile::tempdir;

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

fn commit(seed: u8, message: &str) -> GitCommit {
    GitCommit {
        sha: commit_sha(seed),
        parent_shas: Vec::new(),
        author_name: "History Fixture".to_string(),
        author_email: "fixture@example.invalid".to_string(),
        message: message.to_string(),
    }
}

fn walk_of(commits: Vec<GitCommit>) -> ConsolidatedWalkOutcomeV1 {
    let head = commits
        .first()
        .map(|commit| commit.sha.clone())
        .unwrap_or_default();
    ConsolidatedWalkOutcomeV1 {
        commits,
        head,
        ..Default::default()
    }
}

struct RefreshFixture {
    _directory: tempfile::TempDir,
    store: ProjectCatalogStore,
    generation_store: HistoryGenerationStore,
    cursors: RepoHistoryCursorStoreV1,
    group: RepoHistoryIngestGroupV1,
}

fn refresh_fixture(namespace: &str) -> RefreshFixture {
    let directory = tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap();
    let state = root.join("state");
    std::fs::create_dir_all(&state).unwrap();
    let index_path = state.join("index");

    let store = ProjectCatalogStore::initialize_empty(&state.join("projects.json")).unwrap();
    let epoch = store.snapshot().unwrap().epoch();
    let id = history_id(1);
    let primary = CommitNamespace::parse(namespace.to_string()).unwrap();
    {
        let id = id.clone();
        let primary = primary.clone();
        store
            .transact(epoch, move |catalog, _attachments| {
                catalog.repo_histories.insert(
                    id.clone(),
                    RepoHistoryRecord {
                        repo_history_id: id.clone(),
                        authority: RepoHistoryAuthority::LegacyNamespace(primary.clone()),
                        primary_namespace: primary.clone(),
                        compatibility_namespaces: BTreeSet::new(),
                        materialization: RepoHistoryMaterialization::NotBuilt,
                    },
                );
                Ok(())
            })
            .unwrap();
    }

    let generation_store = HistoryGenerationStore::open_for_index(&index_path).unwrap();
    let cursors = RepoHistoryCursorStoreV1::new(state.join("git_meta"));
    let group = RepoHistoryIngestGroupV1 {
        repo_history_id: id,
        primary_namespace: primary,
        members: BTreeMap::new(),
    };
    RefreshFixture {
        _directory: directory,
        store,
        generation_store,
        cursors,
        group,
    }
}

fn ready_generation_id(fixture: &RefreshFixture) -> String {
    let state = fixture.store.snapshot().unwrap();
    match &state.catalog().repo_histories[&fixture.group.repo_history_id].materialization {
        RepoHistoryMaterialization::Ready { generation_id } => generation_id.as_str().to_string(),
        other => panic!("expected Ready, got {other:?}"),
    }
}

#[test]
fn a_first_refresh_advances_not_built_to_ready_and_writes_the_cursor_last() {
    let fixture = refresh_fixture("owned-ns");
    let walk = walk_of(vec![commit(1, "first commit")]);
    let outcome = refresh_repo_history_generation(
        &fixture.store,
        &fixture.generation_store,
        &fixture.cursors,
        &fixture.group,
        &walk,
    )
    .unwrap();

    assert!(outcome.catalog_epoch_after.is_some());
    assert_eq!(outcome.superseded_generation, None);
    assert_eq!(outcome.new_vector_inputs.len(), 1);
    assert_eq!(
        ready_generation_id(&fixture),
        outcome.generation.id.as_str()
    );

    // The cursor exists, names the head, and binds to the PUBLISHED
    // generation: the after-publication ordering means it can never name a
    // generation the catalog did not adopt.
    let cursor = fixture
        .cursors
        .load(&fixture.group.repo_history_id)
        .unwrap()
        .expect("cursor written after a successful refresh");
    assert_eq!(cursor.last_ingested_sha, commit_sha(1));
    assert_eq!(cursor.generation_id, outcome.generation.id.as_str());
}

#[test]
fn a_superseding_refresh_advances_ready_and_retains_the_outgoing_generation() {
    let fixture = refresh_fixture("owned-ns");
    let first = refresh_repo_history_generation(
        &fixture.store,
        &fixture.generation_store,
        &fixture.cursors,
        &fixture.group,
        &walk_of(vec![commit(1, "first commit")]),
    )
    .unwrap();

    // A complete rewalk per the no-seed rule: the walk carries the full set,
    // not a delta.
    let second = refresh_repo_history_generation(
        &fixture.store,
        &fixture.generation_store,
        &fixture.cursors,
        &fixture.group,
        &walk_of(vec![commit(2, "second commit"), commit(1, "first commit")]),
    )
    .unwrap();

    assert_ne!(second.generation.id, first.generation.id);
    assert_eq!(
        second.superseded_generation.as_deref(),
        Some(first.generation.id.as_str())
    );
    // Only the catalog pointer moved; the outgoing generation is retained on
    // disk, still loadable and self-consistent.
    let retained = fixture.generation_store.load(&first.generation.id).unwrap();
    retained.validate().unwrap();
    assert_eq!(ready_generation_id(&fixture), second.generation.id.as_str());
    // The superseding generation is the complete set; the vector delta is
    // only what this walk newly observed.
    assert_eq!(second.generation.commit_documents.len(), 2);
    assert_eq!(second.new_vector_inputs.len(), 1);
    assert_eq!(
        second.new_vector_inputs[0].entity_id,
        first_entity("owned-ns", 2)
    );
}

fn first_entity(namespace: &str, seed: u8) -> String {
    format!("commit:{namespace}:{}", commit_sha(seed))
}

#[test]
fn a_no_change_refresh_is_a_genuine_no_op() {
    let fixture = refresh_fixture("owned-ns");
    let walk = walk_of(vec![commit(1, "only commit")]);
    let first = refresh_repo_history_generation(
        &fixture.store,
        &fixture.generation_store,
        &fixture.cursors,
        &fixture.group,
        &walk,
    )
    .unwrap();
    let epoch_after_first = fixture.store.snapshot().unwrap().epoch();

    let second = refresh_repo_history_generation(
        &fixture.store,
        &fixture.generation_store,
        &fixture.cursors,
        &fixture.group,
        &walk,
    )
    .unwrap();

    assert_eq!(second.generation.id, first.generation.id);
    assert_eq!(second.catalog_epoch_after, None);
    assert_eq!(second.superseded_generation, None);
    assert!(second.new_vector_inputs.is_empty());
    assert_eq!(fixture.store.snapshot().unwrap().epoch(), epoch_after_first);
}

#[test]
fn a_walk_with_no_head_writes_no_cursor() {
    let fixture = refresh_fixture("owned-ns");
    let outcome = refresh_repo_history_generation(
        &fixture.store,
        &fixture.generation_store,
        &fixture.cursors,
        &fixture.group,
        &walk_of(Vec::new()),
    )
    .unwrap();
    assert!(outcome.cursor.is_none());
    assert!(
        fixture
            .cursors
            .load(&fixture.group.repo_history_id)
            .unwrap()
            .is_none()
    );
}

#[test]
fn a_ready_record_under_a_foreign_namespace_refuses_and_writes_no_cursor() {
    let fixture = refresh_fixture("owned-ns");
    // Materialize the record under a DIFFERENT namespace by hand: the
    // refresh must refuse to build on it rather than silently merge two
    // namespaces' histories.
    let foreign = fixture
        .generation_store
        .create_or_open(HistoryGenerationInputV1 {
            namespace: CommitNamespace::parse("foreign-ns").unwrap(),
            owner: HistoryGenerationOwnerV1::Owned {
                repo_history_id: fixture.group.repo_history_id.clone(),
            },
            commit_documents: Vec::new(),
            vector_inputs: Vec::new(),
            truncated_message_count: 0,
            source_schema_version: "test-schema".to_string(),
            source_schema_fingerprint_sha256: "0".repeat(64),
            source_index_fingerprint_sha256: "1".repeat(64),
        })
        .unwrap();
    let foreign_id = foreign.id.owned().cloned().unwrap();
    let epoch = fixture.store.snapshot().unwrap().epoch();
    let repo_history_id = fixture.group.repo_history_id.clone();
    fixture
        .store
        .transact(epoch, move |catalog, _attachments| {
            catalog
                .repo_histories
                .get_mut(&repo_history_id)
                .unwrap()
                .materialization = RepoHistoryMaterialization::Ready {
                generation_id: foreign_id.clone(),
            };
            Ok(())
        })
        .unwrap();

    let error = refresh_repo_history_generation(
        &fixture.store,
        &fixture.generation_store,
        &fixture.cursors,
        &fixture.group,
        &walk_of(vec![commit(1, "new commit")]),
    )
    .unwrap_err();
    assert_eq!(error.code(), "error.history_commitment_mismatch");
    assert!(error.to_string().contains("foreign namespace"));
    assert!(
        fixture
            .cursors
            .load(&fixture.group.repo_history_id)
            .unwrap()
            .is_none()
    );
}

#[test]
fn a_group_whose_primary_namespace_drifted_refuses() {
    let fixture = refresh_fixture("owned-ns");
    let mut group = fixture.group.clone();
    group.primary_namespace = CommitNamespace::parse("renamed-ns").unwrap();
    let error = refresh_repo_history_generation(
        &fixture.store,
        &fixture.generation_store,
        &fixture.cursors,
        &group,
        &walk_of(vec![commit(1, "new commit")]),
    )
    .unwrap_err();
    assert_eq!(error.code(), "error.history_commitment_mismatch");
    assert!(error.to_string().contains("primary namespace"));
}
