//! Genesis facade coverage: the greenfield route into catalog mode.
//!
//! Every test resolves its layout from an isolated `state_dir` override, so
//! the census reads the tempdir's owner roots and never the host's real
//! index, vector store, or coordination stores.

use std::fs;
use std::path::{Path, PathBuf};

use bbox_config::config::{self, Config, LoadOptions};
use bbox_corpus_core::project_catalog::{
    CatalogOriginV2, LegacyProjectRecordV1, LegacyProjectStoreV1,
};
use bbox_indexing::project_catalog_genesis::{
    GenesisOwnerDispositionV1, ProjectCatalogGenesisFacadeV1, ProjectCatalogGenesisRequestV1,
};
use bbox_indexing::project_catalog_migration::{
    ProjectCatalogMigrationLayoutOverridesV1, ProjectCatalogMigrationResolvedLayoutV1,
};
use bbox_indexing::project_catalog_store::{
    ProjectCatalogStore, ProjectStoreProbe, legacy_pre_genesis_retention_path,
    probe_project_store_mode,
};
use tempfile::tempdir;

fn write(path: &Path, bytes: &[u8]) {
    fs::create_dir_all(path.parent().expect("fixture path has a parent")).unwrap();
    fs::write(path, bytes).unwrap();
}

/// One isolated configuration whose every owner root lives under `root`.
///
/// `vectors_dir` is explicit for the same reason the migration fixtures make
/// it explicit: the vector root otherwise resolves to the PLATFORM state
/// directory, and the census would then read the host's real vector store.
fn config(root: &Path) -> Config {
    let _guard = bbox_util::util::test_env_lock();
    let config_path = root.join("config.toml");
    write(
        &config_path,
        format!(
            "[paths]\nstate_dir = {:?}\nvectors_dir = {:?}\n",
            root.join("state"),
            root.join("state").join("vectors")
        )
        .as_bytes(),
    );
    config::load_with(LoadOptions {
        config_path: Some(config_path),
        ..Default::default()
    })
    .unwrap()
}

fn layout(root: &Path, config: &Config) -> ProjectCatalogMigrationResolvedLayoutV1 {
    ProjectCatalogMigrationResolvedLayoutV1::from_config(
        config,
        ProjectCatalogMigrationLayoutOverridesV1 {
            projects_path: None,
            state_dir: Some(root.join("state")),
        },
    )
    .unwrap()
}

fn genesis(
    root: &Path,
) -> Result<
    bbox_indexing::project_catalog_genesis::ProjectCatalogGenesisReceiptV1,
    bbox_indexing::project_catalog_genesis::ProjectCatalogGenesisError,
> {
    let config = config(root);
    let target_layout = layout(root, &config);
    ProjectCatalogGenesisFacadeV1::initialize(ProjectCatalogGenesisRequestV1 { target_layout })
        .map(|result| result.receipt)
}

/// One version-1 legacy project store, encoded through the shipped type so a
/// schema change cannot leave the fixture silently undecodable (which the
/// census would report as unprobeable rather than as the occupancy the test
/// means to exercise).
fn write_legacy_store(root: &Path, projects: Vec<LegacyProjectRecordV1>) {
    let store = LegacyProjectStoreV1 {
        version: 1,
        projects,
    };
    write(&projects_path(root), &serde_json::to_vec(&store).unwrap());
}

/// One central knowledge row carrying a literal project selector, which is
/// exactly the row shape the owner capture counts.
fn write_project_scoped_knowledge_entry(root: &Path) {
    use bbox_knowledge::knowledge::{
        Approval, Category, KnowledgeEntry, KnowledgeStore, Priority, Scope, Status,
    };

    let mut store = KnowledgeStore::new();
    store.entries.push(KnowledgeEntry {
        id: "k0000001".into(),
        title: "scoped row".into(),
        content: "scoped row".into(),
        cluster: None,
        variants: Default::default(),
        category: Category::Convention,
        scope: Scope::Project,
        project: Some(root.join("absent-checkout").display().to_string()),
        project_id: None,
        providers: Vec::new(),
        priority: Priority::Standard,
        weight: 1,
        status: Status::Active,
        approval: Approval::UserConfirmed,
        render: true,
        decay: true,
        review_at: None,
        supersedes: None,
        links: Vec::new(),
        rationale: None,
        expires_at: None,
        source: "fixture".into(),
        created_at: "2026-08-08T00:00:00Z".into(),
        updated_at: "2026-08-08T00:00:00Z".into(),
        recall_count: 0,
        last_recalled: None,
    });
    write(
        &state(root).join("blackbox-knowledge.json"),
        &serde_json::to_vec(&store).unwrap(),
    );
}

fn state(root: &Path) -> PathBuf {
    root.join("state")
}

fn projects_path(root: &Path) -> PathBuf {
    state(root).join("projects.json")
}

/// Genesis on a bundle that has never held project state writes the fresh-v2
/// pair and nothing else.
///
/// The marker assertion is the load-bearing one. A `FreshV2` catalog carrying
/// a migration marker is refused by strict pair open, so genesis writing one
/// would produce a store that could never be opened again.
#[test]
fn genesis_writes_the_fresh_v2_pair_and_no_migration_marker() {
    let directory = tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap();

    let receipt = genesis(&root).unwrap();
    assert_eq!(receipt.catalog_origin, "fresh_v2");
    assert_eq!(receipt.epoch, 1);
    assert!(!receipt.set_aside_empty_legacy_store);
    assert!(
        receipt
            .owner_census
            .iter()
            .all(|row| row.disposition == GenesisOwnerDispositionV1::Empty),
        "{:?}",
        receipt.owner_census
    );

    let store = ProjectCatalogStore::open_existing(projects_path(&root)).unwrap();
    let snapshot = store.snapshot().unwrap();
    assert!(matches!(
        snapshot.catalog().origin,
        CatalogOriginV2::FreshV2 {}
    ));
    assert!(snapshot.catalog().projects.is_empty());
    assert!(snapshot.catalog().repo_histories.is_empty());
    assert!(snapshot.attachments().attachments.is_empty());
    assert_eq!(snapshot.epoch(), 1);
    assert_eq!(snapshot.catalog_sha256(), receipt.catalog_sha256);
    assert_eq!(snapshot.attachments_sha256(), receipt.attachments_sha256);

    let catalog_state = state(&root);
    assert!(
        !catalog_state
            .join("project-catalog-migration.json")
            .exists()
    );
    assert!(
        !catalog_state
            .join("project-catalog-migration-receipt.json")
            .exists()
    );
    assert!(
        !catalog_state
            .join("project-catalog-migration-assets")
            .exists()
    );
}

/// The catalog and attachment images genesis installs are BYTE-identical to
/// the ones the library initializer installs.
///
/// This is the store-shape contract that lets every consumer already proved
/// against an `initialize_empty` store (the daemon-side catalog fixtures, and
/// through them the collector-backchannel onboarding admission path) transfer
/// to a genesis store without a second proof: the two are the same pair.
#[test]
fn genesis_installs_the_same_pair_as_the_library_initializer() {
    let genesis_directory = tempdir().unwrap();
    let genesis_root = genesis_directory.path().canonicalize().unwrap();
    genesis(&genesis_root).unwrap();

    let library_directory = tempdir().unwrap();
    let library_root = library_directory.path().canonicalize().unwrap();
    let library_state = library_root.join("state");
    fs::create_dir_all(&library_state).unwrap();
    drop(ProjectCatalogStore::initialize_empty(library_state.join("projects.json")).unwrap());

    for name in ["projects.json", "project-attachments.json"] {
        assert_eq!(
            fs::read(state(&genesis_root).join(name)).unwrap(),
            fs::read(library_state.join(name)).unwrap(),
            "{name} differs between the genesis and library initializers"
        );
    }
}

/// Daemon startup selects catalog mode over a genesis store exactly as it
/// would over any other version-2 store: the probe is a version read, and the
/// strict open that follows it is origin-aware rather than origin-specific.
#[test]
fn daemon_startup_probe_selects_catalog_mode_over_a_genesis_store() {
    let directory = tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap();
    genesis(&root).unwrap();

    assert_eq!(
        probe_project_store_mode(&projects_path(&root)).unwrap(),
        ProjectStoreProbe::CatalogV2
    );
    // The open startup performs after the probe. It runs journal recovery and
    // the origin/marker binding check, which is where a wrong genesis shape
    // would surface.
    let store = ProjectCatalogStore::open_existing(projects_path(&root)).unwrap();
    assert_eq!(store.snapshot().unwrap().epoch(), 1);
}

/// A second genesis refuses instead of reinitializing over live authority.
#[test]
fn genesis_refuses_an_existing_catalog() {
    let directory = tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap();
    genesis(&root).unwrap();

    let error = genesis(&root).unwrap_err();
    assert_eq!(error.code, "error.project_catalog_genesis_catalog_exists");
}

/// A legacy store that registers projects is migration input. Genesis refuses
/// it by name rather than replacing it, which is what keeps genesis from being
/// a migration bypass.
#[test]
fn genesis_refuses_a_legacy_store_that_registers_projects() {
    let directory = tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap();
    write_legacy_store(
        &root,
        vec![LegacyProjectRecordV1 {
            project_id: "p_legacy_one00000".into(),
            repo_id: None,
            canonical_path: root.join("absent-checkout").display().to_string(),
            registered_at: "2026-08-08T00:00:00Z".into(),
            is_git_repo: false,
            languages: Default::default(),
            aliases: Default::default(),
        }],
    );

    let error = genesis(&root).unwrap_err();
    assert_eq!(error.code, "error.project_catalog_genesis_owner_not_empty");
    assert!(
        error.message.contains("legacy-projects"),
        "{}",
        error.message
    );
    // Nothing was written: the refusal runs before the initializer.
    assert_eq!(
        probe_project_store_mode(&projects_path(&root)).unwrap(),
        ProjectStoreProbe::LegacyV1
    );
}

/// The one legacy pre-state genesis admits: a bridge daemon that booted and
/// registered nothing. It carries no identity to migrate, and refusing it
/// would leave hand-deleting a state file as the only route forward.
#[test]
fn genesis_sets_aside_a_legacy_store_that_registers_nothing() {
    let directory = tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap();
    write_legacy_store(&root, Vec::new());

    let receipt = genesis(&root).unwrap();
    assert!(receipt.set_aside_empty_legacy_store);
    assert_eq!(receipt.catalog_origin, "fresh_v2");
    assert_eq!(
        probe_project_store_mode(&projects_path(&root)).unwrap(),
        ProjectStoreProbe::CatalogV2
    );
    // Set aside, never destroyed: the legacy bytes stay beside the catalog
    // under the retention sibling.
    let retained = legacy_pre_genesis_retention_path(&projects_path(&root)).unwrap();
    let decoded: LegacyProjectStoreV1 =
        serde_json::from_slice(&fs::read(&retained).unwrap()).unwrap();
    assert_eq!(decoded.version, 1);
    assert!(decoded.projects.is_empty());
}

/// A durable coordination owner holding project-scoped rows refuses, and the
/// refusal names the store so the operator knows which one to look at.
#[test]
fn genesis_refuses_a_knowledge_store_holding_project_scoped_rows() {
    let directory = tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap();
    write_project_scoped_knowledge_entry(&root);

    let error = genesis(&root).unwrap_err();
    assert_eq!(error.code, "error.project_catalog_genesis_owner_not_empty");
    assert!(
        error.message.contains("knowledge-rows"),
        "{}",
        error.message
    );
    assert!(!projects_path(&root).exists());
}

/// An owner store that cannot be read is a refusal, never a zero. Reading an
/// unreadable store as empty is exactly how a bundle with live project state
/// would slip past the census.
#[test]
fn genesis_refuses_an_owner_store_it_cannot_read() {
    let directory = tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap();
    write(
        &state(&root).join("blackbox-knowledge.json"),
        b"{ this is not json",
    );

    let error = genesis(&root).unwrap_err();
    assert_eq!(
        error.code,
        "error.project_catalog_genesis_owner_unprobeable"
    );
    assert!(
        error.message.contains("knowledge-rows"),
        "{}",
        error.message
    );
    assert!(!projects_path(&root).exists());
}

/// Catalog-owned state without a catalog is half-pair state. Genesis refuses
/// it rather than minting a pair beside artifacts it cannot account for.
#[test]
fn genesis_refuses_a_bundle_carrying_catalog_owned_state() {
    let directory = tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap();
    write(
        &state(&root).join("project-attachments.json"),
        br#"{"version":1,"epoch":1,"attachments":{},"legacy_path_bindings":{},"scope_migration_proofs":{}}"#,
    );

    let error = genesis(&root).unwrap_err();
    // The startup probe owns the half-pair rule and refuses first; either
    // refusal is correct here, and both name the artifact class.
    assert!(
        error.code == "error.project_catalog_half_pair"
            || error.code == "error.project_catalog_genesis_catalog_state_present",
        "{error}"
    );
    assert!(!projects_path(&root).exists());
}

/// A migration marker beside an absent catalog refuses for the same reason.
#[test]
fn genesis_refuses_a_bundle_carrying_a_migration_marker() {
    let directory = tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap();
    write(
        &state(&root).join("project-catalog-migration.json"),
        br#"{"version":1}"#,
    );

    let error = genesis(&root).unwrap_err();
    assert!(
        error.code == "error.project_catalog_half_pair"
            || error.code == "error.project_catalog_genesis_catalog_state_present",
        "{error}"
    );
    assert!(!projects_path(&root).exists());
}
