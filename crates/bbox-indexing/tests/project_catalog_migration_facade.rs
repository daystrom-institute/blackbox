use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

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
use bbox_corpus_core::project_catalog::ProjectId;
use bbox_indexing::project_catalog_inventory::{
    ExcludedAttachmentV1, ProjectCatalogMigrationStatusV1, QuarantineCollectedV1,
    RequiredResolutionKindV1, SelectedScopeOwnerV1, decode_migration_report_v1,
    decode_migration_resolution_v1, encode_migration_resolution_v1,
};
use bbox_indexing::project_catalog_migration::{
    ProjectCatalogMigrationApplyOutcomeV1, ProjectCatalogMigrationApplyRequestV1,
    ProjectCatalogMigrationFacadeV1, ProjectCatalogMigrationLayoutOverridesV1,
    ProjectCatalogMigrationPreflightRequestV1, ProjectCatalogMigrationResolvedLayoutV1,
    ProjectCatalogMigrationVerifyRequestV1, project_catalog_migration_store_limits,
};
use bbox_indexing::publisher::PublisherRefStore;
use sha2::{Digest, Sha256};
use tempfile::tempdir;

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

fn config(root: &Path) -> Config {
    let _guard = bbox_util::util::test_env_lock();
    let config_path = root.join("config.toml");
    write(
        &config_path,
        format!("[paths]\nstate_dir = {:?}\n", root.join("protected")).as_bytes(),
    );
    config::load_with(LoadOptions {
        config_path: Some(config_path),
        ..Default::default()
    })
    .unwrap()
}

struct RehearsalFixture {
    winner_checkout: PathBuf,
    loser_checkout: PathBuf,
    winner_project: ProjectId,
    loser_project: ProjectId,
    winner_generation: String,
    loser_generation: String,
    scope: PublishedScope,
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
    write(
        &winner_checkout.join(".bbox/knowledge/context.md"),
        b"# Context\n\nNeutral migration fixture.\n",
    );
    write(
        &winner_checkout.join(".bbox/gaps/open.md"),
        b"# Open question\n\nNo unresolved fixture question.\n",
    );
    git(&winner_checkout, &["add", ".bbox"]);
    git(
        &winner_checkout,
        &["commit", "-qm", "seed migration fixture"],
    );
    let head_commit = git(&winner_checkout, &["rev-parse", "HEAD"]);
    let loser_checkout = root.join("checkouts").join("loser-checkout");
    let clone = Command::new("git")
        .args([
            "clone",
            "-q",
            winner_checkout.to_str().unwrap(),
            loser_checkout.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(
        clone.status.success(),
        "git clone failed: {}",
        String::from_utf8_lossy(&clone.stderr)
    );

    let state = root.join("state");
    fs::create_dir_all(state.join("bro")).unwrap();
    let winner_project = ProjectId::parse("neutral-winner").unwrap();
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
                    "project_id": loser_project,
                    "repo_id": "neutral-repository",
                    "canonical_path": loser_checkout,
                    "registered_at": "2026-01-02T03:04:06Z",
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
    let paths = CodeSourceStorePaths::new(code_sources.root()).unwrap();
    let (winner_generation, winner_selection) =
        write_generation(&paths, &winner_project, &scope, "winner", &head_commit, 1);
    let (loser_generation, loser_selection) =
        write_generation(&paths, &loser_project, &scope, "loser", &head_commit, 2);
    write(
        &paths.anchor(),
        &encode_migration_effective_source_manifest_v1(&MigrationEffectiveSourceManifestV1 {
            version: 1,
            selections: vec![winner_selection, loser_selection],
        })
        .unwrap(),
    );
    let publisher_path = state.join("bro").join("publisher-refs.json");
    let mut publisher = PublisherRefStore::open(&publisher_path).unwrap();
    let pin = publisher.pin_candidate(&scope, &winner_checkout).unwrap();
    publisher.persist_pin_candidate(&pin).unwrap();
    RehearsalFixture {
        winner_checkout,
        loser_checkout,
        winner_project,
        loser_project,
        winner_generation,
        loser_generation,
        scope,
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
        scope: fixture.scope.clone(),
        owner_project_id: fixture.winner_project.clone(),
        losing_project_ids: [fixture.loser_project.clone()].into_iter().collect(),
        owned_aliases: Default::default(),
    });
    let losing_attachment = assessment_report
        .attachments
        .iter()
        .find(|row| row.project_id == fixture.loser_project)
        .unwrap();
    let exclusion = assessment_report
        .required_resolutions
        .iter()
        .find(|row| {
            row.kind == RequiredResolutionKindV1::ExcludeAttachment
                && row
                    .candidate_record_ids
                    .contains(&losing_attachment.observation_id)
        })
        .unwrap();
    resolution.excluded_attachments.push(ExcludedAttachmentV1 {
        resolution_id: exclusion.resolution_id.clone(),
        attachment_id: losing_attachment.attachment_id.clone(),
    });
    let quarantine = assessment_report.activation_conflicts.first().unwrap();
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
    assert_eq!(preflight.receipt.checkout_action_count, 1);
    assert_eq!(preflight.receipt.publisher_pin_count, 1);
    assert_eq!(preflight.receipt.quarantine_root_count, 1);
    assert_eq!(preflight.receipt.attached_project_count, 1);
    assert_eq!(preflight.receipt.omitted_catalog_count, 1);

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

    let verified =
        ProjectCatalogMigrationFacadeV1::verify(ProjectCatalogMigrationVerifyRequestV1 {
            rehearsal_layout: rehearsal.clone(),
        })
        .unwrap();
    assert_eq!(verified.receipt(), &applied.receipt.verification);
    assert_eq!(verified.compatibility().records().len(), 1);
    assert_eq!(verified.compatibility().omitted_catalog_count(), 1);

    let code_source_paths =
        CodeSourceStorePaths::new(rehearsal_root.join("state/code-sources")).unwrap();
    let executable_report = decode_migration_report_v1(&fs::read(&report_path).unwrap()).unwrap();
    let checkout_action = executable_report.checkout_identity_actions.first().unwrap();
    assert_eq!(
        fs::read_to_string(fixture.winner_checkout.join(".bbox/local/checkout-id")).unwrap(),
        format!("{}\n", checkout_action.planned_checkout_id)
    );
    assert!(
        !fixture
            .loser_checkout
            .join(".bbox/local/checkout-id")
            .exists()
    );
    let effective = decode_migration_effective_source_manifest_v1(
        &fs::read(code_source_paths.anchor()).unwrap(),
    )
    .unwrap();
    assert_eq!(effective.selections.len(), 1);
    assert_eq!(effective.selections[0].project_id, fixture.winner_project);
    assert_eq!(
        effective.selections[0].generation_id,
        fixture.winner_generation
    );
    let winner_activation = decode_activation_v2_for_migration(
        &fs::read(code_source_paths.activation(&fixture.winner_project)).unwrap(),
    )
    .unwrap();
    assert_eq!(winner_activation.project_id, fixture.winner_project);
    assert_eq!(winner_activation.published_scope, fixture.scope);
    assert_eq!(winner_activation.generation_id, fixture.winner_generation);
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
    let loser_metadata = decode_stored_generation_v2_for_migration(
        &fs::read(
            code_source_paths
                .generation_metadata(&fixture.scope, &fixture.loser_generation)
                .unwrap(),
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(loser_metadata.published_scope, fixture.scope);
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
            rehearsal_layout: rehearsal,
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
}
