//! End-to-end durable-backfill proof against a REAL MIGRATED root, driving the
//! production stamper through preflight, apply, and verify.
//!
//! This lives root-side because the production `LegacyRowStamperV1` does: the
//! write half needs the owner crates' real schemas, and only the root crate
//! sees all fourteen owners alongside the facade. Every other backfill test is
//! a unit test against a synthetic stamper; this is the one that proves the
//! real one reaches real owner stores through the real facade.
//!
//! DUPLICATION, DELIBERATE AND STATED. The fixture prelude below (`write`,
//! `git`, `initialize_empty_provenance_ref`, `initialize_empty_owner_state`,
//! `config`, `RehearsalFixture`, `write_generation`, `prepare_rehearsal`) is a
//! VERBATIM COPY of the same helpers in
//! `crates/bbox-indexing/tests/project_catalog_migration_facade.rs`. Keep the
//! two in step: a change there that this copy does not track will show up as a
//! fixture that no longer matches the migration it is supposed to rehearse.
//!
//! Sharing them through a `test-support` module in `bbox-indexing/src` was
//! built and REJECTED on evidence, not preference. `prepare_rehearsal` and its
//! helpers shell out to git and construct checkouts, and a `test-support`
//! module ships whenever the feature is on, so relocating them into `src/`
//! added five NEW sites to the catalog ownership proof (`direct_git_process`
//! and `legacy_publisher`). That proof asserts catalog runtime code reaches a
//! checkout only through a capability lease, and its per-site reasons drive the
//! deletion campaign; putting test fixtures inside it would corrupt campaign
//! evidence to save duplication. `tests/` is never scanned, so this copy costs
//! visible, inert lines instead. The trade was ruled deliberately.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use bbox_code_source::{
    GenerationDescriptor, GenerationState, ManifestEntry, SCHEMA_VERSION, WALKER_POLICY_VERSION,
    dirty_fingerprint, generation_id, manifest_sha256, source_selector,
};
use bbox_code_source_store::{
    ActivationRecord, CodeSourceStore, CodeSourceStorePaths, MigrationEffectiveSourceManifestV1,
    MigrationEffectiveSourceSelectionV1, StoredGeneration,
    encode_migration_effective_source_manifest_v1,
};
use bbox_config::config::{self, Config, LoadOptions};
use bbox_corpus_core::identity::PublishedScope;
use bbox_corpus_core::project_catalog::{
    LegacyPathBindingId, LegacyPathBindingStatus, LegacyPathLedgerEntry, LegacyPathRelationship,
    ProjectId,
};
use bbox_corpus_core::project_catalog_snapshot::{
    LegacySelectorMembersV1, OwnerRowRequestV1, singleton_selector_members,
};
use bbox_corpus_index::index::TranscriptIndex;
use bbox_corpus_index::index::schema_replacement::CatalogIndexReplacementCause;
use bbox_edge_sidecar::manifest::ManifestIndex;
use bbox_indexing::index::schema_rebuild::SchemaRebuildResume;
use bbox_indexing::project_catalog_backfill::{
    ATTACHMENT_RELOCATION_SOURCE, DurableBackfillApplyOutcomeV1, DurableBackfillApplyRequestV1,
    DurableBackfillPreflightRequestV1, DurableBackfillStatusV1, DurableBackfillVerifyReceiptV1,
    DurableBackfillVerifyRequestV1, LegacyRowObservationV1, LegacyRowOwnerReaderV1,
    LegacyRowStampCoverageV1, LegacyRowStampOutcomeV1, LegacyRowStamperV1,
    ProjectCatalogDurableBackfillFacadeV1,
};
use bbox_indexing::project_catalog_inventory::{
    LegacyPathStoreKindV1, ProjectCatalogMigrationStatusV1, QuarantineCollectedV1,
    SelectedScopeOwnerV1, decode_migration_report_v1, decode_migration_resolution_v1,
    encode_migration_resolution_v1,
};
use bbox_indexing::project_catalog_migration::{
    ProjectCatalogMigrationApplyRequestV1, ProjectCatalogMigrationError,
    ProjectCatalogMigrationFacadeV1, ProjectCatalogMigrationLayoutOverridesV1,
    ProjectCatalogMigrationPreflightRequestV1, ProjectCatalogMigrationResolvedLayoutV1,
    ProjectCatalogTargetSelectionV1, project_catalog_migration_store_limits,
};
use bbox_indexing::project_catalog_store::ProjectCatalogStore;
use bbox_indexing::publisher::PublisherRefStore;
use bbox_vectors::VectorStore;
use blackbox::project_catalog_stamper::{
    ProjectCatalogOwnerRowReaderV1, ProjectCatalogOwnerRowStamperV1, ProjectCatalogStamperPathsV1,
};
use sha2::{Digest, Sha256};

// ---------------------------------------------------------------------------
// Replicated fixture prelude - see the DUPLICATION note above.
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
        .env("GIT_AUTHOR_NAME", "Migration Fixture")
        .env("GIT_AUTHOR_EMAIL", "fixture@example.invalid")
        .env("GIT_COMMITTER_NAME", "Migration Fixture")
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
        .env("GIT_AUTHOR_NAME", "Migration Fixture")
        .env("GIT_AUTHOR_EMAIL", "fixture@example.invalid")
        .env("GIT_COMMITTER_NAME", "Migration Fixture")
        .env("GIT_COMMITTER_EMAIL", "fixture@example.invalid")
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git mktree failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
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

struct RehearsalFixture {
    winner_checkout: PathBuf,
    collision_winner_checkout: PathBuf,
    loser_checkout: PathBuf,
    winner_project: ProjectId,
    collision_winner_project: ProjectId,
    loser_project: ProjectId,
    winner_generation: String,
    collision_winner_generation: String,
    loser_generation: String,
    scope: PublishedScope,
    collision_scope: PublishedScope,
}

fn write_generation(
    paths: &CodeSourceStorePaths,
    project_id: &ProjectId,
    scope: &PublishedScope,
    producer: &str,
    head_commit: &str,
    ordinal: u64,
) -> (String, MigrationEffectiveSourceSelectionV1) {
    let content = format!("fn {producer}() {{}}\n").into_bytes();
    let entries = vec![ManifestEntry {
        relative_path: format!("src/{producer}.rs"),
        content_sha256: hex::encode(Sha256::digest(&content)),
        size: content.len() as u64,
    }];
    let descriptor = GenerationDescriptor {
        schema_version: SCHEMA_VERSION,
        walker_policy_version: WALKER_POLICY_VERSION.to_string(),
        scope: scope.clone(),
        head_commit: head_commit.to_string(),
        dirty_fingerprint: dirty_fingerprint(head_commit, &entries),
        manifest_sha256: manifest_sha256(&entries),
        file_count: 1,
        logical_bytes: content.len() as u64,
    };
    let generation_id = generation_id(producer, &descriptor);
    let selector = format!(
        "{}:m0123456789abcdef",
        source_selector(project_id.as_str(), &generation_id)
    );
    let stored = StoredGeneration {
        version: 1,
        generation_id: generation_id.clone(),
        producer_id: producer.to_string(),
        ordinal,
        descriptor,
        state: GenerationState::Active,
        diagnostic: None,
        created_unix_secs: ordinal,
        materialized_doc_count: Some(1),
        entity_inventory_sha256: Some("c".repeat(64)),
    };
    write(
        &paths.generation_metadata(scope, &generation_id).unwrap(),
        &serde_json::to_vec(&stored).unwrap(),
    );
    let mut manifest = Vec::new();
    serde_json::to_writer(&mut manifest, &entries[0]).unwrap();
    manifest.push(b'\n');
    write(
        &paths.generation_manifest(scope, &generation_id).unwrap(),
        &manifest,
    );
    write(
        &paths.activation(project_id),
        &serde_json::to_vec(&ActivationRecord {
            version: 1,
            project_id: project_id.to_string(),
            generation_id: generation_id.clone(),
            selector: selector.clone(),
            snapshot_id: format!("collected-{:032x}", ordinal),
            document_count: 1,
            entity_inventory_sha256: "c".repeat(64),
            current_chunk_targets: Default::default(),
            activated_unix_secs: ordinal,
            cutback_pending: false,
            diagnostic: None,
        })
        .unwrap(),
    );
    (
        generation_id.clone(),
        MigrationEffectiveSourceSelectionV1 {
            project_id: project_id.clone(),
            published_scope: scope.clone(),
            generation_id,
            selector,
        },
    )
}

/// `register_history_projects` adds one v1 project, checkout, and `.bbox`
/// repo_id marker per rebuild namespace.
///
/// Without them the four staged namespaces are `Unclaimed` at migration time
/// and the preflight refuses every one with `unsupported_legacy_namespace`,
/// which is why the Drift chain stages AFTER the capture instead. Registering
/// them makes each one `Proved` - exactly one project per repo_id, so no new
/// scope or activation conflict appears and the existing resolution ceremony
/// is untouched. The four buckets are then assigned post-migration by
/// `bind_rebuild_history_records`, where they belong: bucket membership is a
/// CATALOG fact, not a v1 attribution fact.
fn prepare_rehearsal(
    root: &Path,
    config: &Config,
    register_history_projects: bool,
) -> RehearsalFixture {
    let winner_checkout = root.join("checkouts").join("winner-checkout");
    fs::create_dir_all(&winner_checkout).unwrap();
    git(&winner_checkout, &["init", "-q"]);
    git(&winner_checkout, &["checkout", "-qb", "main"]);
    write(
        &winner_checkout.join(".bbox/config.toml"),
        b"[project]\nrepo_id = \"neutral-repository\"\n",
    );
    git(&winner_checkout, &["add", ".bbox"]);
    git(
        &winner_checkout,
        &["commit", "-qm", "seed migration fixture"],
    );
    let winner_head_commit = git(&winner_checkout, &["rev-parse", "HEAD"]);
    let collision_winner_checkout = root.join("checkouts").join("collision-winner-checkout");
    fs::create_dir_all(&collision_winner_checkout).unwrap();
    git(&collision_winner_checkout, &["init", "-q"]);
    git(&collision_winner_checkout, &["checkout", "-qb", "main"]);
    write(
        &collision_winner_checkout.join(".bbox/config.toml"),
        b"[project]\nrepo_id = \"neutral-collision\"\n",
    );
    git(&collision_winner_checkout, &["add", ".bbox"]);
    git(
        &collision_winner_checkout,
        &["commit", "-qm", "seed collision fixture"],
    );
    let collision_head_commit = git(&collision_winner_checkout, &["rev-parse", "HEAD"]);
    let loser_checkout = root.join("checkouts").join("loser-checkout");
    let clone = Command::new("git")
        .args([
            "clone",
            "-q",
            collision_winner_checkout.to_str().unwrap(),
            loser_checkout.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(
        clone.status.success(),
        "git clone failed: {}",
        String::from_utf8_lossy(&clone.stderr)
    );
    for checkout in [
        &winner_checkout,
        &collision_winner_checkout,
        &loser_checkout,
    ] {
        initialize_empty_provenance_ref(checkout, config);
    }

    let history_projects: Vec<(ProjectId, &str, PathBuf)> = if register_history_projects {
        REBUILD_NAMESPACE_STAGING
            .iter()
            .map(|(namespace, _)| {
                let checkout = root.join("checkouts").join(format!("{namespace}-checkout"));
                fs::create_dir_all(&checkout).unwrap();
                git(&checkout, &["init", "-q"]);
                git(&checkout, &["checkout", "-qb", "main"]);
                write(
                    &checkout.join(".bbox/config.toml"),
                    format!("[project]\nrepo_id = \"{namespace}\"\n").as_bytes(),
                );
                git(&checkout, &["add", ".bbox"]);
                git(&checkout, &["commit", "-qm", "seed history namespace"]);
                initialize_empty_provenance_ref(&checkout, config);
                (
                    ProjectId::parse(format!("project-{namespace}")).unwrap(),
                    *namespace,
                    checkout,
                )
            })
            .collect()
    } else {
        Vec::new()
    };

    let state = root.join("state");
    initialize_empty_owner_state(root);
    let winner_project = ProjectId::parse("neutral-winner").unwrap();
    let collision_winner_project = ProjectId::parse("neutral-collision-winner").unwrap();
    let loser_project = ProjectId::parse("neutral-loser").unwrap();
    let mut project_rows = serde_json::json!([
                {
                    "project_id": winner_project,
                    "repo_id": "neutral-repository",
                    "canonical_path": winner_checkout,
                    "registered_at": "2026-01-02T03:04:05Z",
                    "is_git_repo": true,
                    "languages": [],
                    "aliases": []
                },
                {
                    "project_id": collision_winner_project,
                    "repo_id": "neutral-collision",
                    "canonical_path": collision_winner_checkout,
                    "registered_at": "2026-01-02T03:04:06Z",
                    "is_git_repo": true,
                    "languages": [],
                    "aliases": []
                },
                {
                    "project_id": loser_project,
                    "repo_id": "neutral-collision",
                    "canonical_path": loser_checkout,
                    "registered_at": "2026-01-02T03:04:07Z",
                    "is_git_repo": true,
                    "languages": [],
                    "aliases": []
                }
    ]);
    for (ordinal, (project_id, repo_id, checkout)) in history_projects.iter().enumerate() {
        project_rows
            .as_array_mut()
            .unwrap()
            .push(serde_json::json!({
                "project_id": project_id,
                "repo_id": repo_id,
                "canonical_path": checkout,
                "registered_at": format!("2026-01-02T03:05:{:02}Z", ordinal),
                "is_git_repo": true,
                "languages": [],
                "aliases": []
            }));
    }
    write(
        &state.join("projects.json"),
        serde_json::to_vec_pretty(&serde_json::json!({
            "version": 1,
            "projects": project_rows,
        }))
        .unwrap()
        .as_slice(),
    );
    let code_sources = CodeSourceStore::open(
        state.join("code-sources"),
        project_catalog_migration_store_limits(config),
    )
    .unwrap();
    let scope = PublishedScope::try_new("neutral-repository", ".").unwrap();
    let collision_scope = PublishedScope::try_new("neutral-collision", ".").unwrap();
    let paths = CodeSourceStorePaths::new(code_sources.root()).unwrap();
    let (winner_generation, winner_selection) = write_generation(
        &paths,
        &winner_project,
        &scope,
        "winner",
        &winner_head_commit,
        1,
    );
    let (collision_winner_generation, collision_winner_selection) = write_generation(
        &paths,
        &collision_winner_project,
        &collision_scope,
        "collision_winner",
        &collision_head_commit,
        2,
    );
    let (loser_generation, loser_selection) = write_generation(
        &paths,
        &loser_project,
        &collision_scope,
        "loser",
        &collision_head_commit,
        3,
    );
    let mut selections = vec![
        winner_selection,
        collision_winner_selection,
        loser_selection,
    ];
    selections.sort_by(|left, right| left.project_id.cmp(&right.project_id));
    write(
        &paths.anchor(),
        &encode_migration_effective_source_manifest_v1(&MigrationEffectiveSourceManifestV1 {
            version: 1,
            selections,
        })
        .unwrap(),
    );
    let publisher_path = state.join("bro").join("publisher-refs.json");
    let mut publisher = PublisherRefStore::open(&publisher_path).unwrap();
    let pin = publisher.pin_candidate(&scope, &winner_checkout).unwrap();
    publisher.persist_pin_candidate(&pin).unwrap();
    RehearsalFixture {
        winner_checkout,
        collision_winner_checkout,
        loser_checkout,
        winner_project,
        collision_winner_project,
        loser_project,
        winner_generation,
        collision_winner_generation,
        loser_generation,
        scope,
        collision_scope,
    }
}

// ---------------------------------------------------------------------------
// Migrated-root construction and the backfill end-to-end chain
// ---------------------------------------------------------------------------

/// Drive the P1-C rehearsal ceremony to a MIGRATED root.
///
/// The backfill runs on `MigratedV1` origins, and the migration marker its
/// preflight sources publisher dispositions from exists only there, so the
/// fixture cannot shortcut through `initialize_empty` (that yields `FreshV2`
/// with no marker). Hand-installing a marker is also not an option: markers are
/// journal-bound on every read, so a fabricated one without a matching
/// transaction journal refuses. The ceremony is the only honest route.
///
/// `stage_legacy_history` stages the four rebuild namespaces into the index
/// BEFORE the migration captures it, which is the only way an Equality proof is
/// reachable (Q-F). The recorded source fingerprint folds the index's own commit
/// rows, so a namespace staged AFTER the capture makes the recomputed
/// fingerprint differ by construction and lands the rebuild in Drift. Staging
/// before it also puts every namespace in the asset, which Equality mode then
/// holds to its recorded count and commitment - the strictness is the point.
fn migrated_rehearsal_root(
    root: &Path,
    stage_legacy_history: bool,
) -> (ProjectCatalogMigrationResolvedLayoutV1, Config) {
    let config = config(root);
    let rehearsal_root = root.join("rehearsal");
    fs::create_dir_all(&rehearsal_root).unwrap();
    let fixture = prepare_rehearsal(&rehearsal_root, &config, stage_legacy_history);
    let review = rehearsal_root.join("review");
    fs::create_dir_all(&review).unwrap();

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

    if stage_legacy_history {
        let index_root = rehearsal.rebuild_index_paths().index_root;
        for (namespace, commits) in REBUILD_NAMESPACE_STAGING {
            stage_commit_documents(&index_root, namespace, *commits);
        }
    }

    let report_path = review.join("report.json");
    let resolution_path = review.join("resolution.json");
    let preflight = |report: &PathBuf, resolution: &PathBuf| {
        ProjectCatalogMigrationFacadeV1::preflight(ProjectCatalogMigrationPreflightRequestV1 {
            layout: rehearsal.clone(),
            report_path: report.clone(),
            resolution_path: resolution.clone(),
            sensitive_report_path: None,
        })
        .unwrap()
    };
    preflight(&report_path, &resolution_path);
    let report = decode_migration_report_v1(&fs::read(&report_path).unwrap()).unwrap();
    let mut resolution =
        decode_migration_resolution_v1(&fs::read(&resolution_path).unwrap()).unwrap();
    resolution.selected_scope_owners.push(SelectedScopeOwnerV1 {
        resolution_id: report.scope_conflicts[0].conflict_id.clone(),
        scope: fixture.collision_scope.clone(),
        owner_project_id: fixture.collision_winner_project.clone(),
        losing_project_ids: [fixture.loser_project.clone()].into_iter().collect(),
        owned_aliases: Default::default(),
    });
    fs::write(
        &resolution_path,
        encode_migration_resolution_v1(&resolution).unwrap(),
    )
    .unwrap();
    preflight(&report_path, &resolution_path);
    let report = decode_migration_report_v1(&fs::read(&report_path).unwrap()).unwrap();
    resolution.quarantine_collected.push(QuarantineCollectedV1 {
        resolution_id: report.activation_conflicts[0].conflict_id.clone(),
        project_id: fixture.loser_project.clone(),
        generation_id: fixture.loser_generation.clone(),
    });
    fs::write(
        &resolution_path,
        encode_migration_resolution_v1(&resolution).unwrap(),
    )
    .unwrap();
    let clean = preflight(&report_path, &resolution_path);
    assert_eq!(
        clean.receipt.status,
        ProjectCatalogMigrationStatusV1::Clean,
        "the fixture's migration preflight must be clean: {}",
        String::from_utf8_lossy(&fs::read(&report_path).unwrap())
    );
    ProjectCatalogMigrationFacadeV1::apply_rehearsal(ProjectCatalogMigrationApplyRequestV1 {
        rehearsal_layout: rehearsal.clone(),
        protected_layout: protected,
        report_path,
        resolution_path,
    })
    .unwrap();
    (rehearsal, config)
}

/// Write one owner store holding a single row that is VALID for that owner's
/// typed schema.
///
/// Validity is load-bearing rather than tidiness. An owner store is loaded and
/// re-persisted by ordinary daemon and rebuild paths, and a row that fails its
/// typed decode does not survive that round trip: the store loads empty and the
/// next save drops the row. A stub row therefore produces a root whose ledger
/// binds an owner row that quietly stopped existing, which is exactly the
/// inconsistency the durable owner verifier now refuses. Fixtures that mean to
/// represent a coherent migrated root must be coherent under a reload.
fn write_owner(path: &Path, array_field: &str, row: serde_json::Value) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(
        path,
        serde_json::to_vec(&serde_json::json!({
            "version": 1,
            array_field: [row],
        }))
        .unwrap(),
    )
    .unwrap();
}

#[derive(Clone, Copy)]
enum Owner {
    Knowledge,
    Thread,
    Note,
}

/// A migrated root whose CATALOG and OWNER STORES agree: every ledger binding
/// names a `source_row_id` that really exists in the owner store the layout
/// resolves, mapped to a project the catalog really contains.
struct Fixture {
    _dir: tempfile::TempDir,
    /// The canonicalized fixture root. Retained so a bootsmoke can point the
    /// real CLI at this fixture's config instead of the host's.
    root: PathBuf,
    layout: ProjectCatalogMigrationResolvedLayoutV1,
    /// Artifact paths live OUTSIDE the rehearsal root: inside it, the Q-C
    /// artifact-confinement condition refuses them.
    artifacts: PathBuf,
    knowledge_path: PathBuf,
    thread_path: PathBuf,
    note_path: PathBuf,
    project: ProjectId,
}

impl Fixture {
    fn new() -> Self {
        Self::build(false)
    }

    /// The same root with the rebuild's legacy history staged BEFORE the
    /// migration captured it, so the recorded source fingerprint folds those
    /// commit rows and an Equality proof becomes reachable (Q-F).
    fn with_history_staged_before_capture() -> Self {
        Self::build(true)
    }

    fn build(stage_legacy_history: bool) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let (layout, _config) = migrated_rehearsal_root(&root, stage_legacy_history);

        let owners = layout.stamper_owner_paths();
        // ISOLATION ASSERTION (carried condition): every owner path the
        // backfill can touch sits under the fixture root. Without this a
        // fixture that silently resolved a configured default would inventory
        // and STAMP the host's real stores, and the failure would look like a
        // passing test.
        for path in [
            &owners.knowledge_store_path,
            &owners.gap_store_path,
            &owners.thread_store_path,
            &owners.note_store_path,
            &owners.pin_store_path,
            &owners.roadmap_store_path,
            &owners.packet_root,
            &owners.proposal_root,
            &owners.slack_store_root,
            &owners.whiteboard_root,
            &owners.artifact_root,
            &owners.transcript_edge_root,
            &owners.task_store_path,
        ] {
            assert!(
                path.starts_with(&root),
                "fixture leaked outside its root: {}",
                path.display()
            );
        }

        // Two DIFFERENT owner stores, so the run proves cross-store dispatch
        // rather than one store exercised twice.
        write_owner(
            &owners.knowledge_store_path,
            "entries",
            serde_json::json!({
                "id": "kb1",
                "title": "fixture entry",
                "content": "fixture content",
                "category": "convention",
                "scope": "project",
                "project": "/legacy/one",
                "priority": "standard",
                "status": "active",
                "approval": "user_confirmed",
                "source": "fixture",
                "created_at": "2026-01-01T00:00:00Z",
                "updated_at": "2026-01-01T00:00:00Z",
            }),
        );
        write_owner(
            &owners.thread_store_path,
            "threads",
            serde_json::json!({
                "id": "th1",
                "topic": "fixture thread",
                "project": "/legacy/one",
                "status": "open",
                "sessions": [],
                "created_at": "2026-01-01T00:00:00Z",
                "last_activity": "2026-01-01T00:00:00Z",
            }),
        );
        // A third row behind a QUARANTINED binding. Converting it is what makes
        // apply mutate the catalog pair, which is the precondition for the
        // section 3.3 recovery sequencing: stamping alone writes owner stores
        // and leaves the pair (and so the four-hash identity) untouched, so a
        // conversion-free re-apply is merely idempotent rather than stale.
        write_owner(
            &owners.note_store_path,
            "notes",
            serde_json::json!({
                "id": "nt1",
                "kind": "learned",
                "body": "fixture note",
                "project": "/legacy/one",
                "created_at": "2026-01-01T00:00:00Z",
                "updated_at": "2026-01-01T00:00:00Z",
            }),
        );

        let projects_path = layout.projects_path().to_path_buf();
        let store = ProjectCatalogStore::open_existing(&projects_path).unwrap();
        let state = store.snapshot().unwrap();
        let epoch = state.epoch();
        // Map onto a project the MIGRATED catalog actually contains; the
        // validator refuses a binding naming an absent project, which is the
        // coherence a real fixture is supposed to have.
        let project = state
            .catalog()
            .projects
            .keys()
            .next()
            .expect("the migrated catalog carries projects")
            .clone();
        let project_for_fixture = project.clone();
        drop(state);
        store
            .transact(epoch, |_catalog, attachments| {
                for (index, (store_token, row_id, quarantined)) in [
                    ("knowledge", "kb1", false),
                    ("thread", "th1", false),
                    ("note", "nt1", true),
                ]
                .into_iter()
                .enumerate()
                {
                    let entry = LegacyPathLedgerEntry {
                        legacy_path_binding_id: LegacyPathBindingId::parse(format!(
                            "lpb_{:032x}",
                            index + 1
                        ))
                        .unwrap(),
                        historical_path: format!("/host/checkouts/alpha{index}"),
                        source_store: store_token.to_string(),
                        source_row_id: row_id.to_string(),
                        member_row_count: 1,
                        member_commitment_sha256: singleton_selector_members(row_id)
                            .commitment_sha256,
                        inventory_epoch: 1,
                        status: if quarantined {
                            LegacyPathBindingStatus::Quarantined {}
                        } else {
                            LegacyPathBindingStatus::Mapped {
                                project_id: project.clone(),
                                relationship: LegacyPathRelationship::Root,
                            }
                        },
                    };
                    attachments
                        .legacy_path_bindings
                        .insert(entry.legacy_path_binding_id.clone(), entry);
                }
                Ok(())
            })
            .unwrap();
        drop(store);

        let artifacts = root.join("artifacts-outside-the-layout");
        fs::create_dir_all(&artifacts).unwrap();
        Self {
            knowledge_path: owners.knowledge_store_path.clone(),
            thread_path: owners.thread_store_path.clone(),
            note_path: owners.note_store_path.clone(),
            project: project_for_fixture,
            _dir: dir,
            root,
            layout,
            artifacts,
        }
    }

    fn production_stamper(&self) -> Arc<ProjectCatalogOwnerRowStamperV1> {
        let owners = self.layout.stamper_owner_paths();
        Arc::new(
            ProjectCatalogOwnerRowStamperV1::new(
                ProjectCatalogStamperPathsV1 {
                    knowledge_store_path: owners.knowledge_store_path,
                    gap_store_path: owners.gap_store_path,
                    thread_store_path: owners.thread_store_path,
                    note_store_path: owners.note_store_path,
                    pin_store_path: owners.pin_store_path,
                    roadmap_store_path: owners.roadmap_store_path,
                    packet_root: owners.packet_root,
                    proposal_root: owners.proposal_root,
                    slack_store_root: owners.slack_store_root,
                    whiteboard_root: owners.whiteboard_root,
                    artifact_root: owners.artifact_root,
                    transcript_edge_root: owners.transcript_edge_root,
                    task_store_path: owners.task_store_path,
                },
                Default::default(),
            )
            .unwrap(),
        )
    }

    /// The production owner-row READER over the same owner paths the stamper
    /// writes, so a verify proves the rows this fixture's apply really touched.
    fn production_owner_reader(&self) -> Arc<ProjectCatalogOwnerRowReaderV1> {
        let owners = self.layout.stamper_owner_paths();
        Arc::new(
            ProjectCatalogOwnerRowReaderV1::new(
                ProjectCatalogStamperPathsV1 {
                    knowledge_store_path: owners.knowledge_store_path,
                    gap_store_path: owners.gap_store_path,
                    thread_store_path: owners.thread_store_path,
                    note_store_path: owners.note_store_path,
                    pin_store_path: owners.pin_store_path,
                    roadmap_store_path: owners.roadmap_store_path,
                    packet_root: owners.packet_root,
                    proposal_root: owners.proposal_root,
                    slack_store_root: owners.slack_store_root,
                    whiteboard_root: owners.whiteboard_root,
                    artifact_root: owners.artifact_root,
                    transcript_edge_root: owners.transcript_edge_root,
                    task_store_path: owners.task_store_path,
                },
                Default::default(),
            )
            .unwrap(),
        )
    }

    fn verify(&self) -> Result<DurableBackfillVerifyReceiptV1, ProjectCatalogMigrationError> {
        ProjectCatalogDurableBackfillFacadeV1::verify(DurableBackfillVerifyRequestV1 {
            layout: self.layout.clone(),
            target_selection: ProjectCatalogTargetSelectionV1::Rehearsal,
            owner_reader: self.production_owner_reader(),
        })
    }

    fn report_path(&self) -> PathBuf {
        self.artifacts.join("report.json")
    }

    fn resolution_path(&self) -> PathBuf {
        self.artifacts.join("resolution.json")
    }

    fn preflight(
        &self,
        stamper: Arc<dyn LegacyRowStamperV1>,
    ) -> Result<DurableBackfillStatusV1, ProjectCatalogMigrationError> {
        ProjectCatalogDurableBackfillFacadeV1::preflight(DurableBackfillPreflightRequestV1 {
            layout: self.layout.clone(),
            target_selection: ProjectCatalogTargetSelectionV1::Rehearsal,
            report_path: self.report_path(),
            resolution_path: self.resolution_path(),
            stamper,
            generated_at: "2026-08-05T00:00:00Z".to_string(),
        })
        .map(|receipt| receipt.status)
    }

    fn apply(
        &self,
        stamper: Arc<dyn LegacyRowStamperV1>,
    ) -> Result<DurableBackfillApplyOutcomeV1, ProjectCatalogMigrationError> {
        ProjectCatalogDurableBackfillFacadeV1::apply(DurableBackfillApplyRequestV1 {
            layout: self.layout.clone(),
            target_selection: ProjectCatalogTargetSelectionV1::Rehearsal,
            report_path: self.report_path(),
            resolution_path: self.resolution_path(),
            stamper,
            completed_at: "2026-08-05T00:00:01Z".to_string(),
        })
        .map(|receipt| receipt.outcome)
    }

    /// Rewrite the reviewed resolution to CONVERT the quarantined binding.
    ///
    /// Preflight writes the canonical empty resolution on a first run; this
    /// turns it into one that appends a superseding `Mapped` binding, which is
    /// what makes apply move the catalog pair.
    fn convert_the_quarantined_binding(&self) {
        let bytes = fs::read(self.resolution_path()).unwrap();
        let mut resolution: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        resolution["legacy_path_dispositions"] = serde_json::json!([{
            "disposition": "map_to_project_id",
            "legacy_path_binding_id": format!("lpb_{:032x}", 3),
            "project_id": self.project.as_str(),
            "relationship": "root",
        }]);
        fs::write(
            self.resolution_path(),
            serde_json::to_vec(&resolution).unwrap(),
        )
        .unwrap();
    }

    /// Seed an owner whose ONLY effective binding is UNSCOPED.
    ///
    /// This is the store an omission hides in. An unscoped row is counted per
    /// store and then never stamped and never read back, so a store carrying
    /// nothing else appears in the journal's classification and in NEITHER of
    /// the sets a mapped row would put it in. Seeded opt-in so the other
    /// fixtures' counts stay exactly what they were.
    fn seed_the_unscoped_only_store(&self) {
        let owners = self.layout.stamper_owner_paths();
        write_owner(
            &owners.pin_store_path,
            "pins",
            serde_json::json!({
                "id": "pn1",
                "title": "fixture pin",
                "content": "fixture content",
                "scope": "bro",
                "target": "executor",
                "project": "/legacy/unscoped",
                "created_at": "2026-01-01T00:00:00Z",
                "updated_at": "2026-01-01T00:00:00Z",
            }),
        );
        let store = ProjectCatalogStore::open_existing(self.layout.projects_path()).unwrap();
        let epoch = store.snapshot().unwrap().epoch();
        store
            .transact(epoch, |_catalog, attachments| {
                let entry = LegacyPathLedgerEntry {
                    legacy_path_binding_id: LegacyPathBindingId::parse(format!("lpb_{:032x}", 4))
                        .unwrap(),
                    historical_path: "/host/checkouts/unscoped".to_string(),
                    source_store: "pin".to_string(),
                    source_row_id: "pn1".to_string(),
                    member_row_count: 1,
                    member_commitment_sha256: singleton_selector_members("pn1").commitment_sha256,
                    inventory_epoch: 1,
                    status: LegacyPathBindingStatus::Unscoped {},
                };
                attachments
                    .legacy_path_bindings
                    .insert(entry.legacy_path_binding_id.clone(), entry);
                Ok(())
            })
            .unwrap();
    }

    /// Append the ledger binding an attachment RELOCATION mints, byte for byte
    /// as `project_catalog_admin` writes one when a checkout moves.
    ///
    /// It is Mapped, like an owner obligation, and it names an attachment id in
    /// `source_row_id`, which no owner holds and none ever will.
    fn add_relocation_binding(&self, binding_index: u128) {
        let store = ProjectCatalogStore::open_existing(self.layout.projects_path()).unwrap();
        let epoch = store.snapshot().unwrap().epoch();
        let project = self.project.clone();
        store
            .transact(epoch, |_catalog, attachments| {
                // The supersession key includes source_row_id: each minted
                // relocation names its own attachment so two test bindings
                // never collide as duplicate inventory source rows.
                let attachment_id = format!("att_{binding_index:032x}");
                let attachment_id = attachment_id.as_str();
                let entry = LegacyPathLedgerEntry {
                    legacy_path_binding_id: LegacyPathBindingId::parse(format!(
                        "lpb_{binding_index:032x}"
                    ))
                    .unwrap(),
                    historical_path: format!("/host/checkouts/relocated{binding_index}"),
                    source_store: ATTACHMENT_RELOCATION_SOURCE.to_string(),
                    source_row_id: attachment_id.to_string(),
                    member_row_count: 1,
                    member_commitment_sha256: singleton_selector_members(attachment_id)
                        .commitment_sha256,
                    inventory_epoch: 1,
                    status: LegacyPathBindingStatus::Mapped {
                        project_id: project.clone(),
                        relationship: LegacyPathRelationship::Root,
                    },
                };
                attachments
                    .legacy_path_bindings
                    .insert(entry.legacy_path_binding_id.clone(), entry);
                Ok(())
            })
            .unwrap();
    }

    /// The completion journal this fixture's apply published, as raw JSON.
    fn journal_path(&self) -> PathBuf {
        self.layout
            .projects_path()
            .parent()
            .unwrap()
            .join("backfill-completion.json")
    }

    fn journal_json(&self) -> serde_json::Value {
        serde_json::from_slice(&fs::read(self.journal_path()).unwrap()).unwrap()
    }

    fn write_journal_json(&self, journal: &serde_json::Value) {
        fs::write(self.journal_path(), serde_json::to_vec(journal).unwrap()).unwrap();
    }

    fn stamped(&self, which: Owner) -> Option<String> {
        let (path, field) = match which {
            Owner::Knowledge => (&self.knowledge_path, "entries"),
            Owner::Thread => (&self.thread_path, "threads"),
            Owner::Note => (&self.note_path, "notes"),
        };
        let document: serde_json::Value = serde_json::from_slice(&fs::read(path).unwrap()).unwrap();
        document[field][0]["project_id"]
            .as_str()
            .map(str::to_string)
    }
}

/// A stamper that delegates N successful stamps and then fails, leaving the
/// owner stores PARTIALLY stamped exactly as a crash mid-pass would.
///
/// `coverage` DELEGATES rather than answering for itself. If it did not, the
/// fault would change which owners preflight considers writable and the test
/// would exercise a coverage refusal instead of a torn stamping pass.
struct TornStamper {
    inner: Arc<dyn LegacyRowStamperV1>,
    remaining: AtomicUsize,
}

impl LegacyRowStamperV1 for TornStamper {
    fn coverage(&self, store_kind: LegacyPathStoreKindV1) -> LegacyRowStampCoverageV1 {
        self.inner.coverage(store_kind)
    }

    fn stamp(
        &self,
        store_kind: LegacyPathStoreKindV1,
        source_row_id: &str,
        expected_members: &LegacySelectorMembersV1,
        project_id: &ProjectId,
    ) -> Result<LegacyRowStampOutcomeV1, ProjectCatalogMigrationError> {
        if self.remaining.fetch_sub(1, Ordering::SeqCst) == 0 {
            return Err(ProjectCatalogMigrationError::no_mutation(
                "error.project_catalog_inventory_stale_post_image",
                "injected torn-stamper fault",
            ));
        }
        self.inner
            .stamp(store_kind, source_row_id, expected_members, project_id)
    }
}

/// The happy path: preflight, apply, verify against a real migrated root with
/// the production stamper, across two different owner stores.
#[test]
fn the_production_stamper_backfills_two_stores_end_to_end() {
    let fixture = Fixture::new();

    assert_eq!(
        fixture.preflight(fixture.production_stamper()).unwrap(),
        DurableBackfillStatusV1::Clean,
        "a fixture whose ledger matches its owner stores must preflight clean"
    );
    assert_eq!(
        fixture.stamped(Owner::Knowledge),
        None,
        "preflight writes no project state"
    );

    assert_eq!(
        fixture.apply(fixture.production_stamper()).unwrap(),
        DurableBackfillApplyOutcomeV1::Applied
    );
    let stamped = fixture
        .stamped(Owner::Knowledge)
        .expect("knowledge stamped");
    assert_eq!(
        fixture.stamped(Owner::Thread).as_deref(),
        Some(stamped.as_str())
    );

    let verify = fixture.verify().unwrap();
    assert_eq!(verify.journal_stamp_total, 2);
    assert_eq!(verify.observed_mappable_total, 2);
}

/// R2-2. A host that has ever RELOCATED an attachment can still run its
/// backfill, end to end.
///
/// Production mints a mapped ledger binding whose `source_store` is
/// `attachment-relocation` whenever a checkout moves, so path-only rows keep
/// resolving at the old path. It is not an owner row and never can be: no owner
/// holds it, so nothing can stamp it and nothing can be asked about it. Treating
/// every non-owner token as a defect meant the mere existence of one refused
/// planning and refused verification, on a record that carries no obligation.
///
/// It rides through instead: retained, hashed into the predecessor the plan is
/// bound to, and counted in NEITHER the mappable population nor the quarantine.
#[test]
fn a_relocation_binding_rides_through_preflight_apply_and_verify() {
    let fixture = Fixture::new();
    fixture.add_relocation_binding(9);

    assert_eq!(
        fixture.preflight(fixture.production_stamper()).unwrap(),
        DurableBackfillStatusV1::Clean
    );
    fixture.apply(fixture.production_stamper()).unwrap();

    let verify = fixture.verify().unwrap();
    // EXACTLY the counts of the fixture without the relocation binding: the two
    // mapped owner rows. A relocation counted as mappable would have demanded a
    // stamp no owner could perform; counted as unmappable it would have grown a
    // quarantine that no disposition could ever clear.
    assert_eq!(verify.journal_stamp_total, 2);
    assert_eq!(verify.observed_mappable_total, 2);
    assert_eq!(
        fixture.stamped(Owner::Knowledge).as_deref(),
        Some(fixture.project.as_str())
    );

    // And it is still in the ledger afterwards, exactly as read: the backfill
    // excludes it from the owner population without editing it away.
    let store = ProjectCatalogStore::open_existing(fixture.layout.projects_path()).unwrap();
    let state = store.snapshot().unwrap();
    let binding = state
        .attachments()
        .legacy_path_bindings
        .get(&LegacyPathBindingId::parse(format!("lpb_{:032x}", 9)).unwrap())
        .expect("the relocation binding survives the backfill unchanged");
    assert_eq!(binding.source_store, ATTACHMENT_RELOCATION_SOURCE);
}

/// A relocation minted BETWEEN preflight and apply is refused as stale, and
/// that is correct.
///
/// The refusal is not about relocations. The backfill's whole safety story is
/// that a reviewed plan may only be applied against the exact predecessor pair
/// it was computed from, and the four-hash identity gate enforces it over the
/// WHOLE attachment snapshot. Excluding relocation bindings from that hash to
/// make this case pass would mean a concurrent relocation could land unnoticed
/// between review and mutation, which is the opposite of the property the gate
/// exists for. The remedy is the ordinary one: re-run preflight.
///
/// After apply, the same binding is harmless: verify reads the durable ledger
/// rather than an artifact, and a record carrying no obligation changes no
/// count it compares.
#[test]
fn a_relocation_minted_after_preflight_refuses_the_apply_and_not_the_verify() {
    let fixture = Fixture::new();
    assert_eq!(
        fixture.preflight(fixture.production_stamper()).unwrap(),
        DurableBackfillStatusV1::Clean
    );

    fixture.add_relocation_binding(9);
    let stale = fixture
        .apply(fixture.production_stamper())
        .expect_err("the predecessor moved under the reviewed artifacts");
    // The report-staleness gate runs before the four-hash identity gate and
    // catches the moved predecessor first; both are staleness-class
    // refusals whose remedy is a fresh preflight. What this pin protects is
    // that the apply REFUSES, not which of the two ordered gates names it.
    assert_eq!(stale.code, "error.project_catalog_inventory_stale_report");

    // The remedy is an EXPLICIT fresh preflight: the stale artifacts are the
    // operator's to discard, never silently clobbered by a re-run, so a
    // preflight reusing them refuses too.
    let reused = fixture
        .preflight(fixture.production_stamper())
        .expect_err("stale artifacts must not be silently replaced");
    assert_eq!(
        reused.code,
        "error.project_catalog_inventory_stale_resolution"
    );
    fs::remove_file(fixture.report_path()).unwrap();
    fs::remove_file(fixture.resolution_path()).unwrap();
    assert_eq!(
        fixture.preflight(fixture.production_stamper()).unwrap(),
        DurableBackfillStatusV1::Clean
    );
    fixture.apply(fixture.production_stamper()).unwrap();

    // A relocation minted AFTER the apply is not a verify problem: it carries
    // no obligation, so it moves none of the counts verify compares.
    fixture.add_relocation_binding(10);
    let verify = fixture.verify().unwrap();
    assert_eq!(verify.journal_stamp_total, 2);
    assert_eq!(verify.observed_mappable_total, 2);
}

/// THE SECTION 3.3 RECOVERY SEQUENCING.
///
/// Idempotence is proven elsewhere and assumed here. What this proves is the
/// ORDER the plan mandates: after a torn stamping pass, re-applying the SAME
/// reviewed artifacts must REFUSE, and only a fresh preflight against the moved
/// predecessor may be applied. Without the refusal an operator would retry a
/// stale plan; without the completion the recovery would be a dead end.
#[test]
fn a_torn_stamping_pass_refuses_a_stale_reapply_then_completes_after_fresh_preflight() {
    let fixture = Fixture::new();
    // First preflight writes the canonical empty resolution; convert the
    // quarantined binding and re-preflight so the reviewed artifacts describe a
    // backfill that MUTATES THE PAIR. Without a conversion, stamping touches
    // only owner stores, the catalog epoch and the four-hash identity are
    // unchanged, and a re-apply is simply idempotent - there is no stale
    // predecessor to refuse against, so the sequencing this test exists for
    // would never engage.
    assert_eq!(
        fixture.preflight(fixture.production_stamper()).unwrap(),
        DurableBackfillStatusV1::Clean
    );
    fixture.convert_the_quarantined_binding();
    assert_eq!(
        fixture.preflight(fixture.production_stamper()).unwrap(),
        DurableBackfillStatusV1::Clean
    );

    // Tear the pass after exactly one successful stamp.
    let torn = Arc::new(TornStamper {
        inner: fixture.production_stamper(),
        remaining: AtomicUsize::new(1),
    });
    let error = fixture.apply(torn).unwrap_err();
    assert_eq!(
        error.code,
        "error.project_catalog_inventory_stale_post_image"
    );

    // A strict subset landed: the pass is genuinely torn, not all-or-nothing.
    let landed = [Owner::Knowledge, Owner::Thread, Owner::Note]
        .into_iter()
        .filter(|owner| fixture.stamped(*owner).is_some())
        .count();
    assert!(
        (1..3).contains(&landed),
        "expected a partially stamped set, got {landed}"
    );

    // STEP 1 of the recovery contract: the same reviewed artifacts must not be
    // replayable against the moved predecessor.
    let stale = fixture
        .apply(fixture.production_stamper())
        .expect_err("the pair moved, so the reviewed artifacts are stale");
    assert!(
        !stale.code.is_empty(),
        "a stale re-apply must refuse for a reason the operator can act on"
    );

    // The reviewed resolution is stale too, and says so rather than being
    // silently reused: it names the pre-conversion predecessor inventory.
    let stale_resolution = fixture
        .preflight(fixture.production_stamper())
        .expect_err("the reviewed resolution names the old predecessor");
    assert_eq!(
        stale_resolution.code,
        "error.project_catalog_inventory_stale_resolution"
    );

    // STEP 2: a fresh preflight against the CURRENT predecessor, then re-apply.
    // Recovery is "fresh preflight, REVIEW, re-apply", so the operator's stale
    // review is discarded and preflight writes the canonical empty resolution
    // again. That is correct rather than a shortcut: the conversion already
    // landed in the committed pair, so it is visible in the new predecessor and
    // must not be re-applied.
    fs::remove_file(fixture.resolution_path()).unwrap();
    assert_eq!(
        fixture.preflight(fixture.production_stamper()).unwrap(),
        DurableBackfillStatusV1::Clean
    );
    assert_eq!(
        fixture.apply(fixture.production_stamper()).unwrap(),
        DurableBackfillApplyOutcomeV1::Applied
    );

    // Completed without double-stamping.
    let stamped = fixture
        .stamped(Owner::Knowledge)
        .expect("knowledge stamped");
    assert_eq!(
        fixture.stamped(Owner::Thread).as_deref(),
        Some(stamped.as_str())
    );
    assert_eq!(
        fixture.stamped(Owner::Note).as_deref(),
        Some(stamped.as_str()),
        "the converted row is stamped by the recovery pass too"
    );
    for path in [
        &fixture.knowledge_path,
        &fixture.thread_path,
        &fixture.note_path,
    ] {
        let text = fs::read_to_string(path).unwrap();
        assert_eq!(
            text.matches(stamped.as_str()).count(),
            1,
            "a re-apply must not append or duplicate a stamp: {text}"
        );
    }
}

/// A stamper that reports every row STAMPED and writes nothing.
///
/// This is the shape F2 exists for: apply succeeds, the completion journal
/// records a full stamp set, and not one durable owner row has moved. A verify
/// that compares the journal against the catalog ledger cannot tell the
/// difference, because both records agree with each other and neither is the
/// owner store.
struct NoOpStamper {
    inner: Arc<dyn LegacyRowStamperV1>,
}

impl LegacyRowStamperV1 for NoOpStamper {
    fn coverage(&self, store_kind: LegacyPathStoreKindV1) -> LegacyRowStampCoverageV1 {
        self.inner.coverage(store_kind)
    }

    fn stamp(
        &self,
        _store_kind: LegacyPathStoreKindV1,
        _source_row_id: &str,
        _expected_members: &LegacySelectorMembersV1,
        _project_id: &ProjectId,
    ) -> Result<LegacyRowStampOutcomeV1, ProjectCatalogMigrationError> {
        Ok(LegacyRowStampOutcomeV1::Stamped)
    }
}

/// A reader that claims one owner has no durable read-back.
struct UncoveredReader {
    inner: Arc<dyn LegacyRowOwnerReaderV1>,
    uncovered: LegacyPathStoreKindV1,
}

impl LegacyRowOwnerReaderV1 for UncoveredReader {
    fn coverage(&self, store_kind: LegacyPathStoreKindV1) -> LegacyRowStampCoverageV1 {
        if store_kind == self.uncovered {
            LegacyRowStampCoverageV1::NotImplemented
        } else {
            self.inner.coverage(store_kind)
        }
    }

    fn observe(
        &self,
        store_kind: LegacyPathStoreKindV1,
        rows: &OwnerRowRequestV1,
    ) -> Result<BTreeMap<String, LegacyRowObservationV1>, ProjectCatalogMigrationError> {
        self.inner.observe(store_kind, rows)
    }
}

/// Rewrite one owner store's first row through `edit`.
fn edit_owner_row(path: &Path, array_field: &str, edit: impl FnOnce(&mut serde_json::Value)) {
    let mut document: serde_json::Value = serde_json::from_slice(&fs::read(path).unwrap()).unwrap();
    edit(&mut document[array_field][0]);
    fs::write(path, serde_json::to_vec(&document).unwrap()).unwrap();
}

/// A NO-OP stamper produces a backfill that applies and then FAILS to verify.
///
/// Verify previously compared the journal's stamp counts against the catalog
/// ledger's mappable counts and never opened an owner store, so this exact
/// apply verified perfectly while every owner row was still unstamped.
#[test]
fn a_no_op_stamper_applies_but_cannot_verify() {
    let fixture = Fixture::new();
    assert_eq!(
        fixture.preflight(fixture.production_stamper()).unwrap(),
        DurableBackfillStatusV1::Clean
    );
    assert_eq!(
        fixture
            .apply(Arc::new(NoOpStamper {
                inner: fixture.production_stamper()
            }))
            .unwrap(),
        DurableBackfillApplyOutcomeV1::Applied,
        "a stamper that reports success is believed by apply, which is why \
         verify has to read the owners"
    );
    assert_eq!(
        fixture.stamped(Owner::Knowledge),
        None,
        "the fixture must genuinely have unstamped rows for this to mean anything"
    );

    let error = fixture
        .verify()
        .expect_err("an unstamped owner row cannot verify");
    assert_eq!(
        error.code,
        "error.project_catalog_inventory_stale_post_image"
    );
}

/// The durable rows must EXIST and must carry the EXACT project id the ledger
/// binds them to. Both directions of the counterexample, plus the owner whose
/// read-back is missing entirely.
#[test]
fn verify_fails_on_missing_conflicting_or_unverifiable_owner_rows() {
    let fixture = Fixture::new();
    assert_eq!(
        fixture.preflight(fixture.production_stamper()).unwrap(),
        DurableBackfillStatusV1::Clean
    );
    assert_eq!(
        fixture.apply(fixture.production_stamper()).unwrap(),
        DurableBackfillApplyOutcomeV1::Applied
    );
    fixture.verify().expect("the applied backfill verifies");

    // CONFLICTING: the row is stamped, but with another project.
    let stamped = fixture.stamped(Owner::Knowledge).unwrap();
    let mut elsewhere = stamped.clone();
    elsewhere.pop();
    elsewhere.push(if stamped.ends_with('a') { 'b' } else { 'a' });
    assert_ne!(elsewhere, stamped);
    edit_owner_row(&fixture.knowledge_path, "entries", |row| {
        row["project_id"] = serde_json::Value::String(elsewhere.clone());
    });
    assert_eq!(
        fixture
            .verify()
            .expect_err("a row bound to another project cannot verify")
            .code,
        "error.project_catalog_inventory_stale_post_image"
    );

    // UNSTAMPED: the stamp was reverted after apply.
    edit_owner_row(&fixture.knowledge_path, "entries", |row| {
        row.as_object_mut().unwrap().remove("project_id");
    });
    assert_eq!(
        fixture
            .verify()
            .expect_err("an unstamped row cannot verify")
            .code,
        "error.project_catalog_inventory_stale_post_image"
    );

    // MISSING: the row the mapped binding names is gone.
    let mut document: serde_json::Value =
        serde_json::from_slice(&fs::read(&fixture.knowledge_path).unwrap()).unwrap();
    document["entries"] = serde_json::json!([]);
    fs::write(
        &fixture.knowledge_path,
        serde_json::to_vec(&document).unwrap(),
    )
    .unwrap();
    assert_eq!(
        fixture
            .verify()
            .expect_err("an absent row cannot verify")
            .code,
        "error.project_catalog_inventory_stale_post_image"
    );

    // UNVERIFIABLE: an owner carrying mapped bindings whose read-back does not
    // exist is refused by name, never passed over in silence.
    let error = ProjectCatalogDurableBackfillFacadeV1::verify(DurableBackfillVerifyRequestV1 {
        layout: fixture.layout.clone(),
        target_selection: ProjectCatalogTargetSelectionV1::Rehearsal,
        owner_reader: Arc::new(UncoveredReader {
            inner: fixture.production_owner_reader(),
            uncovered: LegacyPathStoreKindV1::Knowledge,
        }),
    })
    .expect_err("an owner with no read-back cannot be verified");
    assert!(
        error.message.contains("knowledge"),
        "the refusal must name the owner: {}",
        error.message
    );
}

/// The journal's COMPLETE per-store classification is checked, not just its
/// stamp total: a journal that misreports its mappable count for one store
/// fails even though the total still adds up against the ledger.
#[test]
fn verify_fails_when_the_journal_misreports_one_stores_classification() {
    let fixture = Fixture::new();
    assert_eq!(
        fixture.preflight(fixture.production_stamper()).unwrap(),
        DurableBackfillStatusV1::Clean
    );
    assert_eq!(
        fixture.apply(fixture.production_stamper()).unwrap(),
        DurableBackfillApplyOutcomeV1::Applied
    );
    fixture.verify().expect("the applied backfill verifies");

    let journal_path = fixture
        .layout
        .projects_path()
        .parent()
        .unwrap()
        .join("backfill-completion.json");
    let mut journal: serde_json::Value =
        serde_json::from_slice(&fs::read(&journal_path).unwrap()).unwrap();
    // Claim one more mappable row than the ledger holds, while leaving the
    // stamped count that the old check compared exactly as it was.
    let counts = &mut journal["stamp_counts"]["knowledge"];
    counts["mappable"] = serde_json::json!(counts["mappable"].as_u64().unwrap() + 1);
    fs::write(&journal_path, serde_json::to_vec(&journal).unwrap()).unwrap();

    assert_eq!(
        fixture
            .verify()
            .expect_err("a journal that misreports a store cannot verify")
            .code,
        "error.project_catalog_inventory_stale_post_image"
    );
}

/// A store the journal OMITS ENTIRELY is compared, not skipped.
///
/// The two per-store comparisons used to iterate one side each: the stamp check
/// walked the ledger's MAPPED stores and the classification check walked the
/// JOURNAL's own keys. A store whose effective rows are only unscoped is in
/// neither list once the journal drops it, so the omission fell between the two
/// checks and verified clean while the journal silently disclaimed a whole
/// store's classification.
#[test]
fn verify_fails_when_the_journal_omits_an_unscoped_only_store() {
    let fixture = Fixture::new();
    fixture.seed_the_unscoped_only_store();
    assert_eq!(
        fixture.preflight(fixture.production_stamper()).unwrap(),
        DurableBackfillStatusV1::Clean
    );
    assert_eq!(
        fixture.apply(fixture.production_stamper()).unwrap(),
        DurableBackfillApplyOutcomeV1::Applied
    );
    fixture.verify().expect("the applied backfill verifies");

    let mut journal = fixture.journal_json();
    // The premise: the store really is in the journal, really is unscoped-only,
    // and really has no stamped rows to give the mappable check something to
    // catch instead.
    let pin = journal["stamp_counts"]["pin"].clone();
    assert_eq!(pin["unscoped"].as_u64(), Some(1));
    assert_eq!(pin["mappable"].as_u64(), Some(0));
    assert_eq!(pin["stamped"].as_u64(), Some(0));

    journal["stamp_counts"]
        .as_object_mut()
        .unwrap()
        .remove("pin");
    fixture.write_journal_json(&journal);

    assert_eq!(
        fixture
            .verify()
            .expect_err("a journal that omits a whole store cannot verify")
            .code,
        "error.project_catalog_inventory_stale_post_image"
    );
}

/// Conversions cannot be REASSIGNED between stores.
///
/// The per-store conversion count used to be bounded rather than exact - a
/// store could claim any number of conversions up to the rows able to carry one
/// - so moving a conversion from the owner that really converted to another
/// owner preserved the global total, the epoch transition, and every other
/// count, and verified clean. The journal now names the bindings it converted,
/// and the store each one counts against comes from the LEDGER.
#[test]
fn verify_fails_when_the_journal_moves_a_conversion_to_another_store() {
    let fixture = Fixture::new();
    assert_eq!(
        fixture.preflight(fixture.production_stamper()).unwrap(),
        DurableBackfillStatusV1::Clean
    );
    fixture.convert_the_quarantined_binding();
    assert_eq!(
        fixture.preflight(fixture.production_stamper()).unwrap(),
        DurableBackfillStatusV1::Clean
    );
    assert_eq!(
        fixture.apply(fixture.production_stamper()).unwrap(),
        DurableBackfillApplyOutcomeV1::Applied
    );
    fixture.verify().expect("the applied backfill verifies");

    let mut journal = fixture.journal_json();
    // The premise: the note store converted exactly one row, and the knowledge
    // store has a mappable row that could plausibly have carried one.
    assert_eq!(
        journal["stamp_counts"]["note"]["converted"].as_u64(),
        Some(1)
    );
    assert_eq!(
        journal["stamp_counts"]["knowledge"]["converted"].as_u64(),
        Some(0)
    );
    assert_eq!(
        journal["stamp_counts"]["knowledge"]["mappable"].as_u64(),
        Some(1)
    );
    let post_image = journal["post_image_catalog_epoch"].clone();
    let predecessor = journal["predecessor_catalog_epoch"].clone();

    journal["stamp_counts"]["note"]["converted"] = serde_json::json!(0);
    journal["stamp_counts"]["knowledge"]["converted"] = serde_json::json!(1);
    fixture.write_journal_json(&journal);

    // Nothing else moved: same global total, same epoch pair, so every check
    // that survived the old bound still passes.
    let rewritten = fixture.journal_json();
    let total: u64 = rewritten["stamp_counts"]
        .as_object()
        .unwrap()
        .values()
        .map(|counts| counts["converted"].as_u64().unwrap())
        .sum();
    assert_eq!(total, 1);
    assert_eq!(rewritten["post_image_catalog_epoch"], post_image);
    assert_eq!(rewritten["predecessor_catalog_epoch"], predecessor);

    assert_eq!(
        fixture
            .verify()
            .expect_err("a conversion reassigned to another store cannot verify")
            .code,
        "error.project_catalog_inventory_stale_post_image"
    );
}

/// The accepted-publication pointer root under a fixture's layout.
///
/// Rebuilt from the projects path the same way `AcceptedPublicationStorePaths`
/// derives it, because that helper is crate-private to `bbox-indexing`. The unit
/// test `the_generation_probe_uses_the_stores_own_path_shape` pins the shape
/// against the store's own, so this reconstruction cannot drift silently.
fn publication_pointers_root(fixture: &Fixture) -> PathBuf {
    fixture
        .layout
        .projects_path()
        .parent()
        .expect("the projects path has a parent")
        .join("accepted-publications")
        .join("pointers")
}

/// Move the target's accepted-publication state exactly as a publisher
/// Establish (or the loss of a seeded publication) between two commands would,
/// driven by the disposition set the REPORT recorded so the mutation is always
/// the one that flips a verdict.
fn move_the_accepted_publication_state(fixture: &Fixture) {
    let report: serde_json::Value =
        serde_json::from_slice(&fs::read(fixture.report_path()).unwrap()).unwrap();
    let rows = report["publisher_verification"].as_array().unwrap();
    assert!(
        !rows.is_empty(),
        "the fixture must carry publisher dispositions for this proof to mean anything"
    );
    let pointers = publication_pointers_root(fixture);
    let project_id = rows[0]["project_id"].as_str().unwrap();
    let pointer = pointers.join(format!("{project_id}.json"));
    match rows[0]["kind"].as_str().unwrap() {
        // D-040 says this project must have no pointer until an explicit
        // Establish. Plant one: the disposition now fails.
        "no_published_content_acknowledged" => {
            fs::create_dir_all(&pointers).unwrap();
            fs::write(&pointer, b"{}").unwrap();
        }
        // The mirror image: the seeded pointer the migration installed goes
        // away.
        "seed_g1" => fs::remove_file(&pointer).unwrap(),
        other => panic!("unexpected publisher disposition kind {other}"),
    }
}

/// THE PREFLIGHT/APPLY RACE. Accepted-publication state is outside the
/// four-hash inventory, so a publication appearing or vanishing between the two
/// commands moves no hash the report carries. Apply must re-prove the complete
/// disposition set against the target rather than trusting preflight's verdict.
#[test]
fn a_publication_that_moves_after_preflight_refuses_the_apply() {
    let fixture = Fixture::new();
    assert_eq!(
        fixture.preflight(fixture.production_stamper()).unwrap(),
        DurableBackfillStatusV1::Clean
    );

    move_the_accepted_publication_state(&fixture);

    let error = fixture
        .apply(fixture.production_stamper())
        .expect_err("a moved publication state must refuse the reviewed apply");
    assert_eq!(
        error.code,
        "error.project_catalog_inventory_stale_post_image"
    );
    assert_eq!(
        fixture.stamped(Owner::Knowledge),
        None,
        "the pre-transaction boundary refuses before anything is stamped"
    );
}

/// The same property on the other side of the cut: verify re-proves the
/// dispositions and compares them against the stamp the journal recorded, so a
/// publication that moved AFTER a clean apply is reported rather than passing on
/// the strength of the journal's own numbers.
#[test]
fn a_publication_that_moves_after_apply_refuses_the_verify() {
    let fixture = Fixture::new();
    assert_eq!(
        fixture.preflight(fixture.production_stamper()).unwrap(),
        DurableBackfillStatusV1::Clean
    );
    assert_eq!(
        fixture.apply(fixture.production_stamper()).unwrap(),
        DurableBackfillApplyOutcomeV1::Applied
    );
    fixture
        .verify()
        .expect("a freshly applied backfill verifies");

    move_the_accepted_publication_state(&fixture);

    let error = fixture
        .verify()
        .expect_err("a moved publication state must refuse the verify");
    assert_eq!(
        error.code,
        "error.project_catalog_inventory_stale_post_image"
    );
}

/// D-026: an existing reviewed resolution is HONOURED, never rewritten, and the
/// hash the report carries is taken over the bytes the operator actually
/// reviewed.
///
/// The previous revision read the resolution, decoded it, and wrote a
/// RE-ENCODING of it back over the operator's file at the end of every
/// preflight. Two things were wrong with that. The reviewed artifact is the
/// operator's evidence and preflight has no business editing it; and the
/// recorded `resolution_artifact_hash` was a digest of bytes that had never
/// existed on disk until preflight put them there, so any resolution whose
/// formatting differed from serde's would have failed apply's identity check
/// had preflight not silently normalised it first.
#[test]
fn a_reviewed_resolution_is_preserved_byte_for_byte_and_hashed_as_reviewed() {
    let fixture = Fixture::new();
    assert_eq!(
        fixture.preflight(fixture.production_stamper()).unwrap(),
        DurableBackfillStatusV1::Clean
    );

    // The operator reviews the canonical empty resolution and saves it with
    // their own formatting: same meaning, different bytes.
    let canonical = fs::read(fixture.resolution_path()).unwrap();
    let reviewed = serde_json::to_vec_pretty(
        &serde_json::from_slice::<serde_json::Value>(&canonical).unwrap(),
    )
    .unwrap();
    assert_ne!(reviewed, canonical, "the fixture must actually differ");
    fs::write(fixture.resolution_path(), &reviewed).unwrap();

    assert_eq!(
        fixture.preflight(fixture.production_stamper()).unwrap(),
        DurableBackfillStatusV1::Clean
    );
    assert_eq!(
        fs::read(fixture.resolution_path()).unwrap(),
        reviewed,
        "preflight rewrote the operator's reviewed resolution"
    );
    // The apply identity check is over the REVIEWED bytes, so it passes without
    // preflight having normalised the file first.
    assert_eq!(
        fixture.apply(fixture.production_stamper()).unwrap(),
        DurableBackfillApplyOutcomeV1::Applied
    );
}

/// The report is replaced through the CONFINED helper, so a symlink planted at
/// the report path refuses instead of being written through.
///
/// `std::fs::write` follows the link: it would have published the reviewed
/// report to wherever the link pointed, leaving the explicit report path
/// holding a link to an artifact outside the review directory.
#[cfg(unix)]
#[test]
fn a_symlinked_report_path_refuses_instead_of_being_written_through() {
    let fixture = Fixture::new();
    let target = fixture.artifacts.join("not-the-report.json");
    fs::write(&target, b"untouched").unwrap();
    std::os::unix::fs::symlink(&target, fixture.report_path()).unwrap();

    let error =
        ProjectCatalogDurableBackfillFacadeV1::preflight(DurableBackfillPreflightRequestV1 {
            layout: fixture.layout.clone(),
            target_selection: ProjectCatalogTargetSelectionV1::Rehearsal,
            report_path: fixture.report_path(),
            resolution_path: fixture.resolution_path(),
            stamper: fixture.production_stamper(),
            generated_at: "2026-08-05T00:00:00Z".to_string(),
        })
        .expect_err("a symlinked report path is not a publication target");

    assert_eq!(error.code, "error.project_catalog_migration_artifact_io");
    assert_eq!(
        fs::read(&target).unwrap(),
        b"untouched",
        "the report must never be written through a link"
    );
}

/// An `AlreadyApplied` answer requires the COMPLETE artifact identity, not the
/// report's byte hash alone.
///
/// The journal names all four hashes and the predecessor it was applied
/// against. Matching only the report meant a second apply carrying the same
/// report but a DIFFERENT resolution was blessed as already-applied: the
/// operator was told their reviewed pair had run when the pair that ran named a
/// different resolution.
#[test]
fn already_applied_requires_the_complete_artifact_identity() {
    let fixture = Fixture::new();
    assert_eq!(
        fixture.preflight(fixture.production_stamper()).unwrap(),
        DurableBackfillStatusV1::Clean
    );
    assert_eq!(
        fixture.apply(fixture.production_stamper()).unwrap(),
        DurableBackfillApplyOutcomeV1::Applied
    );
    // The exact same invocation is genuinely already applied.
    assert_eq!(
        fixture.apply(fixture.production_stamper()).unwrap(),
        DurableBackfillApplyOutcomeV1::AlreadyApplied
    );

    // Same report, resolution bytes the applied journal never saw.
    let applied = fs::read(fixture.resolution_path()).unwrap();
    let other =
        serde_json::to_vec_pretty(&serde_json::from_slice::<serde_json::Value>(&applied).unwrap())
            .unwrap();
    assert_ne!(other, applied);
    fs::write(fixture.resolution_path(), &other).unwrap();

    let error = fixture
        .apply(fixture.production_stamper())
        .expect_err("a report whose resolution is not the applied one is not already applied");
    assert_eq!(
        error.code,
        "error.project_catalog_migration_artifact_identity"
    );
}

/// The torn stamper's `coverage` delegates, which is what keeps the fault a
/// STAMPING fault rather than silently converting this into a coverage test.
#[test]
fn the_torn_stamper_delegates_coverage() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let production: Arc<dyn LegacyRowStamperV1> = Arc::new(
        ProjectCatalogOwnerRowStamperV1::new(
            ProjectCatalogStamperPathsV1 {
                knowledge_store_path: root.join("knowledge.json"),
                gap_store_path: root.join("gaps.json"),
                thread_store_path: root.join("threads.json"),
                note_store_path: root.join("notes.json"),
                pin_store_path: root.join("pins.json"),
                roadmap_store_path: root.join("roadmap.json"),
                packet_root: root.join("packets"),
                proposal_root: root.join("proposals"),
                slack_store_root: root.join("slack"),
                whiteboard_root: root.join("whiteboards"),
                artifact_root: root.join("artifacts"),
                transcript_edge_root: root.join("edges"),
                task_store_path: root.join("tasks.json"),
            },
            Default::default(),
        )
        .unwrap(),
    );
    let torn = TornStamper {
        inner: production.clone(),
        remaining: AtomicUsize::new(1),
    };

    for kind in [
        LegacyPathStoreKindV1::Knowledge,
        LegacyPathStoreKindV1::Thread,
        LegacyPathStoreKindV1::Task,
        LegacyPathStoreKindV1::Provenance,
        LegacyPathStoreKindV1::TranscriptEdge,
    ] {
        assert_eq!(
            torn.coverage(kind),
            production.coverage(kind),
            "the decorator must not invent its own coverage verdict"
        );
    }
}

// ---------------------------------------------------------------------------
// The path-free rebuild chain (P6-C)
// ---------------------------------------------------------------------------
//
// Everything below stages history AFTER the migration, deliberately.
//
// The migration REFUSES to capture an index whose schema marker is not the
// running `INDEX_SCHEMA_VERSION` (`capture_index` marks it Corrupt, which
// surfaces as four `immutable_lane_corrupt` inventory refusals with no
// resolution kind). The destructive replacement, in turn, only runs when that
// marker DOES mismatch. So the marker is stamped after the migration has
// captured, which is also what puts the proof in `Drift`: the recorded
// fingerprint folds a Present owner state and the recomputed one folds a
// Corrupt state. Drift is the mode a real migrate-then-rebuild chain reaches on
// this code, and D-036 refuses it for a CUT-authorizing offline apply, which is
// exactly what the startup gate below is asserted to refuse too.
//
// Staging after the migration is what makes four-bucket coverage reachable at
// all: in Drift mode "a namespace ABSENT from the asset carries no asset
// constraint", so post-migration namespaces classify normally against the
// catalog instead of having to satisfy the migration's own namespace rules.

/// Primary namespace of a repo-history record: the manifest's OWNED bucket.
const REBUILD_OWNED_NAMESPACE: &str = "rebuild-owned-namespace";
/// A COMPATIBILITY namespace of that same record. It has no catalog `Ready`
/// owner of its own and is reachable only through the manifest's
/// `compatibility_generation_ids` bucket (D-037) - the bucket a verifier that
/// walked `Ready` requirements would silently skip.
const REBUILD_COMPAT_NAMESPACE: &str = "rebuild-compat-namespace";
/// Named by an ambiguous-namespace record: an `rhq_`-id'd quarantine
/// generation in the AMBIGUOUS bucket.
const REBUILD_AMBIGUOUS_NAMESPACE: &str = "rebuild-ambiguous-namespace";
/// Claimed by nothing: the UNCLAIMED bucket, whose only durable owner is
/// likewise the manifest.
const REBUILD_UNCLAIMED_NAMESPACE: &str = "rebuild-orphan-namespace";

/// The marker the staged index is left at.
///
/// It must differ from the running `INDEX_SCHEMA_VERSION`: that difference is
/// what `schema_was_reset()` observes, and without it the shared driver returns
/// `NotRequired` and no destructive pass runs at all.
const REBUILD_OUTGOING_SCHEMA: &str = "outgoing-test-schema";

/// Host path baked into the staged commit documents, so a consumer can assert
/// the re-emitted documents do not reproduce it.
const REBUILD_OUTGOING_HOST_PATH: &str = "/host-checkouts/outgoing-fixture";

/// The staged population, one entry per manifest bucket. Shared by both
/// staging points so the two chains cover the identical four dispositions.
const REBUILD_NAMESPACE_STAGING: &[(&str, usize)] = &[
    (REBUILD_OWNED_NAMESPACE, 3),
    (REBUILD_COMPAT_NAMESPACE, 2),
    (REBUILD_AMBIGUOUS_NAMESPACE, 2),
    (REBUILD_UNCLAIMED_NAMESPACE, 1),
];

/// A migrated root carrying rebuildable legacy history in all four dispositions.
struct RebuildFixture {
    fixture: Fixture,
}

impl RebuildFixture {
    /// Build the root, stage the history, bind the records that put each
    /// namespace in a DIFFERENT manifest bucket, run the real backfill, and
    /// leave the index at the outgoing marker.
    ///
    /// ORDER IS LOAD-BEARING at three points, each of which refuses if moved:
    ///
    /// 1. the records are bound BEFORE the backfill, because the backfill
    ///    journal binds the epoch it observed and a `transact` afterwards makes
    ///    the rebuild's predecessor stale;
    /// 2. the marker is stamped AFTER the backfill, because a backfill whose
    ///    capture reached a marker-mismatched index would refuse it as corrupt;
    /// 3. the marker is stamped AFTER every index write, because reopening a
    ///    marker-mismatched index triggers the very replacement under test.
    fn new() -> Self {
        Self::build(true)
    }

    /// The same root left at the RUNNING `INDEX_SCHEMA_VERSION` (Q-F).
    ///
    /// This is the shape a real Phase 6 cut is in, and the shape that made the
    /// contradiction visible. The migration refuses to capture a
    /// marker-mismatched index as `Corrupt`, so an index left at the running
    /// version is the only one whose recomputed source fingerprint can fold the
    /// same owner state the recorded one did - which is to say, the only one
    /// that can prove `Equality`. Under the pre-Q-F mismatch-only trigger that
    /// same index could never be replaced, so "Equality AND Completed" had no
    /// reachable state at all. The operator cause is what makes it reachable.
    fn at_current_schema() -> Self {
        Self::build(false)
    }

    fn build(stamp_outgoing_marker: bool) -> Self {
        // The marker decides WHERE the history is staged, because the two
        // triggers need opposite things from the source fingerprint. The
        // mismatch chain stages after the capture and lands in Drift by
        // construction; the operator chain stages before it so the recorded
        // and recomputed fingerprints can agree. Same four namespaces, same
        // four buckets, either way.
        let fixture = if stamp_outgoing_marker {
            let fixture = Fixture::new();
            let index_root = fixture.layout.rebuild_index_paths().index_root;
            for (namespace, commits) in REBUILD_NAMESPACE_STAGING {
                stage_commit_documents(&index_root, namespace, *commits);
            }
            fixture
        } else {
            Fixture::with_history_staged_before_capture()
        };
        let paths = fixture.layout.rebuild_index_paths();
        bind_rebuild_history_records(&fixture.layout);

        // The real backfill, whose journal is the rebuild preflight's
        // predecessor binding.
        let stamper = fixture.production_stamper();
        fixture.preflight(stamper.clone()).unwrap();
        fixture.apply(stamper).unwrap();

        // The stamped owner rows are KEPT, and that is the point of writing
        // schema-valid fixture rows. An earlier revision emptied the knowledge
        // and thread stores here, because the minimal stub rows the fixture
        // then wrote could not survive the rebuild's real reindex pass. That
        // retirement left a root whose ledger bound owner rows that no longer
        // existed - invisible while verify only compared the journal against
        // the ledger, and refused the moment verify started reading the owners.
        // The rows are full documents now, so the reindex parses them and the
        // smoke root stays coherent across BOTH verbs.

        if stamp_outgoing_marker {
            write(
                &paths.index_root.join("schema_version.txt"),
                format!("{REBUILD_OUTGOING_SCHEMA}\n").as_bytes(),
            );
        }
        Self { fixture }
    }

    /// The rebuild's own artifact pair, deliberately NOT the backfill's.
    ///
    /// They share a directory outside the layout (Q-C confinement refuses
    /// artifacts inside the target), but reusing the filenames would overwrite
    /// the backfill journal's bound artifacts and turn the rebuild's
    /// predecessor binding into a self-reference.
    fn rebuild_artifacts(&self) -> (PathBuf, PathBuf) {
        (
            self.fixture.artifacts.join("rebuild-report.json"),
            self.fixture.artifacts.join("rebuild-resolution.json"),
        )
    }

    fn index_root(&self) -> PathBuf {
        self.fixture.layout.rebuild_index_paths().index_root
    }

    fn store(&self) -> ProjectCatalogStore {
        ProjectCatalogStore::open_existing(self.fixture.layout.projects_path()).unwrap()
    }

    /// Drive the replacement exactly as daemon startup does: classify recovery
    /// before the open, inject the SAME guard, open the index, then hand the
    /// SHARED driver the resume signal.
    ///
    /// It is composed here rather than reached through the offline apply
    /// because the offline apply re-proves D-036 Equality immediately before
    /// mutation and this chain is in Drift; the driver, the guard, and the
    /// committer being exercised are the same ones either way.
    fn drive_replacement(
        &self,
    ) -> blackbox::project_catalog_rebuild_admin::CatalogSchemaReplacementDriveV1 {
        self.drive_as_daemon_open().1
    }

    /// The daemon-open composition, returning the recovery classification too.
    ///
    /// `force = false` is the load-bearing argument: daemon startup remains
    /// `SchemaMismatch`-only (Q-F), so this composition can RECOVER a crashed
    /// operator replacement but can never INITIATE one.
    fn drive_as_daemon_open(
        &self,
    ) -> (
        SchemaRebuildResume,
        blackbox::project_catalog_rebuild_admin::CatalogSchemaReplacementDriveV1,
    ) {
        use bbox_corpus_index::index::history_generations::HistoryScanLimitsV1;
        use bbox_indexing::index::schema_rebuild::{
            catalog_schema_replacement_guard, recover_rebuild_manifest_before_open,
        };

        let paths = self.fixture.layout.rebuild_index_paths();
        let store = Arc::new(ProjectCatalogStore::open_existing(&paths.projects_path).unwrap());
        let resume = recover_rebuild_manifest_before_open(&paths.index_root).unwrap();
        let intent =
            blackbox::project_catalog_rebuild_admin::replacement_intent_for(&resume, false);
        let guard = catalog_schema_replacement_guard(
            store.clone(),
            HistoryScanLimitsV1::default(),
            paths.vector_root.clone(),
        );
        let records: Arc<dyn bbox_corpus_core::project_record::ProjectRecordsProvider> = Arc::new(
            bbox_indexing::catalog_records::CatalogProjectRecordsProvider::new(store.clone()),
        );
        let index =
            bbox_corpus_index::index::TranscriptIndex::open_or_create_at_replacement_boundary(
                &paths.index_root,
                Vec::new(),
                None,
                paths.projects_path.clone(),
                paths.code_source_root.clone(),
                paths.knowledge_path.clone(),
                paths.threads_path.clone(),
                paths.roadmap_path.clone(),
                records.clone(),
                Some(guard),
                intent,
            )
            .unwrap();
        let broker = Arc::new(bbox_indexing::checkout_access::CheckoutAccessBroker::new(
            Arc::new(
                bbox_indexing::checkout_access_v2::V2CatalogCheckoutAccessAuthority::new(store),
            ),
            bbox_indexing::checkout_access::CheckoutAccessObservations::in_memory(),
        ));
        let writer =
            bbox_indexing::index::writer_actor::IndexWriterActor::spawn_for_with_checkout_access(
                &index, records, broker,
            );
        let drive = blackbox::project_catalog_rebuild_admin::drive_catalog_schema_replacement(
            &index, &writer, &resume,
        )
        .unwrap();
        (resume, drive)
    }

    /// Run ONLY the pre-replacement guard, exactly as the open boundary
    /// invokes it, and stop.
    ///
    /// This IS crash state (1): the guard has durably published the Prepared
    /// manifest and the process died before `fs::remove_dir_all`. Nothing is
    /// simulated; the next production statement is simply not executed.
    fn run_guard_only(&self, cause: CatalogIndexReplacementCause) -> String {
        use bbox_corpus_index::index::history_generations::HistoryScanLimitsV1;
        use bbox_corpus_index::index::schema_replacement::SchemaReplacementRequest;
        use bbox_indexing::index::schema_rebuild::catalog_schema_replacement_guard;

        let paths = self.fixture.layout.rebuild_index_paths();
        let store = Arc::new(ProjectCatalogStore::open_existing(&paths.projects_path).unwrap());
        let guard = catalog_schema_replacement_guard(
            store,
            HistoryScanLimitsV1::default(),
            paths.vector_root.clone(),
        );
        let observed = fs::read_to_string(paths.index_root.join("schema_version.txt"))
            .map(|raw| raw.trim().to_string())
            .ok();
        guard(&SchemaReplacementRequest {
            index_path: &paths.index_root,
            projects_path: &paths.projects_path,
            code_source_store_path: &paths.code_source_root,
            observed_schema_version: observed,
            target_schema_version: bbox_corpus_index::index::INDEX_SCHEMA_VERSION,
            cause,
        })
        .expect("the guard authorizes the replacement and publishes the prepared manifest");
        read_rebuild_id(&paths.index_root).expect("the guard published a prepared manifest")
    }

    /// Leave the root in exactly the on-disk state one crash point produces.
    ///
    /// Every state is REACHED by running the real steps and stopping, or by
    /// undoing precisely the one step that a crash would have prevented. The
    /// Prepared manifest bytes restored for state (3) are the bytes the real
    /// guard wrote, captured before the drive consumed them, so nothing here
    /// is a hand-authored artifact.
    fn stage_crash(&self, point: CrashPoint, cause: CatalogIndexReplacementCause) -> String {
        let index_root = self.index_root();
        let manifest_path = rebuild_manifest_path(&index_root);
        let rebuild_id = self.run_guard_only(cause);
        let prepared_bytes = fs::read(&manifest_path).unwrap();
        if point == CrashPoint::AfterPreparedBeforeDrop {
            return rebuild_id;
        }

        // The exact next production statement.
        fs::remove_dir_all(&index_root).unwrap();
        if point == CrashPoint::AfterDrop {
            return rebuild_id;
        }

        // Run the recovery drive to completion, then rewind the one or two
        // steps the crash under test would have prevented.
        let drive = self.drive_as_daemon_open().1;
        assert_eq!(
            drive,
            blackbox::project_catalog_rebuild_admin::CatalogSchemaReplacementDriveV1::Completed,
            "staging a late crash point requires the replacement to have landed first"
        );
        fs::remove_file(index_root.join("schema_version.txt")).unwrap();
        if point == CrashPoint::AfterIndexCommitBeforeManifestCommit {
            fs::write(&manifest_path, &prepared_bytes).unwrap();
        }
        rebuild_id
    }
}

/// Where a crash landed, named by the plan's four recovery states.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CrashPoint {
    /// (1) The guard published Prepared; the index was never dropped.
    AfterPreparedBeforeDrop,
    /// (2) The index is gone and only the Prepared manifest survives.
    AfterDrop,
    /// (3) The replacement index carries its documents, the marker is still
    /// withheld, and the manifest never reached Committed.
    AfterIndexCommitBeforeManifestCommit,
    /// (4) The manifest is Committed and only the marker publication is
    /// missing.
    CommittedBeforeMarker,
}

fn rebuild_manifest_path(index_root: &Path) -> PathBuf {
    bbox_corpus_index::index::history_generations::generations_root_for_index(index_root)
        .unwrap()
        .join("rebuild-manifest.json")
}

/// The rebuild id currently on disk, whatever state the manifest is in.
///
/// Identity is what proves the guard did not rerun: a second guard pass mints
/// a NEW rebuild id, so an unchanged id across a recovery is direct evidence
/// that no second manifest was prepared over generations the first one pins.
fn read_rebuild_id(index_root: &Path) -> Option<String> {
    let bytes = fs::read(rebuild_manifest_path(index_root)).ok()?;
    let value: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
    value
        .get("rebuild_id")
        .and_then(|id| id.as_str())
        .map(str::to_string)
}

/// Documents currently in the index carrying one exact term.
fn indexed_term_hits(index_root: &Path, field: &str, value: &str) -> usize {
    use tantivy::collector::DocSetCollector;
    use tantivy::query::TermQuery;
    use tantivy::schema::IndexRecordOption;

    let index = tantivy::Index::open_in_dir(index_root).unwrap();
    bbox_corpus_index::index::register_code_tokenizer(&index);
    let schema = index.schema();
    let reader = index.reader().unwrap();
    let searcher = reader.searcher();
    let query = TermQuery::new(
        tantivy::Term::from_field_text(schema.get_field(field).unwrap(), value),
        IndexRecordOption::Basic,
    );
    searcher.search(&query, &DocSetCollector).unwrap().len()
}

/// The entity ids `stage_commit_documents` wrote for one namespace.
///
/// Recomputed from the same recipe rather than captured, so a change to the
/// staging shape cannot leave these silently pointing at nothing.
fn staged_entity_ids(namespace: &str, commits: usize) -> Vec<String> {
    (0..commits)
        .map(|ordinal| {
            let sha = hex::encode(Sha256::digest(format!("{namespace}:{ordinal}").as_bytes()))
                [..40]
                .to_string();
            format!("commit:{namespace}:{sha}")
        })
        .collect()
}

/// Every staged commit document is present EXACTLY once.
///
/// Duplication is the failure mode idempotent re-emission exists to prevent,
/// and it is invisible to a total count here: the reindex pass also walks the
/// registered checkouts, so a namespace legitimately carries git-walked commits
/// the generation never pinned. Per-entity-id uniqueness is the assertion that
/// isolates re-emission from that traffic.
fn assert_staged_documents_present_exactly_once(
    index_root: &Path,
    namespace: &str,
    commits: usize,
    label: &str,
) {
    for entity_id in staged_entity_ids(namespace, commits) {
        assert_eq!(
            indexed_term_hits(index_root, "entity_id", &entity_id),
            1,
            "{label}: {entity_id} must appear exactly once"
        );
    }
}

/// Bind the catalog records that put each staged namespace in a different
/// manifest bucket.
///
/// Repo-history and ambiguous-namespace records are NOT owner-controlled, so a
/// regular `transact` may add them. The ORIGIN, which is owner-controlled and
/// refuses closure mutation, came from the real migration.
fn bind_rebuild_history_records(layout: &ProjectCatalogMigrationResolvedLayoutV1) {
    use bbox_corpus_core::project_catalog::{
        AmbiguousNamespaceRecord, AmbiguousNamespaceStatus, CommitNamespace, RepoHistoryAuthority,
        RepoHistoryId, RepoHistoryMaterialization, RepoHistoryRecord,
    };

    let namespace = |value: &str| CommitNamespace::parse(value.to_string()).unwrap();
    let store = ProjectCatalogStore::open_existing(layout.projects_path()).unwrap();
    let epoch = store.snapshot().unwrap().epoch();
    store
        .transact(epoch, |catalog, _attachments| {
            // Retire whatever the MIGRATION bound for these four namespaces
            // first. In the Equality chain the namespaces are registered v1
            // repo_ids, so the migration installs an owning record for each and
            // `validate_catalog` refuses a second record claiming the same
            // namespace. Bucket membership is what this function assigns, and
            // it can only assign it from a clean slate. In the Drift chain the
            // namespaces are post-migration and this removes nothing.
            let staged = REBUILD_NAMESPACE_STAGING
                .iter()
                .map(|(name, _)| namespace(name))
                .collect::<Vec<_>>();
            let retired = catalog
                .repo_histories
                .iter()
                .filter(|(_, record)| {
                    staged.contains(&record.primary_namespace)
                        || staged
                            .iter()
                            .any(|name| record.compatibility_namespaces.contains(name))
                })
                .map(|(id, _)| id.clone())
                .collect::<Vec<_>>();
            catalog.repo_histories.retain(|id, _| !retired.contains(id));
            // A project pointing at a retired record dangles, and the
            // authority check pairs the two directions, so the project side
            // has to be cleared in the SAME transaction rather than left for
            // validation to catch.
            for project in catalog.projects.values_mut() {
                if project
                    .repo_history
                    .as_ref()
                    .is_some_and(|id| retired.contains(id))
                {
                    project.repo_history = None;
                }
            }
            catalog
                .ambiguous_namespaces
                .retain(|name, _| !staged.contains(name));

            let owned = RepoHistoryId::parse(format!("rh_{}", "a1".repeat(16))).unwrap();
            catalog.repo_histories.insert(
                owned.clone(),
                RepoHistoryRecord {
                    repo_history_id: owned.clone(),
                    authority: RepoHistoryAuthority::LegacyNamespace(namespace(
                        REBUILD_OWNED_NAMESPACE,
                    )),
                    primary_namespace: namespace(REBUILD_OWNED_NAMESPACE),
                    // The compatibility namespace rides on the SAME record.
                    // That is what gives it a generation with no catalog
                    // `Ready` owner of its own.
                    compatibility_namespaces: [namespace(REBUILD_COMPAT_NAMESPACE)]
                        .into_iter()
                        .collect(),
                    materialization: RepoHistoryMaterialization::NotBuilt,
                },
            );
            // `validate_catalog` requires an ambiguous record to name at least
            // two EXISTING candidates, so the second candidate is a real
            // record the migration already installed.
            let other = catalog
                .repo_histories
                .keys()
                .find(|id| *id != &owned)
                .expect("the migration installed repo-history records")
                .clone();
            catalog.ambiguous_namespaces.insert(
                namespace(REBUILD_AMBIGUOUS_NAMESPACE),
                AmbiguousNamespaceRecord {
                    namespace: namespace(REBUILD_AMBIGUOUS_NAMESPACE),
                    candidate_repo_history_ids: [owned, other].into_iter().collect(),
                    status: AmbiguousNamespaceStatus::Quarantined,
                    materialization: Default::default(),
                },
            );
            Ok(())
        })
        .unwrap();
}

/// Append commit documents carrying BOTH path-bearing fields, mirroring what a
/// pre-cut `build_commit_doc` wrote.
fn stage_commit_documents(index_path: &Path, namespace: &str, commits: usize) {
    let index = tantivy::Index::open_in_dir(index_path).unwrap();
    bbox_corpus_index::index::register_code_tokenizer(&index);
    let schema = index.schema();
    let mut writer: tantivy::IndexWriter = index.writer(15_000_000).unwrap();
    let field = |name: &str| schema.get_field(name).unwrap();
    for ordinal in 0..commits {
        let message = format!("carried subject {namespace} {ordinal}\n\nbody {ordinal}");
        let sha = hex::encode(Sha256::digest(format!("{namespace}:{ordinal}").as_bytes()))[..40]
            .to_string();
        let mut doc = tantivy::TantivyDocument::new();
        doc.add_text(field("doc_type"), "commit");
        doc.add_text(field("chunk_kind"), "git_message");
        doc.add_text(field("entity_id"), format!("commit:{namespace}:{sha}"));
        doc.add_text(field("content"), &message);
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
        doc.add_text(field("commit_sha"), &sha);
        doc.add_text(field("commit_author_name"), "History Fixture");
        doc.add_text(field("commit_author_email"), "fixture@example.invalid");
        doc.add_text(field("session_id"), "");
        doc.add_text(field("account"), "git");
        doc.add_text(field("project"), REBUILD_OUTGOING_HOST_PATH);
        doc.add_text(field("file_path"), "git:proj-outgoing");
        doc.add_text(field("role"), "commit");
        doc.add_u64(field("byte_offset"), 0);
        doc.add_u64(field("is_subagent"), 0);
        writer.add_document(doc).unwrap();
    }
    writer.commit().unwrap();
}

/// Run one drive on a worker thread behind a bounded wait.
///
/// Not stylistic: every defect this phase found in this code path was a lock
/// that never returns, and an unbounded test surfaces that as an anonymous
/// harness kill naming no call. The panic names the mechanism, so the next
/// occurrence is diagnosed from the failure message alone.
fn watchdogged<T: Send + 'static>(
    what: &'static str,
    drive: impl FnOnce() -> T + Send + 'static,
) -> T {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(drive());
    });
    rx.recv_timeout(std::time::Duration::from_secs(120))
        .unwrap_or_else(|_| {
            panic!(
                "{what} did not return within 120s: it is blocked, most likely re-acquiring \
                 a store or lifetime lock it already holds"
            )
        })
}

/// The SHARED driver rebuilds legacy history into EVERY manifest bucket.
///
/// The standing question for this phase is "has anything actually executed this
/// end to end?", and this is the answer for the replacement half: a real
/// migrated root, a real backfill journal, the real guard, the real P3-E
/// committer, and the one shared `drive_catalog_schema_replacement` that daemon
/// startup also calls. Nothing here is a synthetic manifest.
///
/// Four buckets in one pass is the load-bearing part (D-037). A compatibility
/// generation has no catalog `Ready` owner, so any verification driven from
/// `Ready` requirements would report success having skipped it, and the
/// manifest is that generation's only durable identity and GC root.
#[test]
fn the_shared_driver_rebuilds_legacy_history_into_every_manifest_bucket() {
    use bbox_corpus_index::index::history_generations::HistoryProofModeV1;
    use bbox_indexing::project_catalog_rebuild::{
        RebuildManifestBucketV1, read_committed_rebuild_manifest, require_equality_proof_mode,
        verify_manifest_generations,
    };
    use blackbox::project_catalog_rebuild_admin::CatalogSchemaReplacementDriveV1;
    use std::collections::BTreeSet;

    // The fixture is held by THIS binding for the whole test. It owns the
    // tempdir, and an earlier shape that moved it into the watchdog closure
    // destroyed the root the instant the worker thread finished: every
    // assertion afterwards ran against a deleted directory, which reads
    // identically to "the manifest was never written".
    let fixture = Arc::new(RebuildFixture::new());
    let index_root = fixture.index_root();
    let drive = watchdogged("the shared replacement driver", {
        let handle = fixture.clone();
        move || handle.drive_replacement()
    });
    // `NotRequired` would mean the index was never reset, so nothing was
    // rebuilt and any manifest found afterwards was inherited rather than
    // produced by this pass.
    assert_eq!(drive, CatalogSchemaReplacementDriveV1::Completed);

    let manifest = read_committed_rebuild_manifest(&index_root)
        .expect("the P3-E pass committed the manifest, not merely prepared it");
    let verified = verify_manifest_generations(&index_root, &manifest)
        .expect("every named generation is present and hash-verified");
    let buckets = verified
        .iter()
        .map(|row| row.bucket)
        .collect::<BTreeSet<_>>();
    for bucket in [
        RebuildManifestBucketV1::Owned,
        RebuildManifestBucketV1::Compatibility,
        RebuildManifestBucketV1::Ambiguous,
        RebuildManifestBucketV1::Unclaimed,
    ] {
        assert!(
            buckets.contains(&bucket),
            "bucket {bucket:?} carries no verified generation: {verified:?}"
        );
    }

    // The proof mode this chain actually reaches, pinned rather than assumed.
    // The migration refuses to capture a marker-mismatched index, so the
    // recorded fingerprint always folds a Present owner state; the replacement
    // only runs when that marker DOES mismatch, so the recomputed one folds a
    // Corrupt state. `Drift` is the consequence, and D-036 refuses it for a
    // cut-authorizing offline apply. Pinned here so that if the fingerprint
    // recipe or the reset trigger changes, this row names what changed instead
    // of a downstream test failing for an unrelated-looking reason.
    assert_eq!(manifest.prepared.proof_mode, HistoryProofModeV1::Drift);
    let refusal = require_equality_proof_mode(&manifest).unwrap_err();
    assert_eq!(refusal.code, "error.project_catalog_rebuild_proof_mode");
}

/// D1: the offline apply drives the Q-F operator-triggered replacement all the
/// way to a COMMITTED manifest, and returns success.
///
/// This is the state that did not exist before Q-F. The pre-Q-F trigger was a
/// marker mismatch, and the migration refuses to capture a mismatched marker as
/// `Corrupt`; one pinned binary means one marker value, so "the recapture proves
/// Equality" and "the replacement runs" could not both hold. Every earlier
/// exercise of this path therefore stopped at a refusal, and the success return
/// was asserted about nowhere.
///
/// What it drives is the REAL entrypoint, not a composition: artifact
/// authorization, the immediate Equality recapture, recovery classification
/// before the open, the same guard the daemon injects, the shared driver, the
/// P3-E committer, and the committed-manifest verifier. The assertions are the
/// two facts a receipt is allowed to claim: the drive ran (`Completed`, never
/// `NotRequired`) and verification observed the committed manifest.
#[test]
fn the_offline_apply_drives_the_operator_triggered_replacement_to_committed() {
    use bbox_corpus_index::index::history_generations::{HistoryProofModeV1, HistoryScanLimitsV1};
    use bbox_indexing::project_catalog_rebuild::{
        RebuildManifestBucketV1, read_committed_rebuild_manifest, require_equality_proof_mode,
        verify_manifest_generations,
    };
    use bbox_indexing::project_catalog_rebuild_planning::{
        PathFreeRebuildPreflightRequestV1, PathFreeRebuildStatusV1,
    };
    use blackbox::project_catalog_rebuild_admin::{
        CatalogSchemaReplacementDriveV1, PathFreeRebuildApplyRequestV1,
    };
    use std::collections::BTreeSet;

    // Held by THIS binding for the whole test: it owns the tempdir, and moving
    // it into the watchdog closure destroys the root the instant the worker
    // finishes, which reads exactly like "the manifest was never written".
    let fixture = Arc::new(RebuildFixture::at_current_schema());
    let index_root = fixture.index_root();
    let (report_path, resolution_path) = fixture.rebuild_artifacts();

    // The read-only half writes the artifacts the apply is authorized by. It
    // must observe Equality here too: an apply cannot be authorized by a report
    // that recorded Drift.
    let preflight = watchdogged("the rebuild preflight", {
        let handle = fixture.clone();
        let (report_path, resolution_path) = (report_path.clone(), resolution_path.clone());
        move || {
            blackbox::project_catalog_rebuild_admin::preflight(PathFreeRebuildPreflightRequestV1 {
                layout: handle.fixture.layout.clone(),
                target_selection: ProjectCatalogTargetSelectionV1::Rehearsal,
                report_path,
                resolution_path,
                scan_limits: HistoryScanLimitsV1::default(),
                generated_at: "2026-08-05T00:00:02Z".to_string(),
            })
        }
    })
    .expect("the rebuild preflight must succeed against an index at the running schema");
    assert_eq!(
        preflight.status,
        PathFreeRebuildStatusV1::Clean,
        "a refused report cannot authorize an apply"
    );

    let receipt = watchdogged("the offline rebuild apply", {
        let handle = fixture.clone();
        move || {
            blackbox::project_catalog_rebuild_admin::apply(PathFreeRebuildApplyRequestV1 {
                layout: handle.fixture.layout.clone(),
                target_selection: ProjectCatalogTargetSelectionV1::Rehearsal,
                report_path,
                resolution_path,
                scan_limits: HistoryScanLimitsV1::default(),
            })
        }
    })
    .expect("the operator-triggered replacement must reach a committed manifest");

    // `NotRequired` is the failure this receipt exists to exclude: it would
    // mean the destructive pass never ran and nothing committed the manifest
    // the verification below claims to have observed.
    assert_eq!(receipt.drive, CatalogSchemaReplacementDriveV1::Completed);

    let manifest = read_committed_rebuild_manifest(&index_root)
        .expect("the P3-E pass committed the manifest, not merely prepared it");
    // The whole point of the operator cause: Equality, on a manifest that a
    // real destructive pass produced.
    assert_eq!(manifest.prepared.proof_mode, HistoryProofModeV1::Equality);
    require_equality_proof_mode(&manifest)
        .expect("a cut-authorizing rebuild records Equality (D-036)");

    // Every bucket, observed across the same four dispositions the
    // mismatch-triggered chain covers.
    let verified = verify_manifest_generations(&index_root, &manifest).unwrap();
    let buckets = verified
        .iter()
        .map(|row| row.bucket)
        .collect::<BTreeSet<_>>();
    for bucket in [
        RebuildManifestBucketV1::Owned,
        RebuildManifestBucketV1::Compatibility,
        RebuildManifestBucketV1::Ambiguous,
        RebuildManifestBucketV1::Unclaimed,
    ] {
        assert!(
            buckets.contains(&bucket),
            "bucket {bucket:?} carries no verified generation: {verified:?}"
        );
    }

    // The marker is published LAST, and it is published: an apply that
    // returned success having left it withheld would leave the next boot
    // classifying this store as an interrupted replacement forever.
    assert_eq!(
        fs::read_to_string(index_root.join("schema_version.txt"))
            .unwrap()
            .trim(),
        bbox_corpus_index::index::INDEX_SCHEMA_VERSION
    );
}

/// D1, second half: with a real Equality manifest in place, the startup gate's
/// VERIFIED arm is reachable.
///
/// The refusal arms were already provable without it; the PASSING arm was not,
/// for the same Q-F reason the success return was not. A gate that has only
/// ever been observed refusing is a gate nobody has shown will let a correct
/// daemon boot.
#[test]
fn the_startup_gate_verifies_a_committed_equality_rebuild() {
    use bbox_corpus_index::index::history_generations::HistoryScanLimitsV1;
    use bbox_indexing::project_catalog_rebuild_planning::PathFreeRebuildPreflightRequestV1;
    use blackbox::project_catalog_rebuild_admin::{
        PathFreeRebuildApplyRequestV1, RebuildStartupGateV1,
    };

    let fixture = Arc::new(RebuildFixture::at_current_schema());
    let index_root = fixture.index_root();
    let (report_path, resolution_path) = fixture.rebuild_artifacts();
    watchdogged("the offline rebuild apply", {
        let handle = fixture.clone();
        move || {
            blackbox::project_catalog_rebuild_admin::preflight(PathFreeRebuildPreflightRequestV1 {
                layout: handle.fixture.layout.clone(),
                target_selection: ProjectCatalogTargetSelectionV1::Rehearsal,
                report_path: report_path.clone(),
                resolution_path: resolution_path.clone(),
                scan_limits: HistoryScanLimitsV1::default(),
                generated_at: "2026-08-05T00:00:02Z".to_string(),
            })
            .unwrap();
            blackbox::project_catalog_rebuild_admin::apply(PathFreeRebuildApplyRequestV1 {
                layout: handle.fixture.layout.clone(),
                target_selection: ProjectCatalogTargetSelectionV1::Rehearsal,
                report_path,
                resolution_path,
                scan_limits: HistoryScanLimitsV1::default(),
            })
            .unwrap()
        }
    });

    let store = fixture.store();
    let coverage = blackbox::project_catalog_rebuild_admin::validate_rebuild_coverage_before_bind(
        &store,
        &index_root,
    )
    .expect("a committed Equality manifest with every generation on disk must pass the gate");
    let RebuildStartupGateV1::Verified {
        cut_time_generations,
        live_refresh_generations,
        ..
    } = coverage
    else {
        panic!("a migrated store carrying legacy namespaces is not exempt: {coverage:?}");
    };
    assert!(
        cut_time_generations >= 4,
        "all four manifest buckets are verified through the cut-time tier, got \
         {cut_time_generations}"
    );
    // Nothing has advanced past the cut yet, so the live-refresh tier is
    // legitimately empty. The tier is exercised on its own below.
    assert_eq!(live_refresh_generations, 0);
}

/// D1, third part: a post-cut live history refresh advances `Ready` WITHOUT a
/// manifest write, and the gate still passes (P6-C task 1, P3-F item 3).
///
/// This is the tier that decides whether a routine transaction can make the
/// daemon unbootable. The manifest is cut-time evidence and is deliberately not
/// rewritten here; the record is the authority for its own primary namespace,
/// and the gate verifies the generation the record now names directly.
#[test]
fn the_gate_verifies_a_live_refresh_generation_the_manifest_never_named() {
    use bbox_corpus_index::index::history_generations::HistoryScanLimitsV1;
    use bbox_indexing::project_catalog_rebuild::read_committed_rebuild_manifest;
    use bbox_indexing::project_catalog_rebuild_planning::PathFreeRebuildPreflightRequestV1;
    use blackbox::project_catalog_rebuild_admin::{
        PathFreeRebuildApplyRequestV1, RebuildStartupGateV1,
    };

    let fixture = Arc::new(RebuildFixture::at_current_schema());
    let index_root = fixture.index_root();
    let (report_path, resolution_path) = fixture.rebuild_artifacts();
    watchdogged("the offline rebuild apply", {
        let handle = fixture.clone();
        move || {
            blackbox::project_catalog_rebuild_admin::preflight(PathFreeRebuildPreflightRequestV1 {
                layout: handle.fixture.layout.clone(),
                target_selection: ProjectCatalogTargetSelectionV1::Rehearsal,
                report_path: report_path.clone(),
                resolution_path: resolution_path.clone(),
                scan_limits: HistoryScanLimitsV1::default(),
                generated_at: "2026-08-05T00:00:02Z".to_string(),
            })
            .unwrap();
            blackbox::project_catalog_rebuild_admin::apply(PathFreeRebuildApplyRequestV1 {
                layout: handle.fixture.layout.clone(),
                target_selection: ProjectCatalogTargetSelectionV1::Rehearsal,
                report_path,
                resolution_path,
                scan_limits: HistoryScanLimitsV1::default(),
            })
            .unwrap()
        }
    });

    let manifest_before = read_committed_rebuild_manifest(&index_root).unwrap();
    let refreshed = advance_one_history_record_to_a_fresh_generation(&fixture);

    // The manifest is untouched by the refresh, which is the property under
    // test: the gate must not require a cut-time artifact to describe
    // post-cut history.
    let manifest_after = read_committed_rebuild_manifest(&index_root).unwrap();
    assert_eq!(manifest_before.rebuild_id, manifest_after.rebuild_id);

    let store = fixture.store();
    let coverage = blackbox::project_catalog_rebuild_admin::validate_rebuild_coverage_before_bind(
        &store,
        &index_root,
    )
    .expect("a live-refresh generation is verified against the record, not the manifest");
    let RebuildStartupGateV1::Verified {
        live_refresh_generations,
        ..
    } = coverage
    else {
        panic!("expected the gate to run in full: {coverage:?}");
    };
    assert_eq!(
        live_refresh_generations, 1,
        "the refreshed record's generation {refreshed} is verified through the live tier"
    );
}

/// Advance ONE repo-history record onto a freshly created generation, the way
/// a post-cut `transact` does, and return the new generation id.
///
/// The generation is created through the real store so it carries real
/// commitments: the gate's live tier verifies by LOADING it, and a fabricated
/// row would fail for a reason unrelated to what the test is asserting.
fn advance_one_history_record_to_a_fresh_generation(fixture: &RebuildFixture) -> String {
    use bbox_corpus_core::project_catalog::{RepoHistoryGenerationId, RepoHistoryMaterialization};
    use bbox_corpus_index::index::history_generations::{
        HistoryGenerationInputV1, HistoryGenerationOwnerV1, HistoryGenerationStore,
    };

    let index_root = fixture.index_root();
    let generations = HistoryGenerationStore::open_for_index(&index_root).unwrap();
    let store = fixture.store();
    let state = store.snapshot().unwrap();
    let epoch = state.epoch();
    let (record_id, namespace) = state
        .catalog()
        .repo_histories
        .values()
        .find(|record| record.primary_namespace.as_str() == REBUILD_OWNED_NAMESPACE)
        .map(|record| {
            (
                record.repo_history_id.clone(),
                record.primary_namespace.clone(),
            )
        })
        .expect("the fixture bound the owned namespace to a record");
    drop(state);

    // A second generation over the SAME namespace with DIFFERENT content.
    // Generation identity is content-addressed, so the differing commit row is
    // what makes this a new id rather than an idempotent reopen of the one the
    // manifest already names.
    let created = generations
        .create_or_open(HistoryGenerationInputV1 {
            namespace: namespace.clone(),
            owner: HistoryGenerationOwnerV1::Owned {
                repo_history_id: record_id.clone(),
            },
            commit_documents: vec![live_refresh_commit_row(namespace.as_str())],
            vector_inputs: Vec::new(),
            truncated_message_count: 0,
            source_schema_version: bbox_corpus_index::index::INDEX_SCHEMA_VERSION.to_string(),
            source_schema_fingerprint_sha256: "7".repeat(64),
            source_index_fingerprint_sha256: "8".repeat(64),
        })
        .expect("creating a live-refresh generation");
    let generation_id = created.id.as_str().to_string();
    assert!(
        generation_id.starts_with("rhg_"),
        "an owned live-refresh generation carries a catalog-attributed id: {generation_id}"
    );
    let advanced = RepoHistoryGenerationId::parse(generation_id.clone()).unwrap();
    store
        .transact(epoch, |catalog, _attachments| {
            let record = catalog
                .repo_histories
                .get_mut(&record_id)
                .expect("the record survived the rebuild");
            record.materialization = RepoHistoryMaterialization::Ready {
                generation_id: advanced.clone(),
            };
            Ok(())
        })
        .unwrap();
    generation_id
}

/// One commit row for the live-refresh generation, distinct from every row the
/// cut-time pass staged.
fn live_refresh_commit_row(
    namespace: &str,
) -> bbox_corpus_index::index::history_generations::HistoryCommitDocumentV1 {
    let sha = hex::encode(Sha256::digest(
        format!("{namespace}:live-refresh").as_bytes(),
    ))[..40]
        .to_string();
    let content = format!("live refresh subject for {namespace}");
    bbox_corpus_index::index::history_generations::HistoryCommitDocumentV1 {
        entity_id: format!("commit:{namespace}:{sha}"),
        doc_type: "commit".into(),
        chunk_kind: "git_message".into(),
        repo_id: namespace.into(),
        commit_sha: sha,
        content_hash: hex::encode(Sha256::digest(content.as_bytes())),
        path_tokens: content.clone(),
        content,
        parser_version: "test-parser".into(),
        commit_author_name: "History Fixture".into(),
        commit_author_email: "fixture@example.invalid".into(),
        session_id: String::new(),
        account: "git".into(),
        role: "commit".into(),
        byte_offset: 0,
        is_subagent: 0,
    }
}

/// EVERY bucket, including the one a `Ready` walk cannot see.
///
/// The compatibility generation is removed specifically because it is the
/// bucket with no catalog record naming it: a verifier that walked `Ready`
/// requirements would pass this state, and the generation would be both
/// unverified and sweepable, since the manifest is its only GC root.
#[test]
fn manifest_verification_refuses_an_absent_generation_in_the_compatibility_bucket() {
    use bbox_indexing::project_catalog_rebuild::{
        RebuildManifestBucketV1, read_committed_rebuild_manifest, verify_manifest_generations,
    };

    let fixture = Arc::new(RebuildFixture::new());
    let index_root = fixture.index_root();
    watchdogged("the shared replacement driver", {
        let handle = fixture.clone();
        move || handle.drive_replacement()
    });

    let manifest = read_committed_rebuild_manifest(&index_root).unwrap();
    let verified = verify_manifest_generations(&index_root, &manifest).unwrap();
    let compatibility = verified
        .iter()
        .find(|row| row.bucket == RebuildManifestBucketV1::Compatibility)
        .expect("the fixture produced a compatibility generation");

    // Remove the generation's stored record. Its manifest row survives, which
    // is the state the refusal exists for: the manifest still CLAIMS it.
    let removed = remove_generation_record(&index_root, &compatibility.generation_id);
    assert!(removed, "the generation record was located and removed");

    let refusal = verify_manifest_generations(&index_root, &manifest).unwrap_err();
    assert_eq!(
        refusal.code,
        "error.project_catalog_rebuild_generation_unverified"
    );
    assert!(
        refusal.message.contains(&compatibility.generation_id),
        "the refusal names the generation it could not verify: {}",
        refusal.message
    );
}

/// Delete the on-disk record for one generation id, whatever the store named
/// the file. Returns whether anything was removed.
fn remove_generation_record(index_root: &Path, generation_id: &str) -> bool {
    let root =
        bbox_corpus_index::index::history_generations::generations_root_for_index(index_root)
            .unwrap();
    let mut removed = false;
    for entry in fs::read_dir(&root).unwrap().flatten() {
        let name = entry.file_name();
        if name.to_string_lossy() != generation_id {
            continue;
        }
        let path = entry.path();
        if path.is_dir() {
            fs::remove_dir_all(&path).unwrap();
        } else {
            fs::remove_file(&path).unwrap();
        }
        removed = true;
    }
    removed
}

/// The startup gate refuses a cut-time manifest whose proof mode is not
/// Equality (P6-C task 1, D-036), driven through the real gate against a real
/// committed manifest.
#[test]
fn the_startup_gate_refuses_a_non_equality_cut_time_manifest() {
    let fixture = Arc::new(RebuildFixture::new());
    let index_root = fixture.index_root();
    watchdogged("the shared replacement driver", {
        let handle = fixture.clone();
        move || handle.drive_replacement()
    });
    let layout_store = fixture.store();

    let refusal = blackbox::project_catalog_rebuild_admin::validate_rebuild_coverage_before_bind(
        &layout_store,
        &index_root,
    )
    .unwrap_err();
    assert_eq!(refusal.code, "error.project_catalog_rebuild_proof_mode");
}

/// The startup gate refuses a migrated store carrying legacy namespaces with
/// no committed rebuild manifest at all.
#[test]
fn the_startup_gate_refuses_a_migrated_store_with_no_rebuild_manifest() {
    let fixture = Fixture::new();
    let index_root = fixture.layout.rebuild_index_paths().index_root;
    let store = ProjectCatalogStore::open_existing(fixture.layout.projects_path()).unwrap();

    let refusal = blackbox::project_catalog_rebuild_admin::validate_rebuild_coverage_before_bind(
        &store,
        &index_root,
    )
    .unwrap_err();
    assert_eq!(
        refusal.code,
        "error.project_catalog_rebuild_manifest_missing"
    );
}

/// A fresh-v2 store boots UNGATED (D-011, D-030).
///
/// The negative direction matters as much as the refusals: a gate that fired
/// here would make a correct, never-migrated store unbootable, and a fresh
/// store has no legacy commit documents and no rollback assets to verify.
#[test]
fn a_fresh_v2_store_boots_without_the_gate() {
    use blackbox::project_catalog_rebuild_admin::RebuildStartupGateV1;

    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let store = ProjectCatalogStore::initialize_empty(root.join("projects.json")).unwrap();

    // The index deliberately does not exist. A fresh-v2 store must boot
    // without one, so reaching any manifest read at all would be the defect.
    let coverage = blackbox::project_catalog_rebuild_admin::validate_rebuild_coverage_before_bind(
        &store,
        &root.join("index"),
    )
    .expect("a fresh-v2 origin is exempt");
    assert_eq!(coverage, RebuildStartupGateV1::ExemptFreshOrigin);
}

// ---------------------------------------------------------------------------
// Marker-driven GC exclusion (P6-C task 3, plan section 10.2)
// ---------------------------------------------------------------------------

/// A migrated origin publishes the section 10.2 protected roots, authorized by
/// its committed marker's named inventory.
#[test]
fn a_migrated_origin_publishes_marker_driven_gc_roots() {
    use bbox_indexing::project_catalog_store::{CatalogGcExclusionsV1, plan_catalog_gc_exclusions};

    let fixture = Fixture::new();
    let paths = fixture.layout.rebuild_index_paths();
    let exclusions =
        plan_catalog_gc_exclusions(fixture.layout.projects_path(), &paths.index_root).unwrap();

    let CatalogGcExclusionsV1::MarkerDriven {
        named_immutable_assets,
        roots,
        ..
    } = &exclusions
    else {
        panic!("a migrated origin is marker-driven, not exempt: {exclusions:?}");
    };
    // The section 10.2 set: transaction stage, history-rebuild stage, backup,
    // G1, and quarantine (quarantine generations live under the
    // history-generations root).
    for role in [
        "transaction_stage",
        "catalog_backup",
        "migration_immutable_assets",
        "accepted_publication_generations",
        "history_generations",
    ] {
        assert!(
            roots.iter().any(|root| root.role == role),
            "protected root {role} is missing: {roots:?}"
        );
    }
    // Marker-DRIVEN, not a glob: the real migration installed named immutable
    // assets, and each is protected individually by the name the marker
    // records.
    assert!(
        *named_immutable_assets > 0,
        "the fixture's migration installed named immutable assets"
    );
    assert_eq!(
        roots
            .iter()
            .filter(|root| root.role == "migration_immutable_asset")
            .count() as u64,
        *named_immutable_assets
    );

    // The predicate an external sweep actually calls.
    let generations = roots
        .iter()
        .find(|root| root.role == "history_generations")
        .unwrap();
    assert!(exclusions.protects(&generations.path.join("rhg_whatever")));
    assert!(!exclusions.protects(Path::new("/tmp/somewhere-else")));
}

/// THE REACHABILITY PROOF for reading the marker directly.
///
/// This is why the planner does not compose on an opened store.
/// `open_existing` validates the marker on every open, so a planner that went
/// through a store could never observe an absent one: the open would refuse
/// first and this refusal would be unreachable by construction. The assertion
/// pair below is the evidence - the store refuses to open, AND the planner
/// still produces its own refusal from the same damaged state.
#[test]
fn an_absent_marker_refuses_the_sweep_on_a_migrated_origin() {
    use bbox_indexing::project_catalog_store::plan_catalog_gc_exclusions;

    let fixture = Fixture::new();
    let paths = fixture.layout.rebuild_index_paths();
    let marker = marker_path(&fixture.layout);
    assert!(marker.exists(), "the real migration installed a marker");
    fs::remove_file(&marker).unwrap();

    // A store open cannot reach this state: it refuses first.
    let open_refusal =
        ProjectCatalogStore::open_existing(fixture.layout.projects_path()).unwrap_err();
    assert_eq!(
        open_refusal.code(),
        "error.project_catalog_migration_incomplete"
    );

    let refusal =
        plan_catalog_gc_exclusions(fixture.layout.projects_path(), &paths.index_root).unwrap_err();
    assert_eq!(refusal.code(), "error.project_catalog_migration_incomplete");
}

/// A corrupt marker refuses too, and for the same reason: nothing can vouch
/// for the rollback inventory, so sweeping is worse than stopping.
#[test]
fn a_corrupt_marker_refuses_the_sweep_on_a_migrated_origin() {
    use bbox_indexing::project_catalog_store::plan_catalog_gc_exclusions;

    let fixture = Fixture::new();
    let paths = fixture.layout.rebuild_index_paths();
    write(
        &marker_path(&fixture.layout),
        b"{\"version\":1,\"truncated\":",
    );

    let refusal =
        plan_catalog_gc_exclusions(fixture.layout.projects_path(), &paths.index_root).unwrap_err();
    assert_eq!(refusal.code(), "error.project_catalog_migration_incomplete");
}

/// Fresh-v2 is EXEMPT, not refused.
///
/// The negative direction is the one that matters here: D-011 says a fresh-v2
/// origin does not require a marker, so a refusal would make a correct store
/// permanently unsweepable while protecting nothing, since it carries no
/// rollback assets at all.
#[test]
fn a_fresh_v2_origin_is_exempt_from_the_marker_refusal() {
    use bbox_indexing::project_catalog_store::{CatalogGcExclusionsV1, plan_catalog_gc_exclusions};

    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let projects_path = root.join("projects.json");
    drop(ProjectCatalogStore::initialize_empty(&projects_path).unwrap());

    let exclusions = plan_catalog_gc_exclusions(&projects_path, &root.join("index")).unwrap();
    assert_eq!(exclusions, CatalogGcExclusionsV1::ExemptFreshOrigin);
    assert!(!exclusions.protects(&root.join("index")));
}

/// A store that is not a version-2 catalog carries none of these roots, so it
/// is exempt rather than refused: section 10.2 is a catalog-mode contract.
#[test]
fn a_non_catalog_store_is_exempt_from_the_marker_refusal() {
    use bbox_indexing::project_catalog_store::{CatalogGcExclusionsV1, plan_catalog_gc_exclusions};

    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let exclusions =
        plan_catalog_gc_exclusions(&root.join("projects.json"), &root.join("index")).unwrap();
    assert_eq!(exclusions, CatalogGcExclusionsV1::ExemptNonCatalogStore);
}

/// The committed migration marker's path for a layout.
fn marker_path(layout: &ProjectCatalogMigrationResolvedLayoutV1) -> PathBuf {
    let projects_path = layout.projects_path();
    projects_path
        .parent()
        .unwrap()
        .join("project-catalog-migration.json")
}

// ---------------------------------------------------------------------------
// D5: the crash matrix (P6-C task 4, states refined by Q-F)
// ---------------------------------------------------------------------------
//
// Four recovery states, both trigger classes, both callers. The states are
// on-disk facts, so the trigger class is carried by which marker the outgoing
// index holds: `REBUILD_OUTGOING_SCHEMA` for the daemon-upgrade class and the
// running `INDEX_SCHEMA_VERSION` for the operator class. Only the offline apply
// ever INITIATES the same-schema force; the daemon composition below always
// passes `force = false`, which is the property `daemon_open_never_initiates_
// the_same_schema_force` pins directly.

/// State (1), operator class, daemon recovery: the source is intact, the
/// Prepared manifest rolls back, and the daemon serves what it already had.
///
/// The refusal this encodes is the important one: a daemon restart must NEVER
/// silently restart an operator command it did not issue. The index here is at
/// the running schema, so after the rollback there is no mismatch to act on and
/// the correct outcome is to do nothing at all.
#[test]
fn a_pre_drop_operator_crash_rolls_back_and_serves_the_intact_source() {
    use bbox_indexing::index::schema_rebuild::SchemaRebuildResume;
    use blackbox::project_catalog_rebuild_admin::CatalogSchemaReplacementDriveV1;

    let fixture = Arc::new(RebuildFixture::at_current_schema());
    let index_root = fixture.index_root();
    let prepared = watchdogged("staging the pre-drop crash", {
        let handle = fixture.clone();
        move || {
            handle.stage_crash(
                CrashPoint::AfterPreparedBeforeDrop,
                CatalogIndexReplacementCause::OperatorPathFreeRebuild,
            )
        }
    });
    assert_eq!(
        read_rebuild_id(&index_root).as_deref(),
        Some(prepared.as_str())
    );
    assert_staged_documents_present_exactly_once(
        &index_root,
        REBUILD_OWNED_NAMESPACE,
        3,
        "the source index was never dropped",
    );

    let (resume, drive) = watchdogged("the daemon-open recovery", {
        let handle = fixture.clone();
        move || handle.drive_as_daemon_open()
    });
    assert_eq!(resume, SchemaRebuildResume::RolledBack);
    assert_eq!(drive, CatalogSchemaReplacementDriveV1::NotRequired);
    assert_eq!(
        read_rebuild_id(&index_root),
        None,
        "the prepared manifest was rolled back, not left to authorize a later drop"
    );
    assert_staged_documents_present_exactly_once(
        &index_root,
        REBUILD_OWNED_NAMESPACE,
        3,
        "the intact source survives the recovery untouched",
    );
}

/// State (1), operator class, offline retry: the retry BLOCKS for diagnosis.
///
/// FINDING, and it contradicts the optimistic reading of P6-C task 4 ("a later
/// offline retry reauthorizes Equality and starts a fresh forced operation").
/// It cannot, and the reason is structural rather than incidental: the guard
/// that published the Prepared manifest also drove every observed namespace
/// from `NotBuilt` to `Ready`, and that materialization advances the catalog
/// epoch BEFORE the crash. The backfill completion journal pins the epoch it
/// observed, so the retry's predecessor binding now names an epoch the target
/// has moved past.
///
/// The refusal is section 6.2 working as designed: stale recapture blocks for
/// diagnosis and never loops. What it means operationally is that a pre-drop
/// crash is not self-healing from the operator side. The daemon rolls the
/// manifest back and keeps serving the intact source (proven above), but
/// resuming the CUT requires re-running the durable backfill to rebind the
/// predecessor, not just re-running `path-free-rebuild --apply`.
///
/// Pinned here rather than reported and forgotten, because the failure it
/// prevents is the one this discipline exists for: an apply authorized by
/// artifacts that describe a catalog state the target no longer has.
#[test]
fn an_offline_retry_after_a_pre_drop_crash_blocks_on_the_moved_predecessor() {
    use bbox_corpus_index::index::history_generations::HistoryScanLimitsV1;
    use bbox_indexing::project_catalog_rebuild_planning::PathFreeRebuildPreflightRequestV1;

    let fixture = Arc::new(RebuildFixture::at_current_schema());
    let index_root = fixture.index_root();
    let epoch_before = fixture.store().snapshot().unwrap().epoch();
    watchdogged("staging the pre-drop crash", {
        let handle = fixture.clone();
        move || {
            handle.stage_crash(
                CrashPoint::AfterPreparedBeforeDrop,
                CatalogIndexReplacementCause::OperatorPathFreeRebuild,
            )
        }
    });
    // The guard's materialization is what moved the epoch, before any drop.
    let epoch_after = fixture.store().snapshot().unwrap().epoch();
    assert!(
        epoch_after > epoch_before,
        "the guard advanced the catalog while preparing: {epoch_before} -> {epoch_after}"
    );

    // The daemon rolls the abandoned manifest back first, which is the state a
    // real operator retry begins from.
    watchdogged("the daemon-open rollback", {
        let handle = fixture.clone();
        move || handle.drive_as_daemon_open()
    });

    let (report_path, resolution_path) = fixture.rebuild_artifacts();
    let refusal = watchdogged("the offline retry", {
        let handle = fixture.clone();
        move || {
            blackbox::project_catalog_rebuild_admin::preflight(PathFreeRebuildPreflightRequestV1 {
                layout: handle.fixture.layout.clone(),
                target_selection: ProjectCatalogTargetSelectionV1::Rehearsal,
                report_path,
                resolution_path,
                scan_limits: HistoryScanLimitsV1::default(),
                generated_at: "2026-08-05T00:00:03Z".to_string(),
            })
            .map(|_| ())
        }
    })
    .expect_err("the predecessor moved when the guard materialized");
    assert_eq!(
        refusal.code, "error.project_catalog_inventory_stale_post_image",
        "the refusal is the section 7.2 staleness family, not a rebuild-specific code: {}",
        refusal.message
    );
    // Nothing was mutated by the refused retry, and the source is still there.
    assert_eq!(read_rebuild_id(&index_root), None);
    assert_staged_documents_present_exactly_once(
        &index_root,
        REBUILD_OWNED_NAMESPACE,
        3,
        "a blocked retry leaves the intact source untouched",
    );
}

/// State (1), daemon-upgrade class: the rollback is followed by the ordinary
/// mismatch replacement, because that trigger is still armed.
///
/// The contrast with the operator class is the point. Same crash, same
/// rollback, opposite next step, and the marker is the only thing that differs.
#[test]
fn a_pre_drop_mismatch_crash_rolls_back_then_replaces_on_the_next_boot() {
    use bbox_indexing::index::schema_rebuild::SchemaRebuildResume;
    use bbox_indexing::project_catalog_rebuild::read_committed_rebuild_manifest;
    use blackbox::project_catalog_rebuild_admin::CatalogSchemaReplacementDriveV1;

    let fixture = Arc::new(RebuildFixture::new());
    let index_root = fixture.index_root();
    let abandoned = watchdogged("staging the pre-drop crash", {
        let handle = fixture.clone();
        move || {
            handle.stage_crash(
                CrashPoint::AfterPreparedBeforeDrop,
                CatalogIndexReplacementCause::SchemaMismatch,
            )
        }
    });

    let (resume, drive) = watchdogged("the daemon-open recovery", {
        let handle = fixture.clone();
        move || handle.drive_as_daemon_open()
    });
    assert_eq!(resume, SchemaRebuildResume::RolledBack);
    assert_eq!(drive, CatalogSchemaReplacementDriveV1::Completed);
    // The manifest is prepared FRESH by this boot's own guard pass. Its id is
    // deliberately NOT asserted to differ: rebuild ids are content-addressed,
    // so re-preparing over identical history reproduces the same id by design.
    // The rollback above is what proves the abandoned manifest did not
    // authorize this drop, and the id is recorded only to show the fresh pass
    // converged on the same content.
    let manifest = read_committed_rebuild_manifest(&index_root).unwrap();
    assert_eq!(manifest.rebuild_id, abandoned);
}

/// State (2), both classes: `ResumePrepared` re-emits from the pinned
/// generations WITHOUT rerunning the guard.
///
/// The rebuild id is the evidence. A rerun guard mints a new one, so an
/// unchanged id across the recovery proves no second manifest was prepared over
/// generations the first already pins. The index is gone here, so there is no
/// last-good state to return to and re-emission is the only recovery.
#[test]
fn a_post_drop_crash_resumes_from_pinned_generations_without_rerunning_the_guard() {
    use bbox_indexing::index::schema_rebuild::SchemaRebuildResume;
    use bbox_indexing::project_catalog_rebuild::read_committed_rebuild_manifest;
    use blackbox::project_catalog_rebuild_admin::CatalogSchemaReplacementDriveV1;

    for (label, fixture, cause) in [
        (
            "operator",
            RebuildFixture::at_current_schema(),
            CatalogIndexReplacementCause::OperatorPathFreeRebuild,
        ),
        (
            "mismatch",
            RebuildFixture::new(),
            CatalogIndexReplacementCause::SchemaMismatch,
        ),
    ] {
        let fixture = Arc::new(fixture);
        let index_root = fixture.index_root();
        let prepared = watchdogged("staging the post-drop crash", {
            let handle = fixture.clone();
            move || handle.stage_crash(CrashPoint::AfterDrop, cause)
        });
        assert!(!index_root.exists(), "{label}: the drop landed");

        let (resume, drive) = watchdogged("the daemon-open resume", {
            let handle = fixture.clone();
            move || handle.drive_as_daemon_open()
        });
        assert!(
            matches!(resume, SchemaRebuildResume::Resume { .. }),
            "{label}: a Prepared manifest past the drop is the resume evidence, got {resume:?}"
        );
        assert_eq!(drive, CatalogSchemaReplacementDriveV1::Completed, "{label}");
        let manifest = read_committed_rebuild_manifest(&index_root).unwrap();
        assert_eq!(
            manifest.rebuild_id, prepared,
            "{label}: the surviving manifest was committed, not replaced by a second one"
        );
        // The stronger evidence that the guard did not rerun. A rerun would
        // have scanned the DROPPED index and prepared over an empty inventory;
        // the four pinned namespaces survive only because the manifest that
        // pinned them was reused rather than re-derived.
        assert_eq!(
            manifest.prepared.namespace_inventory.len(),
            REBUILD_NAMESPACE_STAGING.len(),
            "{label}: the pinned namespace inventory survived intact"
        );
        assert_staged_documents_present_exactly_once(
            &index_root,
            REBUILD_OWNED_NAMESPACE,
            3,
            &format!("{label}: the pinned generation was re-emitted exactly once"),
        );
    }
}

/// State (3), both classes: the idempotent pass reruns and commits, with the
/// marker still withheld on entry.
///
/// Duplication is what this catches. The documents are already in the index
/// when recovery starts, so a re-emission that appended instead of
/// delete-term-then-adding would double every namespace and no other assertion
/// in this file would notice.
#[test]
fn a_crash_between_the_index_commit_and_the_manifest_commit_reruns_idempotently() {
    use bbox_indexing::index::schema_rebuild::SchemaRebuildResume;
    use bbox_indexing::project_catalog_rebuild::read_committed_rebuild_manifest;
    use blackbox::project_catalog_rebuild_admin::CatalogSchemaReplacementDriveV1;

    for (label, fixture, cause) in [
        (
            "operator",
            RebuildFixture::at_current_schema(),
            CatalogIndexReplacementCause::OperatorPathFreeRebuild,
        ),
        (
            "mismatch",
            RebuildFixture::new(),
            CatalogIndexReplacementCause::SchemaMismatch,
        ),
    ] {
        let fixture = Arc::new(fixture);
        let index_root = fixture.index_root();
        let prepared = watchdogged("staging the mid-commit crash", {
            let handle = fixture.clone();
            move || handle.stage_crash(CrashPoint::AfterIndexCommitBeforeManifestCommit, cause)
        });
        assert!(
            !index_root.join("schema_version.txt").exists(),
            "{label}: the marker is still withheld at this crash point"
        );
        assert_staged_documents_present_exactly_once(
            &index_root,
            REBUILD_OWNED_NAMESPACE,
            3,
            &format!("{label}: the index commit landed before the crash"),
        );

        let (resume, drive) = watchdogged("the daemon-open rerun", {
            let handle = fixture.clone();
            move || handle.drive_as_daemon_open()
        });
        assert!(
            matches!(resume, SchemaRebuildResume::Resume { .. }),
            "{label}: a Prepared manifest with the marker withheld resumes, got {resume:?}"
        );
        assert_eq!(drive, CatalogSchemaReplacementDriveV1::Completed, "{label}");
        let manifest = read_committed_rebuild_manifest(&index_root).unwrap();
        assert_eq!(
            manifest.rebuild_id, prepared,
            "{label}: the same manifest was committed"
        );
        assert_eq!(
            manifest.prepared.namespace_inventory.len(),
            REBUILD_NAMESPACE_STAGING.len(),
            "{label}: the guard did not rerun over the already-replaced index"
        );
        assert_staged_documents_present_exactly_once(
            &index_root,
            REBUILD_OWNED_NAMESPACE,
            3,
            &format!("{label}: the rerun did not duplicate a single document"),
        );
        assert_eq!(
            fs::read_to_string(index_root.join("schema_version.txt"))
                .unwrap()
                .trim(),
            bbox_corpus_index::index::INDEX_SCHEMA_VERSION,
            "{label}: the marker is published last"
        );
    }
}

/// State (4), both classes: a Committed manifest with an unpublished marker is
/// FINALIZED, never dropped and never re-Prepared.
///
/// Both wrong answers are excluded explicitly. Dropping would destroy a
/// finished replacement to redo work that already landed, and re-Preparing
/// would replace committed evidence with a plan for work nobody needs to do.
#[test]
fn a_committed_manifest_with_an_unpublished_marker_is_finalized_not_redone() {
    use bbox_indexing::index::schema_rebuild::SchemaRebuildResume;
    use bbox_indexing::project_catalog_rebuild::read_committed_rebuild_manifest;
    use blackbox::project_catalog_rebuild_admin::CatalogSchemaReplacementDriveV1;

    for (label, fixture, cause) in [
        (
            "operator",
            RebuildFixture::at_current_schema(),
            CatalogIndexReplacementCause::OperatorPathFreeRebuild,
        ),
        (
            "mismatch",
            RebuildFixture::new(),
            CatalogIndexReplacementCause::SchemaMismatch,
        ),
    ] {
        let fixture = Arc::new(fixture);
        let index_root = fixture.index_root();
        let committed = watchdogged("staging the pre-marker crash", {
            let handle = fixture.clone();
            move || handle.stage_crash(CrashPoint::CommittedBeforeMarker, cause)
        });
        assert!(!index_root.join("schema_version.txt").exists(), "{label}");
        let before = read_committed_rebuild_manifest(&index_root)
            .unwrap_or_else(|_| panic!("{label}: the manifest is already Committed"));
        assert_eq!(before.rebuild_id, committed, "{label}");

        let (resume, drive) = watchdogged("the daemon-open finalization", {
            let handle = fixture.clone();
            move || handle.drive_as_daemon_open()
        });
        assert_eq!(resume, SchemaRebuildResume::AlreadyCommitted, "{label}");
        assert_eq!(
            drive,
            CatalogSchemaReplacementDriveV1::FinalizedCommitted,
            "{label}: nothing is re-emitted; only the interrupted publication is finished"
        );
        assert_eq!(
            read_committed_rebuild_manifest(&index_root)
                .unwrap()
                .rebuild_id,
            committed,
            "{label}: the Committed manifest was never replaced with a fresh Prepared one"
        );
        assert_staged_documents_present_exactly_once(
            &index_root,
            REBUILD_OWNED_NAMESPACE,
            3,
            &format!("{label}: the replacement index was never dropped"),
        );
        assert_eq!(
            fs::read_to_string(index_root.join("schema_version.txt"))
                .unwrap()
                .trim(),
            bbox_corpus_index::index::INDEX_SCHEMA_VERSION,
            "{label}: the withheld marker is published"
        );
    }
}

/// Daemon startup NEVER initiates the same-schema force (Q-F).
///
/// Asserted at the intent derivation both callers share, so the property is
/// pinned where it is decided rather than inferred from a call site that a
/// later edit could change without failing anything.
#[test]
fn daemon_open_never_initiates_the_same_schema_force() {
    use bbox_corpus_index::index::schema_replacement::CatalogReplacementIntentV1;
    use bbox_indexing::index::schema_rebuild::SchemaRebuildResume;
    use blackbox::project_catalog_rebuild_admin::replacement_intent_for;

    for resume in [SchemaRebuildResume::None, SchemaRebuildResume::RolledBack] {
        assert_eq!(
            replacement_intent_for(&resume, false),
            CatalogReplacementIntentV1::MismatchOnly,
            "daemon startup stays SchemaMismatch-only from {resume:?}"
        );
        assert_eq!(
            replacement_intent_for(&resume, true),
            CatalogReplacementIntentV1::ForceSameSchema,
            "the offline apply is the caller that forces, from {resume:?}"
        );
    }
    // Surviving manifest evidence OUTRANKS the force in BOTH directions: an
    // operator authorization describes a predecessor index, and an interrupted
    // replacement is not that predecessor.
    for resume in [SchemaRebuildResume::AlreadyCommitted] {
        for force in [false, true] {
            assert_eq!(
                replacement_intent_for(&resume, force),
                CatalogReplacementIntentV1::PreserveInterrupted,
                "{resume:?} with force={force} must not start a fresh operation"
            );
        }
    }
}

/// The forced path REFUSES a predecessor whose marker is not the running
/// version, and refuses before anything is touched.
///
/// This is the Q-F precondition, and it is checked at the boundary rather than
/// by the caller so no future caller can reach the drop without it. A stale or
/// missing marker means the index is not the predecessor the authorization
/// named.
#[test]
fn the_forced_replacement_refuses_a_stale_predecessor_marker() {
    use bbox_corpus_index::index::schema_replacement::CatalogReplacementIntentV1;

    let fixture = RebuildFixture::new(); // left at REBUILD_OUTGOING_SCHEMA
    let paths = fixture.fixture.layout.rebuild_index_paths();
    let store = Arc::new(ProjectCatalogStore::open_existing(&paths.projects_path).unwrap());
    let guard = bbox_indexing::index::schema_rebuild::catalog_schema_replacement_guard(
        store.clone(),
        bbox_corpus_index::index::history_generations::HistoryScanLimitsV1::default(),
        paths.vector_root.clone(),
    );
    let records: Arc<dyn bbox_corpus_core::project_record::ProjectRecordsProvider> =
        Arc::new(bbox_indexing::catalog_records::CatalogProjectRecordsProvider::new(store));
    let error = bbox_corpus_index::index::TranscriptIndex::open_or_create_at_replacement_boundary(
        &paths.index_root,
        Vec::new(),
        None,
        paths.projects_path.clone(),
        paths.code_source_root.clone(),
        paths.knowledge_path.clone(),
        paths.threads_path.clone(),
        paths.roadmap_path.clone(),
        records,
        Some(guard),
        CatalogReplacementIntentV1::ForceSameSchema,
    )
    .err()
    .expect("a marker that is not the running version is a stale predecessor");
    assert!(
        format!("{error:#}").contains("error.schema_replacement_stale_predecessor"),
        "the refusal names the stale predecessor: {error:#}"
    );
    // Nothing was touched: no manifest was prepared and the source survives.
    assert_eq!(read_rebuild_id(&paths.index_root), None);
    assert_staged_documents_present_exactly_once(
        &paths.index_root,
        REBUILD_OWNED_NAMESPACE,
        3,
        "a refused force leaves the source untouched",
    );
}

// ---------------------------------------------------------------------------
// D4: bootsmokes (P6-C task 3, the D-030 pattern)
// ---------------------------------------------------------------------------
//
// D-030's shape is: a facade-driving test PRODUCES a real catalog-mode root,
// and the CLI the operator will actually run VERIFIES it. The value is in the
// seam. A facade test proves the engine; only the CLI proves that the envelope,
// the layout resolution, and the target flags reach that engine at all, and
// those are exactly the parts a parser test cannot reach and a live smoke finds
// at the worst possible moment.

/// Run the real `blackbox` binary against one fixture's config.
///
/// The env is scrubbed for the same reason `project_catalog_cli` scrubs it: an
/// inherited `BLACKBOX_STATE_DIR` or index path would point the command at the
/// HOST's real state, and the command would then succeed or fail for reasons
/// having nothing to do with this fixture.
fn run_cli_against(fixture: &Fixture, args: &[&str]) -> std::process::Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_blackbox"));
    command
        .args(args)
        .env_remove("BLACKBOX_STATE_DIR")
        .env_remove("BLACKBOX_VECTORS_PATH")
        .env_remove("TRANSCRIPT_SEARCH_INDEX_PATH")
        .env("BLACKBOX_CONFIG", fixture.root.join("config.toml"));
    command.output().unwrap()
}

/// The CLI verifies the smoke root both new verbs produced.
///
/// One root, both verbs, in the order the cut runs them: the backfill's
/// completion journal is the rebuild's predecessor binding, so verifying them
/// against the same root is the only way to catch an envelope that resolved a
/// different layout for the second command than the first.
#[test]
fn the_cli_verifies_the_smoke_root_for_both_new_verbs() {
    use bbox_corpus_index::index::history_generations::HistoryScanLimitsV1;
    use bbox_indexing::project_catalog_rebuild_planning::PathFreeRebuildPreflightRequestV1;
    use blackbox::project_catalog_rebuild_admin::PathFreeRebuildApplyRequestV1;

    let fixture = Arc::new(RebuildFixture::at_current_schema());
    let (report_path, resolution_path) = fixture.rebuild_artifacts();
    watchdogged("producing the P6 smoke root", {
        let handle = fixture.clone();
        move || {
            blackbox::project_catalog_rebuild_admin::preflight(PathFreeRebuildPreflightRequestV1 {
                layout: handle.fixture.layout.clone(),
                target_selection: ProjectCatalogTargetSelectionV1::Rehearsal,
                report_path: report_path.clone(),
                resolution_path: resolution_path.clone(),
                scan_limits: HistoryScanLimitsV1::default(),
                generated_at: "2026-08-05T00:00:04Z".to_string(),
            })
            .unwrap();
            blackbox::project_catalog_rebuild_admin::apply(PathFreeRebuildApplyRequestV1 {
                layout: handle.fixture.layout.clone(),
                target_selection: ProjectCatalogTargetSelectionV1::Rehearsal,
                report_path,
                resolution_path,
                scan_limits: HistoryScanLimitsV1::default(),
            })
            .unwrap()
        }
    });

    let rehearsal_root = fixture.fixture.root.join("rehearsal");
    let rehearsal_root = rehearsal_root.to_str().unwrap();
    for verb in ["durable-backfill", "path-free-rebuild"] {
        let output = run_cli_against(
            &fixture.fixture,
            &[
                "project-catalog",
                verb,
                "--verify",
                "--rehearsal-root",
                rehearsal_root,
            ],
        );
        assert!(
            output.status.success(),
            "the CLI must verify the smoke root for {verb}:\nstdout: {}\nstderr: {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

/// A fresh-v2 root boots with NO manifest anywhere, through a restart-shaped
/// reopen and the pre-bind gate (D-011, D-030).
///
/// The negative direction is the whole assertion. A gate that fired here would
/// make a correct, never-migrated store permanently unbootable, and a fresh
/// store has no legacy commit documents and no rollback assets to verify. The
/// reopen is included because a gate is only half the boot path: an open that
/// tried to replace this index would be just as fatal as a gate that refused it.
#[test]
fn a_fresh_v2_root_boots_twice_without_a_manifest() {
    use bbox_indexing::index::schema_rebuild::recover_rebuild_manifest_before_open;
    use blackbox::project_catalog_rebuild_admin::RebuildStartupGateV1;

    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let index_root = root.join("index");
    let projects_path = root.join("projects.json");
    let store = ProjectCatalogStore::initialize_empty(&projects_path).unwrap();

    for pass in 0..2 {
        let resume = recover_rebuild_manifest_before_open(&index_root).unwrap();
        let intent =
            blackbox::project_catalog_rebuild_admin::replacement_intent_for(&resume, false);
        let index = TranscriptIndex::open_or_create_at_replacement_boundary(
            &index_root,
            Vec::new(),
            None,
            projects_path.clone(),
            root.join("code-sources"),
            root.join("blackbox-knowledge.json"),
            root.join("blackbox-threads.json"),
            root.join("blackbox-roadmap.json"),
            Arc::new(bbox_corpus_index::index::StaticProjectRecordsProvider::empty()),
            None,
            intent,
        )
        .unwrap_or_else(|error| panic!("pass {pass}: a fresh-v2 root must open: {error:#}"));
        // No guard is injected above, so a replacement attempt would have
        // refused outright. Reaching here already proves none was attempted;
        // this pins WHY rather than leaving it to the absent guard.
        assert!(!index.schema_was_reset(), "pass {pass}");
        assert!(!index.schema_marker_withheld(), "pass {pass}");
        drop(index);

        let coverage =
            blackbox::project_catalog_rebuild_admin::validate_rebuild_coverage_before_bind(
                &store,
                &index_root,
            )
            .unwrap_or_else(|error| panic!("pass {pass}: a fresh-v2 origin is exempt: {error:?}"));
        assert_eq!(
            coverage,
            RebuildStartupGateV1::ExemptFreshOrigin,
            "pass {pass}"
        );
        assert!(
            !rebuild_manifest_path(&index_root).exists(),
            "pass {pass}: no manifest is written by a fresh-v2 boot"
        );
    }
}

/// A post-cut live history refresh advances `Ready` with NO manifest write, and
/// the daemon RESTARTS over it (P6-C task 3, P3-F item 3).
///
/// This is the tier that decides whether ordinary post-cut traffic can brick a
/// daemon. The refresh writes no manifest deliberately, so a restart that
/// demanded one, or that read the marker-and-manifest state as an interrupted
/// replacement, would refuse to boot a store that is entirely correct.
#[test]
fn a_post_cut_live_refresh_survives_a_restart() {
    use bbox_corpus_index::index::history_generations::HistoryScanLimitsV1;
    use bbox_indexing::project_catalog_rebuild::read_committed_rebuild_manifest;
    use bbox_indexing::project_catalog_rebuild_planning::PathFreeRebuildPreflightRequestV1;
    use blackbox::project_catalog_rebuild_admin::{
        CatalogSchemaReplacementDriveV1, PathFreeRebuildApplyRequestV1, RebuildStartupGateV1,
    };

    let fixture = Arc::new(RebuildFixture::at_current_schema());
    let index_root = fixture.index_root();
    let (report_path, resolution_path) = fixture.rebuild_artifacts();
    watchdogged("the cut", {
        let handle = fixture.clone();
        move || {
            blackbox::project_catalog_rebuild_admin::preflight(PathFreeRebuildPreflightRequestV1 {
                layout: handle.fixture.layout.clone(),
                target_selection: ProjectCatalogTargetSelectionV1::Rehearsal,
                report_path: report_path.clone(),
                resolution_path: resolution_path.clone(),
                scan_limits: HistoryScanLimitsV1::default(),
                generated_at: "2026-08-05T00:00:05Z".to_string(),
            })
            .unwrap();
            blackbox::project_catalog_rebuild_admin::apply(PathFreeRebuildApplyRequestV1 {
                layout: handle.fixture.layout.clone(),
                target_selection: ProjectCatalogTargetSelectionV1::Rehearsal,
                report_path,
                resolution_path,
                scan_limits: HistoryScanLimitsV1::default(),
            })
            .unwrap()
        }
    });
    let manifest_before = read_committed_rebuild_manifest(&index_root).unwrap();
    let manifest_bytes_before = fs::read(rebuild_manifest_path(&index_root)).unwrap();

    let refreshed = advance_one_history_record_to_a_fresh_generation(&fixture);

    // THE RESTART. Same composition daemon startup runs, over a store whose
    // epoch moved after the cut.
    let (resume, drive) = watchdogged("the post-refresh restart", {
        let handle = fixture.clone();
        move || handle.drive_as_daemon_open()
    });
    assert_eq!(
        resume,
        SchemaRebuildResume::AlreadyCommitted,
        "the committed cut manifest is observed, not re-prepared"
    );
    assert_eq!(
        drive,
        CatalogSchemaReplacementDriveV1::NotRequired,
        "a post-cut restart drives nothing: the marker is published and the index is intact"
    );
    assert_eq!(
        fs::read(rebuild_manifest_path(&index_root)).unwrap(),
        manifest_bytes_before,
        "the live refresh and the restart both left the cut-time manifest byte-identical"
    );

    let coverage = blackbox::project_catalog_rebuild_admin::validate_rebuild_coverage_before_bind(
        &fixture.store(),
        &index_root,
    )
    .expect("a restarted post-cut store binds");
    let RebuildStartupGateV1::Verified {
        rebuild_id,
        live_refresh_generations,
        ..
    } = coverage
    else {
        panic!("expected the gate to run in full: {coverage:?}");
    };
    assert_eq!(rebuild_id, manifest_before.rebuild_id);
    assert_eq!(
        live_refresh_generations, 1,
        "generation {refreshed} is verified through the record, which the manifest never named"
    );
}

/// Re-running a SUCCESSFUL offline apply refuses; it never reports success a
/// second time.
///
/// This is the reachability probe behind the `NotRequired` rejection inside
/// `apply`. A mutation-verify pass showed that removing that rejection failed
/// no test, so the question it raises is whether the state is reachable at all.
/// It is not, and which guard blocks it is the interesting part: not the D-036
/// recapture, but the PREDECESSOR binding, one step earlier. The first apply's
/// guard drove every namespace to `Ready`, and that materialization advanced
/// the catalog epoch past the epoch the backfill completion journal pinned, so
/// the re-run's predecessor is stale before its fingerprints are ever compared.
///
/// That is the same mechanism as the pre-drop crash above, and observing it
/// twice is what makes it a general rule rather than an anecdote: ANY second
/// rebuild operation against one backfill journal blocks on predecessor
/// staleness, because the first one moved the epoch that journal names.
///
/// The `NotRequired` arm downstream is therefore a fail-safe covering a state
/// two upstream gates currently make unreachable, not dead weight: it is the
/// assertion that a drive which did nothing can never be reported as a
/// committed rebuild if a future change moves either gate.
///
/// What this test guarantees regardless of which guard fires is the property an
/// operator depends on: a second `path-free-rebuild --apply` does not
/// silently claim a cut it did not perform.
#[test]
fn a_second_offline_apply_refuses_rather_than_reporting_success_again() {
    use bbox_corpus_index::index::history_generations::HistoryScanLimitsV1;
    use bbox_indexing::project_catalog_rebuild_planning::PathFreeRebuildPreflightRequestV1;
    use blackbox::project_catalog_rebuild_admin::{
        CatalogSchemaReplacementDriveV1, PathFreeRebuildApplyRequestV1,
    };

    let fixture = Arc::new(RebuildFixture::at_current_schema());
    let (report_path, resolution_path) = fixture.rebuild_artifacts();
    let first = watchdogged("the first apply", {
        let handle = fixture.clone();
        let (report_path, resolution_path) = (report_path.clone(), resolution_path.clone());
        move || {
            blackbox::project_catalog_rebuild_admin::preflight(PathFreeRebuildPreflightRequestV1 {
                layout: handle.fixture.layout.clone(),
                target_selection: ProjectCatalogTargetSelectionV1::Rehearsal,
                report_path: report_path.clone(),
                resolution_path: resolution_path.clone(),
                scan_limits: HistoryScanLimitsV1::default(),
                generated_at: "2026-08-05T00:00:06Z".to_string(),
            })
            .unwrap();
            blackbox::project_catalog_rebuild_admin::apply(PathFreeRebuildApplyRequestV1 {
                layout: handle.fixture.layout.clone(),
                target_selection: ProjectCatalogTargetSelectionV1::Rehearsal,
                report_path,
                resolution_path,
                scan_limits: HistoryScanLimitsV1::default(),
            })
            .unwrap()
        }
    });
    assert_eq!(first.drive, CatalogSchemaReplacementDriveV1::Completed);

    // Same authorized artifacts, same target, immediately again.
    let refusal = watchdogged("the second apply", {
        let handle = fixture.clone();
        move || {
            blackbox::project_catalog_rebuild_admin::apply(PathFreeRebuildApplyRequestV1 {
                layout: handle.fixture.layout.clone(),
                target_selection: ProjectCatalogTargetSelectionV1::Rehearsal,
                report_path,
                resolution_path,
                scan_limits: HistoryScanLimitsV1::default(),
            })
            .map(|receipt| receipt.drive)
        }
    })
    .expect_err("a second apply must not report a second success");
    assert_eq!(
        refusal.code, "error.project_catalog_inventory_stale_post_image",
        "the first apply moved the catalog epoch the backfill journal pinned, so the \
         predecessor binding refuses upstream of both the D-036 gate and the drive: {}",
        refusal.message
    );
}
