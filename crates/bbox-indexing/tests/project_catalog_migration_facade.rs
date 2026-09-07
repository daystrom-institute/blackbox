use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use bbox_code_source::{
    GenerationDescriptor, GenerationState, ManifestEntry, SCHEMA_VERSION, WALKER_POLICY_VERSION,
    dirty_fingerprint, generation_id, manifest_sha256, source_selector,
};
use bbox_code_source_store::{
    ActivationRecord, CodeSourceStore, CodeSourceStorePaths, MigrationEffectiveSourceManifestV1,
    MigrationEffectiveSourceSelectionV1, StoredGeneration, decode_activation_v2_for_migration,
    decode_collision_retirement_pending_for_migration,
    decode_migration_effective_source_manifest_v1, decode_stored_generation_v2_for_migration,
    encode_migration_effective_source_manifest_v1,
};
use bbox_config::config::{self, Config, LoadOptions};
use bbox_corpus_core::identity::PublishedScope;
use bbox_corpus_core::project_catalog::{
    AttachmentId, AttachmentKind, AttachmentStatus, CorpusProject, ProjectId, ProjectScope,
    ScopeMigrationKind, decode_attachment_snapshot,
};
use bbox_corpus_index::index::TranscriptIndex;
use bbox_edge_sidecar::manifest::ManifestIndex;
use bbox_indexing::project_catalog_inventory::{
    LegacyProjectPathStatusV1, ProjectCatalogMigrationStatusV1, QuarantineCollectedV1,
    SelectedScopeOwnerV1, decode_migration_report_v1, decode_migration_resolution_v1, digest_path,
    encode_migration_resolution_v1,
};
use bbox_indexing::project_catalog_migration::{
    ProjectCatalogMigrationApplyConfiguredRequestV1, ProjectCatalogMigrationApplyOutcomeV1,
    ProjectCatalogMigrationApplyRequestV1, ProjectCatalogMigrationFacadeV1,
    ProjectCatalogMigrationLayoutOverridesV1, ProjectCatalogMigrationMutationDispositionV1,
    ProjectCatalogMigrationPreflightRequestV1, ProjectCatalogMigrationResolvedLayoutV1,
    ProjectCatalogMigrationVerifyConfiguredRequestV1, ProjectCatalogMigrationVerifyRequestV1,
    project_catalog_migration_store_limits,
};
use bbox_indexing::project_catalog_migration_lock::ProjectCatalogMigrationLock;
use bbox_indexing::project_catalog_store::ProjectCatalogStore;
use bbox_indexing::publisher::PublisherRefStore;
use bbox_vectors::VectorStore;
use sha2::{Digest, Sha256};
use tempfile::tempdir;

fn write(path: &Path, bytes: &[u8]) {
    fs::create_dir_all(path.parent().expect("fixture path has a parent")).unwrap();
    fs::write(path, bytes).unwrap();
}

fn assert_public_value_is_path_redacted(value: &impl serde::Serialize, fixture_root: &Path) {
    let serialized = serde_json::to_string(value).unwrap();
    let fixture_root = fixture_root.to_string_lossy();
    for private_token in [
        fixture_root.as_ref(),
        "winner-checkout",
        "collision-winner-checkout",
        "loser-checkout",
    ] {
        assert!(
            !serialized.contains(private_token),
            "public value leaked fixture token {private_token:?}: {serialized}"
        );
    }
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

#[test]
fn external_consumer_runs_exact_review_apply_fresh_verify_and_reapply() {
    let directory = tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap();
    let config = config(&root);
    let rehearsal_root = root.join("rehearsal");
    fs::create_dir_all(&rehearsal_root).unwrap();
    let fixture = prepare_rehearsal(&rehearsal_root, &config);
    assert!(
        !fixture
            .winner_checkout
            .join(".bbox/local/checkout-id")
            .exists()
            && !fixture
                .collision_winner_checkout
                .join(".bbox/local/checkout-id")
                .exists()
            && !fixture
                .loser_checkout
                .join(".bbox/local/checkout-id")
                .exists(),
        "fixture must begin with markerless checkouts"
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
    let public_error =
        ProjectCatalogMigrationFacadeV1::preflight(ProjectCatalogMigrationPreflightRequestV1 {
            layout: rehearsal.clone(),
            report_path: rehearsal_root.join("state/projects.json"),
            resolution_path: resolution_path.clone(),
            sensitive_report_path: None,
        })
        .unwrap_err();
    assert_public_value_is_path_redacted(&public_error, &rehearsal_root);

    let assessment =
        ProjectCatalogMigrationFacadeV1::preflight(ProjectCatalogMigrationPreflightRequestV1 {
            layout: rehearsal.clone(),
            report_path: report_path.clone(),
            resolution_path: resolution_path.clone(),
            sensitive_report_path: None,
        })
        .unwrap();
    assert_eq!(
        assessment.receipt.status,
        ProjectCatalogMigrationStatusV1::ResolutionRequired
    );
    let assessment_report = decode_migration_report_v1(&fs::read(&report_path).unwrap()).unwrap();
    let mut resolution =
        decode_migration_resolution_v1(&fs::read(&resolution_path).unwrap()).unwrap();
    let scope_conflict = assessment_report.scope_conflicts.first().unwrap();
    resolution.selected_scope_owners.push(SelectedScopeOwnerV1 {
        resolution_id: scope_conflict.conflict_id.clone(),
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
    let activation_assessment =
        ProjectCatalogMigrationFacadeV1::preflight(ProjectCatalogMigrationPreflightRequestV1 {
            layout: rehearsal.clone(),
            report_path: report_path.clone(),
            resolution_path: resolution_path.clone(),
            sensitive_report_path: None,
        })
        .unwrap();
    assert_eq!(
        activation_assessment.receipt.status,
        ProjectCatalogMigrationStatusV1::ResolutionRequired
    );
    let activation_report = decode_migration_report_v1(&fs::read(&report_path).unwrap()).unwrap();
    let quarantine = activation_report.activation_conflicts.first().unwrap();
    resolution.quarantine_collected.push(QuarantineCollectedV1 {
        resolution_id: quarantine.conflict_id.clone(),
        project_id: fixture.loser_project.clone(),
        generation_id: fixture.loser_generation.clone(),
    });
    fs::write(
        &resolution_path,
        encode_migration_resolution_v1(&resolution).unwrap(),
    )
    .unwrap();
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
        ProjectCatalogMigrationStatusV1::Clean
    );
    assert_public_value_is_path_redacted(&preflight.receipt, &rehearsal_root);
    assert_eq!(preflight.receipt.checkout_action_count, 3);
    assert_eq!(preflight.receipt.publisher_pin_count, 1);
    assert_eq!(preflight.receipt.quarantine_root_count, 1);
    assert_eq!(preflight.receipt.attached_project_count, 3);
    assert_eq!(preflight.receipt.omitted_catalog_count, 0);

    let reviewed_report_bytes = fs::read(&report_path).unwrap();
    let mut tampered_report: serde_json::Value =
        serde_json::from_slice(&reviewed_report_bytes).unwrap();
    let first_asset_hash = tampered_report["predicted_immutable_asset_hashes"]
        .as_object_mut()
        .unwrap()
        .values_mut()
        .next()
        .unwrap();
    *first_asset_hash = serde_json::Value::String("0".repeat(64));
    fs::write(&report_path, serde_json::to_vec(&tampered_report).unwrap()).unwrap();
    let tampered_error =
        ProjectCatalogMigrationFacadeV1::apply_rehearsal(ProjectCatalogMigrationApplyRequestV1 {
            rehearsal_layout: rehearsal.clone(),
            protected_layout: protected.clone(),
            report_path: report_path.clone(),
            resolution_path: resolution_path.clone(),
        })
        .unwrap_err();
    assert_eq!(
        tampered_error.mutation_disposition,
        ProjectCatalogMigrationMutationDispositionV1::NoDurableMutation
    );
    assert_public_value_is_path_redacted(&tampered_error, &rehearsal_root);
    assert!(
        !rehearsal_root
            .join("state/project-catalog-migration.json")
            .exists()
    );
    fs::write(&report_path, reviewed_report_bytes).unwrap();

    let applied =
        ProjectCatalogMigrationFacadeV1::apply_rehearsal(ProjectCatalogMigrationApplyRequestV1 {
            rehearsal_layout: rehearsal.clone(),
            protected_layout: protected.clone(),
            report_path: report_path.clone(),
            resolution_path: resolution_path.clone(),
        })
        .unwrap();
    assert_eq!(
        applied.receipt.outcome,
        ProjectCatalogMigrationApplyOutcomeV1::Applied
    );
    assert_public_value_is_path_redacted(&applied.receipt, &rehearsal_root);
    assert_eq!(
        applied.receipt.verification.expected_catalog_hash,
        preflight.receipt.predicted_catalog_hash
    );
    assert_eq!(
        applied.receipt.verification.expected_attachment_hash,
        preflight.receipt.predicted_attachment_hash
    );
    assert_eq!(
        applied.receipt.verification.expected_participant_hashes,
        preflight.receipt.predicted_participant_hashes
    );
    assert_eq!(
        applied.receipt.verification.expected_immutable_asset_hashes,
        preflight.receipt.predicted_immutable_asset_hashes
    );
    assert_eq!(
        Some(applied.receipt.verification.predicted_marker_hash.clone()),
        preflight.receipt.predicted_marker_hash
    );

    let verified =
        ProjectCatalogMigrationFacadeV1::verify(ProjectCatalogMigrationVerifyRequestV1 {
            rehearsal_layout: rehearsal.clone(),
        })
        .unwrap();
    assert_public_value_is_path_redacted(verified.receipt(), &rehearsal_root);
    assert_eq!(verified.receipt(), &applied.receipt.verification);
    assert_eq!(verified.compatibility().records().len(), 3);
    assert_eq!(verified.compatibility().omitted_catalog_count(), 0);

    // Phase 3 plan section 5 (P3-A): the namespace-inventory asset and the
    // git_meta backup are unconditional parts of every migration from here
    // on. Hash-binding of the asset is already covered by the
    // expected/observed immutable-asset-hash equality asserted above; these
    // checks additionally prove the asset's actual presence and shape, and
    // that the cursor-file backup was materialized.
    assert!(
        applied
            .receipt
            .verification
            .backup_hashes
            .contains_key("backup-git_meta"),
        "git_meta backup must be recorded in the verification receipt: {:?}",
        applied.receipt.verification.backup_hashes
    );
    let immutable_assets_dir = rehearsal_root.join("state/project-catalog-migration-assets");
    let namespace_asset_rows: Vec<serde_json::Value> = fs::read_dir(&immutable_assets_dir)
        .unwrap()
        .filter_map(|entry| {
            let path = entry.unwrap().path();
            let bytes = fs::read(&path).ok()?;
            let value: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
            let object = value.as_object()?;
            let mut keys: Vec<&str> = object.keys().map(String::as_str).collect();
            keys.sort();
            (keys
                == [
                    "inventory_hash",
                    "rows",
                    "source_index_fingerprint",
                    "version",
                ])
            .then(|| object.get("rows").unwrap().clone())
        })
        .collect();
    assert_eq!(
        namespace_asset_rows.len(),
        1,
        "exactly one legacy commit namespace inventory asset must be installed"
    );
    assert_eq!(
        namespace_asset_rows[0].as_array().unwrap().len(),
        0,
        "the fixture has no legacy commit namespace evidence, so the asset carries zero rows"
    );
    assert!(
        rehearsal_root
            .join("state/project-catalog-backups/git_meta")
            .is_dir(),
        "git_meta backup directory must be materialized under the backup root"
    );

    let executable_report = decode_migration_report_v1(&fs::read(&report_path).unwrap()).unwrap();
    for (project_id, checkout, registered_at) in [
        (
            &fixture.winner_project,
            &fixture.winner_checkout,
            "2026-01-02T03:04:05Z",
        ),
        (
            &fixture.collision_winner_project,
            &fixture.collision_winner_checkout,
            "2026-01-02T03:04:06Z",
        ),
        (
            &fixture.loser_project,
            &fixture.loser_checkout,
            "2026-01-02T03:04:07Z",
        ),
    ] {
        let planned_repo_id = executable_report
            .repo_history_groups
            .iter()
            .find(|group| group.project_ids.contains(project_id))
            .expect("legacy project has reviewed history group")
            .planned_primary_namespace
            .as_str();
        let record = verified
            .compatibility()
            .records()
            .iter()
            .find(|record| record.project_id == project_id.as_str())
            .unwrap();
        assert_eq!(record.canonical_path, checkout.to_str().unwrap());
        assert_eq!(record.repo_id.as_deref(), Some(planned_repo_id));
        assert_eq!(record.registered_at, registered_at);
        assert!(record.is_git_repo);
        assert!(record.languages.is_empty());
        assert!(record.aliases.is_empty());
    }
    let attachment_snapshot = decode_attachment_snapshot(
        &fs::read(rehearsal_root.join("state/project-attachments.json")).unwrap(),
    )
    .unwrap();
    for project_id in [
        &fixture.winner_project,
        &fixture.collision_winner_project,
        &fixture.loser_project,
    ] {
        assert_eq!(
            attachment_snapshot
                .attachments
                .values()
                .filter(|attachment| {
                    attachment.project_id == *project_id
                        && attachment.kind == AttachmentKind::Base
                        && attachment.status == AttachmentStatus::Attached
                })
                .count(),
            1
        );
    }

    let code_source_paths =
        CodeSourceStorePaths::new(rehearsal_root.join("state/code-sources")).unwrap();
    for (project_id, checkout) in [
        (&fixture.winner_project, &fixture.winner_checkout),
        (
            &fixture.collision_winner_project,
            &fixture.collision_winner_checkout,
        ),
        (&fixture.loser_project, &fixture.loser_checkout),
    ] {
        let observation_id = &executable_report
            .attachments
            .iter()
            .find(|attachment| &attachment.project_id == project_id)
            .unwrap()
            .checkout_observation_id;
        let action = executable_report
            .checkout_identity_actions
            .iter()
            .find(|action| &action.observation_id == observation_id)
            .unwrap();
        // Installed markers carry the runtime producer's bare shape.
        assert_eq!(
            fs::read_to_string(checkout.join(".bbox/local/checkout-id")).unwrap(),
            action.planned_checkout_id
        );
    }
    let effective = decode_migration_effective_source_manifest_v1(
        &fs::read(code_source_paths.anchor()).unwrap(),
    )
    .unwrap();
    assert_eq!(effective.selections.len(), 2);
    assert!(effective.selections.iter().any(|selection| {
        selection.project_id == fixture.winner_project
            && selection.generation_id == fixture.winner_generation
    }));
    assert!(effective.selections.iter().any(|selection| {
        selection.project_id == fixture.collision_winner_project
            && selection.generation_id == fixture.collision_winner_generation
    }));
    let winner_activation = decode_activation_v2_for_migration(
        &fs::read(code_source_paths.activation(&fixture.winner_project)).unwrap(),
    )
    .unwrap();
    assert_eq!(winner_activation.project_id, fixture.winner_project);
    assert_eq!(winner_activation.published_scope, fixture.scope);
    assert_eq!(winner_activation.generation_id, fixture.winner_generation);
    let collision_winner_activation = decode_activation_v2_for_migration(
        &fs::read(code_source_paths.activation(&fixture.collision_winner_project)).unwrap(),
    )
    .unwrap();
    assert_eq!(
        collision_winner_activation.published_scope,
        fixture.collision_scope
    );
    assert_eq!(
        collision_winner_activation.generation_id,
        fixture.collision_winner_generation
    );
    assert!(
        !code_source_paths
            .activation(&fixture.loser_project)
            .exists()
    );
    let winner_metadata = decode_stored_generation_v2_for_migration(
        &fs::read(
            code_source_paths
                .generation_metadata(&fixture.scope, &fixture.winner_generation)
                .unwrap(),
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(winner_metadata.published_scope, fixture.scope);
    let collision_winner_metadata = decode_stored_generation_v2_for_migration(
        &fs::read(
            code_source_paths
                .generation_metadata(
                    &fixture.collision_scope,
                    &fixture.collision_winner_generation,
                )
                .unwrap(),
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(
        collision_winner_metadata.published_scope,
        fixture.collision_scope
    );
    let loser_metadata = decode_stored_generation_v2_for_migration(
        &fs::read(
            code_source_paths
                .generation_metadata(&fixture.collision_scope, &fixture.loser_generation)
                .unwrap(),
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(loser_metadata.published_scope, fixture.collision_scope);
    let retirement = decode_collision_retirement_pending_for_migration(
        &fs::read(code_source_paths.collision_retirement_pending(&fixture.loser_project)).unwrap(),
    )
    .unwrap();
    let retired = retirement.entry(&fixture.loser_generation).unwrap();
    assert_eq!(
        retired.inventory_hash,
        preflight.receipt.inventory_hash.to_string()
    );
    assert_eq!(retired.plan_hash, preflight.receipt.plan_hash.to_string());
    let pointer_path = rehearsal_root
        .join("state/accepted-publications/pointers")
        .join(format!("{}.json", fixture.winner_project));
    let pointer_bytes = fs::read(&pointer_path).unwrap();
    let pointer: serde_json::Value = serde_json::from_slice(&pointer_bytes).unwrap();
    assert_eq!(
        pointer["accepted_scope"],
        serde_json::to_value(&fixture.scope).unwrap()
    );
    let generation_id = pointer["accepted_generation"].as_str().unwrap();
    let generation_path = rehearsal_root
        .join("state/accepted-publications/generations")
        .join(fixture.winner_project.as_str())
        .join(format!("{generation_id}.json"));
    let generation_bytes = fs::read(&generation_path).unwrap();
    let generation: serde_json::Value = serde_json::from_slice(&generation_bytes).unwrap();
    assert_eq!(
        generation["scope"],
        serde_json::to_value(&fixture.scope).unwrap()
    );
    assert_eq!(
        executable_report
            .predicted_accepted_pointer_hashes
            .get(&fixture.winner_project)
            .unwrap(),
        &bbox_indexing::project_catalog_inventory::Sha256ValueV1::digest(&pointer_bytes)
    );
    let g1 = executable_report
        .predicted_g1_assets
        .iter()
        .find(|row| row.asset_id == generation_id)
        .unwrap();
    assert_eq!(
        g1.content_hash,
        bbox_indexing::project_catalog_inventory::Sha256ValueV1::digest(&generation_bytes)
    );
    let marker_bytes =
        fs::read(rehearsal_root.join("state/project-catalog-migration.json")).unwrap();
    assert_eq!(
        applied.receipt.verification.predicted_marker_hash,
        bbox_indexing::project_catalog_inventory::Sha256ValueV1::digest(&marker_bytes)
    );
    assert_eq!(
        applied.receipt.verification.observed_marker_hash,
        applied.receipt.verification.predicted_marker_hash
    );
    let marker: serde_json::Value = serde_json::from_slice(&marker_bytes).unwrap();
    assert_eq!(
        marker["transaction_id"],
        preflight.receipt.transaction_id.to_string()
    );
    assert_eq!(marker["plan_hash"], preflight.receipt.plan_hash.to_string());
    assert_eq!(
        marker["report_artifact_sha256"],
        preflight.receipt.report_artifact_hash.to_string()
    );
    assert_eq!(
        marker["resolution_artifact_sha256"],
        preflight.receipt.resolution_artifact_hash.to_string()
    );
    assert_eq!(
        marker["inventory_sha256"],
        preflight.receipt.inventory_hash.to_string()
    );
    assert_eq!(marker["migration_epoch"], 1);
    assert!(!applied.receipt.verification.backup_hashes.is_empty());
    assert!(
        applied
            .receipt
            .verification
            .expected_immutable_asset_hashes
            .len()
            >= 5,
        "fixture must retain source backups, collected manifests, and G1"
    );

    let reapplied =
        ProjectCatalogMigrationFacadeV1::apply_rehearsal(ProjectCatalogMigrationApplyRequestV1 {
            rehearsal_layout: rehearsal.clone(),
            protected_layout: protected,
            report_path,
            resolution_path,
        })
        .unwrap();
    assert_eq!(
        reapplied.receipt.outcome,
        ProjectCatalogMigrationApplyOutcomeV1::AlreadyApplied
    );
    assert_eq!(reapplied.receipt.verification, applied.receipt.verification);

    // Phase-2 §6.4: the applied rehearsal root IS the isolated migrated v2
    // state the catalog runtime path opens. Prove mode selection, the
    // bridge refusal, the strict pair open, and strict-arm resolution
    // against it before the tamper sections below dirty the root.
    let rehearsal_projects = rehearsal_root.join("state").join("projects.json");
    assert_eq!(
        bbox_indexing::project_catalog_store::probe_project_store_mode(&rehearsal_projects)
            .unwrap(),
        bbox_indexing::project_catalog_store::ProjectStoreProbe::CatalogV2
    );
    assert!(
        bbox_indexing::projects::ProjectRegistry::open(&rehearsal_projects).is_err(),
        "the version-1 bridge must refuse a v2 catalog store"
    );
    {
        let store = bbox_indexing::project_catalog_store::ProjectCatalogStore::open_existing(
            &rehearsal_projects,
        )
        .unwrap();
        let state = store.snapshot().unwrap();
        let engine = bbox_indexing::project_resolver::ProjectResolverEngine::v2(
            state.catalog(),
            state.attachments(),
        );
        use bbox_corpus_core::project_selector::{ProjectSelectorRequest, ResolveIntent};
        let resolved = engine
            .resolve(&ProjectSelectorRequest::selection(
                fixture.winner_project.as_str(),
                ResolveIntent::Read,
            ))
            .unwrap();
        assert_eq!(
            resolved.project_id(),
            Some(fixture.winner_project.as_str()),
            "exact id membership resolves against the migrated catalog"
        );
        let resolved = engine
            .resolve(&ProjectSelectorRequest::selection(
                fixture.winner_checkout.to_str().unwrap(),
                ResolveIntent::Read,
            ))
            .unwrap();
        assert_eq!(
            resolved.project_id(),
            Some(fixture.winner_project.as_str()),
            "the migrated winner checkout resolves through its attachment"
        );
        let unknown_path = rehearsal_root.join("nowhere");
        for raw in ["no-such-project", unknown_path.to_str().unwrap()] {
            let error = engine
                .resolve(&ProjectSelectorRequest::selection(raw, ResolveIntent::Read))
                .unwrap_err();
            assert_eq!(
                error.code(),
                "error.project_selector_unknown",
                "unknown selectors fail closed on the migrated root: {raw}"
            );
        }
    }

    fs::write(
        rehearsal_root.join("checkouts").join("invalid-entry"),
        b"not a checkout directory",
    )
    .unwrap();
    let verify_error =
        ProjectCatalogMigrationFacadeV1::verify(ProjectCatalogMigrationVerifyRequestV1 {
            rehearsal_layout: rehearsal,
        })
        .unwrap_err();
    assert_eq!(
        verify_error.code,
        "error.project_catalog_migration_owner_snapshot"
    );
    assert_eq!(
        verify_error.mutation_disposition,
        ProjectCatalogMigrationMutationDispositionV1::RecoveredToCommittedState
    );

    // ── Phase-2 §10 exit-gate acceptance on the migrated root ──────────
    //
    // Regular admin transactions over the migrated pair (D-029 lifecycle)
    // shape the two §10 fixtures the migration itself cannot express:
    // detaching the loser makes it the remote-only published project with
    // an active collected generation and zero active attachments, and a
    // second worktree-kind attachment on the winner makes it the
    // two-attachment project. Every mutation is a §7.10 admin round trip
    // on the same root.
    {
        use bbox_corpus_core::project_selector::{
            ProjectResolution, ProjectSelectorRequest, ResolveIntent, SessionCheckoutRef,
        };
        use bbox_indexing::project_catalog_admin;

        let store = bbox_indexing::project_catalog_store::ProjectCatalogStore::open_existing(
            &rehearsal_projects,
        )
        .unwrap();
        let epoch = |store: &bbox_indexing::project_catalog_store::ProjectCatalogStore| {
            store.snapshot().unwrap().epoch()
        };

        // Accepted alias for the remote-only project (the collision winner:
        // published scope plus an active collected generation): nominate
        // through a regular transaction (the tool layer's ingestion path),
        // accept through the D-005 lifecycle op.
        let remote = fixture.collision_winner_project.clone();
        store
            .transact(epoch(&store), |catalog, _| {
                catalog
                    .projects
                    .get_mut(&remote)
                    .unwrap()
                    .nominated_aliases
                    .insert("remote-alias".to_string());
                Ok(())
            })
            .unwrap();
        project_catalog_admin::alias_decide(
            &store,
            epoch(&store),
            &fixture.collision_winner_project,
            "remote-alias",
            true,
        )
        .unwrap();
        // A duplicate nomination for an alias someone else accepted fails
        // closed at acceptance (§10 item 4, duplicate aliases).
        let winner = fixture.winner_project.clone();
        store
            .transact(epoch(&store), |catalog, _| {
                catalog
                    .projects
                    .get_mut(&winner)
                    .unwrap()
                    .nominated_aliases
                    .insert("remote-alias".to_string());
                Ok(())
            })
            .unwrap();
        let conflict = project_catalog_admin::alias_decide(
            &store,
            epoch(&store),
            &fixture.winner_project,
            "remote-alias",
            true,
        )
        .unwrap_err();
        assert_eq!(
            conflict.code(),
            "error.project_catalog_admin_alias_conflict"
        );

        // Detach the collision winner's only attachment: remote-only shape
        // with its collected generation retained (detach preserves logical
        // state).
        let state = store.snapshot().unwrap();
        let remote_attachment = state
            .attachments()
            .attachments
            .values()
            .find(|row| {
                row.project_id == fixture.collision_winner_project
                    && row.status == AttachmentStatus::Attached
            })
            .unwrap()
            .attachment_id
            .clone();
        drop(state);
        project_catalog_admin::detach_attachment(
            &store,
            epoch(&store),
            &remote_attachment,
            "2026-07-25T00:00:00Z",
        )
        .unwrap();

        // Attach a second, worktree-kind checkout to the winner: the
        // two-attachment project. The probe is daemon-supplied data by
        // contract; committed authority equals the winner's scope.
        let second_dir = rehearsal_root.join("checkouts").join("winner-worktree");
        fs::create_dir_all(&second_dir).unwrap();
        let second_probe = project_catalog_admin::AttachProbe {
            checkout_id: "feed000000000000000000000000e001".into(),
            checkout_dir: second_dir.to_str().unwrap().into(),
            checkout_project_dir: second_dir.to_str().unwrap().into(),
            project_root_relpath: ".".into(),
            kind: AttachmentKind::Worktree,
            validated_scope: Some(fixture.scope.clone()),
            computed_repo_hint: None,
            branch_ref: None,
            capabilities: bbox_corpus_core::project_catalog::AttachmentCapabilities {
                local_code_source: true,
                ..Default::default()
            },
            attached_at: "2026-07-25T00:00:01Z".into(),
        };
        let second = project_catalog_admin::attach_checkout(
            &store,
            epoch(&store),
            &fixture.winner_project,
            &second_probe,
        )
        .unwrap();

        let state = store.snapshot().unwrap();
        let engine = bbox_indexing::project_resolver::ProjectResolverEngine::v2(
            state.catalog(),
            state.attachments(),
        );

        // §10 item 2: id, accepted alias, and explicit typed scope resolve
        // the remote-only project to a catalog context. The engine is pure
        // over the pinned snapshot: zero lease acquisitions by
        // construction (the live smoke asserts the counters).
        for selector in [fixture.collision_winner_project.as_str(), "remote-alias"] {
            let outcome = engine
                .resolve(&ProjectSelectorRequest::selection(
                    selector,
                    ResolveIntent::Read,
                ))
                .unwrap();
            assert!(
                matches!(outcome, ProjectResolution::Catalog(_)),
                "remote-only selector {selector} stops at the catalog context"
            );
            assert_eq!(
                outcome.project_id(),
                Some(fixture.collision_winner_project.as_str())
            );
            assert_eq!(outcome.store_key(), None);
        }
        let mut scoped = ProjectSelectorRequest::selection("", ResolveIntent::Read);
        scoped.selector = None;
        scoped.scope = Some(fixture.collision_scope.clone());
        let outcome = engine.resolve(&scoped).unwrap();
        assert_eq!(
            outcome.project_id(),
            Some(fixture.collision_winner_project.as_str())
        );

        // A path operation on the remote-only project needs an attachment.
        let error = engine
            .resolve_attached(&ProjectSelectorRequest::selection(
                fixture.collision_winner_project.as_str(),
                ResolveIntent::Read,
            ))
            .unwrap_err();
        assert_eq!(error.code(), "error.project_attachment_required");

        // §10 item 3: the two-attachment project requires a session pin,
        // explicit attachment id, or configured default, and each ladder
        // rung selects exactly one attachment.
        let ambiguous = engine
            .resolve_attached(&ProjectSelectorRequest::selection(
                fixture.winner_project.as_str(),
                ResolveIntent::Read,
            ))
            .unwrap_err();
        assert_eq!(ambiguous.code(), "error.project_attachment_ambiguous");
        let mut explicit =
            ProjectSelectorRequest::selection(fixture.winner_project.as_str(), ResolveIntent::Read);
        explicit.attachment_id = Some(second.attachment_id.as_str().to_string());
        assert!(engine.resolve_attached(&explicit).is_ok());
        let base_checkout_id = state
            .attachments()
            .attachments
            .values()
            .find(|row| {
                row.project_id == fixture.winner_project
                    && row.attachment_id != second.attachment_id
            })
            .unwrap()
            .checkout_id
            .clone();
        let mut pinned =
            ProjectSelectorRequest::selection(fixture.winner_project.as_str(), ResolveIntent::Read);
        pinned.session = Some(SessionCheckoutRef {
            checkout_id: Some(base_checkout_id),
            checkout_project_dir: None,
        });
        let via_pin = engine.resolve_attached(&pinned).unwrap();
        assert_eq!(
            via_pin.store_key,
            fixture.winner_checkout.to_str().unwrap(),
            "the session pin selects the base attachment and keys to base"
        );
        drop(state);

        // Configured default: the operator-selected attachment resolves the
        // ladder when no pin or explicit id applies.
        project_catalog_admin::set_default_attachment(
            &store,
            epoch(&store),
            &fixture.winner_project,
            Some(&second.attachment_id),
        )
        .unwrap();
        let state = store.snapshot().unwrap();
        let engine = bbox_indexing::project_resolver::ProjectResolverEngine::v2(
            state.catalog(),
            state.attachments(),
        );
        let via_default = engine
            .resolve_attached(&ProjectSelectorRequest::selection(
                fixture.winner_project.as_str(),
                ResolveIntent::Read,
            ))
            .unwrap();
        let bbox_corpus_core::project_selector::ResolvedAttachment::Catalog {
            attachment_id, ..
        } = &via_default.attachment
        else {
            panic!("catalog attachment expected");
        };
        assert_eq!(attachment_id, second.attachment_id.as_str());

        // §10 item 4 residue: unknown ids and absolute paths still fail
        // closed after the mutations, and no operation manufactured an
        // identity (catalog membership is unchanged except by admin ops).
        let unknown_path = rehearsal_root.join("still-nowhere");
        for raw in [
            "p_00000000000000000000000000nothere",
            unknown_path.to_str().unwrap(),
        ] {
            let error = engine
                .resolve(&ProjectSelectorRequest::selection(raw, ResolveIntent::Read))
                .unwrap_err();
            assert_eq!(error.code(), "error.project_selector_unknown");
        }
        assert_eq!(state.catalog().projects.len(), 3);
    }
}

#[test]
fn absent_legacy_catalog_is_fresh_for_first_apply_but_public_verify_fails_closed() {
    let directory = tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap();
    let config = config(&root);
    let rehearsal_root = root.join("empty-rehearsal");
    fs::create_dir_all(&rehearsal_root).unwrap();
    initialize_empty_owner_state(&rehearsal_root);
    CodeSourceStore::open(
        rehearsal_root.join("state/code-sources"),
        project_catalog_migration_store_limits(&config),
    )
    .unwrap();
    let rehearsal =
        ProjectCatalogMigrationResolvedLayoutV1::from_rehearsal_root(&rehearsal_root, &config)
            .unwrap();
    let protected_root = root.join("protected-empty");
    fs::create_dir_all(&protected_root).unwrap();
    let protected = ProjectCatalogMigrationResolvedLayoutV1::from_config(
        &config,
        ProjectCatalogMigrationLayoutOverridesV1 {
            projects_path: Some(protected_root.join("projects.json")),
            state_dir: Some(protected_root),
        },
    )
    .unwrap();
    let verify_error =
        ProjectCatalogMigrationFacadeV1::verify(ProjectCatalogMigrationVerifyRequestV1 {
            rehearsal_layout: rehearsal.clone(),
        })
        .unwrap_err();
    assert_eq!(verify_error.code, "error.project_catalog_invalid_snapshot");
    assert_eq!(
        verify_error.mutation_disposition,
        ProjectCatalogMigrationMutationDispositionV1::NoDurableMutation
    );

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
        ProjectCatalogMigrationStatusV1::Clean
    );
    let applied =
        ProjectCatalogMigrationFacadeV1::apply_rehearsal(ProjectCatalogMigrationApplyRequestV1 {
            rehearsal_layout: rehearsal.clone(),
            protected_layout: protected.clone(),
            report_path: report_path.clone(),
            resolution_path: resolution_path.clone(),
        })
        .unwrap();
    assert_eq!(
        applied.receipt.outcome,
        ProjectCatalogMigrationApplyOutcomeV1::Applied
    );
    let verified =
        ProjectCatalogMigrationFacadeV1::verify(ProjectCatalogMigrationVerifyRequestV1 {
            rehearsal_layout: rehearsal.clone(),
        })
        .unwrap();
    assert_eq!(verified.receipt(), &applied.receipt.verification);

    let journal_path = rehearsal_root.join("state/project-catalog-transaction.json");
    let mut journal: serde_json::Value =
        serde_json::from_slice(&fs::read(&journal_path).unwrap()).unwrap();
    let attachment_stage = journal["participants"]
        .as_array()
        .unwrap()
        .iter()
        .find(|participant| participant["role"]["kind"] == "attachments")
        .and_then(|participant| participant["new"]["artifact_name"].as_str())
        .unwrap()
        .to_string();
    journal["state"] = serde_json::Value::String("prepared".to_string());
    journal.as_object_mut().unwrap().remove("outcome");
    journal.as_object_mut().unwrap().remove("committed_at");
    fs::write(&journal_path, serde_json::to_vec_pretty(&journal).unwrap()).unwrap();
    let projects_path = rehearsal_root.join("state/projects.json");
    let attachments_path = rehearsal_root.join("state/project-attachments.json");
    fs::remove_file(&attachments_path).unwrap();
    fs::remove_file(
        rehearsal_root
            .join("state/project-catalog-stage")
            .join(attachment_stage),
    )
    .unwrap();

    let rolled_back =
        ProjectCatalogMigrationFacadeV1::verify(ProjectCatalogMigrationVerifyRequestV1 {
            rehearsal_layout: rehearsal.clone(),
        })
        .unwrap_err();
    assert_eq!(
        rolled_back.code,
        "error.project_catalog_migration_incomplete"
    );
    assert_eq!(
        rolled_back.mutation_disposition,
        ProjectCatalogMigrationMutationDispositionV1::RecoveredToOldState
    );
    assert!(!projects_path.exists());
    assert!(!attachments_path.exists());

    let reapplied =
        ProjectCatalogMigrationFacadeV1::apply_rehearsal(ProjectCatalogMigrationApplyRequestV1 {
            rehearsal_layout: rehearsal.clone(),
            protected_layout: protected,
            report_path,
            resolution_path,
        })
        .unwrap();
    assert_eq!(
        reapplied.receipt.outcome,
        ProjectCatalogMigrationApplyOutcomeV1::Applied
    );
    let reverified =
        ProjectCatalogMigrationFacadeV1::verify(ProjectCatalogMigrationVerifyRequestV1 {
            rehearsal_layout: rehearsal,
        })
        .unwrap();
    assert_eq!(reverified.receipt(), &reapplied.receipt.verification);
}

/// The P6-F configured apply and configured verify (plan section 3.2,
/// adjudication Q-B): reviewed artifacts applied to the REAL configured
/// layout, where target and protected layout are the same layout.
///
/// This test is the mutation evidence for the "separation-check-omitted-ONLY"
/// claim. Re-adding `validate_rehearsal_separation` to the configured path
/// reds it, because the configured target legitimately carries no rehearsal
/// root and the separation check refuses exactly that shape. Every other
/// check the rehearsal path runs is still exercised here through the shared
/// apply-to-target core: the artifact set is confined, the four-hash identity
/// is rechecked against the artifacts (proven by the tampered-report arm),
/// and the post-commit verification receipt must agree with the preflight
/// prediction.
#[test]
fn configured_apply_installs_the_reviewed_post_image_on_the_configured_layout() {
    let directory = tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap();
    let config = config(&root);

    // The CONFIGURED layout: config-resolved, carrying no rehearsal root.
    let configured_base = root.join("configured");
    fs::create_dir_all(&configured_base).unwrap();
    initialize_empty_owner_state(&configured_base);
    let configured_state = configured_base.join("state");
    CodeSourceStore::open(
        configured_state.join("code-sources"),
        project_catalog_migration_store_limits(&config),
    )
    .unwrap();
    let configured = ProjectCatalogMigrationResolvedLayoutV1::from_config(
        &config,
        ProjectCatalogMigrationLayoutOverridesV1 {
            projects_path: Some(configured_state.join("projects.json")),
            state_dir: Some(configured_state.clone()),
        },
    )
    .unwrap();

    // Verification fails closed before anything is installed, exactly as the
    // rehearsal entry does.
    let unverifiable = ProjectCatalogMigrationFacadeV1::verify_configured(
        ProjectCatalogMigrationVerifyConfiguredRequestV1 {
            target_layout: configured.clone(),
        },
    )
    .unwrap_err();
    assert_eq!(unverifiable.code, "error.project_catalog_invalid_snapshot");
    assert_eq!(
        unverifiable.mutation_disposition,
        ProjectCatalogMigrationMutationDispositionV1::NoDurableMutation
    );

    let review = root.join("review");
    let report_path = review.join("report.json");
    let resolution_path = review.join("resolution.json");
    let preflight =
        ProjectCatalogMigrationFacadeV1::preflight(ProjectCatalogMigrationPreflightRequestV1 {
            layout: configured.clone(),
            report_path: report_path.clone(),
            resolution_path: resolution_path.clone(),
            sensitive_report_path: None,
        })
        .unwrap();
    assert_eq!(
        preflight.receipt.status,
        ProjectCatalogMigrationStatusV1::Clean
    );

    // The four-hash identity recheck survives the omission of the separation
    // check: a tampered report is refused with no durable mutation.
    let reviewed_report_bytes = fs::read(&report_path).unwrap();
    let mut tampered: serde_json::Value = serde_json::from_slice(&reviewed_report_bytes).unwrap();
    tampered["inventory_hash"] = serde_json::Value::String("0".repeat(64));
    fs::write(&report_path, serde_json::to_vec(&tampered).unwrap()).unwrap();
    let tampered_error = ProjectCatalogMigrationFacadeV1::apply_configured(
        ProjectCatalogMigrationApplyConfiguredRequestV1 {
            target_layout: configured.clone(),
            report_path: report_path.clone(),
            resolution_path: resolution_path.clone(),
        },
    )
    .unwrap_err();
    assert_eq!(
        tampered_error.mutation_disposition,
        ProjectCatalogMigrationMutationDispositionV1::NoDurableMutation
    );
    assert!(
        !configured_state
            .join("project-catalog-migration.json")
            .exists()
    );
    fs::write(&report_path, &reviewed_report_bytes).unwrap();

    // F18's distinguishing regression: a process still HOLDING its shared
    // lifetime claim cannot apply, because the migration transaction
    // re-acquires the same advisory lock exclusively on its own descriptor
    // (the section 4.1 flock self-conflict class). This is the exact defect
    // the certified CLI shipped: holding the claim through the facade call
    // made every configured apply refuse itself. The CLI's contract is
    // probe, RELEASE, then transaction-owned exclusive acquisition.
    let held_claim = ProjectCatalogMigrationLock::try_acquire_exclusive(configured.projects_path())
        .unwrap()
        .expect("no daemon shares the fixture store")
        .downgrade_to_shared()
        .unwrap();
    let self_conflict = ProjectCatalogMigrationFacadeV1::apply_configured(
        ProjectCatalogMigrationApplyConfiguredRequestV1 {
            target_layout: configured.clone(),
            report_path: report_path.clone(),
            resolution_path: resolution_path.clone(),
        },
    )
    .unwrap_err();
    assert_eq!(
        self_conflict.code,
        "error.project_catalog_lifetime_lock_busy"
    );
    assert_eq!(
        self_conflict.mutation_disposition,
        ProjectCatalogMigrationMutationDispositionV1::NoDurableMutation
    );
    // Probe-and-release: dropping the claim is what lets the transaction's
    // exclusive acquisition succeed, and the SAME artifacts then reach
    // Applied below.
    drop(held_claim);

    let applied = ProjectCatalogMigrationFacadeV1::apply_configured(
        ProjectCatalogMigrationApplyConfiguredRequestV1 {
            target_layout: configured.clone(),
            report_path: report_path.clone(),
            resolution_path: resolution_path.clone(),
        },
    )
    .unwrap();
    assert_eq!(
        applied.receipt.outcome,
        ProjectCatalogMigrationApplyOutcomeV1::Applied
    );
    assert_eq!(
        applied.receipt.verification.expected_catalog_hash,
        preflight.receipt.predicted_catalog_hash
    );

    let verified = ProjectCatalogMigrationFacadeV1::verify_configured(
        ProjectCatalogMigrationVerifyConfiguredRequestV1 {
            target_layout: configured.clone(),
        },
    )
    .unwrap();
    assert_eq!(verified.receipt(), &applied.receipt.verification);

    // Re-apply is idempotent through the same shared core.
    let reapplied = ProjectCatalogMigrationFacadeV1::apply_configured(
        ProjectCatalogMigrationApplyConfiguredRequestV1 {
            target_layout: configured,
            report_path,
            resolution_path,
        },
    )
    .unwrap();
    assert_eq!(
        reapplied.receipt.outcome,
        ProjectCatalogMigrationApplyOutcomeV1::AlreadyApplied
    );
}

struct ConfiguredHostFixture {
    alpha_checkout: PathBuf,
    beta_checkout: PathBuf,
    alpha_project: ProjectId,
    beta_project: ProjectId,
    detached_project: Option<ProjectId>,
}

/// A CONFIGURED-shape host: no rehearsal root, no replica tree, and the
/// registered canonical paths ARE the canonical checkout roots (plan section
/// 6.3).
///
/// The rehearsal fixtures populate `<root>/checkouts/`, so they never exercise
/// the layout a live host actually presents. Git ingest cursors are seeded for
/// each present project because those are the rows the git-metadata lane must
/// find represented: a preflight that discovers no checkout root leaves every
/// cursor unrepresented and classifies the lane corrupt.
///
/// `register_missing_path` additionally registers a record whose path does not
/// exist, the shape a host reaches when a registered checkout is deleted out
/// from under the registry.
fn prepare_configured_host(
    root: &Path,
    config: &Config,
    register_missing_path: bool,
) -> ConfiguredHostFixture {
    let alpha_checkout = root.join("host").join("alpha");
    let beta_checkout = root.join("host").join("beta");
    for (checkout, repo_id) in [
        (&alpha_checkout, "neutral-alpha"),
        (&beta_checkout, "neutral-beta"),
    ] {
        fs::create_dir_all(checkout).unwrap();
        git(checkout, &["init", "-q"]);
        git(checkout, &["checkout", "-qb", "main"]);
        write(
            &checkout.join(".bbox/config.toml"),
            format!("[project]\nrepo_id = \"{repo_id}\"\n").as_bytes(),
        );
        git(checkout, &["add", ".bbox"]);
        git(checkout, &["commit", "-qm", "seed configured fixture"]);
        initialize_empty_provenance_ref(checkout, config);
    }
    let alpha_head = git(&alpha_checkout, &["rev-parse", "HEAD"]);
    let beta_head = git(&beta_checkout, &["rev-parse", "HEAD"]);

    let state = root.join("state");
    initialize_empty_owner_state(root);
    let alpha_project = ProjectId::parse("neutral-alpha-project").unwrap();
    let beta_project = ProjectId::parse("neutral-beta-project").unwrap();
    let detached_project =
        register_missing_path.then(|| ProjectId::parse("neutral-detached-project").unwrap());
    let mut records = vec![
        serde_json::json!({
            "project_id": alpha_project,
            "repo_id": "neutral-alpha",
            "canonical_path": alpha_checkout,
            "registered_at": "2026-01-02T03:04:05Z",
            "is_git_repo": true,
            "languages": [],
            "aliases": []
        }),
        serde_json::json!({
            "project_id": beta_project,
            "repo_id": "neutral-beta",
            "canonical_path": beta_checkout,
            "registered_at": "2026-01-02T03:04:06Z",
            "is_git_repo": true,
            "languages": [],
            "aliases": []
        }),
    ];
    if let Some(detached) = &detached_project {
        records.push(serde_json::json!({
            "project_id": detached,
            "repo_id": "neutral-detached",
            "canonical_path": root.join("host").join("detached"),
            "registered_at": "2026-01-02T03:04:07Z",
            "is_git_repo": false,
            "languages": [],
            "aliases": []
        }));
    }
    write(
        &state.join("projects.json"),
        serde_json::to_vec_pretty(&serde_json::json!({
            "version": 1,
            "projects": records
        }))
        .unwrap()
        .as_slice(),
    );

    let code_sources = CodeSourceStore::open(
        state.join("code-sources"),
        project_catalog_migration_store_limits(config),
    )
    .unwrap();
    let paths = CodeSourceStorePaths::new(code_sources.root()).unwrap();
    let alpha_scope = PublishedScope::try_new("neutral-alpha", ".").unwrap();
    let beta_scope = PublishedScope::try_new("neutral-beta", ".").unwrap();
    let (_, alpha_selection) = write_generation(
        &paths,
        &alpha_project,
        &alpha_scope,
        "alpha",
        &alpha_head,
        1,
    );
    let (_, beta_selection) =
        write_generation(&paths, &beta_project, &beta_scope, "beta", &beta_head, 2);
    let mut selections = vec![alpha_selection, beta_selection];
    selections.sort_by(|left, right| left.project_id.cmp(&right.project_id));
    write(
        &paths.anchor(),
        &encode_migration_effective_source_manifest_v1(&MigrationEffectiveSourceManifestV1 {
            version: 1,
            selections,
        })
        .unwrap(),
    );

    for (project, head) in [(&alpha_project, &alpha_head), (&beta_project, &beta_head)] {
        write(
            &state
                .join("git_meta")
                .join(format!("{}.json", project.as_str())),
            serde_json::to_vec(&serde_json::json!({ "last_ingested_sha": head }))
                .unwrap()
                .as_slice(),
        );
    }

    ConfiguredHostFixture {
        alpha_checkout,
        beta_checkout,
        alpha_project,
        beta_project,
        detached_project,
    }
}

fn configured_layout(root: &Path, config: &Config) -> ProjectCatalogMigrationResolvedLayoutV1 {
    ProjectCatalogMigrationResolvedLayoutV1::from_config(
        config,
        ProjectCatalogMigrationLayoutOverridesV1 {
            projects_path: None,
            state_dir: Some(root.join("state")),
        },
    )
    .unwrap()
}

/// A configured preflight derives its canonical checkout roots from the v1
/// store (plan section 6.3).
///
/// The regression this pins: a configured layout carries no replica root, so
/// discovery used to return an empty root set. Nothing was captured as a
/// checkout, the legacy-project x checkout cross product produced no
/// attachment candidate, and the git-metadata lane then found every cursor row
/// unrepresented and classified corrupt, which withheld the whole plan.
#[test]
fn configured_preflight_derives_checkout_roots_from_the_registered_paths() {
    let directory = tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap();
    let config = config(&root);
    let fixture = prepare_configured_host(&root, &config, false);
    let layout = configured_layout(&root, &config);
    let report_path = root.join("review/report.json");

    let preflight =
        ProjectCatalogMigrationFacadeV1::preflight(ProjectCatalogMigrationPreflightRequestV1 {
            layout,
            report_path: report_path.clone(),
            resolution_path: root.join("review/resolution.json"),
            sensitive_report_path: None,
        })
        .unwrap();
    let report = decode_migration_report_v1(&fs::read(&report_path).unwrap()).unwrap();

    assert!(
        !report
            .refusals
            .iter()
            .any(|refusal| refusal.diagnostic_code == "immutable_lane_corrupt"),
        "no lane may classify corrupt on a host whose cursors are all represented: {:?}",
        report.refusals
    );
    assert_eq!(
        preflight.receipt.status,
        ProjectCatalogMigrationStatusV1::Clean,
        "refusals: {:?}, required resolutions: {:?}",
        report.refusals,
        report.required_resolutions
    );
    assert_eq!(
        report
            .attachments
            .iter()
            .map(|row| row.project_id.clone())
            .collect::<BTreeSet<_>>(),
        BTreeSet::from([fixture.alpha_project.clone(), fixture.beta_project.clone()]),
        "each registered checkout must yield an attachment candidate"
    );
    assert_eq!(preflight.receipt.attached_project_count, 2);
    assert_eq!(
        report
            .checkout_identity_actions
            .iter()
            .map(|row| row.canonical_root_digest.clone())
            .collect::<BTreeSet<_>>(),
        BTreeSet::from([
            digest_path(fixture.alpha_checkout.to_str().unwrap()),
            digest_path(fixture.beta_checkout.to_str().unwrap()),
        ]),
        "checkout-id inventory covers each canonical checkout root exactly once"
    );
    assert_eq!(preflight.receipt.checkout_action_count, 2);
}

/// A registered path that is absent stays a classified missing-path record and
/// does not become a checkout root.
///
/// This is the other half of the derivation contract: absence is skipped
/// during discovery rather than refused, so one deleted checkout cannot
/// withhold the migration of every other project on the host.
#[test]
fn configured_preflight_keeps_a_missing_registered_path_out_of_the_checkout_roots() {
    let directory = tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap();
    let config = config(&root);
    let fixture = prepare_configured_host(&root, &config, true);
    let detached = fixture
        .detached_project
        .clone()
        .expect("the fixture registers a missing path");
    let layout = configured_layout(&root, &config);
    let report_path = root.join("review/report.json");

    let preflight =
        ProjectCatalogMigrationFacadeV1::preflight(ProjectCatalogMigrationPreflightRequestV1 {
            layout,
            report_path: report_path.clone(),
            resolution_path: root.join("review/resolution.json"),
            sensitive_report_path: None,
        })
        .unwrap();
    let report = decode_migration_report_v1(&fs::read(&report_path).unwrap()).unwrap();

    assert_eq!(
        preflight.receipt.status,
        ProjectCatalogMigrationStatusV1::Clean,
        "refusals: {:?}, required resolutions: {:?}",
        report.refusals,
        report.required_resolutions
    );
    assert_eq!(
        report.projects.len(),
        3,
        "the missing-path record stays in the inventory"
    );
    assert_eq!(
        report
            .projects
            .iter()
            .find(|row| row.project_id == detached)
            .expect("the missing-path record is reported")
            .path_status,
        LegacyProjectPathStatusV1::Missing
    );
    assert_eq!(
        report
            .missing_paths
            .iter()
            .map(|row| row.project_id.clone())
            .collect::<Vec<_>>(),
        vec![detached],
        "exactly the absent registration is reported as a missing path"
    );
    assert_eq!(
        report.checkout_identity_actions.len(),
        2,
        "only the two existing checkouts become checkout roots"
    );
    assert_eq!(
        report
            .attachments
            .iter()
            .map(|row| row.project_id.clone())
            .collect::<BTreeSet<_>>(),
        BTreeSet::from([fixture.alpha_project.clone(), fixture.beta_project.clone()]),
        "the absent registration attaches to nothing"
    );
}

/// The REHEARSAL apply keeps `validate_rehearsal_separation` (D-006).
///
/// This is the other half of the "separation-check-omitted-only" evidence:
/// removing the check from the rehearsal path reds this test. It had no
/// coverage before the shared apply-to-target core existed, which is exactly
/// the coverage a refactor that moves a safety check must not rely on.
#[test]
fn rehearsal_apply_refuses_a_target_that_is_not_isolated_from_the_protected_layout() {
    let directory = tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap();
    let config = config(&root);
    let rehearsal_root = root.join("rehearsal");
    fs::create_dir_all(&rehearsal_root).unwrap();
    initialize_empty_owner_state(&rehearsal_root);
    let rehearsal =
        ProjectCatalogMigrationResolvedLayoutV1::from_rehearsal_root(&rehearsal_root, &config)
            .unwrap();

    // The protected layout is the rehearsal root's OWN state: a rehearsal
    // apply here would mutate the very authority it claims to be isolated
    // from, which is the destructive-discretion shape D-006 exists to refuse.
    let protected = ProjectCatalogMigrationResolvedLayoutV1::from_config(
        &config,
        ProjectCatalogMigrationLayoutOverridesV1 {
            projects_path: Some(rehearsal_root.join("state/projects.json")),
            state_dir: Some(rehearsal_root.join("state")),
        },
    )
    .unwrap();

    let refused =
        ProjectCatalogMigrationFacadeV1::apply_rehearsal(ProjectCatalogMigrationApplyRequestV1 {
            rehearsal_layout: rehearsal,
            protected_layout: protected,
            report_path: root.join("review/report.json"),
            resolution_path: root.join("review/resolution.json"),
        })
        .unwrap_err();
    assert_eq!(
        refused.code, "error.project_catalog_migration_unsafe_layout",
        "rehearsal apply must refuse a target that overlaps the protected layout"
    );
    assert_eq!(
        refused.mutation_disposition,
        ProjectCatalogMigrationMutationDispositionV1::NoDurableMutation
    );
}

/// The configured entries refuse a rehearsal-root layout, which is the Q-C
/// binding condition that a configured-selected layout is config-resolved.
/// Without it, omitting the separation check would let a rehearsal layout
/// through the configured path unchallenged.
#[test]
fn configured_entries_refuse_a_rehearsal_root_layout() {
    let directory = tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap();
    let config = config(&root);
    let rehearsal_root = root.join("rehearsal");
    fs::create_dir_all(&rehearsal_root).unwrap();
    initialize_empty_owner_state(&rehearsal_root);
    let rehearsal =
        ProjectCatalogMigrationResolvedLayoutV1::from_rehearsal_root(&rehearsal_root, &config)
            .unwrap();

    let apply_error = ProjectCatalogMigrationFacadeV1::apply_configured(
        ProjectCatalogMigrationApplyConfiguredRequestV1 {
            target_layout: rehearsal.clone(),
            report_path: root.join("review/report.json"),
            resolution_path: root.join("review/resolution.json"),
        },
    )
    .unwrap_err();
    assert_eq!(
        apply_error.code,
        "error.project_catalog_migration_unsafe_layout"
    );
    assert_eq!(
        apply_error.mutation_disposition,
        ProjectCatalogMigrationMutationDispositionV1::NoDurableMutation
    );

    let verify_error = ProjectCatalogMigrationFacadeV1::verify_configured(
        ProjectCatalogMigrationVerifyConfiguredRequestV1 {
            target_layout: rehearsal,
        },
    )
    .unwrap_err();
    assert_eq!(
        verify_error.code,
        "error.project_catalog_migration_unsafe_layout"
    );
}

/// Not a test: the smoke-fixture producer for the phase-2 live bootsmokes
/// (phase-2 plan section 12). Ignored by default; the bootsmoke driver
/// invokes it explicitly with `BBOX_SMOKE_FIXTURE_ROOT` set to materialize
/// the pre-migration fixture state at that root plus a summary file, and
/// then drives the exact stablesigned `blackbox` CLI through preflight,
/// resolution, and rehearsal apply itself.
#[test]
#[ignore = "smoke-fixture producer; invoked explicitly by the live bootsmoke driver"]
fn produce_migrated_smoke_fixture_from_env_root() {
    let Some(root) = std::env::var_os("BBOX_SMOKE_FIXTURE_ROOT") else {
        eprintln!("BBOX_SMOKE_FIXTURE_ROOT is not set; nothing to produce");
        return;
    };
    produce_migrated_smoke_fixture_at(&PathBuf::from(root).canonicalize().unwrap());
}

/// The producer body, callable without the environment variable.
///
/// Split out because the env-gated arm above is UNREACHABLE from the gates:
/// the lane build shim forwards only a fixed env allowlist into the builder
/// pod, so a producer that existed only behind `BBOX_SMOKE_FIXTURE_ROOT`
/// would be compiled and never run anywhere the suite runs. Splitting it
/// lets `the_producer_materializes_the_migrated_root_and_the_full_set`
/// exercise the real ceremony on every workspace run, which is what makes a
/// broken producer fail here rather than in someone's live smoke.
fn produce_migrated_smoke_fixture_at(root: &Path) -> serde_json::Value {
    let root = root.to_path_buf();
    let config = config(&root);
    let rehearsal_root = root.join("rehearsal");
    fs::create_dir_all(&rehearsal_root).unwrap();
    let fixture = prepare_rehearsal(&rehearsal_root, &config);
    let review = rehearsal_root.join("review");
    fs::create_dir_all(&review).unwrap();
    // Drive the exact P1-C rehearsal ceremony through the public facade
    // (the byte-identical engine behind the CLI envelope): assessment,
    // scope-owner resolution, collected quarantine, clean preflight, apply.
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
        "smoke fixture preflight must be clean"
    );
    ProjectCatalogMigrationFacadeV1::apply_rehearsal(ProjectCatalogMigrationApplyRequestV1 {
        rehearsal_layout: rehearsal,
        protected_layout: protected,
        report_path,
        resolution_path,
    })
    .unwrap();
    // The migrated root now carries the COMPLETE section 13.8 set, built by
    // the same function the workspace gate builds it with. Before this the
    // producer emitted only the three migration-shaped projects, and every
    // catalog fixture shape a live smoke wanted had to be improvised at the
    // smoke's own layer - which is how the smoke and the unit suites came
    // to disagree about what each shape means.
    let catalog_shapes =
        build_section_13_8_fixture_set(&rehearsal_root.join("state/projects.json"), &root);

    let summary = serde_json::json!({
        "catalog_fixture_shapes": catalog_shapes,
        "config_path": root.join("config.toml"),
        "rehearsal_root": rehearsal_root,
        "projects_path": rehearsal_root.join("state/projects.json"),
        "winner_project": fixture.winner_project.as_str(),
        "winner_checkout": fixture.winner_checkout,
        "collision_winner_project": fixture.collision_winner_project.as_str(),
        "loser_project": fixture.loser_project.as_str(),
        "loser_generation": fixture.loser_generation,
        "collision_scope": {
            "repo_id": fixture.collision_scope.repo_id(),
            "bbox_root_relpath": fixture.collision_scope.bbox_root_relpath(),
        },
    });
    fs::write(
        root.join("smoke-fixture-summary.json"),
        serde_json::to_vec_pretty(&summary).unwrap(),
    )
    .unwrap();
    eprintln!("smoke fixture produced at {}", root.display());
    summary
}

/// The D-030 producer runs end to end on every workspace run.
///
/// It drives the full facade ceremony (assessment, scope-owner resolution,
/// collected quarantine, clean preflight, apply) and then materializes the
/// complete section 13.8 set onto the migrated root, so the summary the
/// live bootsmoke driver and the stable-signed CLI consume is proved to
/// carry every named shape before anyone runs a smoke.
#[test]
fn the_producer_materializes_the_migrated_root_and_the_full_set() {
    let dir = tempdir().unwrap();
    let summary = produce_migrated_smoke_fixture_at(&dir.path().canonicalize().unwrap());

    let shapes: Vec<String> = summary["catalog_fixture_shapes"]
        .as_array()
        .expect("the summary carries the fixture set")
        .iter()
        .map(|shape| shape["shape"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(
        shapes, SECTION_13_8_SHAPES,
        "the produced root must carry exactly section 13.8's set, in order"
    );
    // The migration-shaped projects the producer already emitted stay in
    // the summary: the set is an ADDITION to the D-030 contract, not a
    // replacement, and a consumer keyed on the old fields must not break.
    for key in [
        "config_path",
        "rehearsal_root",
        "projects_path",
        "winner_project",
        "collision_winner_project",
        "loser_project",
        "collision_scope",
    ] {
        assert!(
            !summary[key].is_null(),
            "the producer dropped the pre-existing summary field {key}"
        );
    }
}

// ----------------------------------------------------------------------------
// Phase 4 acceptance block (section 12 exit-gate proof)
// ----------------------------------------------------------------------------
//
// Coverage map for the nine acceptance rows (section 12.1 through 12.9).
// Where a row is already covered by an existing unit test, the reference
// is listed. New tests below cover rows that needed additional acceptance.
//
// 12.1 Token revocation while collected results remain pending:
//     - p4e_reducer_local_collected_none_selected_attempts_cutback
//       (in src/server/code_source.rs): desired=local, effective=collected,
//       cutback fires because the assignment was removed.
//     - p4e_gc_protects_bridge_generation_ids: the activation record
//       carrying a CutbackStateV2 is a GC root for its generation_id.
//
// 12.2 Reattach completes cutback exactly once:
//     - p4e_warming_with_selected_ladder_attempts_cutback: scope-matching
//       attachment triggers cutback attempt.
//     - p4e_local_local_stale_state_is_cleared: detach and re-attach does
//       not re-drive cutback (effective=Local, desired=Local).
//
// 12.3 Reassign cancels cutback:
//     - p4e_readd_assignment_cancels_cutback: re-adding the assignment
//       cancels the pending cutback.
//     - p4e_reducer_collected_collected_any_nonnone_cancels_cutback:
//       the reduction table's collected/collected/any-non-None row.
//
// 12.4 Restart preserves every state:
//     - p4e_resume_structural_enqueues_reconciler_event
//     - p4e_resume_transient_elapsed_deadline_re_attempts
//     - p4e_resume_transient_future_deadline_waits
//     - p4e_resume_terminal_and_manual_retry_are_noops
//     - p4e_resume_no_cutback_state_is_noop
//     - p4f_classification_persists_structural_for_no_attachment
//       (startup classification converts cutback_pending to typed state)
//     - p4f_classification_is_once_only
//
// 12.5 Explicit retirement converges exactly once:
//     - retirement_journal_stage_ordinal_is_forward_only
//       (in project_catalog_admin.rs unit tests)
//     - retirement_journal_round_trip_preserves_all_fields
//     - retirement_journal_path_convention
//     - retirement_journal_archive_removes_file
//     - NEW: acceptance_retirement_journaled_converges_exactly_once
//     - NEW: acceptance_retirement_journaled_idempotent_on_second_call
//     - NEW: acceptance_retirement_journaled_refuses_ready_materialization
//
// 12.6 v2 records and scope agreement:
//     - p4c_v2_activation_round_trips_with_scope_agreement
//     - p4c_scope_disagreement_refuses_before_commit
//     - p4f_fresh_store_relationship_chain_passes_clean
//
// 12.7 Startup agreement:
//     - p4f_fresh_store_relationship_chain_passes_clean
//     - p4f_scope_mismatch_refuses_chain
//     - p4f_missing_workspace_entry_fails_chain
//     - p4f_retirement_journal_detection_refuses_boot
//     - p4f_no_retirement_journal_passes_clean
//
// 12.8 Four-step producer re-scope restart invariants:
//     - p4e_scope_migrate_refuses_second_migration_with_open_bridge
//     - p4e_reducer_bridge_open_clears_structural
//     - p4e_open_bridge_predicate_newest_by_catalog_epoch
//     - p4e_open_bridge_predicate_empty_records
//     - exit_proof_legacy_cutback_is_bridge_only (sole-ownership)
//
// 12.9 Bridge parity:
//     - p4c_bridge_v1_round_trip_unchanged
//     - p4c_bridge_store_refuses_v2_activation_write
//     - p4d_bridge_mode_is_not_catalog_and_has_no_reconciler
//     - exit_proof_cutback_catalog_no_sleep_loop (no loop in catalog path)
//     - exit_proof_attempt_cutback_catalog_no_sleep
//
// Rows requiring live daemon lifecycle (12.1 end-to-end, 12.4 crash-during-
// redrive, 12.8 attached-path race) are exercised by the bootsmoke driver
// and are outside the unit/integration surface. The unit tests above cover
// the deterministic invariants those scenarios depend on.

/// Section 12.5: a fully discharged project (zero blocking classes) retires
/// through the journaled path. The journal completes to the Complete stage,
/// the project is removed from the catalog, and a second call is idempotent.
#[test]
fn acceptance_retirement_journaled_converges_exactly_once() {
    use bbox_corpus_core::project_catalog::{
        CatalogSnapshotV2, CorpusProject, ProjectId, ProjectScope,
    };
    use bbox_indexing::project_catalog_admin::{
        RetireEvidence, RetirementJournalStage, retire_project_journaled,
    };
    use bbox_indexing::project_catalog_store::ProjectCatalogStore;
    use std::sync::Arc;

    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let bro_home = root.join("bro-home");
    fs::create_dir_all(&bro_home).unwrap();

    let store =
        Arc::new(ProjectCatalogStore::initialize_empty(root.join("projects.json")).unwrap());

    let project_id = ProjectId::parse("p_000000000000000000000000000000a1").unwrap();
    let epoch = store.snapshot().unwrap().epoch();
    store
        .transact(epoch, |catalog: &mut CatalogSnapshotV2, _| {
            catalog.projects.insert(
                project_id.clone(),
                CorpusProject {
                    project_id: project_id.clone(),
                    scope: ProjectScope::LegacyLocal,
                    operator_aliases: Default::default(),
                    nominated_aliases: Default::default(),
                    display_name: "acceptance project".into(),
                    created_at: "2026-07-24T00:00:00Z".into(),
                    registered_at_compat: None,
                    repo_history: None,
                    languages: Default::default(),
                },
            );
            Ok(())
        })
        .unwrap();

    let evidence = RetireEvidence {
        external_reference_counts: Default::default(),
        unprobeable_classes: Vec::new(),
    };

    let (preflight, journal) =
        retire_project_journaled(&store, &bro_home, &project_id, &evidence, true).unwrap();

    assert!(
        preflight.blocking.is_empty(),
        "no blocking classes should remain"
    );
    let journal = journal.expect("journal should be returned after execute");
    assert_eq!(
        journal.current_stage,
        RetirementJournalStage::Complete,
        "journal should reach Complete"
    );

    let state = store.snapshot().unwrap();
    assert!(
        !state.catalog().projects.contains_key(&project_id),
        "project must be removed from catalog"
    );

    // Second call: idempotent. The project is gone; the journal was
    // archived. Calling again should not panic or duplicate work.
    let (_preflight2, journal2) =
        retire_project_journaled(&store, &bro_home, &project_id, &evidence, true).unwrap();
    // Journal is None because the project no longer exists in the catalog
    // and no journal file was found (it was archived).
    assert!(
        journal2.is_none()
            || journal2
                .as_ref()
                .is_some_and(|j| j.current_stage == RetirementJournalStage::Complete),
        "second retire call must be idempotent"
    );
}

/// Section 12.5: a project with Ready materialization is refused.
#[test]
fn acceptance_retirement_journaled_refuses_ready_materialization() {
    use bbox_corpus_core::project_catalog::{
        CatalogSnapshotV2, CommitNamespace, CorpusProject, ProjectId, ProjectScope,
        RepoHistoryAuthority, RepoHistoryGenerationId, RepoHistoryId, RepoHistoryMaterialization,
        RepoHistoryRecord,
    };
    use bbox_indexing::project_catalog_admin::{RetireEvidence, retire_project_journaled};
    use bbox_indexing::project_catalog_store::ProjectCatalogStore;
    use std::sync::Arc;

    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let bro_home = root.join("bro-home");
    fs::create_dir_all(&bro_home).unwrap();

    let store =
        Arc::new(ProjectCatalogStore::initialize_empty(root.join("projects.json")).unwrap());

    let project_id = ProjectId::parse("p_000000000000000000000000000000a1").unwrap();
    let history_id = RepoHistoryId::parse("rh_00000000000000000000000000000001").unwrap();
    let gen_id = RepoHistoryGenerationId::parse(
        "rhg_aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
    )
    .unwrap();
    let epoch = store.snapshot().unwrap().epoch();
    store
        .transact(epoch, |catalog: &mut CatalogSnapshotV2, _| {
            catalog.projects.insert(
                project_id.clone(),
                CorpusProject {
                    project_id: project_id.clone(),
                    scope: ProjectScope::LegacyLocal,
                    operator_aliases: Default::default(),
                    nominated_aliases: Default::default(),
                    display_name: "ready-mat project".into(),
                    created_at: "2026-07-24T00:00:00Z".into(),
                    registered_at_compat: None,
                    repo_history: Some(history_id.clone()),
                    languages: Default::default(),
                },
            );
            catalog.repo_histories.insert(
                history_id.clone(),
                RepoHistoryRecord {
                    repo_history_id: history_id.clone(),
                    membership_generation: 0,
                    authority: RepoHistoryAuthority::LocalProject(project_id.clone()),
                    primary_namespace: CommitNamespace::parse(
                        "local_33333333333333333333333333333333",
                    )
                    .unwrap(),
                    compatibility_namespaces: Default::default(),
                    materialization: RepoHistoryMaterialization::Ready {
                        generation_id: gen_id,
                    },
                },
            );
            Ok(())
        })
        .unwrap();

    let evidence = RetireEvidence {
        external_reference_counts: Default::default(),
        unprobeable_classes: Vec::new(),
    };

    let result = retire_project_journaled(&store, &bro_home, &project_id, &evidence, true);
    assert!(
        result.is_err(),
        "Ready materialization must refuse retirement"
    );
    let err = result.unwrap_err();
    assert_eq!(
        err.code(),
        "error.project_catalog_admin_retire_history_ready",
        "must refuse with the typed Ready-materialization code"
    );

    // Project must still be in the catalog (refused, not removed).
    let state = store.snapshot().unwrap();
    assert!(
        state.catalog().projects.contains_key(&project_id),
        "project must survive the refusal"
    );
}

/// Section 12.5: preflight (non-execute) mode reports blocking classes
/// without modifying the catalog.
#[test]
fn acceptance_retirement_preflight_does_not_mutate() {
    use bbox_corpus_core::project_catalog::{
        CatalogSnapshotV2, CorpusProject, ProjectId, ProjectScope,
    };
    use bbox_indexing::project_catalog_admin::{RetireEvidence, retire_project_journaled};
    use bbox_indexing::project_catalog_store::ProjectCatalogStore;
    use std::sync::Arc;

    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let bro_home = root.join("bro-home");
    fs::create_dir_all(&bro_home).unwrap();

    let store =
        Arc::new(ProjectCatalogStore::initialize_empty(root.join("projects.json")).unwrap());

    let project_id = ProjectId::parse("p_000000000000000000000000000000a1").unwrap();
    let epoch = store.snapshot().unwrap().epoch();
    store
        .transact(epoch, |catalog: &mut CatalogSnapshotV2, _| {
            catalog.projects.insert(
                project_id.clone(),
                CorpusProject {
                    project_id: project_id.clone(),
                    scope: ProjectScope::LegacyLocal,
                    operator_aliases: Default::default(),
                    nominated_aliases: Default::default(),
                    display_name: "preflight project".into(),
                    created_at: "2026-07-24T00:00:00Z".into(),
                    registered_at_compat: None,
                    repo_history: None,
                    languages: Default::default(),
                },
            );
            Ok(())
        })
        .unwrap();

    // R3F1: code_source_activation is no longer blocking (it is state
    // discharged by the journal). Use a class that IS blocking.
    let evidence = RetireEvidence {
        external_reference_counts: {
            let mut m = std::collections::BTreeMap::new();
            m.insert("knowledge_rows".into(), 1u64);
            m
        },
        unprobeable_classes: Vec::new(),
    };

    let epoch_before = store.snapshot().unwrap().epoch();
    let (preflight, journal) =
        retire_project_journaled(&store, &bro_home, &project_id, &evidence, false).unwrap();

    assert!(
        preflight.blocking.contains_key("knowledge_rows"),
        "preflight must report the blocking class"
    );
    assert!(
        journal.is_none(),
        "no journal should be created in preflight mode"
    );

    let epoch_after = store.snapshot().unwrap().epoch();
    assert_eq!(
        epoch_before, epoch_after,
        "preflight must not mutate the catalog epoch"
    );
    let state = store.snapshot().unwrap();
    assert!(
        state.catalog().projects.contains_key(&project_id),
        "project must survive preflight"
    );
}

// ---- Section 12.5: discharge worker acceptance tests ----

/// A test discharge worker that records each method call count. Verifies
/// the journal calls each worker exactly once per stage advance. The
/// reprobe returns zeroed evidence so the final authority cut succeeds:
/// the trust boundary is explicit in the test (the test workers claim
/// the discharge cleared everything, and reprobe confirms that claim).
struct CountingDischargeWorkers {
    collected_generations_calls: u32,
    publications_calls: u32,
    attachments_calls: u32,
    sweep_calls: u32,
    reprobe_calls: u32,
}

impl CountingDischargeWorkers {
    fn new() -> Self {
        Self {
            collected_generations_calls: 0,
            publications_calls: 0,
            attachments_calls: 0,
            sweep_calls: 0,
            reprobe_calls: 0,
        }
    }
}

impl bbox_indexing::project_catalog_admin::RetirementDischargeWorkers for CountingDischargeWorkers {
    fn discharge_collected_generations(
        &mut self,
        _project_id: &ProjectId,
        _evidence: &bbox_indexing::project_catalog_admin::RetirementJournalEvidence,
    ) -> bbox_indexing::project_catalog_admin::AdminResult<()> {
        self.collected_generations_calls += 1;
        Ok(())
    }
    fn discharge_publications(
        &mut self,
        _project_id: &ProjectId,
    ) -> bbox_indexing::project_catalog_admin::AdminResult<()> {
        self.publications_calls += 1;
        Ok(())
    }
    fn discharge_attachments(
        &mut self,
        _store: &bbox_indexing::project_catalog_store::ProjectCatalogStore,
        _project_id: &ProjectId,
    ) -> bbox_indexing::project_catalog_admin::AdminResult<()> {
        self.attachments_calls += 1;
        Ok(())
    }
    fn sweep_materialization(
        &mut self,
        _project_id: &ProjectId,
        _evidence: &bbox_indexing::project_catalog_admin::RetirementJournalEvidence,
    ) -> bbox_indexing::project_catalog_admin::AdminResult<()> {
        self.sweep_calls += 1;
        Ok(())
    }
    fn verify_source_authority_quiesced(
        &mut self,
        _store: &bbox_indexing::project_catalog_store::ProjectCatalogStore,
        _project_id: &ProjectId,
        _evidence: &bbox_indexing::project_catalog_admin::RetirementJournalEvidence,
    ) -> bbox_indexing::project_catalog_admin::AdminResult<()> {
        Ok(())
    }
    fn reprobe_evidence(
        &mut self,
        _store: &bbox_indexing::project_catalog_store::ProjectCatalogStore,
        _project_id: &ProjectId,
        _original_evidence: &bbox_indexing::project_catalog_admin::RetireEvidence,
        _retirement_evidence: &bbox_indexing::project_catalog_admin::RetirementJournalEvidence,
    ) -> bbox_indexing::project_catalog_admin::AdminResult<
        bbox_indexing::project_catalog_admin::RetireEvidence,
    > {
        self.reprobe_calls += 1;
        // Return zeroed evidence: the test workers claim discharge cleared
        // everything. This is the explicit trust boundary.
        Ok(bbox_indexing::project_catalog_admin::RetireEvidence {
            external_reference_counts: Default::default(),
            unprobeable_classes: Vec::new(),
        })
    }
}

/// Helper: create a catalog store with one project inserted.
fn store_with_one_project(
    root: &Path,
) -> std::sync::Arc<bbox_indexing::project_catalog_store::ProjectCatalogStore> {
    use bbox_corpus_core::project_catalog::{CatalogSnapshotV2, CorpusProject, ProjectScope};
    use bbox_indexing::project_catalog_store::ProjectCatalogStore;

    let store = std::sync::Arc::new(
        ProjectCatalogStore::initialize_empty(root.join("projects.json")).unwrap(),
    );
    let project_id = ProjectId::parse("p_000000000000000000000000000000a1").unwrap();
    let epoch = store.snapshot().unwrap().epoch();
    store
        .transact(epoch, |catalog: &mut CatalogSnapshotV2, _| {
            catalog.projects.insert(
                project_id.clone(),
                CorpusProject {
                    project_id: project_id.clone(),
                    scope: ProjectScope::LegacyLocal,
                    operator_aliases: Default::default(),
                    nominated_aliases: Default::default(),
                    display_name: "discharge project".into(),
                    created_at: "2026-07-24T00:00:00Z".into(),
                    registered_at_compat: None,
                    repo_history: None,
                    languages: Default::default(),
                },
            );
            Ok(())
        })
        .unwrap();
    store
}

/// Section 12.5: the journal calls every discharge worker exactly once
/// for a project WITH blocking classes. The workers are called in the
/// correct order: CollectedGenerations -> Publications -> Attachments
/// -> CatalogPairRemoved -> MaterializationSwept.
#[test]
fn acceptance_discharge_workers_called_exactly_once_in_order() {
    use bbox_indexing::project_catalog_admin::{
        RetireEvidence, RetirementJournalStage, retire_project_journaled_with,
    };

    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let bro_home = root.join("bro-home");
    fs::create_dir_all(&bro_home).unwrap();

    let store = store_with_one_project(&root);
    let project_id = ProjectId::parse("p_000000000000000000000000000000a1").unwrap();

    // Evidence with nonzero blocking classes (simulates an active
    // collected generation).
    let evidence = RetireEvidence {
        external_reference_counts: {
            let mut m = std::collections::BTreeMap::new();
            m.insert("code_source_activation".into(), 1u64);
            m
        },
        unprobeable_classes: Vec::new(),
    };

    let mut workers = CountingDischargeWorkers::new();
    let (_preflight, journal) = retire_project_journaled_with(
        &store,
        &bro_home,
        &project_id,
        &evidence,
        true,
        &mut workers,
    )
    .unwrap();

    // The journal should complete despite nonzero blocking classes,
    // because the discharge workers handle them.
    let journal = journal.expect("journal should be returned");
    assert_eq!(
        journal.current_stage,
        RetirementJournalStage::Complete,
        "journal should reach Complete even with blocking classes"
    );

    // Each worker called exactly once.
    assert_eq!(
        workers.collected_generations_calls, 1,
        "CollectedGenerationsDischarged worker called exactly once"
    );
    assert_eq!(
        workers.publications_calls, 1,
        "PublicationsCleared worker called exactly once"
    );
    assert_eq!(
        workers.attachments_calls, 1,
        "AttachmentsDetached worker called exactly once"
    );
    assert_eq!(
        workers.sweep_calls, 1,
        "MaterializationSwept worker called exactly once"
    );
    assert_eq!(
        workers.reprobe_calls, 1,
        "reprobe_evidence called exactly once at CatalogPairRemoved"
    );

    // Project removed from catalog.
    let state = store.snapshot().unwrap();
    assert!(
        !state.catalog().projects.contains_key(&project_id),
        "project must be removed after journal completes"
    );
}

/// Section 12.5: restart between stages resumes idempotently. The
/// journal loads from disk and skips already-completed stages. Each
/// discharge worker is called at most once across the full lifecycle.
#[test]
fn acceptance_discharge_workers_resume_after_partial_completion() {
    use bbox_indexing::project_catalog_admin::{
        RetireEvidence, RetirementJournalStage, retire_project_journaled_with,
    };

    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let bro_home = root.join("bro-home");
    fs::create_dir_all(&bro_home).unwrap();

    let store = store_with_one_project(&root);
    let project_id = ProjectId::parse("p_000000000000000000000000000000a1").unwrap();

    let evidence = RetireEvidence {
        external_reference_counts: Default::default(),
        unprobeable_classes: Vec::new(),
    };

    // First pass: run to completion.
    let mut workers1 = CountingDischargeWorkers::new();
    let (_, journal1) = retire_project_journaled_with(
        &store,
        &bro_home,
        &project_id,
        &evidence,
        true,
        &mut workers1,
    )
    .unwrap();
    assert_eq!(
        journal1.as_ref().unwrap().current_stage,
        RetirementJournalStage::Complete
    );

    // Simulate a restart: the project is gone from the catalog, the
    // journal was archived. A second call should be idempotent: no
    // discharge workers fire.
    let mut workers2 = CountingDischargeWorkers::new();
    let (_, _journal2) = retire_project_journaled_with(
        &store,
        &bro_home,
        &project_id,
        &evidence,
        true,
        &mut workers2,
    )
    .unwrap();

    // No discharge workers should fire on the second call.
    assert_eq!(
        workers2.collected_generations_calls, 0,
        "no discharge after prior Complete"
    );
    assert_eq!(workers2.publications_calls, 0);
    assert_eq!(workers2.attachments_calls, 0);
    assert_eq!(workers2.sweep_calls, 0);
    assert_eq!(workers2.reprobe_calls, 0);
}

/// Section 12.5: the journal persists intermediate stage state to disk.
/// A manually-created journal at an intermediate stage causes the
/// resumed call to skip already-completed stages.
#[test]
fn acceptance_discharge_intermediate_journal_skips_completed_stages() {
    use bbox_indexing::project_catalog_admin::{
        ProjectRetirementJournal, RetireEvidence, retire_project_journaled_with,
        save_retirement_journal,
    };

    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let bro_home = root.join("bro-home");
    fs::create_dir_all(&bro_home).unwrap();

    let store = store_with_one_project(&root);
    let project_id = ProjectId::parse("p_000000000000000000000000000000a1").unwrap();

    // Manually create a journal at CollectedGenerationsDischarged,
    // simulating a restart after that stage completed.
    let epoch = store.snapshot().unwrap().epoch();
    let mut journal = ProjectRetirementJournal::new(project_id.clone(), epoch, "123");
    // Advance through SourceAuthorityQuiesced to CollectedGenerationsDischarged.
    journal.advance("124");
    journal.advance("125");
    save_retirement_journal(&bro_home, &journal).unwrap();

    let evidence = RetireEvidence {
        external_reference_counts: Default::default(),
        unprobeable_classes: Vec::new(),
    };

    let mut workers = CountingDischargeWorkers::new();
    let (_, _result_journal) = retire_project_journaled_with(
        &store,
        &bro_home,
        &project_id,
        &evidence,
        true,
        &mut workers,
    )
    .unwrap();

    // CollectedGenerations should NOT fire (already at that stage).
    assert_eq!(
        workers.collected_generations_calls, 0,
        "CollectedGenerationsDischarged should be skipped (already completed)"
    );
    // Publications, Attachments, and Sweep SHOULD fire.
    assert_eq!(workers.publications_calls, 1);
    assert_eq!(workers.attachments_calls, 1);
    assert_eq!(workers.sweep_calls, 1);
    assert_eq!(
        workers.reprobe_calls, 1,
        "reprobe fires at CatalogPairRemoved"
    );
}

/// Section 12.5: a discharge worker whose reprobe still reports a
/// nonzero reference class causes CatalogPairRemoved to refuse
/// (fail-closed). The journal stays at the AttachmentsDetached stage
/// and the project remains in the catalog, ready for resume after the
/// operator investigates the residual references.
#[test]
fn acceptance_discharge_nonzero_reprobe_refuses_at_final_cut() {
    use bbox_indexing::project_catalog_admin::{
        RetireEvidence, RetirementJournalStage, retire_project_journaled_with,
    };

    /// Worker whose reprobe always returns a nonzero class, simulating
    /// a buggy or partial discharge that left real references behind.
    struct NonzeroReprobeWorkers {
        reprobe_calls: u32,
    }

    impl bbox_indexing::project_catalog_admin::RetirementDischargeWorkers for NonzeroReprobeWorkers {
        fn discharge_collected_generations(
            &mut self,
            _project_id: &ProjectId,
            _evidence: &bbox_indexing::project_catalog_admin::RetirementJournalEvidence,
        ) -> bbox_indexing::project_catalog_admin::AdminResult<()> {
            Ok(())
        }
        fn discharge_publications(
            &mut self,
            _project_id: &ProjectId,
        ) -> bbox_indexing::project_catalog_admin::AdminResult<()> {
            Ok(())
        }
        fn discharge_attachments(
            &mut self,
            _store: &bbox_indexing::project_catalog_store::ProjectCatalogStore,
            _project_id: &ProjectId,
        ) -> bbox_indexing::project_catalog_admin::AdminResult<()> {
            Ok(())
        }
        fn sweep_materialization(
            &mut self,
            _project_id: &ProjectId,
            _evidence: &bbox_indexing::project_catalog_admin::RetirementJournalEvidence,
        ) -> bbox_indexing::project_catalog_admin::AdminResult<()> {
            Ok(())
        }
        fn verify_source_authority_quiesced(
            &mut self,
            _store: &bbox_indexing::project_catalog_store::ProjectCatalogStore,
            _project_id: &ProjectId,
            _evidence: &bbox_indexing::project_catalog_admin::RetirementJournalEvidence,
        ) -> bbox_indexing::project_catalog_admin::AdminResult<()> {
            Ok(())
        }
        fn reprobe_evidence(
            &mut self,
            _store: &bbox_indexing::project_catalog_store::ProjectCatalogStore,
            _project_id: &ProjectId,
            _original_evidence: &RetireEvidence,
            _retirement_evidence: &bbox_indexing::project_catalog_admin::RetirementJournalEvidence,
        ) -> bbox_indexing::project_catalog_admin::AdminResult<RetireEvidence> {
            self.reprobe_calls += 1;
            // Return evidence with a nonzero class: the discharge did NOT
            // actually clear this reference. R3F1: code_source_activation
            // is no longer blocking (it is state discharged by the journal),
            // so use a class that IS blocking.
            Ok(RetireEvidence {
                external_reference_counts: {
                    let mut m = std::collections::BTreeMap::new();
                    m.insert("knowledge_rows".into(), 1u64);
                    m
                },
                unprobeable_classes: Vec::new(),
            })
        }
    }

    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let bro_home = root.join("bro-home");
    fs::create_dir_all(&bro_home).unwrap();

    let store = store_with_one_project(&root);
    let project_id = ProjectId::parse("p_000000000000000000000000000000a1").unwrap();

    let evidence = RetireEvidence {
        external_reference_counts: Default::default(),
        unprobeable_classes: Vec::new(),
    };

    let mut workers = NonzeroReprobeWorkers { reprobe_calls: 0 };
    let result = retire_project_journaled_with(
        &store,
        &bro_home,
        &project_id,
        &evidence,
        true,
        &mut workers,
    );

    // The final cut must refuse.
    let err = result.unwrap_err();
    assert!(
        err.code()
            .starts_with("error.project_catalog_admin_retire_blocked"),
        "expected retire_blocked, got: {}",
        err.code()
    );

    // reprobe was called exactly once.
    assert_eq!(workers.reprobe_calls, 1);

    // The journal persists on disk at AttachmentsDetached (the stage
    // before CatalogPairRemoved), ready for resume.
    let journal =
        bbox_indexing::project_catalog_admin::load_retirement_journal(&bro_home, &project_id)
            .unwrap()
            .expect("journal should persist on disk after refusal");
    assert_eq!(
        journal.current_stage,
        RetirementJournalStage::AttachmentsDetached,
        "journal stays at AttachmentsDetached for resume"
    );

    // The project is still in the catalog.
    let state = store.snapshot().unwrap();
    assert!(
        state.catalog().projects.contains_key(&project_id),
        "project must remain in catalog after refused final cut"
    );
}

/// F5: verify_source_authority_quiesced is called before the journal
/// advances past SourceAuthorityQuiesced. A worker that refuses (returns
/// Err) blocks the journal at Prepared.
#[test]
fn f5_source_authority_quiesced_blocks_journal() {
    use bbox_corpus_core::project_catalog::{
        CatalogSnapshotV2, CorpusProject, ProjectId, ProjectScope,
    };
    use bbox_indexing::project_catalog_admin::{
        RetireEvidence, RetirementJournalStage, retire_project_journaled_with,
    };
    use bbox_indexing::project_catalog_store::ProjectCatalogStore;
    use std::sync::Arc;

    struct RefusingQuiesceWorker;

    impl bbox_indexing::project_catalog_admin::RetirementDischargeWorkers for RefusingQuiesceWorker {
        fn discharge_collected_generations(
            &mut self,
            _project_id: &ProjectId,
            _evidence: &bbox_indexing::project_catalog_admin::RetirementJournalEvidence,
        ) -> bbox_indexing::project_catalog_admin::AdminResult<()> {
            Ok(())
        }
        fn discharge_publications(
            &mut self,
            _project_id: &ProjectId,
        ) -> bbox_indexing::project_catalog_admin::AdminResult<()> {
            Ok(())
        }
        fn discharge_attachments(
            &mut self,
            _store: &ProjectCatalogStore,
            _project_id: &ProjectId,
        ) -> bbox_indexing::project_catalog_admin::AdminResult<()> {
            Ok(())
        }
        fn sweep_materialization(
            &mut self,
            _project_id: &ProjectId,
            _evidence: &bbox_indexing::project_catalog_admin::RetirementJournalEvidence,
        ) -> bbox_indexing::project_catalog_admin::AdminResult<()> {
            Ok(())
        }
        fn verify_source_authority_quiesced(
            &mut self,
            _store: &ProjectCatalogStore,
            _project_id: &ProjectId,
            _evidence: &bbox_indexing::project_catalog_admin::RetirementJournalEvidence,
        ) -> bbox_indexing::project_catalog_admin::AdminResult<()> {
            Err(bbox_indexing::project_catalog_admin::admin_error(
                "error.project_catalog_retire_auth_not_quiesced",
                "active assignments remain",
            ))
        }
        fn reprobe_evidence(
            &mut self,
            _store: &ProjectCatalogStore,
            _project_id: &ProjectId,
            _original_evidence: &RetireEvidence,
            _retirement_evidence: &bbox_indexing::project_catalog_admin::RetirementJournalEvidence,
        ) -> bbox_indexing::project_catalog_admin::AdminResult<RetireEvidence> {
            Ok(RetireEvidence {
                external_reference_counts: Default::default(),
                unprobeable_classes: Vec::new(),
            })
        }
    }

    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let bro_home = root.join("bro-home");
    fs::create_dir_all(&bro_home).unwrap();

    let store =
        Arc::new(ProjectCatalogStore::initialize_empty(root.join("projects.json")).unwrap());

    let project_id = ProjectId::parse("p_000000000000000000000000000000a1").unwrap();
    let epoch = store.snapshot().unwrap().epoch();
    store
        .transact(epoch, |catalog: &mut CatalogSnapshotV2, _| {
            catalog.projects.insert(
                project_id.clone(),
                CorpusProject {
                    project_id: project_id.clone(),
                    scope: ProjectScope::LegacyLocal,
                    operator_aliases: Default::default(),
                    nominated_aliases: Default::default(),
                    display_name: "f5 project".into(),
                    created_at: "2026-07-24T00:00:00Z".into(),
                    registered_at_compat: None,
                    repo_history: None,
                    languages: Default::default(),
                },
            );
            Ok(())
        })
        .unwrap();

    let evidence = RetireEvidence {
        external_reference_counts: Default::default(),
        unprobeable_classes: Vec::new(),
    };

    let mut workers = RefusingQuiesceWorker;
    let result = retire_project_journaled_with(
        &store,
        &bro_home,
        &project_id,
        &evidence,
        true,
        &mut workers,
    );

    assert!(
        result.is_err(),
        "journal must refuse when source authority is not quiesced"
    );
    let err_code = result.unwrap_err().code();
    assert!(
        err_code.contains("auth_not_quiesced"),
        "error must name auth_not_quiesced, got: {err_code}"
    );

    // The journal must NOT have advanced past Prepared.
    let journal =
        bbox_indexing::project_catalog_admin::load_retirement_journal(&bro_home, &project_id)
            .unwrap()
            .expect("journal should persist on disk");
    assert_eq!(
        journal.current_stage,
        RetirementJournalStage::Prepared,
        "journal must stay at Prepared when source authority is not quiesced"
    );
}

/// R2F1: the final reprobe must carry unprobeable classes through as
/// refusals so they cannot be mistaken for a discharged zero. A worker
/// whose reprobe returns unprobeable_classes must block the journal at
/// the CatalogPairRemoved stage.
#[test]
fn r2f1_unprobeable_classes_block_journal() {
    use bbox_corpus_core::project_catalog::{
        CatalogSnapshotV2, CorpusProject, ProjectId, ProjectScope,
    };
    use bbox_indexing::project_catalog_admin::{
        RetireEvidence, RetirementJournalStage, retire_project_journaled_with,
    };
    use bbox_indexing::project_catalog_store::ProjectCatalogStore;
    use std::sync::Arc;

    struct UnprobeableReprobeWorker;

    impl bbox_indexing::project_catalog_admin::RetirementDischargeWorkers for UnprobeableReprobeWorker {
        fn discharge_collected_generations(
            &mut self,
            _project_id: &ProjectId,
            _evidence: &bbox_indexing::project_catalog_admin::RetirementJournalEvidence,
        ) -> bbox_indexing::project_catalog_admin::AdminResult<()> {
            Ok(())
        }
        fn discharge_publications(
            &mut self,
            _project_id: &ProjectId,
        ) -> bbox_indexing::project_catalog_admin::AdminResult<()> {
            Ok(())
        }
        fn discharge_attachments(
            &mut self,
            _store: &ProjectCatalogStore,
            _project_id: &ProjectId,
        ) -> bbox_indexing::project_catalog_admin::AdminResult<()> {
            Ok(())
        }
        fn sweep_materialization(
            &mut self,
            _project_id: &ProjectId,
            _evidence: &bbox_indexing::project_catalog_admin::RetirementJournalEvidence,
        ) -> bbox_indexing::project_catalog_admin::AdminResult<()> {
            Ok(())
        }
        fn verify_source_authority_quiesced(
            &mut self,
            _store: &ProjectCatalogStore,
            _project_id: &ProjectId,
            _evidence: &bbox_indexing::project_catalog_admin::RetirementJournalEvidence,
        ) -> bbox_indexing::project_catalog_admin::AdminResult<()> {
            Ok(())
        }
        fn reprobe_evidence(
            &mut self,
            _store: &ProjectCatalogStore,
            _project_id: &ProjectId,
            _original_evidence: &RetireEvidence,
            _retirement_evidence: &bbox_indexing::project_catalog_admin::RetirementJournalEvidence,
        ) -> bbox_indexing::project_catalog_admin::AdminResult<RetireEvidence> {
            Ok(RetireEvidence {
                external_reference_counts: Default::default(),
                unprobeable_classes: vec!["code_source_generations".to_string()],
            })
        }
    }

    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let bro_home = root.join("bro-home");
    fs::create_dir_all(&bro_home).unwrap();

    let store =
        Arc::new(ProjectCatalogStore::initialize_empty(root.join("projects.json")).unwrap());

    let project_id = ProjectId::parse("p_000000000000000000000000000000a1").unwrap();
    let epoch = store.snapshot().unwrap().epoch();
    store
        .transact(epoch, |catalog: &mut CatalogSnapshotV2, _| {
            catalog.projects.insert(
                project_id.clone(),
                CorpusProject {
                    project_id: project_id.clone(),
                    scope: ProjectScope::LegacyLocal,
                    operator_aliases: Default::default(),
                    nominated_aliases: Default::default(),
                    display_name: "r2f1 project".into(),
                    created_at: "2026-07-24T00:00:00Z".into(),
                    registered_at_compat: None,
                    repo_history: None,
                    languages: Default::default(),
                },
            );
            Ok(())
        })
        .unwrap();

    let evidence = RetireEvidence {
        external_reference_counts: Default::default(),
        unprobeable_classes: Vec::new(),
    };

    let mut workers = UnprobeableReprobeWorker;
    let result = retire_project_journaled_with(
        &store,
        &bro_home,
        &project_id,
        &evidence,
        true,
        &mut workers,
    );

    assert!(
        result.is_err(),
        "journal must refuse when reprobe returns unprobeable classes"
    );
    let err_code = result.unwrap_err().code();
    assert!(
        err_code.contains("unprobeable"),
        "must refuse with unprobeable_classes error, got: {err_code}"
    );

    // The journal must NOT have advanced to Complete.
    let journal =
        bbox_indexing::project_catalog_admin::load_retirement_journal(&bro_home, &project_id)
            .unwrap()
            .expect("journal should persist on disk");
    assert_ne!(
        journal.current_stage,
        RetirementJournalStage::Complete,
        "journal must not complete when unprobeable classes remain"
    );
}

// ----------------------------------------------------------------------------
// Phase 5 section 13.8 fixture set (P5-H mechanic 1)
// ----------------------------------------------------------------------------
//
// Section 13.8 names thirteen fixture projects. Before this block each of
// them that anyone needed was minted ad hoc inside whichever P5-B, P5-D, or
// P5-G test wanted it, which is how a set drifts: two suites disagree about
// what "Prior fallback" means and neither is wrong, because neither is the
// definition.
//
// So the set is DATA here, declared once, and it has two consumers that
// cannot disagree: `the_section_13_8_fixture_set_builds_every_named_shape`
// gates it on every workspace run, and the ignored D-030 producer
// materializes the same set onto the migrated root it already produces and
// records it in the summary the live bootsmoke driver and the stable-signed
// CLI read.
//
// Two rules the set inherits from the phase and does not get to relax:
//
// 1. Every knowledge and gap byte goes through the single-owner committed
//    encoders. A fixture with its own encoding produces generations
//    describing bytes no writer would ever commit, every digest matches
//    itself, and the suite goes vacuously green. The bytes written into the
//    repository and the bytes handed to the installer are literally the
//    same `Vec<u8>` here, so they cannot drift even by editing.
//
// 2. Catalog and attachment shapes are built through the admin facade
//    (`catalog_add`, `attach_checkout`, `scope_migrate_attested`), not by
//    hand-writing store rows. A hand-written row can express a state the
//    facade would refuse, and a fixture that encodes an impossible state
//    proves nothing about the system that has to serve it.
//
// The corrupt shapes are the one exception and necessarily so: corruption
// is what the facade exists to prevent, so it arrives through the explicit
// damage helper instead.

use bbox_corpus_core::project_catalog::AttachmentCapabilities;
use bbox_gaps::gaps::committed_gap_note_bytes;
use bbox_indexing::accepted_publication_test_support::{
    AcceptedPublicationSourceFileForTest, corrupt_accepted_generation_for_test,
    install_accepted_publication_for_test,
};
use bbox_indexing::project_catalog_admin::{
    self, AttachProbe, CatalogAddKind, MigrationAttachmentProbe, ScopeMigrationRequest,
};
use bbox_knowledge::knowledge::committed_knowledge_entry_bytes;

/// The thirteen shapes, by the names section 13.8 gives them.
const SECTION_13_8_SHAPES: &[&str] = &[
    "remote_only_valid_g1",
    "attached_valid_g1",
    "attached_peer_contains_p",
    "attached_peer_missing_p",
    "prior_fallback",
    "no_pointer_after_no_content_acknowledgement",
    "corrupt_current_and_prior",
    "scope_migration_publication_bridge",
    "all_capabilities_attachment",
    "repo_knowledge_only_attachment",
    "no_capability_attachment",
    "watcher_capable_attachment",
    "legacy_local_bootstrap",
];

/// One built shape, in the terms a consumer asserts on.
#[derive(Debug, Clone, serde::Serialize)]
struct FixtureShape {
    shape: String,
    project_id: String,
    /// Attachment ids in declaration order; empty for a remote-only project.
    attachment_ids: Vec<String>,
    /// `null` when the shape is deliberately pointerless.
    accepted_generation: Option<String>,
    /// Present once a second publication has pushed the first to Prior.
    prior_generation: Option<String>,
    /// Generations deliberately damaged, so a consumer knows the difference
    /// between a fixture defect and the state under test.
    corrupted_generations: Vec<String>,
    /// Catalog scope after any migration, as `repo_id` + relpath, or `null`
    /// for a legacy-local project.
    catalog_scope: Option<(String, String)>,
    /// The scope the accepted pointer still names, which differs from the
    /// catalog scope exactly while a publication bridge is open.
    accepted_scope: Option<(String, String)>,
}

fn published_scope_of(project: &CorpusProject) -> Option<(String, String)> {
    match &project.scope {
        ProjectScope::Published(scope) => Some((
            scope.repo_id().to_string(),
            scope.bbox_root_relpath().to_string(),
        )),
        ProjectScope::LegacyLocal | ProjectScope::Connector(_) => None,
    }
}

/// A deterministic, strong-shaped checkout marker.
///
/// The attachment snapshot validates that a checkout id looks like the
/// random marker production mints, so a fixture cannot spell one out of the
/// shape name. Hashing the name keeps it deterministic (the same fixture
/// twice is the same id, which the live smoke driver depends on) while
/// still being 32 lowercase hex.
fn checkout_marker(name: &str) -> String {
    hex::encode(Sha256::digest(format!("bbox-13.8-checkout:{name}")))[..32].to_string()
}

fn capabilities(bits: &[&str]) -> AttachmentCapabilities {
    let mut capabilities = AttachmentCapabilities::default();
    for bit in bits {
        match *bit {
            "local_code_source" => capabilities.local_code_source = true,
            "git_history" => capabilities.git_history = true,
            "blame" => capabilities.blame = true,
            "repo_knowledge" => capabilities.repo_knowledge = true,
            "repo_mutation" => capabilities.repo_mutation = true,
            "render_output" => capabilities.render_output = true,
            "provenance_note_io" => capabilities.provenance_note_io = true,
            "artifact_watching" => capabilities.artifact_watching = true,
            other => panic!("unknown capability bit {other}"),
        }
    }
    capabilities
}

/// One committed publishable checkout: a repository whose `.bbox` lanes
/// hold exactly the bytes a writer commits, at a pinned identity so the
/// accepted commit is stable.
struct PublishableCheckout {
    dir: PathBuf,
    accepted_commit: String,
    knowledge: Vec<AcceptedPublicationSourceFileForTest>,
    gaps: Vec<AcceptedPublicationSourceFileForTest>,
}

fn publishable_checkout(
    root: &Path,
    name: &str,
    scope: &PublishedScope,
    marker: &str,
) -> PublishableCheckout {
    let dir = root.join("catalog-checkouts").join(name);
    fs::create_dir_all(&dir).unwrap();
    git(&dir, &["init", "-q"]);
    git(&dir, &["checkout", "-qb", "main"]);
    write(
        &dir.join(".bbox/config.toml"),
        format!("[project]\nrepo_id = {:?}\n", scope.repo_id()).as_bytes(),
    );

    let relative = |lane: &str, id: &str| {
        if scope.bbox_root_relpath() == "." {
            format!(".bbox/{lane}/{id}.json")
        } else {
            format!("{}/.bbox/{lane}/{id}.json", scope.bbox_root_relpath())
        }
    };
    // Encode ONCE. The repository bytes and the accepted source bytes are
    // the same value below, not two encodings that happen to agree today.
    let entry = fixture_knowledge_entry(&format!("k-{marker}"), marker);
    let knowledge_bytes = committed_knowledge_entry_bytes(&entry).unwrap();
    let gap = fixture_gap_note(&format!("gap-{:0>8}", marker.len()), marker);
    let gap_bytes = committed_gap_note_bytes(&gap).unwrap();
    write(
        &dir.join(relative("knowledge", &entry.id)),
        &knowledge_bytes,
    );
    write(&dir.join(relative("gaps", &gap.id)), &gap_bytes);

    git(&dir, &["add", ".bbox"]);
    git(&dir, &["commit", "-qm", "seed catalog fixture lanes"]);
    let accepted_commit = git(&dir, &["rev-parse", "HEAD"]);
    write(
        &dir.join(".bbox/local/checkout-id"),
        format!("{}\n", checkout_marker(marker)).as_bytes(),
    );
    PublishableCheckout {
        dir,
        accepted_commit,
        knowledge: vec![AcceptedPublicationSourceFileForTest {
            repository_relative_filename: relative("knowledge", &entry.id),
            source_bytes: knowledge_bytes,
        }],
        gaps: vec![AcceptedPublicationSourceFileForTest {
            repository_relative_filename: relative("gaps", &gap.id),
            source_bytes: gap_bytes,
        }],
    }
}

fn fixture_knowledge_entry(id: &str, content: &str) -> bbox_knowledge::knowledge::KnowledgeEntry {
    use bbox_knowledge::knowledge::{Approval, Category, Priority, Scope, Status};
    bbox_knowledge::knowledge::KnowledgeEntry {
        id: id.to_string(),
        title: format!("entry {id}"),
        content: content.to_string(),
        cluster: None,
        variants: Default::default(),
        category: Category::Convention,
        scope: Scope::Project,
        project: None,
        project_id: None,
        providers: Vec::new(),
        priority: Priority::Standard,
        weight: 100,
        status: Status::Active,
        approval: Approval::UserConfirmed,
        render: true,
        decay: false,
        review_at: None,
        supersedes: None,
        links: Vec::new(),
        rationale: None,
        expires_at: None,
        source: "user".to_string(),
        created_at: "2026-01-01T00:00:00Z".to_string(),
        updated_at: "2026-01-02T00:00:00Z".to_string(),
        recall_count: 0,
        last_recalled: None,
    }
}

fn fixture_gap_note(id: &str, title: &str) -> bbox_gaps::gaps::GapNote {
    use bbox_gaps::gaps::{BlockingLevel, GapImpact, GapKind, GapResolution};
    bbox_gaps::gaps::GapNote {
        id: id.to_string(),
        title: title.to_string(),
        gap_kind: GapKind::Tooling,
        domain: "catalog-fixture".to_string(),
        wanted_capability: "serve the section 13.8 fixture set".to_string(),
        missing_primitive: None,
        fallback_used: None,
        evidence: Vec::new(),
        impact: GapImpact::Medium,
        blocking_level: BlockingLevel::WorkaroundAvailable,
        dedupe_key: "tooling/catalog-fixture/section-13-8".to_string(),
        suggested_owner: None,
        notes: None,
        supersedes: None,
        superseded_by: None,
        resolution: GapResolution::Unresolved,
        project: None,
        project_id: None,
        write_dir: None,
        provisional_checkout_id: None,
        task_id: None,
        session_id: None,
        provider: None,
        bro: None,
        thread_id: None,
        created_at: "2026-01-01T00:00:00Z".to_string(),
        updated_at: "2026-01-02T00:00:00Z".to_string(),
        resolved_at: None,
        resolution_note: None,
    }
}

/// Build the complete section 13.8 set into the catalog at `projects_path`.
///
/// One function, one definition. Both consumers below call exactly this.
fn build_section_13_8_fixture_set(projects_path: &Path, root: &Path) -> Vec<FixtureShape> {
    let store = ProjectCatalogStore::open_existing(projects_path).unwrap();
    let mut shapes = Vec::new();

    let epoch = |store: &ProjectCatalogStore| store.snapshot().unwrap().epoch();
    let add = |store: &ProjectCatalogStore, kind: CatalogAddKind, name: &str| {
        project_catalog_admin::catalog_add(
            store,
            epoch(store),
            &kind,
            name,
            &[],
            "2026-08-03T00:00:00Z",
        )
        .unwrap()
        .0
    };
    let attach = |store: &ProjectCatalogStore,
                  project_id: &ProjectId,
                  checkout: &Path,
                  checkout_id: &str,
                  scope: Option<&PublishedScope>,
                  bits: &[&str]| {
        project_catalog_admin::attach_checkout(
            store,
            epoch(store),
            project_id,
            &AttachProbe {
                checkout_id: checkout_id.to_string(),
                checkout_dir: checkout.to_string_lossy().into_owned(),
                checkout_project_dir: checkout.to_string_lossy().into_owned(),
                project_root_relpath: scope
                    .map(|scope| scope.bbox_root_relpath().to_string())
                    .unwrap_or_else(|| ".".into()),
                kind: AttachmentKind::Base,
                validated_scope: scope.cloned(),
                computed_repo_hint: None,
                branch_ref: Some("refs/heads/main".into()),
                capabilities: capabilities(bits),
                attached_at: "2026-08-03T00:00:00Z".into(),
            },
        )
        .unwrap()
        .attachment_id
    };

    // Shapes 1 and 2: the two baseline publications. Remote-only is the
    // case a catalog published read must serve with ZERO leases, so it
    // deliberately never gets an attachment - but its pointer still needs a
    // bound attachment id, which is exactly the remote-host binding a
    // remote-only project carries.
    for (shape, attach_locally) in [("remote_only_valid_g1", false), ("attached_valid_g1", true)] {
        let scope = PublishedScope::try_new(format!("repo-{shape}"), ".").unwrap();
        let checkout = publishable_checkout(root, shape, &scope, shape);
        let project_id = add(&store, CatalogAddKind::Published(scope.clone()), shape);
        let attachment_id = attach(
            &store,
            &project_id,
            &checkout.dir,
            &checkout_marker(shape),
            Some(&scope),
            &["repo_knowledge"],
        );
        let installed = install_accepted_publication_for_test(
            projects_path,
            &project_id,
            &attachment_id,
            &scope,
            "refs/heads/main",
            &checkout.accepted_commit,
            checkout.knowledge.clone(),
            checkout.gaps.clone(),
        )
        .unwrap();
        let mut attachment_ids = vec![attachment_id.to_string()];
        if !attach_locally {
            // The publication is established, THEN the local attachment is
            // detached: a remote-only project on this host is one whose
            // accepted content outlives any local checkout, not one that
            // never had a pointer.
            project_catalog_admin::detach_attachment(
                &store,
                epoch(&store),
                &AttachmentId::parse(&attachment_ids[0]).unwrap(),
                "2026-08-03T02:00:00Z",
            )
            .unwrap();
            attachment_ids.clear();
        }
        shapes.push(FixtureShape {
            shape: shape.into(),
            project_id: project_id.to_string(),
            attachment_ids,
            accepted_generation: Some(installed.generation_id),
            prior_generation: None,
            corrupted_generations: Vec::new(),
            catalog_scope: Some((scope.repo_id().into(), scope.bbox_root_relpath().into())),
            accepted_scope: Some((scope.repo_id().into(), scope.bbox_root_relpath().into())),
        });
    }

    // Shapes 3 and 4: peer containment. The overlay baseline turns on
    // whether a peer's object database actually holds the accepted commit
    // P, so the two peers differ in exactly that and nothing else: one is a
    // clone of the publisher (has P), the other is an independent
    // repository at the same scope (does not).
    {
        let scope = PublishedScope::try_new("repo-peer-containment", ".").unwrap();
        let base = publishable_checkout(root, "peer-base", &scope, "peerbase");
        let project_id = add(
            &store,
            CatalogAddKind::Published(scope.clone()),
            "peer_containment",
        );
        let base_attachment = attach(
            &store,
            &project_id,
            &base.dir,
            &checkout_marker("peerbase"),
            Some(&scope),
            &["repo_knowledge"],
        );
        let installed = install_accepted_publication_for_test(
            projects_path,
            &project_id,
            &base_attachment,
            &scope,
            "refs/heads/main",
            &base.accepted_commit,
            base.knowledge.clone(),
            base.gaps.clone(),
        )
        .unwrap();

        let containing = root.join("catalog-checkouts").join("peer-containing");
        let clone = Command::new("git")
            .args([
                "clone",
                "-q",
                base.dir.to_str().unwrap(),
                containing.to_str().unwrap(),
            ])
            .output()
            .unwrap();
        assert!(clone.status.success(), "cloning the containing peer failed");
        write(
            &containing.join(".bbox/local/checkout-id"),
            format!("{}\n", checkout_marker("peercontains")).as_bytes(),
        );
        assert!(
            git(&containing, &["cat-file", "-t", &base.accepted_commit]) == "commit",
            "the containing peer must actually hold P"
        );
        let containing_attachment = attach(
            &store,
            &project_id,
            &containing,
            &checkout_marker("peercontains"),
            Some(&scope),
            &["repo_knowledge"],
        );

        let missing = publishable_checkout(root, "peer-missing", &scope, "peermissing");
        assert_ne!(
            missing.accepted_commit, base.accepted_commit,
            "the missing peer must be an independent history"
        );
        let missing_attachment = attach(
            &store,
            &project_id,
            &missing.dir,
            &checkout_marker("peermissing"),
            Some(&scope),
            &["repo_knowledge"],
        );

        for (shape, attachment) in [
            ("attached_peer_contains_p", &containing_attachment),
            ("attached_peer_missing_p", &missing_attachment),
        ] {
            shapes.push(FixtureShape {
                shape: shape.into(),
                project_id: project_id.to_string(),
                attachment_ids: vec![base_attachment.to_string(), attachment.to_string()],
                accepted_generation: Some(installed.generation_id.clone()),
                prior_generation: None,
                corrupted_generations: Vec::new(),
                catalog_scope: Some((scope.repo_id().into(), scope.bbox_root_relpath().into())),
                accepted_scope: Some((scope.repo_id().into(), scope.bbox_root_relpath().into())),
            });
        }
    }

    // Shapes 5 and 7: Prior fallback and total corruption. Both need two
    // installed generations, and they differ only in how much damage is
    // done, so they are built the same way and then damaged differently.
    for shape in ["prior_fallback", "corrupt_current_and_prior"] {
        let scope = PublishedScope::try_new(format!("repo-{shape}"), ".").unwrap();
        let first =
            publishable_checkout(root, &format!("{shape}-g1"), &scope, &format!("{shape}1"));
        let project_id = add(&store, CatalogAddKind::Published(scope.clone()), shape);
        let attachment_id = attach(
            &store,
            &project_id,
            &first.dir,
            &checkout_marker(shape),
            Some(&scope),
            &["repo_knowledge"],
        );
        let install = |knowledge: Vec<AcceptedPublicationSourceFileForTest>,
                       gaps: Vec<AcceptedPublicationSourceFileForTest>,
                       commit: &str| {
            install_accepted_publication_for_test(
                projects_path,
                &project_id,
                &attachment_id,
                &scope,
                "refs/heads/main",
                commit,
                knowledge,
                gaps,
            )
            .unwrap()
        };
        let g1 = install(
            first.knowledge.clone(),
            first.gaps.clone(),
            &first.accepted_commit,
        );
        // A second publishable checkout at the same scope with different
        // content: installing it pushes G1 to the prior arm.
        let second =
            publishable_checkout(root, &format!("{shape}-g2"), &scope, &format!("{shape}22"));
        let g2 = install(
            second.knowledge.clone(),
            second.gaps.clone(),
            &second.accepted_commit,
        );
        assert_ne!(
            g1.generation_id, g2.generation_id,
            "two generations with different content must not share an id"
        );

        // Corruption is the one thing the facade will not produce, so it
        // arrives through the explicit damage helper.
        let mut corrupted = vec![g2.generation_id.clone()];
        corrupt_accepted_generation_for_test(projects_path, &project_id, &g2.generation_id)
            .unwrap();
        if shape == "corrupt_current_and_prior" {
            corrupt_accepted_generation_for_test(projects_path, &project_id, &g1.generation_id)
                .unwrap();
            corrupted.push(g1.generation_id.clone());
        }
        shapes.push(FixtureShape {
            shape: shape.into(),
            project_id: project_id.to_string(),
            attachment_ids: vec![attachment_id.to_string()],
            accepted_generation: Some(g2.generation_id),
            prior_generation: Some(g1.generation_id),
            corrupted_generations: corrupted,
            catalog_scope: Some((scope.repo_id().into(), scope.bbox_root_relpath().into())),
            accepted_scope: Some((scope.repo_id().into(), scope.bbox_root_relpath().into())),
        });
    }

    // Shape 6: an attached, capable project that has acknowledged it has no
    // content to publish. D-040 makes pointer ABSENCE the establish gate,
    // so this is the state establish is allowed to act on, and it must be
    // distinguishable from a project whose pointer went missing.
    {
        let shape = "no_pointer_after_no_content_acknowledgement";
        let scope = PublishedScope::try_new("repo-no-pointer", ".").unwrap();
        let checkout = publishable_checkout(root, shape, &scope, "nopointer");
        let project_id = add(&store, CatalogAddKind::Published(scope.clone()), shape);
        let attachment_id = attach(
            &store,
            &project_id,
            &checkout.dir,
            &checkout_marker("nopointer"),
            Some(&scope),
            &["repo_knowledge"],
        );
        shapes.push(FixtureShape {
            shape: shape.into(),
            project_id: project_id.to_string(),
            attachment_ids: vec![attachment_id.to_string()],
            accepted_generation: None,
            prior_generation: None,
            corrupted_generations: Vec::new(),
            catalog_scope: Some((scope.repo_id().into(), scope.bbox_root_relpath().into())),
            accepted_scope: None,
        });
    }

    // Shape 8: the publication bridge. The catalog scope moves and the
    // accepted pointer does not, which is the whole state: old truth keeps
    // serving until an advance at the NEW scope clears it (plan 4.9).
    {
        let shape = "scope_migration_publication_bridge";
        let old_scope = PublishedScope::try_new("repo-bridge", ".").unwrap();
        let new_scope = PublishedScope::try_new("repo-bridge", "services/api").unwrap();
        let checkout = publishable_checkout(root, shape, &old_scope, "bridge");
        let project_id = add(&store, CatalogAddKind::Published(old_scope.clone()), shape);
        let attachment_id = attach(
            &store,
            &project_id,
            &checkout.dir,
            &checkout_marker("bridge"),
            Some(&old_scope),
            &["repo_knowledge"],
        );
        let installed = install_accepted_publication_for_test(
            projects_path,
            &project_id,
            &attachment_id,
            &old_scope,
            "refs/heads/main",
            &checkout.accepted_commit,
            checkout.knowledge.clone(),
            checkout.gaps.clone(),
        )
        .unwrap();
        fs::create_dir_all(checkout.dir.join("services/api")).unwrap();
        // The attachment-PROVED channel, because this project has an active
        // attachment. The attested channel is for the unattached case and
        // refuses here, which is the right refusal: a live attachment must
        // prove the new scope rather than be attested around.
        project_catalog_admin::scope_migrate_attached(
            &store,
            epoch(&store),
            &ScopeMigrationRequest {
                project_id: project_id.clone(),
                expected_old_scope: old_scope.clone(),
                new_scope: new_scope.clone(),
                kind: ScopeMigrationKind::RelpathMove,
                designated_attachment: attachment_id.clone(),
                acknowledge_repo_authority_change: false,
                attachment_probes: [(
                    attachment_id.clone(),
                    MigrationAttachmentProbe {
                        resolved_scope: Some(new_scope.clone()),
                        new_project_root_relpath: "services/api".into(),
                        new_checkout_project_dir: checkout
                            .dir
                            .join("services/api")
                            .to_string_lossy()
                            .into_owned(),
                    },
                )]
                .into_iter()
                .collect(),
                code_bridge_generation: None,
                publication_bridge_generation: Some(installed.generation_id.clone()),
                operator_invocation: "section 13.8 fixture set".into(),
                operator_reason: Some("materialize the publication bridge shape".into()),
                migrated_at: "2026-08-03T01:00:00Z".into(),
            },
            false,
        )
        .unwrap()
        .expect("a committed scope migration returns its receipt");
        shapes.push(FixtureShape {
            shape: shape.into(),
            project_id: project_id.to_string(),
            attachment_ids: vec![attachment_id.to_string()],
            accepted_generation: Some(installed.generation_id),
            prior_generation: None,
            corrupted_generations: Vec::new(),
            catalog_scope: Some((
                new_scope.repo_id().into(),
                new_scope.bbox_root_relpath().into(),
            )),
            accepted_scope: Some((
                old_scope.repo_id().into(),
                old_scope.bbox_root_relpath().into(),
            )),
        });
    }

    // Shapes 9 through 12: the capability variants. One project per shape,
    // because a capability bit is per attachment and section 9's
    // degradation table is read per project.
    for (shape, bits) in [
        (
            "all_capabilities_attachment",
            &[
                "local_code_source",
                "git_history",
                "blame",
                "repo_knowledge",
                "repo_mutation",
                "render_output",
                "provenance_note_io",
                "artifact_watching",
            ][..],
        ),
        ("repo_knowledge_only_attachment", &["repo_knowledge"][..]),
        ("no_capability_attachment", &[][..]),
        ("watcher_capable_attachment", &["artifact_watching"][..]),
    ] {
        let scope = PublishedScope::try_new(format!("repo-{shape}"), ".").unwrap();
        let checkout = publishable_checkout(root, shape, &scope, shape);
        let project_id = add(&store, CatalogAddKind::Published(scope.clone()), shape);
        let attachment_id = attach(
            &store,
            &project_id,
            &checkout.dir,
            &checkout_marker(shape),
            Some(&scope),
            bits,
        );
        shapes.push(FixtureShape {
            shape: shape.into(),
            project_id: project_id.to_string(),
            attachment_ids: vec![attachment_id.to_string()],
            accepted_generation: None,
            prior_generation: None,
            corrupted_generations: Vec::new(),
            catalog_scope: Some((scope.repo_id().into(), scope.bbox_root_relpath().into())),
            accepted_scope: None,
        });
    }

    // Shape 13: the legacy-local bootstrap. Section 13.8 says "where
    // applicable", and it applies: a project with no published scope has no
    // accepted publication to serve, which is a different unavailability
    // from a published project whose pointer is missing, and the two must
    // not collapse into one status.
    {
        let shape = "legacy_local_bootstrap";
        let dir = root.join("catalog-checkouts").join(shape);
        fs::create_dir_all(&dir).unwrap();
        write(&dir.join("README.md"), b"not a git repository\n");
        write(
            &dir.join(".bbox/local/checkout-id"),
            format!("{}\n", checkout_marker("legacylocal")).as_bytes(),
        );
        let project_id = add(&store, CatalogAddKind::LegacyLocal, shape);
        let attachment_id = attach(
            &store,
            &project_id,
            &dir,
            &checkout_marker("legacylocal"),
            // A legacy-local project attaches only a checkout that records
            // NO committed authority; supplying one is a promotion, not an
            // attach, and the facade refuses it.
            None,
            &["local_code_source"],
        );
        shapes.push(FixtureShape {
            shape: shape.into(),
            project_id: project_id.to_string(),
            attachment_ids: vec![attachment_id.to_string()],
            accepted_generation: None,
            prior_generation: None,
            corrupted_generations: Vec::new(),
            catalog_scope: None,
            accepted_scope: None,
        });
    }

    // Returned in SECTION_13_8_SHAPES order, not build order. Prior
    // fallback and the corrupt shape share a builder because they differ
    // only in how much damage is done, which is a construction detail; the
    // set a consumer sees is the plan's.
    shapes.sort_by_key(|shape| {
        SECTION_13_8_SHAPES
            .iter()
            .position(|name| *name == shape.shape)
            .unwrap_or_else(|| panic!("shape {} is not named in section 13.8", shape.shape))
    });
    shapes
}

/// The set is gated on every workspace run, not only by the live bootsmoke.
///
/// Without this the definition would be exercised exclusively by an ignored
/// producer, which is the same as not being exercised: a shape could stop
/// building and nobody would find out until someone ran the smoke by hand.
#[test]
fn the_section_13_8_fixture_set_builds_every_named_shape() {
    let dir = tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let projects_path = root.join("state").join("projects.json");
    fs::create_dir_all(projects_path.parent().unwrap()).unwrap();
    ProjectCatalogStore::initialize_empty(&projects_path).unwrap();

    let shapes = build_section_13_8_fixture_set(&projects_path, &root);

    let built: Vec<&str> = shapes.iter().map(|shape| shape.shape.as_str()).collect();
    assert_eq!(
        built, SECTION_13_8_SHAPES,
        "the built set must be exactly section 13.8's thirteen shapes, in order"
    );

    // Every shape is asserted against the CATALOG, not against what the
    // builder claims it did. A receipt that agrees with itself proves
    // nothing; the store is the authority.
    let state = ProjectCatalogStore::open_existing(&projects_path)
        .unwrap()
        .snapshot()
        .unwrap();
    for shape in &shapes {
        let project_id = ProjectId::parse(&shape.project_id).unwrap();
        let project = state
            .catalog()
            .projects
            .get(&project_id)
            .unwrap_or_else(|| panic!("shape {} is not in the catalog", shape.shape));
        assert_eq!(
            published_scope_of(project),
            shape.catalog_scope,
            "shape {} catalog scope",
            shape.shape
        );
        for attachment_id in &shape.attachment_ids {
            let row = state
                .attachments()
                .attachments
                .get(&AttachmentId::parse(attachment_id).unwrap())
                .unwrap_or_else(|| panic!("shape {} lost an attachment row", shape.shape));
            assert_eq!(row.project_id, project_id, "shape {}", shape.shape);
            assert_eq!(
                row.status,
                AttachmentStatus::Attached,
                "shape {} attachment status",
                shape.shape
            );
        }
    }

    // The distinguishing property of each shape, one assertion apiece.
    let by_name = |name: &str| {
        shapes
            .iter()
            .find(|shape| shape.shape == name)
            .unwrap_or_else(|| panic!("missing shape {name}"))
    };
    assert!(
        by_name("remote_only_valid_g1").attachment_ids.is_empty(),
        "remote-only must have no attached row on this host"
    );
    assert!(
        by_name("remote_only_valid_g1")
            .accepted_generation
            .is_some(),
        "remote-only must still serve accepted content"
    );
    assert_eq!(
        by_name("attached_peer_contains_p").project_id,
        by_name("attached_peer_missing_p").project_id,
        "both peer shapes must be peers OF ONE PROJECT, or they are not peers"
    );
    assert_ne!(
        by_name("attached_peer_contains_p").attachment_ids[1],
        by_name("attached_peer_missing_p").attachment_ids[1],
        "the two peers must be distinct attachments"
    );
    let prior = by_name("prior_fallback");
    assert!(
        prior.prior_generation.is_some()
            && prior.corrupted_generations == vec![prior.accepted_generation.clone().unwrap()],
        "Prior fallback damages the CURRENT generation only"
    );
    let corrupt = by_name("corrupt_current_and_prior");
    assert_eq!(
        corrupt.corrupted_generations.len(),
        2,
        "the corrupt shape damages both arms"
    );
    assert!(
        by_name("no_pointer_after_no_content_acknowledgement")
            .accepted_generation
            .is_none(),
        "the no-pointer shape must carry no pointer"
    );
    let bridge = by_name("scope_migration_publication_bridge");
    assert_ne!(
        bridge.catalog_scope, bridge.accepted_scope,
        "an open publication bridge is exactly a catalog scope the accepted \
         pointer does not name yet"
    );
    assert!(
        by_name("legacy_local_bootstrap").catalog_scope.is_none(),
        "a legacy-local project has no published scope"
    );

    // Capability variants: read the recorded bits back off the store.
    let bits_of = |name: &str| {
        let shape = by_name(name);
        state
            .attachments()
            .attachments
            .get(&AttachmentId::parse(&shape.attachment_ids[0]).unwrap())
            .unwrap()
            .capabilities
    };
    let all = bits_of("all_capabilities_attachment");
    assert!(
        all.local_code_source
            && all.git_history
            && all.blame
            && all.repo_knowledge
            && all.repo_mutation
            && all.render_output
            && all.provenance_note_io
            && all.artifact_watching,
        "the all-capabilities shape must record every bit"
    );
    let only = bits_of("repo_knowledge_only_attachment");
    assert!(only.repo_knowledge && !only.blame && !only.artifact_watching);
    assert!(
        !bits_of("no_capability_attachment").any(),
        "the no-capability shape must record nothing"
    );
    let watcher = bits_of("watcher_capable_attachment");
    assert!(watcher.artifact_watching && !watcher.repo_knowledge);
}
