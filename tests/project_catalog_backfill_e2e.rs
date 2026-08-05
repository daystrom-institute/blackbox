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
use bbox_corpus_index::index::TranscriptIndex;
use bbox_edge_sidecar::manifest::ManifestIndex;
use bbox_indexing::project_catalog_backfill::{
    DurableBackfillApplyOutcomeV1, DurableBackfillApplyRequestV1,
    DurableBackfillPreflightRequestV1, DurableBackfillStatusV1, DurableBackfillVerifyRequestV1,
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
    ProjectCatalogOwnerRowStamperV1, ProjectCatalogStamperPathsV1,
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
    write(&index_path.join("_meta.json"), b"{}");

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

fn prepare_rehearsal(root: &Path, config: &Config) -> RehearsalFixture {
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

    let state = root.join("state");
    initialize_empty_owner_state(root);
    let winner_project = ProjectId::parse("neutral-winner").unwrap();
    let collision_winner_project = ProjectId::parse("neutral-collision-winner").unwrap();
    let loser_project = ProjectId::parse("neutral-loser").unwrap();
    write(
        &state.join("projects.json"),
        serde_json::to_vec_pretty(&serde_json::json!({
            "version": 1,
            "projects": [
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
            ]
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
fn migrated_rehearsal_root(root: &Path) -> (ProjectCatalogMigrationResolvedLayoutV1, Config) {
    let config = config(root);
    let rehearsal_root = root.join("rehearsal");
    fs::create_dir_all(&rehearsal_root).unwrap();
    let fixture = prepare_rehearsal(&rehearsal_root, &config);
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
        "the fixture's migration preflight must be clean"
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

fn write_owner(path: &Path, array_field: &str, row_id: &str) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(
        path,
        format!(
            r#"{{"version": 1, "{array_field}": [{{"id": "{row_id}", "project": "/legacy/one"}}]}}
"#
        ),
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
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let (layout, _config) = migrated_rehearsal_root(&root);

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
        write_owner(&owners.knowledge_store_path, "entries", "kb1");
        write_owner(&owners.thread_store_path, "threads", "th1");
        // A third row behind a QUARANTINED binding. Converting it is what makes
        // apply mutate the catalog pair, which is the precondition for the
        // section 3.3 recovery sequencing: stamping alone writes owner stores
        // and leaves the pair (and so the four-hash identity) untouched, so a
        // conversion-free re-apply is merely idempotent rather than stale.
        write_owner(&owners.note_store_path, "notes", "nt1");

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
        project_id: &ProjectId,
    ) -> Result<LegacyRowStampOutcomeV1, ProjectCatalogMigrationError> {
        if self.remaining.fetch_sub(1, Ordering::SeqCst) == 0 {
            return Err(ProjectCatalogMigrationError::no_mutation(
                "error.project_catalog_inventory_stale_post_image",
                "injected torn-stamper fault",
            ));
        }
        self.inner.stamp(store_kind, source_row_id, project_id)
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

    let verify = ProjectCatalogDurableBackfillFacadeV1::verify(DurableBackfillVerifyRequestV1 {
        layout: fixture.layout.clone(),
        target_selection: ProjectCatalogTargetSelectionV1::Rehearsal,
    })
    .unwrap();
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
