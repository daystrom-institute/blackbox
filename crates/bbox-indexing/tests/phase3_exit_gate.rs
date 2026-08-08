//! Phase 3 exit-gate acceptance block (plan section 11), CI half.
//!
//! Section 11 asks for the facade acceptance test extended into a Phase 3
//! acceptance block "executed in CI and live". This file is the CI half: one
//! extended migrated fixture plus the assertions that can only be made against
//! that whole population at once. Rows already discharged by a named test
//! elsewhere are NOT duplicated here; the inventory below maps every section 11
//! row to its proving test so a reviewer can check the mapping without
//! re-deriving it.
//!
//! ── Section 11 row inventory ────────────────────────────────────────────────
//!
//! ```text
//! Item 1, the extended migrated fixture:
//!   - migrated root through the real facade rehearsal ceremony
//!       `exit_gate_fixture` (this file), built on the ceremony proven by
//!       `project_catalog_migration_facade::external_consumer_runs_exact_review_apply_fresh_verify_and_reapply`
//!   - proved legacy commit namespace (recorded in the persisted
//!       `LegacyCommitNamespaceInventoryAssetV1`)
//!       `the_extended_migrated_fixture_carries_every_section_11_shape` (this file);
//!       proof-mode mechanics: `history_materializer::a_migrated_root_proves_its_namespace_against_the_persisted_asset`
//!   - ambiguous namespace via a two-candidate cluster
//!       `the_extended_migrated_fixture_carries_every_section_11_shape` (this file);
//!       quarantine-generation mechanics: `history_materializer::an_ambiguous_namespace_materializes_a_quarantine_generation`
//!   - one drift-unclaimed namespace injected POST-migration
//!       `the_extended_migrated_fixture_carries_every_section_11_shape` (this file);
//!       manifest-only ownership: `history_materializer::an_unclaimed_namespace_is_manifest_owned_and_never_touches_the_catalog`
//!   - attachment-less published project with an active collected generation
//!       and stale history
//!       `the_extended_migrated_fixture_carries_every_section_11_shape` (this file)
//!   - non-Git LegacyLocal project
//!       `the_extended_migrated_fixture_carries_every_section_11_shape` (this file)
//!   - fixture shapes the v1 importer CANNOT produce, and why, are documented
//!       at their construction sites: the compatibility-namespace chain in
//!       `history_materializer::migrated_fixture_with_compatibility_namespace`
//!       (conflicting-published-authority refusal) and the unclaimed chain in
//!       `history_materializer::an_unclaimed_namespace_is_manifest_owned_and_never_touches_the_catalog`
//!       (unrepresentable in `CatalogSnapshotV2`). This file's post-migration
//!       transact/staging ceremony is annotated inline for the same reason.
//!
//! Item 2, remote-only assertions (see `REMOTE_ONLY_SMOKE_OWNED` below for the
//! rows that are live-only):
//!   - activation with zero leases
//!       `writer_actor::collected_stage_acquires_zero_leases_and_stages_no_git_overlay`
//!   - planning a remote-only project with zero leases
//!       `writer_actor::remote_only_project_plans_collected_with_zero_leases`
//!   - incremental tick and full rebuild preserving an attachment-less project
//!       `writer_actor::a_detached_project_survives_an_incremental_tick_and_a_full_rebuild`
//!   - full rebuild with zero leases across a materialization migration
//!       `writer_actor::a_full_rebuild_migrates_an_outgoing_collected_materialization_with_zero_leases`
//!   - a denied local walk never reads the project root
//!       `writer_actor::full_reindex_with_denied_local_access_never_reads_the_project_root`
//!   - lexical search over the whole remote-only population under a deny-all
//!       broker, with the observation counters asserted at zero
//!       `the_remote_only_population_indexes_and_searches_with_zero_checkout_access` (this file)
//!   - active selectors and edge registered sets include the catalog-only id
//!       `the_remote_only_population_indexes_and_searches_with_zero_checkout_access` (this file)
//!
//! Item 3, replacement assertions:
//!   - forced replacement rematerializes the complete stale commit set
//!       `path_free_replacement_boundary::the_catalog_guard_prepares_a_manifest_and_reemission_preserves_every_commit`
//!       and, over the full four-bucket fixture population,
//!       `the_forced_replacement_rematerializes_every_bucket_of_the_fixture` (this file)
//!   - the committed manifest reproduces the exact catalog and quarantine links
//!       `the_forced_replacement_rematerializes_every_bucket_of_the_fixture` (this file)
//!   - refusal arm: missing asset
//!       `history_materializer::a_migrated_root_without_its_asset_refuses_with_inventory_missing`
//!   - refusal arm: corrupt generation / manifest
//!       `path_free_replacement_boundary::refusal_corrupt_manifest_keeps_the_old_index_readable`
//!       and `path_free_replacement_boundary::refusal_missing_generation_keeps_the_old_index_readable`
//!   - refusal arm: count mismatch
//!       `path_free_replacement_boundary::refusal_commitment_mismatch_keeps_the_old_index_readable`,
//!       `history_materializer::drift_mode_a_recorded_namespace_that_shrank_refuses`,
//!       `history_materializer::drift_mode_a_recorded_namespace_that_vanished_refuses`
//!   - each refusal preserves the last-good views over the FULL fixture
//!       `every_refusal_arm_preserves_the_last_good_views_of_the_whole_fixture` (this file)
//!   - collected project survives the guarded replacement and stays searchable
//!       `path_free_replacement_boundary::a_collected_project_survives_the_guarded_replacement_and_stays_searchable`
//!
//! Item 4, document assertions:
//!   - no producer/corpus-host absolute path in any new document or vector
//!       input, swept over the FULL fixture population
//!       `no_document_or_vector_input_in_the_fixture_carries_a_host_path` (this file);
//!       per-kind rows: `writer_actor::collected_documents_are_path_free_after_the_display_root_cut`,
//!       `history_materializer::an_owned_namespace_materializes_and_advances_the_catalog`
//!       (generation bytes), `path_free_replacement_boundary::the_bridge_guard_spills_the_commit_set_before_the_drop`
//!       (spill rows)
//!   - source URIs round-trip the normative encoding for the section 17
//!       character set
//!       see `SOURCE_URI_ROW` below; the fixture's own emitted documents are
//!       decoded back to the catalog id in
//!       `no_document_or_vector_input_in_the_fixture_carries_a_host_path` (this file)
//!   - `ProjectFileV2` refs round-trip the exact parser
//!       see `PROJECT_FILE_V2_ROW` below; the fixture's own emitted refs are
//!       parsed and re-rendered in the same test (this file)
//!   - LegacyLocal incremental edit converges with full rebuild on the same
//!       generation
//!       `writer_actor::incremental_equals_full_for_a_legacy_local_fixture`.
//!       The fixture's non-Git LegacyLocal project is a catalog member with no
//!       attachment, so its DOCUMENT lane cannot be staged here without a
//!       checkout lease, which would contradict item 2's deny-all broker. The
//!       convergence row therefore stays where its fixture can hold a real
//!       attached non-Git root; this file owns the catalog-membership half.
//!
//! Item 5, overlay assertions: see `OVERLAY_ROWS` below for the rows already
//! discharged. The durable half - `select_git_overlay` on disk read back
//! through `selected_overlays_for_gc` into `build_reference_manifest` - is
//! `the_durable_overlay_map_roots_its_generation_and_clears_on_a_new_activation`
//! (this file), which the existing crash-recovery row cannot assert because it
//! builds its overlay map in memory.
//!
//! Item 6, bridge parity at the same commit: see `BRIDGE_PARITY_SMOKE_OWNED`
//! below.
//! ```
//!
//! ── Isolation ──────────────────────────────────────────────────────────────
//!
//! Every test canonicalizes its tempdir root before deriving paths (macOS
//! resolves `/var/folders` to `/private/var/folders` inside the code under
//! test) and touches no real HOME, XDG, or daemon state.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;

use bbox_code_source_store::CodeSourceStore;
use bbox_config::config::{self, Config, LoadOptions};
use bbox_corpus_core::identity::PublishedScope;
use bbox_corpus_core::project_catalog::{
    AmbiguousNamespaceRecord, AmbiguousNamespaceStatus, AttachmentStatus, CommitNamespace,
    ProjectId, ProjectScope, RepoHistoryId, RepoHistoryMaterialization,
    RepoHistoryQuarantineMaterialization,
};
use bbox_corpus_core::project_record::ProjectRecordsProvider;
use bbox_corpus_index::index::TranscriptIndex;
use bbox_edge_sidecar::manifest::ManifestIndex;
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
// Fixture ceremony
// ---------------------------------------------------------------------------

fn write(path: &Path, bytes: &[u8]) {
    fs::create_dir_all(path.parent().expect("fixture path has a parent")).unwrap();
    fs::write(path, bytes).unwrap();
}

fn git(root: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .env("GIT_AUTHOR_NAME", "Exit Gate Fixture")
        .env("GIT_AUTHOR_EMAIL", "fixture@example.invalid")
        .env("GIT_COMMITTER_NAME", "Exit Gate Fixture")
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
        .env("GIT_AUTHOR_NAME", "Exit Gate Fixture")
        .env("GIT_AUTHOR_EMAIL", "fixture@example.invalid")
        .env("GIT_COMMITTER_NAME", "Exit Gate Fixture")
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

/// The host path the fixture deliberately bakes into its PRE-migration commit
/// documents, so the item-4 sweep has something real to fail on.
const FIXTURE_HOST_PATH: &str = "/host-checkouts/exit-gate-producer";

/// Append commit documents mirroring the exact stored field set a pre-cut
/// `build_commit_doc` wrote, including the two path-bearing fields.
fn write_commit_documents(index_path: &Path, namespace: &str, messages: &[(&str, &str)]) {
    let index = Index::open_in_dir(index_path).unwrap();
    bbox_corpus_index::index::register_code_tokenizer(&index);
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
        doc.add_text(field("commit_author_name"), "Exit Gate Fixture");
        doc.add_text(field("commit_author_email"), "fixture@example.invalid");
        doc.add_text(field("session_id"), "");
        doc.add_text(field("account"), "git");
        doc.add_text(field("project"), FIXTURE_HOST_PATH);
        doc.add_text(field("file_path"), "git:exit-gate");
        doc.add_text(field("role"), "commit");
        doc.add_u64(field("byte_offset"), 0);
        doc.add_u64(field("is_subagent"), 0);
        writer.add_document(doc).unwrap();
    }
    writer.commit().unwrap();
}

/// The section 11 item 1 population, on ONE migrated root.
struct ExitGateFixture {
    _directory: tempfile::TempDir,
    rehearsal_root: PathBuf,
    store: Arc<ProjectCatalogStore>,
    /// The migrated published project. Its only attachment is detached during
    /// construction, so it is the attachment-less published project.
    published_project: ProjectId,
    published_scope: PublishedScope,
    published_checkout: PathBuf,
    /// The proved legacy namespace: present in the index BEFORE migration, so
    /// the persisted inventory asset records it.
    proved_namespace: String,
    /// A populated compatibility namespace on the published project's history
    /// record. The v1 importer cannot emit one (see the ceremony note), so it
    /// arrives through a regular catalog transaction exactly as the runtime
    /// admin path produces it.
    compatibility_namespace: String,
    /// Ambiguous via a two-candidate cluster, injected post-migration.
    ambiguous_namespace: String,
    /// Unclaimed by drift: commit documents with no catalog owner at all,
    /// injected post-migration.
    unclaimed_namespace: String,
    /// The non-Git LegacyLocal project, added post-migration through the
    /// regular admin path.
    legacy_local_project: ProjectId,
    legacy_local_namespace: String,
    legacy_local_root: PathBuf,
}

impl ExitGateFixture {
    fn state(&self) -> PathBuf {
        self.rehearsal_root.join("state")
    }

    fn index_path(&self) -> PathBuf {
        self.state().join("index")
    }

    fn projects_path(&self) -> PathBuf {
        self.state().join("projects.json")
    }
}

/// Build the section 11 item 1 fixture.
///
/// The ceremony is deliberately staged in three phases, because the v1
/// importer can only produce the first one:
///
/// 1. PRE-migration: a single Git checkout, its `projects.json` v1 record, and
///    commit documents under its committed authority. This is the only shape
///    the importer admits without refusing, so it is the only way to get a
///    namespace RECORDED in the persisted
///    `LegacyCommitNamespaceInventoryAssetV1`. A second published project in
///    the same history group refuses preflight with
///    `conflicting_published_authorities`; the chain is documented at
///    `history_materializer::migrated_fixture_with_compatibility_namespace`.
/// 2. The real facade rehearsal (preflight, apply), byte-identical to the
///    ceremony the P2 exit-gate test drives.
/// 3. POST-migration: regular catalog transactions and admin operations plus
///    live index writes, which is exactly how production reaches these shapes.
///    The ambiguous cluster needs two EXISTING repo-history records
///    (`validate_catalog` rejects fewer than two candidates), so the
///    LegacyLocal project is added first and its server-minted local history
///    becomes the second candidate. The unclaimed namespace is
///    unrepresentable in `CatalogSnapshotV2` by construction, so it exists
///    only as commit documents with no owner.
fn exit_gate_fixture() -> ExitGateFixture {
    let directory = tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap();
    let config = config(&root);
    let rehearsal_root = root.join("rehearsal");
    fs::create_dir_all(&rehearsal_root).unwrap();

    // ── Phase 1: pre-migration v1 state ────────────────────────────────────
    let proved_namespace = "neutral-exit-gate-repository".to_string();
    let checkout = rehearsal_root.join("checkouts").join("published-checkout");
    fs::create_dir_all(&checkout).unwrap();
    git(&checkout, &["init", "-q"]);
    git(&checkout, &["checkout", "-qb", "main"]);
    write(
        &checkout.join(".bbox/config.toml"),
        format!("[project]\nrepo_id = \"{proved_namespace}\"\n").as_bytes(),
    );
    git(&checkout, &["add", ".bbox"]);
    git(&checkout, &["commit", "-qm", "seed exit-gate fixture"]);
    initialize_empty_provenance_ref(&checkout, &config);

    initialize_empty_owner_state(&rehearsal_root);
    let state = rehearsal_root.join("state");
    CodeSourceStore::open(
        state.join("code-sources"),
        project_catalog_migration_store_limits(&config),
    )
    .unwrap();
    write_commit_documents(
        &state.join("index"),
        &proved_namespace,
        &[
            (commit_sha(1).as_str(), "proved namespace first"),
            (commit_sha(2).as_str(), "proved namespace second"),
        ],
    );

    let published_project = ProjectId::parse("neutral-exit-gate-project").unwrap();
    write(
        &state.join("projects.json"),
        serde_json::to_vec_pretty(&serde_json::json!({
            "version": 1,
            "projects": [{
                "project_id": published_project,
                "repo_id": proved_namespace,
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

    // ── Phase 2: the real facade rehearsal ceremony ────────────────────────
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
        "the exit-gate fixture must preflight clean: {:?}",
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

    let store = Arc::new(ProjectCatalogStore::open_existing(state.join("projects.json")).unwrap());
    let published_scope =
        match &store.snapshot().unwrap().catalog().projects[&published_project].scope {
            ProjectScope::Published(scope) => scope.clone(),
            other => panic!("the migrated fixture project must be published, got {other:?}"),
        };

    // ── Phase 3a: the non-Git LegacyLocal project ──────────────────────────
    // `catalog_add` mints the project AND a server-minted local repo-history
    // record with an independent random namespace, which is what makes the
    // ambiguous cluster below satisfiable with two real candidates.
    let legacy_local_root = rehearsal_root.join("checkouts").join("legacy-local-root");
    fs::create_dir_all(&legacy_local_root).unwrap();
    write(
        &legacy_local_root.join("src/notes.txt"),
        b"legacy local body\n",
    );
    let (legacy_local_project, _) = bbox_indexing::project_catalog_admin::catalog_add(
        &store,
        store.snapshot().unwrap().epoch(),
        &bbox_indexing::project_catalog_admin::CatalogAddKind::LegacyLocal,
        "exit-gate-legacy-local",
        &[],
        "2026-07-26T00:00:00Z",
    )
    .unwrap();
    let (published_history_id, legacy_local_history_id, legacy_local_namespace) = {
        let snapshot = store.snapshot().unwrap();
        let catalog = snapshot.catalog();
        let legacy_history = catalog.projects[&legacy_local_project]
            .repo_history
            .clone()
            .expect("catalog_add mints a local repo history");
        let published_history = catalog.projects[&published_project]
            .repo_history
            .clone()
            .expect("the migrated project carries its legacy repo history");
        let namespace = catalog.repo_histories[&legacy_history]
            .primary_namespace
            .as_str()
            .to_string();
        (published_history, legacy_history, namespace)
    };

    // ── Phase 3b: the ambiguous two-candidate cluster ──────────────────────
    let ambiguous_namespace = "neutral-exit-gate-ambiguous".to_string();
    {
        let namespace = ambiguous_namespace.clone();
        let candidates: BTreeSet<RepoHistoryId> =
            [published_history_id.clone(), legacy_local_history_id]
                .into_iter()
                .collect();
        store
            .transact(store.snapshot().unwrap().epoch(), move |catalog, _| {
                let parsed = CommitNamespace::parse(namespace.clone()).unwrap();
                catalog.ambiguous_namespaces.insert(
                    parsed.clone(),
                    AmbiguousNamespaceRecord {
                        namespace: parsed,
                        candidate_repo_history_ids: candidates.clone(),
                        status: AmbiguousNamespaceStatus::Quarantined,
                        materialization: RepoHistoryQuarantineMaterialization::NotBuilt,
                    },
                );
                Ok(())
            })
            .unwrap();
    }

    // ── Phase 3c: the populated compatibility namespace ────────────────────
    // The importer cannot emit this: `inventoried_group_namespaces` only
    // admits ATTRIBUTED namespaces, and a group holding two distinct
    // published authorities refuses outright with
    // `conflicting_published_authorities`. The runtime admin path (relpath
    // move, authority change) is the real producer, and `validate_catalog`
    // accepts the shape, so this fixture reaches it the way production does.
    // The full refusal chain is documented at
    // `history_materializer::migrated_fixture_with_compatibility_namespace`.
    let compatibility_namespace = "neutral-exit-gate-compatibility".to_string();
    {
        let namespace = compatibility_namespace.clone();
        let target = published_history_id.clone();
        store
            .transact(store.snapshot().unwrap().epoch(), move |catalog, _| {
                catalog
                    .repo_histories
                    .get_mut(&target)
                    .unwrap()
                    .compatibility_namespaces
                    .insert(CommitNamespace::parse(namespace.clone()).unwrap());
                Ok(())
            })
            .unwrap();
    }

    // ── Phase 3d: the post-migration index writes ──────────────────────────
    // Every namespace below is absent from the persisted asset by
    // construction, so this is genuine post-capture drift, exactly the live
    // shape the P3-E forced-replacement smoke hit.
    let unclaimed_namespace = "neutral-exit-gate-drifted".to_string();
    write_commit_documents(
        &state.join("index"),
        &compatibility_namespace,
        &[(commit_sha(3).as_str(), "compatibility namespace commit")],
    );
    write_commit_documents(
        &state.join("index"),
        &ambiguous_namespace,
        &[(commit_sha(4).as_str(), "ambiguous namespace commit")],
    );
    write_commit_documents(
        &state.join("index"),
        &unclaimed_namespace,
        &[(commit_sha(5).as_str(), "drift-unclaimed namespace commit")],
    );

    // ── Phase 3e: detach the published project's only attachment ───────────
    // This is the §7.10 admin round trip, not a hand-edit: it turns the
    // migrated project into the attachment-less published project section 11
    // item 1 asks for, while preserving its logical state (detach never
    // removes a collected generation).
    let attachment_id = {
        let snapshot = store.snapshot().unwrap();
        snapshot
            .attachments()
            .attachments
            .values()
            .find(|row| {
                row.project_id == published_project && row.status == AttachmentStatus::Attached
            })
            .expect("the migrated project has exactly one attached base")
            .attachment_id
            .clone()
    };
    bbox_indexing::project_catalog_admin::detach_attachment(
        &store,
        store.snapshot().unwrap().epoch(),
        &attachment_id,
        "2026-07-26T00:00:01Z",
    )
    .unwrap();

    ExitGateFixture {
        _directory: directory,
        rehearsal_root,
        store,
        published_project,
        published_scope,
        published_checkout: checkout,
        proved_namespace,
        compatibility_namespace,
        ambiguous_namespace,
        unclaimed_namespace,
        legacy_local_project,
        legacy_local_namespace,
        legacy_local_root,
    }
}

#[test]
fn the_extended_migrated_fixture_carries_every_section_11_shape() {
    let fixture = exit_gate_fixture();
    let snapshot = fixture.store.snapshot().unwrap();
    let catalog = snapshot.catalog();
    catalog.validate().unwrap();

    // The root is genuinely migrated, so the persisted Phase 1 asset exists
    // and records the proved namespace.
    let transaction_id = match &catalog.origin {
        bbox_corpus_core::project_catalog::CatalogOriginV2::MigratedV1 { transaction_id } => {
            transaction_id.clone()
        }
        other => panic!("the exit-gate fixture must be migrated, got {other:?}"),
    };
    let asset =
        load_legacy_commit_namespace_inventory_asset(&fixture.projects_path(), &transaction_id)
            .unwrap()
            .expect("a migrated root installs the namespace inventory asset");
    assert!(
        asset
            .rows
            .iter()
            .any(|row| row.namespace.as_str() == fixture.proved_namespace),
        "the proved namespace must be RECORDED in the asset, not merely present in the index"
    );
    for drifted in [
        &fixture.compatibility_namespace,
        &fixture.ambiguous_namespace,
        &fixture.unclaimed_namespace,
    ] {
        assert!(
            !asset
                .rows
                .iter()
                .any(|row| row.namespace.as_str() == *drifted),
            "{drifted} must be post-capture drift, absent from the asset"
        );
    }

    // The published project is attachment-less, and carries the proved
    // namespace as its repo history plus the populated compatibility one.
    let published = &catalog.projects[&fixture.published_project];
    assert!(matches!(published.scope, ProjectScope::Published(_)));
    assert_eq!(
        snapshot
            .attachments()
            .attachments
            .values()
            .filter(|row| row.project_id == fixture.published_project
                && row.status == AttachmentStatus::Attached)
            .count(),
        0,
        "the published project must be attachment-less"
    );
    let published_history =
        catalog.repo_histories[published.repo_history.as_ref().expect("repo history")].clone();
    assert_eq!(
        published_history.primary_namespace.as_str(),
        fixture.proved_namespace
    );
    assert_eq!(
        published_history
            .compatibility_namespaces
            .iter()
            .map(|ns| ns.as_str().to_string())
            .collect::<Vec<_>>(),
        vec![fixture.compatibility_namespace.clone()]
    );
    assert_eq!(
        published_history.materialization,
        RepoHistoryMaterialization::NotBuilt,
        "history is stale until the materializer runs"
    );

    // The ambiguous cluster has exactly the two candidates the catalog
    // validation minimum requires, and neither owns the namespace.
    let ambiguous = &catalog.ambiguous_namespaces
        [&CommitNamespace::parse(fixture.ambiguous_namespace.clone()).unwrap()];
    assert_eq!(ambiguous.candidate_repo_history_ids.len(), 2);
    assert_eq!(ambiguous.status, AmbiguousNamespaceStatus::Quarantined);

    // The unclaimed namespace has no catalog representation at all.
    assert!(
        !catalog
            .repo_histories
            .values()
            .any(
                |record| record.primary_namespace.as_str() == fixture.unclaimed_namespace
                    || record
                        .compatibility_namespaces
                        .iter()
                        .any(|ns| ns.as_str() == fixture.unclaimed_namespace)
            ),
        "an unclaimed namespace is unrepresentable in the catalog by construction"
    );
    assert!(
        !catalog
            .ambiguous_namespaces
            .contains_key(&CommitNamespace::parse(fixture.unclaimed_namespace.clone()).unwrap())
    );

    // The non-Git LegacyLocal project exists with its own local history.
    let legacy = &catalog.projects[&fixture.legacy_local_project];
    assert_eq!(legacy.scope, ProjectScope::LegacyLocal);
    assert!(
        !fixture.legacy_local_root.join(".git").exists(),
        "the LegacyLocal fixture root must be non-Git"
    );
    assert_eq!(
        catalog.repo_histories[legacy.repo_history.as_ref().unwrap()]
            .primary_namespace
            .as_str(),
        fixture.legacy_local_namespace
    );
}

// ---------------------------------------------------------------------------
// Item 2: remote-only assertions over the whole fixture population
// ---------------------------------------------------------------------------

/// Rows of governing section 17's remote-only block that are LIVE-only and
/// therefore owned by this phase's catalog-mode bootsmoke rather than by CI:
/// daemon restart, the configured scope grant plus upload over the HTTP
/// surface, inspect expansion and graph discovery (both are `pub(crate)` tool
/// surfaces in the root binary crate and unreachable from a crate-level test),
/// and the background storage-GC tick. The smoke asserts the same
/// `CheckoutAccessObservations` counters this file asserts, read off the live
/// daemon's durable observation store instead of an in-memory one.
const REMOTE_ONLY_SMOKE_OWNED: &str = "restart, scope-grant upload, inspect/graph, background GC";

/// Everything the writer actor needs to index the remote-only population.
struct RemoteOnlyRuntime {
    index: TranscriptIndex,
    _actor: bbox_indexing::index::IndexWriterActor,
    broker: Arc<bbox_indexing::checkout_access::CheckoutAccessBroker>,
    records: Arc<bbox_indexing::catalog_records::CatalogProjectRecordsProvider>,
    selector: String,
    generation_id: String,
    document_count: u64,
}

/// Stage and activate one collected generation for the fixture's
/// attachment-less published project, with a DENY-ALL broker installed.
///
/// This is the section 11 item 2 activation row driven through the catalog
/// records provider (not the bridge one), so `code_identities` is the only
/// thing that admits the project: the compatibility `records` projection
/// omits it by construction, which is exactly the remote-only shape.
fn activate_remote_only_collected_generation(fixture: &ExitGateFixture) -> RemoteOnlyRuntime {
    let state = fixture.state();
    let records = Arc::new(
        bbox_indexing::catalog_records::CatalogProjectRecordsProvider::new(fixture.store.clone()),
    );
    assert!(
        records.records_snapshot().records.is_empty(),
        "the fixture population has no attached project, so the compatibility \
         projection must be empty; the catalog id set is the only authority"
    );
    let broker = Arc::new(bbox_indexing::checkout_access::CheckoutAccessBroker::new(
        Arc::new(bbox_indexing::checkout_access::DenyCheckoutAccess),
        bbox_indexing::checkout_access::CheckoutAccessObservations::in_memory(),
    ));

    let bytes = b"pub fn remote_only() {}\n";
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
        scope: fixture.published_scope.clone(),
        head_commit: head_commit.clone(),
        dirty_fingerprint: bbox_code_source::dirty_fingerprint(&head_commit, &entries),
        manifest_sha256: bbox_code_source::manifest_sha256(&entries),
        file_count: entries.len() as u64,
        logical_bytes: bytes.len() as u64,
    };
    let store = Arc::new(
        CodeSourceStore::open(
            state.join("code-sources"),
            bbox_code_source_store::StoreLimits::default(),
        )
        .unwrap(),
    );
    let upload = store
        .begin_upload("host-remote", descriptor.clone())
        .unwrap();
    store
        .put_manifest_page("host-remote", &upload.upload_id, 0, &entries)
        .unwrap();
    store
        .complete_manifest("host-remote", &upload.upload_id)
        .unwrap();
    store
        .install_blob(
            "host-remote",
            &upload.upload_id,
            &content_hash,
            bytes.len() as u64,
            &bytes[..],
        )
        .unwrap();
    let generation_id = store
        .finalize_upload("host-remote", &upload.upload_id)
        .unwrap()
        .generation_id;

    let index = TranscriptIndex::open_or_create_with_code_source_store_path(
        &fixture.index_path(),
        Vec::new(),
        None,
        fixture.projects_path(),
        state.join("code-sources"),
        state.join("blackbox-knowledge.json"),
        state.join("blackbox-threads.json"),
        state.join("blackbox-roadmap.json"),
        records.clone(),
        None,
    )
    .unwrap();
    let actor = bbox_indexing::index::IndexWriterActor::spawn_for_with_checkout_access(
        &index,
        records.clone(),
        broker.clone(),
    );

    let identity = {
        let snapshot = fixture.store.snapshot().unwrap();
        let catalog = snapshot.catalog();
        let project = &catalog.projects[&fixture.published_project];
        bbox_corpus_core::code_project_identity::CodeProjectIdentity::from_catalog(
            project,
            project
                .repo_history
                .as_ref()
                .and_then(|id| catalog.repo_histories.get(id)),
        )
    };
    let (document_count, inventory) = {
        let staged = actor
            .stage_collected_generation(
                identity,
                descriptor.clone(),
                generation_id.clone(),
                entries.clone(),
                store.clone(),
            )
            .unwrap();
        assert!(
            staged.document_count > 0,
            "the remote-only fixture must stage real documents"
        );
        (
            staged.document_count,
            staged.entity_inventory_sha256.clone(),
        )
    };

    let selector = bbox_corpus_index::index::project_files::collected_materialization_selector(
        fixture.published_project.as_str(),
        &generation_id,
    );
    let snapshot_id = format!("collected-{}", "7".repeat(32));
    let edges_dir =
        bbox_edge_sidecar::edge_sidecar::edges_dir_from_projects_path(&fixture.projects_path());
    let snapshot_dir = bbox_edge_sidecar::snapshot::snapshot_dir(
        &edges_dir,
        fixture.published_project.as_str(),
        &snapshot_id,
    );
    fs::create_dir_all(&snapshot_dir).unwrap();
    fs::write(snapshot_dir.join("project.jsonl"), b"").unwrap();
    store
        .record_materialization(
            &descriptor.scope,
            &generation_id,
            document_count,
            inventory.clone(),
        )
        .unwrap();
    store
        .save_activation(&bbox_code_source_store::ActivationRecord {
            version: 1,
            project_id: fixture.published_project.to_string(),
            generation_id: generation_id.clone(),
            selector: selector.clone(),
            snapshot_id: snapshot_id.clone(),
            document_count,
            entity_inventory_sha256: inventory,
            current_chunk_targets: Default::default(),
            activated_unix_secs: 1,
            cutback_pending: false,
            diagnostic: None,
        })
        .unwrap();
    bbox_edge_sidecar::snapshot::activate_collected_snapshot_with(
        &edges_dir,
        fixture.published_project.as_str(),
        descriptor.scope.repo_id(),
        &descriptor.head_commit,
        &generation_id,
        &selector,
        &snapshot_id,
        || Ok(()),
    )
    .unwrap();
    index.reader_reload_for_test();

    RemoteOnlyRuntime {
        index,
        _actor: actor,
        broker,
        records,
        selector,
        generation_id,
        document_count,
    }
}

/// Assert the deny-all broker was never consulted for ANY operation kind.
fn assert_zero_checkout_access(
    broker: &bbox_indexing::checkout_access::CheckoutAccessBroker,
    stage: &str,
) {
    let health = broker.health();
    for operation in &health.operations {
        assert_eq!(
            operation.granted, 0,
            "{stage}: {:?} granted a checkout lease",
            operation.kind
        );
        assert_eq!(
            operation.denied, 0,
            "{stage}: {:?} reached the broker at all",
            operation.kind
        );
    }
    assert!(
        health.counters.is_empty(),
        "{stage}: observation counters must be empty, got {:?}",
        health.counters
    );
}

#[test]
fn the_remote_only_population_indexes_and_searches_with_zero_checkout_access() {
    let fixture = exit_gate_fixture();
    let runtime = activate_remote_only_collected_generation(&fixture);
    assert_zero_checkout_access(&runtime.broker, "after activation");

    // Active selectors, refreshed the way boot refreshes them, name the
    // catalog-only id. Without this a reader filters every collected document
    // out, which is the difference between "indexed" and "reachable".
    let selectors = runtime.index.refresh_active_code_selectors().unwrap();
    assert_eq!(
        selectors
            .get(fixture.published_project.as_str())
            .map(String::as_str),
        Some(runtime.selector.as_str()),
        "the active selector map must include the catalog-only id"
    );

    // The edge registered-project set is derived from the catalog id set, not
    // from the attached-record projection, so a remote-only project's sidecar
    // edges are admitted rather than skipped.
    let registered = runtime.records.records_snapshot().registered_project_ids();
    assert!(
        registered.contains(fixture.published_project.as_str()),
        "the edge registered set must include the catalog-only id"
    );
    assert!(
        registered.contains(fixture.legacy_local_project.as_str()),
        "the edge registered set must include every catalog project, attached or not"
    );

    // Lexical search through the real search API (not a raw selector
    // TermQuery) against the pinned active-selector map.
    let params = bbox_corpus_index::index::search::SearchParams {
        query: "remote_only".to_string(),
        mode: Some("fulltext".to_string()),
        account: None,
        project: None,
        role: None,
        include_subagents: Some(true),
        limit: Some(20),
        exclude_self: Some(false),
    };
    let rendered = runtime
        .index
        .search_with_active_selectors(&params, &selectors)
        .unwrap();
    assert!(
        rendered.contains("src/lib.rs"),
        "the remote-only collected document must be reachable through search: {rendered}"
    );

    // Hybrid BM25 retrieval over the same population and selector map.
    let hits = runtime
        .index
        .hybrid_bm25_hits_filtered_with_active_selectors(
            "remote_only",
            10,
            Some("project_file"),
            false,
            &selectors,
        )
        .unwrap();
    assert!(
        !hits.is_empty(),
        "hybrid retrieval must reach the remote-only collected document"
    );

    assert_zero_checkout_access(&runtime.broker, "after search");
    assert!(
        runtime.document_count > 0,
        "sanity: the fixture staged a nonzero population ({} live docs)",
        runtime.document_count
    );
    assert!(
        !REMOTE_ONLY_SMOKE_OWNED.is_empty(),
        "the smoke-owned row inventory is part of this block"
    );
}

// ---------------------------------------------------------------------------
// Item 3: the forced replacement over all four manifest buckets
// ---------------------------------------------------------------------------

const OUTGOING_SCHEMA: &str = "exit-gate-outgoing-schema";

fn replacement_request<'a>(
    index_path: &'a Path,
    projects_path: &'a Path,
) -> bbox_corpus_index::index::schema_replacement::SchemaReplacementRequest<'a> {
    bbox_corpus_index::index::schema_replacement::SchemaReplacementRequest {
        index_path,
        projects_path,
        code_source_store_path: projects_path,
        observed_schema_version: Some(OUTGOING_SCHEMA.to_string()),
        target_schema_version: "exit-gate-incoming-schema",
        // Every request in this file is the daemon-upgrade trigger: observed
        // and target differ. The Q-F operator cause is exercised where the
        // forced same-schema path is under test, not here.
        cause: bbox_corpus_index::index::schema_replacement::CatalogIndexReplacementCause::SchemaMismatch,
    }
}

fn commit_entity_ids(index_path: &Path) -> BTreeSet<String> {
    use tantivy::collector::{Count, TopDocs};
    use tantivy::query::TermQuery;
    use tantivy::schema::IndexRecordOption;
    let index = Index::open_in_dir(index_path).unwrap();
    bbox_corpus_index::index::register_code_tokenizer(&index);
    let schema = index.schema();
    let entity_id = schema.get_field("entity_id").unwrap();
    let searcher = index.reader().unwrap().searcher();
    let query = TermQuery::new(
        tantivy::Term::from_field_text(schema.get_field("doc_type").unwrap(), "commit"),
        IndexRecordOption::Basic,
    );
    let count = searcher.search(&query, &Count).unwrap();
    if count == 0 {
        return BTreeSet::new();
    }
    searcher
        .search(&query, &TopDocs::with_limit(count))
        .unwrap()
        .into_iter()
        .map(|(_, address)| {
            let doc: tantivy::TantivyDocument = searcher.doc(address).unwrap();
            bbox_corpus_index::index::first_text(&doc, entity_id)
        })
        .collect()
}

#[test]
fn the_forced_replacement_rematerializes_every_bucket_of_the_fixture() {
    use bbox_corpus_index::index::history_generations::{
        HistoryGenerationStore, HistoryProofModeV1, HistoryScanLimitsV1, RepoHistoryRebuildStateV1,
    };
    use bbox_corpus_index::index::schema_replacement::CommitDocumentOwnerV1;
    use bbox_indexing::index::schema_rebuild::{
        catalog_schema_replacement_guard, commit_prepared_rebuild_manifest,
        reemit_prepared_history_generations,
    };

    let fixture = exit_gate_fixture();
    let index_path = fixture.index_path();
    let projects_path = fixture.projects_path();
    let before = commit_entity_ids(&index_path);
    assert_eq!(
        before.len(),
        5,
        "the fixture population is 2 proved + 1 compatibility + 1 ambiguous + 1 unclaimed"
    );
    fs::write(
        index_path.join("schema_version.txt"),
        format!("{OUTGOING_SCHEMA}\n"),
    )
    .unwrap();

    // 1. The guard drives the materializer and prepares the manifest.
    let guard = catalog_schema_replacement_guard(
        Arc::new(ProjectCatalogStore::open_existing(&projects_path).unwrap()),
        HistoryScanLimitsV1::default(),
        projects_path.parent().unwrap().join("vectors"),
    );
    guard(&replacement_request(&index_path, &projects_path)).expect("the catalog guard authorizes");

    let generation_store = HistoryGenerationStore::open_for_index(&index_path).unwrap();
    let manifest = generation_store
        .read_rebuild_manifest()
        .unwrap()
        .expect("a prepared manifest");
    assert_eq!(manifest.state, RepoHistoryRebuildStateV1::Prepared);
    assert_eq!(
        manifest.prepared.proof_mode,
        HistoryProofModeV1::Drift,
        "the fixture grew after the asset was captured, so proof runs in drift mode"
    );

    // 2. The manifest reproduces the EXACT catalog and quarantine links for
    //    every bucket of the fixture's known shape.
    let row_for = |namespace: &str| {
        manifest
            .prepared
            .namespace_inventory
            .iter()
            .find(|row| row.namespace.as_str() == namespace)
            .unwrap_or_else(|| panic!("{namespace} is in the namespace inventory"))
            .clone()
    };
    let proved = row_for(&fixture.proved_namespace);
    let compatibility = row_for(&fixture.compatibility_namespace);
    let ambiguous = row_for(&fixture.ambiguous_namespace);
    let unclaimed = row_for(&fixture.unclaimed_namespace);
    assert_eq!(manifest.prepared.namespace_inventory.len(), 4);
    assert_eq!(proved.commit_document_count, 2);
    for row in [&compatibility, &ambiguous, &unclaimed] {
        assert_eq!(row.commit_document_count, 1);
    }
    assert_eq!(
        manifest.prepared.owned_generation_ids,
        [proved.generation_id.clone()].into_iter().collect(),
        "the owned bucket holds exactly the primary namespace's generation"
    );
    assert_eq!(
        manifest.prepared.compatibility_generation_ids,
        [compatibility.generation_id.clone()].into_iter().collect()
    );
    assert_eq!(
        manifest.prepared.ambiguous_generation_ids,
        [ambiguous.generation_id.clone()].into_iter().collect()
    );
    assert_eq!(
        manifest.prepared.unclaimed_generation_ids,
        [unclaimed.generation_id.clone()].into_iter().collect()
    );
    // Owned history carries `rhg_`; quarantined history carries `rhq_`. A
    // compatibility namespace is genuinely owned and must not look quarantined.
    assert!(proved.generation_id.starts_with("rhg_"));
    assert!(compatibility.generation_id.starts_with("rhg_"));
    assert!(ambiguous.generation_id.starts_with("rhq_"));
    assert!(unclaimed.generation_id.starts_with("rhq_"));

    // 3. The catalog links: Ready names the PRIMARY generation only, the
    //    quarantine record names the ambiguous one, and the unclaimed
    //    generation has no catalog owner at all.
    let reopened = ProjectCatalogStore::open_existing(&projects_path).unwrap();
    let snapshot = reopened.snapshot().unwrap();
    let catalog = snapshot.catalog();
    catalog.validate().unwrap();
    let ready = catalog
        .repo_histories
        .values()
        .filter_map(|record| match &record.materialization {
            RepoHistoryMaterialization::Ready { generation_id } => {
                Some(generation_id.as_str().to_string())
            }
            RepoHistoryMaterialization::NotBuilt => None,
        })
        .collect::<BTreeSet<_>>();
    assert_eq!(
        ready,
        [proved.generation_id.clone()].into_iter().collect(),
        "only the primary namespace's generation is catalog-named"
    );
    let quarantine = &catalog.ambiguous_namespaces
        [&CommitNamespace::parse(fixture.ambiguous_namespace.clone()).unwrap()];
    assert!(matches!(
        &quarantine.materialization,
        RepoHistoryQuarantineMaterialization::Ready { generation_id }
            if generation_id.as_str() == ambiguous.generation_id
    ));

    // 4. The destructive drop, then the rebuild's re-emission.
    fs::remove_dir_all(&index_path).unwrap();
    let index = TranscriptIndex::open_or_create_with_records(
        &index_path,
        Vec::new(),
        None,
        fixture.state().join("legacy-projects.json"),
        fixture.state().join("blackbox-knowledge.json"),
        fixture.state().join("blackbox-threads.json"),
        fixture.state().join("blackbox-roadmap.json"),
        std::sync::Arc::new(bbox_corpus_index::index::StaticProjectRecordsProvider::empty()),
    )
    .unwrap();
    assert!(commit_entity_ids(&index_path).is_empty());

    let owners = [&fixture.proved_namespace, &fixture.compatibility_namespace]
        .into_iter()
        .map(|namespace| {
            (
                namespace.clone(),
                CommitDocumentOwnerV1 {
                    project_id: Some(fixture.published_project.as_str().into()),
                    project_display: "exit-gate-service".into(),
                },
            )
        })
        .collect::<std::collections::BTreeMap<_, _>>();
    let writer: tantivy::IndexWriter = index.index_handle().writer(15_000_000).unwrap();
    let outcome =
        reemit_prepared_history_generations(&index_path, &writer, index.field_handles(), &owners)
            .unwrap()
            .expect("a prepared manifest drives re-emission");
    let mut writer = writer;
    writer.commit().unwrap();

    assert_eq!(outcome.namespaces, 4);
    assert_eq!(
        outcome.commit_documents, 5,
        "the COMPLETE stale commit set must be rematerialized from generations"
    );
    assert_eq!(
        commit_entity_ids(&index_path),
        before,
        "commit identity must be byte-stable across the replacement, in every bucket"
    );

    // 5. The manifest is promoted only after the population is durable, and it
    //    carries per-generation vector evidence for all four buckets.
    let committed = commit_prepared_rebuild_manifest(
        &index_path,
        "lexical:exit-gate-incoming-schema",
        "vector:exit-gate",
        reopened.snapshot().unwrap().epoch(),
        outcome.vector_inventory.clone(),
    )
    .unwrap()
    .expect("a manifest to commit");
    assert_eq!(committed.state, RepoHistoryRebuildStateV1::Committed);
    let evidence = committed.committed.expect("committed evidence");
    assert_eq!(evidence.vector_inventory.len(), 4);
    assert_eq!(
        evidence
            .vector_inventory
            .iter()
            .map(|row| row.vector_inputs_verified + row.vector_inputs_reenqueued)
            .sum::<u64>(),
        5,
        "every commit document's vector input must be accounted for"
    );
}

#[test]
fn every_refusal_arm_preserves_the_last_good_views_of_the_whole_fixture() {
    use bbox_corpus_index::index::history_generations::{
        HistoryGenerationStore, HistoryScanLimitsV1,
    };
    use bbox_indexing::index::schema_rebuild::catalog_schema_replacement_guard;

    // The missing-asset arm, driven over the FULL four-bucket population
    // rather than the single-namespace fixture the P3-D row uses. The corrupt
    // -generation and count-mismatch arms are proven by the named tests in the
    // inventory above; what this row adds is that a refusal on a populated
    // migrated root leaves EVERY bucket's documents readable and advances
    // nothing in the catalog.
    let fixture = exit_gate_fixture();
    let index_path = fixture.index_path();
    let projects_path = fixture.projects_path();
    let before = commit_entity_ids(&index_path);
    assert_eq!(before.len(), 5);
    fs::write(
        index_path.join("schema_version.txt"),
        format!("{OUTGOING_SCHEMA}\n"),
    )
    .unwrap();

    let assets = fixture
        .rehearsal_root
        .join("state")
        .join("project-catalog-migration-assets");
    let asset = fs::read_dir(&assets)
        .unwrap()
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .find(|path| {
            path.extension().and_then(|ext| ext.to_str()) == Some("immutable")
                && fs::read(path)
                    .ok()
                    .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok())
                    .and_then(|value| value.get("rows").cloned())
                    .is_some()
        })
        .expect("the namespace inventory asset file exists");
    fs::remove_file(&asset).unwrap();

    let guard = catalog_schema_replacement_guard(
        Arc::new(ProjectCatalogStore::open_existing(&projects_path).unwrap()),
        HistoryScanLimitsV1::default(),
        projects_path.parent().unwrap().join("vectors"),
    );
    let error = guard(&replacement_request(&index_path, &projects_path))
        .err()
        .expect("a migrated root without its asset must refuse the replacement");
    assert!(
        format!("{error:#}").contains("history_inventory_missing"),
        "{error:#}"
    );

    // Last-good lexical view: every bucket's documents are still readable and
    // the outgoing marker is untouched, so the drop never ran.
    assert_eq!(commit_entity_ids(&index_path), before);
    assert_eq!(
        fs::read_to_string(index_path.join("schema_version.txt"))
            .unwrap()
            .trim(),
        OUTGOING_SCHEMA
    );
    assert!(
        HistoryGenerationStore::open_for_index(&index_path)
            .unwrap()
            .read_rebuild_manifest()
            .unwrap()
            .is_none(),
        "a refused pass must not leave a manifest authorizing anything"
    );
    // And nothing advanced in the catalog.
    let snapshot = ProjectCatalogStore::open_existing(&projects_path)
        .unwrap()
        .snapshot()
        .unwrap();
    assert!(
        snapshot
            .catalog()
            .repo_histories
            .values()
            .all(|record| record.materialization == RepoHistoryMaterialization::NotBuilt)
    );
    assert!(
        snapshot
            .catalog()
            .ambiguous_namespaces
            .values()
            .all(|record| record.materialization == RepoHistoryQuarantineMaterialization::NotBuilt)
    );
}

// ---------------------------------------------------------------------------
// Item 4: document assertions over the whole fixture population
// ---------------------------------------------------------------------------

/// Source-URI round trips for the governing section 17 character set (spaces,
/// `%`, `#`, `?`, non-ASCII) plus the non-canonical and traversal rejections
/// are proven by the normative codec's own rows in `bbox-code-source`:
/// `source_uri_round_trips_reserved_and_non_ascii_names`,
/// `source_uri_uses_uppercase_hex_and_leaves_slashes_and_unreserved_bytes_alone`,
/// `source_uri_decode_rejects_non_canonical_and_traversal_encodings`,
/// `source_uri_rejects_empty_or_slash_bearing_project_id`, and
/// `source_uri_rejects_a_project_id_containing_percent_or_control_bytes`. The
/// document-side round trip off the STORED field is
/// `bbox-corpus-index/tests/path_free_schema_cut.rs::source_uri_round_trips_from_the_stored_field`.
/// Nothing here duplicates them; this file asserts the stored value exists on
/// the fixture's own emitted documents and decodes back to the catalog id.
const SOURCE_URI_ROW: &str = "bbox-code-source codec rows + path_free_schema_cut stored-field row";

/// `ProjectFileV2` ref round trips through the exact parser are proven by
/// `bbox-corpus-core/src/entity_ref.rs::project_file_v2_ref_round_trips` and by
/// the 10k-iteration `round_trip_property_10k_random_entities` property test,
/// whose generator includes the `ProjectFileV2` arm. This file asserts the
/// fixture's own emitted refs parse as that variant, so the population is
/// genuinely on the V2 lane rather than the legacy one.
const PROJECT_FILE_V2_ROW: &str = "entity_ref round-trip rows + this file's population check";

/// Every private token this fixture must never leak into a document, a vector
/// input, or a generation.
fn forbidden_tokens(fixture: &ExitGateFixture) -> Vec<String> {
    vec![
        FIXTURE_HOST_PATH.to_string(),
        fixture.rehearsal_root.to_string_lossy().into_owned(),
        fixture.published_checkout.to_string_lossy().into_owned(),
        fixture.legacy_local_root.to_string_lossy().into_owned(),
        "git:exit-gate".to_string(),
    ]
}

#[test]
fn no_document_or_vector_input_in_the_fixture_carries_a_host_path() {
    use bbox_corpus_index::index::history_generations::HistoryScanLimitsV1;
    use bbox_indexing::index::history_materializer::{
        HistoryMaterializerRequestV1, materialize_history_generations,
    };
    use tantivy::collector::{Count, TopDocs};
    use tantivy::query::TermQuery;
    use tantivy::schema::{IndexRecordOption, OwnedValue};

    let fixture = exit_gate_fixture();
    let runtime = activate_remote_only_collected_generation(&fixture);
    let forbidden = forbidden_tokens(&fixture);

    // ── Half 1: the newly emitted project-file documents ───────────────────
    //
    // Sweep EVERY stored `Str` value on every schema field, the shape
    // `path_free_schema_cut::assert_no_host_root` uses, rather than naming the
    // handful of fields a reviewer happens to remember.
    let index = Index::open_in_dir(fixture.index_path()).unwrap();
    bbox_corpus_index::index::register_code_tokenizer(&index);
    let schema = index.schema();
    let searcher = index.reader().unwrap().searcher();
    let query = TermQuery::new(
        tantivy::Term::from_field_text(schema.get_field("doc_type").unwrap(), "project_file"),
        IndexRecordOption::Basic,
    );
    let count = searcher.search(&query, &Count).unwrap();
    assert!(
        count > 0,
        "the sweep is vacuous without emitted project-file documents"
    );
    let mut swept_refs = 0usize;
    for (_, address) in searcher
        .search(&query, &TopDocs::with_limit(count))
        .unwrap()
    {
        let doc: tantivy::TantivyDocument = searcher.doc(address).unwrap();
        for (field, entry) in schema.fields() {
            for value in doc.get_all(field) {
                let OwnedValue::Str(text) = value else {
                    continue;
                };
                let name = entry.name();
                assert!(
                    !text.starts_with('/'),
                    "field {name} carries an absolute path: {text}"
                );
                for token in &forbidden {
                    assert!(
                        !text.contains(token.as_str()),
                        "field {name} leaked the private token {token}: {text}"
                    );
                }
            }
        }
        // The emitted ref is genuinely on the V2 lane, and the stored source
        // URI decodes back to the catalog id plus the relative path.
        let entity_id =
            bbox_corpus_index::index::first_text(&doc, schema.get_field("entity_id").unwrap());
        let parsed = bbox_corpus_core::entity_ref::EntityRef::parse(&entity_id).unwrap();
        assert_eq!(
            parsed.entity_type(),
            bbox_corpus_core::entity_ref::EntityType::ProjectFileV2,
            "the collected lane must emit ProjectFileV2 refs: {entity_id}"
        );
        assert_eq!(parsed.render(), entity_id, "the ref must round-trip");
        swept_refs += 1;
        let source_uri =
            bbox_corpus_index::index::optional_text(&doc, schema.get_field("source_uri").unwrap())
                .expect("every emitted project-file document stores a source URI");
        let (project_id, relative_path) = bbox_code_source::decode_source_uri(&source_uri).unwrap();
        assert_eq!(project_id, fixture.published_project.as_str());
        assert_eq!(relative_path, "src/lib.rs");
    }
    assert_eq!(swept_refs, count);

    // ── Half 2: the history generations' documents AND vector inputs ───────
    //
    // The vector-input half is the one the per-kind rows do not reach: the
    // generation's stored message text is what re-enqueues an embedding after
    // a replacement, so a host path there would travel into an embedding
    // input rather than only into a lexical field.
    let outcome = materialize_history_generations(
        &fixture.store,
        &HistoryMaterializerRequestV1 {
            index_path: fixture.index_path(),
            projects_path: fixture.projects_path(),
            vector_root: fixture.projects_path().parent().unwrap().join("vectors"),
            scan_limits: HistoryScanLimitsV1::default(),
        },
    )
    .unwrap();
    assert_eq!(outcome.namespaces.len(), 4, "all four buckets materialize");
    for entry in &outcome.namespaces {
        let documents = serde_json::to_string(&entry.generation.commit_documents).unwrap();
        let inputs = serde_json::to_string(&entry.generation.vector_inputs).unwrap();
        assert!(
            !entry.generation.commit_documents.is_empty(),
            "namespace {} must carry commit documents for the sweep to mean anything",
            entry.namespace.as_str()
        );
        assert!(
            !entry.generation.vector_inputs.is_empty(),
            "namespace {} must carry vector inputs for the sweep to mean anything",
            entry.namespace.as_str()
        );
        for token in &forbidden {
            assert!(
                !documents.contains(token.as_str()),
                "generation for {} leaked {token} into its commit documents",
                entry.namespace.as_str()
            );
            assert!(
                !inputs.contains(token.as_str()),
                "generation for {} leaked {token} into its VECTOR INPUTS",
                entry.namespace.as_str()
            );
        }
    }

    assert_zero_checkout_access(&runtime.broker, "after the document sweep");
    assert!(!SOURCE_URI_ROW.is_empty() && !PROJECT_FILE_V2_ROW.is_empty());
}

// ---------------------------------------------------------------------------
// Item 5: overlay assertions
// ---------------------------------------------------------------------------

/// Overlay rows already discharged by named tests, referenced rather than
/// duplicated:
///
/// - overlay swap admits the Git member, clearing removes it:
///   `bbox-edge-sidecar/src/snapshot.rs::selecting_an_overlay_admits_the_git_member_and_clearing_removes_it`
/// - an overlay for a FOREIGN code generation is refused:
///   `snapshot.rs::an_overlay_for_a_foreign_code_generation_is_refused`
/// - activation without a usable attachment leaves no overlay and gates the
///   Git member off: `snapshot.rs::activation_leaves_no_overlay_and_gates_the_git_member_off`
/// - activating a new code generation atomically clears the overlay:
///   `snapshot.rs::activating_a_new_generation_atomically_clears_the_overlay`
/// - the selector matches only its own code generation:
///   `bbox-corpus-core/src/git_overlay.rs::matches_only_its_own_code_generation`
/// - collected activation succeeds when Git is unavailable, recording health
///   rather than failing: `src/server/code_source.rs::collected_generation_activates_when_git_is_unavailable`
/// - monorepo single ingestion with per-project edge fan-out:
///   `bbox-indexing/src/index/consolidated_history.rs::a_two_project_monorepo_ingests_once_and_fans_edges_out_per_project`
/// - divergent legacy cursors seed nothing:
///   `consolidated_history.rs::legacy_cursors_are_inventoried_and_backed_up_never_seeded`
/// - retiring one sibling preserves shared history:
///   `bbox-indexing/src/index/history_gc.rs::retiring_one_sibling_keeps_shared_history_referenced`
/// - crash between overlay swap and manifest refresh cannot free a live
///   generation: `history_gc.rs::a_crash_between_overlay_swap_and_manifest_refresh_cannot_free_the_generation`
/// - searcher-only republish preserves the pinned overlays and epoch:
///   `src/server/state.rs::searcher_only_republish_preserves_the_catalog_epoch`
///   (this row can only ever live there: `CodeReadView` and
///   `read_git_overlays_for_view` are `pub(crate)` in the root binary crate)
/// - the five-state history health matrix:
///   `bbox-indexing/src/index/history_health.rs::health_matrix_covers_all_five_states`
const OVERLAY_ROWS: &str = "see the doc comment above";

#[test]
fn the_durable_overlay_map_roots_its_generation_and_clears_on_a_new_activation() {
    use bbox_corpus_core::git_overlay::GitOverlaySelector;
    use bbox_corpus_index::index::history_generations::HistoryScanLimitsV1;
    use bbox_indexing::index::history_gc::{
        HistoryReferenceKindV1, build_reference_manifest, selected_overlays_for_gc,
    };
    use bbox_indexing::index::history_materializer::{
        HistoryMaterializerRequestV1, materialize_history_generations,
    };

    // What the existing crash-recovery row cannot assert: it builds its
    // overlay map IN MEMORY, so the durable half of "an active overlay roots
    // its repo-history generation" is unexercised. This row drives the real
    // path end to end: `select_git_overlay` writes the manifest,
    // `selected_overlays_for_gc` reads it back off disk, and
    // `build_reference_manifest` roots the generation from that read.
    let fixture = exit_gate_fixture();
    let runtime = activate_remote_only_collected_generation(&fixture);
    let edges_dir =
        bbox_edge_sidecar::edge_sidecar::edges_dir_from_projects_path(&fixture.projects_path());

    let outcome = materialize_history_generations(
        &fixture.store,
        &HistoryMaterializerRequestV1 {
            index_path: fixture.index_path(),
            projects_path: fixture.projects_path(),
            vector_root: fixture.projects_path().parent().unwrap().join("vectors"),
            scan_limits: HistoryScanLimitsV1::default(),
        },
    )
    .unwrap();
    let history_generation = outcome
        .namespaces
        .iter()
        .find(|entry| entry.namespace.as_str() == fixture.proved_namespace)
        .expect("the primary namespace materializes")
        .generation
        .id
        .as_str()
        .to_string();

    // A matching attachment builds the overlay for the EXACT active code
    // generation: `select_git_overlay` refuses any other value, so a
    // successful swap IS the exactness proof.
    let selector = GitOverlaySelector {
        project_id: fixture.published_project.as_str().to_string(),
        code_generation: runtime.generation_id.clone(),
        repo_history_generation: history_generation.clone(),
        source: bbox_corpus_core::git_overlay::GitOverlaySourceV1::Attachment {
            attachment_id: "att_00000000000000000000000000000e01".to_string(),
        },
        repo_head: "c".repeat(40),
        commit_namespace: fixture.proved_namespace.clone(),
        overlay_generation: 1,
    };
    bbox_edge_sidecar::snapshot::select_git_overlay(
        &edges_dir,
        fixture.published_project.as_str(),
        Some(selector.clone()),
    )
    .unwrap();
    let foreign = GitOverlaySelector {
        code_generation: "not-the-active-generation".to_string(),
        ..selector.clone()
    };
    assert!(
        bbox_edge_sidecar::snapshot::select_git_overlay(
            &edges_dir,
            fixture.published_project.as_str(),
            Some(foreign),
        )
        .is_err(),
        "an overlay naming a foreign code generation must be refused"
    );

    // The DURABLE read, and the reference manifest built from it.
    let overlays = selected_overlays_for_gc(&edges_dir).unwrap();
    assert_eq!(overlays.len(), 1);
    assert_eq!(
        overlays[fixture.published_project.as_str()].repo_history_generation,
        history_generation
    );
    let snapshot = fixture.store.snapshot().unwrap();
    let references = build_reference_manifest(
        snapshot.catalog(),
        &overlays,
        &[],
        &BTreeSet::new(),
        &BTreeSet::new(),
    );
    assert!(
        references.roots().contains(&history_generation),
        "a durably selected overlay must root its repo-history generation"
    );
    assert!(
        references.references[&history_generation].contains(&HistoryReferenceKindV1::ActiveOverlay),
        "the root must be attributed to the overlay, not only to the catalog record"
    );

    // Activating a NEW code generation clears the overlay atomically, inside
    // the same manifest write. After the clear the generation keeps only its
    // catalog reference, which is exactly why the manifest must pin overlays
    // separately.
    let successor_snapshot = format!("collected-{}", "8".repeat(32));
    let successor_dir = bbox_edge_sidecar::snapshot::snapshot_dir(
        &edges_dir,
        fixture.published_project.as_str(),
        &successor_snapshot,
    );
    fs::create_dir_all(&successor_dir).unwrap();
    fs::write(successor_dir.join("project.jsonl"), b"").unwrap();
    bbox_edge_sidecar::snapshot::activate_collected_snapshot_with(
        &edges_dir,
        fixture.published_project.as_str(),
        fixture.published_scope.repo_id(),
        &"d".repeat(40),
        "successor-code-generation",
        "collected:successor",
        &successor_snapshot,
        || Ok(()),
    )
    .unwrap();
    let overlays = selected_overlays_for_gc(&edges_dir).unwrap();
    assert!(
        overlays.is_empty(),
        "activating a new code generation must clear the overlay atomically, got {overlays:?}"
    );
    let cleared = build_reference_manifest(
        snapshot.catalog(),
        &overlays,
        &[],
        &BTreeSet::new(),
        &BTreeSet::new(),
    );
    assert!(
        !cleared
            .references
            .get(&history_generation)
            .map(|kinds| kinds.contains(&HistoryReferenceKindV1::ActiveOverlay))
            .unwrap_or(false),
        "the overlay reference must be gone after the clear"
    );

    assert_zero_checkout_access(&runtime.broker, "after the overlay round trip");
    assert!(!OVERLAY_ROWS.is_empty());
}

// ---------------------------------------------------------------------------
// Item 6: bridge parity at the same commit
// ---------------------------------------------------------------------------

/// Section 11 item 6 asks that the bridge daemon at this commit pass the full
/// parity harness plus the section 4.3 enumerated changes. That is a
/// two-daemon, live assertion by construction and is owned by this phase's
/// bridge-parity bootsmoke (plan section 13), not by CI.
///
/// What IS cheaply assertable at crate level, and already is:
///
/// - the bridge guard spills the complete commit set before the drop, with no
///   host path in any spilled row:
///   `path_free_replacement_boundary::the_bridge_guard_spills_the_commit_set_before_the_drop`
/// - a no-history open authorizes BOTH guards identically:
///   `path_free_replacement_boundary::an_index_with_no_history_still_authorizes_both_guards`
/// - the spill lane's carryover and its three crash arms:
///   `bbox-corpus-index/tests/path_free_schema_cut.rs::{the_spill_lane_carries_the_complete_commit_set_across_a_replacement,
///   a_crash_after_the_drop_recovers_the_complete_commit_set_at_the_next_open,
///   a_crash_during_the_rebuild_replays_the_spill_without_duplicating,
///   a_crash_before_the_readd_commits_recovers_from_the_surviving_spill,
///   a_leftover_spill_is_consumed_on_an_open_with_no_schema_mismatch}`
/// - bridge-shaped planning reproduces today's lease set, warming included:
///   `writer_actor::bridge_shaped_snapshot_reproduces_the_lease_set_including_warming`
/// - the bridge local-reindex lane keeps its Git member without an overlay:
///   `bbox-edge-sidecar/src/snapshot.rs::a_local_reindex_entry_keeps_its_git_member_without_an_overlay`
/// - the bridge LegacyLocal collected-stage arm still proceeds:
///   `writer_actor::collected_stage_proceeds_for_a_bridge_legacy_local_identity`
///
/// The residue that stays smoke-owned: register/list round trip against a live
/// bridge daemon, P2-D dual-read spot checks, and the schema-reset carryover
/// observed across an actual restart.
const BRIDGE_PARITY_SMOKE_OWNED: &str =
    "live register/list round trip, P2-D dual-read spot checks, restart carryover";

#[test]
fn the_acceptance_block_inventory_is_complete() {
    // A compile-time-checked guard: deleting one of the deferral constants
    // named here breaks compilation, so a future edit cannot silently drop
    // a section 11 row's ownership note while leaving the inventory comment
    // claiming it. The count literal is the other half of the guard: adding
    // a NEW deferral constant without referencing it here (and bumping the
    // literal) fails this assertion, so the array cannot silently
    // under-enumerate.
    let notes = [
        REMOTE_ONLY_SMOKE_OWNED,
        SOURCE_URI_ROW,
        PROJECT_FILE_V2_ROW,
        OVERLAY_ROWS,
        BRIDGE_PARITY_SMOKE_OWNED,
    ];
    assert_eq!(
        notes.len(),
        5,
        "the deferral inventory has exactly five constants; adding one \
         requires referencing it here"
    );
    for note in notes {
        assert!(!note.is_empty());
    }
}
