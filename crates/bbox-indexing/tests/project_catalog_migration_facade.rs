use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use bbox_code_source_store::CodeSourceStore;
use bbox_config::config::{self, Config, LoadOptions};
use bbox_corpus_core::identity::PublishedScope;
use bbox_indexing::project_catalog_migration::{
    ProjectCatalogMigrationApplyOutcomeV1, ProjectCatalogMigrationApplyRequestV1,
    ProjectCatalogMigrationFacadeV1, ProjectCatalogMigrationLayoutOverridesV1,
    ProjectCatalogMigrationPreflightRequestV1, ProjectCatalogMigrationResolvedLayoutV1,
    ProjectCatalogMigrationVerifyRequestV1, project_catalog_migration_store_limits,
};
use bbox_indexing::publisher::PublisherRefStore;
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

fn prepare_rehearsal(root: &Path, config: &Config) -> PathBuf {
    let checkout = root.join("checkouts").join("neutral-checkout");
    fs::create_dir_all(&checkout).unwrap();
    git(&checkout, &["init", "-q"]);
    git(&checkout, &["checkout", "-qb", "main"]);
    write(
        &checkout.join(".bbox/config.toml"),
        b"[project]\nrepo_id = \"neutral-repository\"\n",
    );
    write(
        &checkout.join(".bbox/knowledge/context.md"),
        b"# Context\n\nNeutral migration fixture.\n",
    );
    write(
        &checkout.join(".bbox/gaps/open.md"),
        b"# Open question\n\nNo unresolved fixture question.\n",
    );
    git(&checkout, &["add", ".bbox"]);
    git(&checkout, &["commit", "-qm", "seed migration fixture"]);

    let state = root.join("state");
    fs::create_dir_all(state.join("bro")).unwrap();
    write(
        &state.join("projects.json"),
        serde_json::to_vec_pretty(&serde_json::json!({
            "version": 1,
            "projects": [{
                "project_id": "neutral-project",
                "repo_id": "neutral-repository",
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
    let _code_sources = CodeSourceStore::open(
        state.join("code-sources"),
        project_catalog_migration_store_limits(config),
    )
    .unwrap();
    let scope = PublishedScope::try_new("neutral-repository", ".").unwrap();
    let publisher_path = state.join("bro").join("publisher-refs.json");
    let mut publisher = PublisherRefStore::open(&publisher_path).unwrap();
    let pin = publisher.pin_candidate(&scope, &checkout).unwrap();
    publisher.persist_pin_candidate(&pin).unwrap();
    checkout
}

#[test]
fn external_consumer_runs_exact_review_apply_fresh_verify_and_reapply() {
    let directory = tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap();
    let config = config(&root);
    let rehearsal_root = root.join("rehearsal");
    fs::create_dir_all(&rehearsal_root).unwrap();
    let checkout = prepare_rehearsal(&rehearsal_root, &config);
    assert!(
        !checkout.join(".bbox/local/checkout-id").exists(),
        "fixture must begin with a markerless checkout"
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
        bbox_indexing::project_catalog_inventory::ProjectCatalogMigrationStatusV1::Clean
    );
    assert_eq!(preflight.receipt.checkout_action_count, 1);
    assert_eq!(preflight.receipt.publisher_pin_count, 1);
    assert_eq!(preflight.receipt.attached_project_count, 1);

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
    assert_eq!(verified.compatibility().omitted_catalog_count(), 0);

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
