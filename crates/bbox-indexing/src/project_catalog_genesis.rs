//! Genesis: explicit initialization of an empty catalog-v2 store on a state
//! bundle that has never held project state.
//!
//! # Why this exists
//!
//! The durable-catalog plan always described two ways a version-2 store comes
//! into being. The parent plan's origin section names them directly: "A fresh
//! v2 initializer writes `FreshV2`. The v1 importer writes
//! `MigratedV1 { transaction_id }`." Only the importer ever got an operator
//! surface. `ProjectCatalogStore::initialize_empty` existed as a library
//! entry, used exclusively by tests, so on a real host the ONLY documented
//! route into catalog mode ran through `project-catalog migrate`.
//!
//! That route is closed to a fresh bundle by construction, not by accident.
//! Migration preflight inventories the owner stores through ten immutable
//! lanes; on a bundle where the corpus index, vector root, edge manifests, and
//! Git cursor directory have never been written, six of those lanes
//! (`project-scoped-refs`, `edge-workspaces`, `git-metadata`,
//! `legacy-path-observations`, `repo-grouping-proofs`,
//! `legacy-namespace-clusters`) capture as `Missing`, each becomes an
//! `immutable_lane_missing` hard refusal, the report is not `Clean`, and
//! `apply --configured` refuses with
//! `error.project_catalog_migration_report_not_clean`. A greenfield deployment
//! therefore could not reach the split topology at all.
//!
//! # What genesis writes, and why it is not a synthesized migration
//!
//! Genesis does NOT fabricate an empty migration. It cannot: strict pair open
//! refuses a `FreshV2` catalog that carries a migration marker
//! ("fresh v2 catalog unexpectedly has a migration marker") and refuses a
//! `MigratedV1` catalog that lacks one. The two origins are a deliberate
//! discriminant, so "indistinguishable from a migrated store" is not an
//! available shape, and every downstream consumer already branches on it:
//! sweep planning exempts a fresh origin from marker-driven exclusions,
//! rebuild planning reads no predecessor fingerprint from it, backfill scopes
//! itself to migrated origins, and the Git transport parity proof classifies
//! it `VacuousFreshV2`. The six inventory lanes have no durable representation
//! in the catalog pair at all: they are migration EVIDENCE about legacy owner
//! stores, retained in the marker and immutable assets, which a fresh origin
//! legitimately has none of.
//!
//! So the sound empty representation is the one the plan already specified:
//! the `FreshV2` pair at epoch one, with no marker and no immutable assets.
//! Genesis is the operator surface for it, plus the refusals that keep it from
//! becoming a migration bypass.
//!
//! # The refusal
//!
//! Genesis proves the bundle is fresh before it writes anything:
//!
//! 1. no catalog-family artifact may exist (a v2 catalog, an attachment
//!    snapshot, a transaction journal, a migration marker or receipt, the
//!    immutable-asset root, the stage or backup roots, or the
//!    accepted-publication root). These are written only by catalog
//!    authority, so any presence means this bundle is not fresh;
//! 2. every legacy owner store the migration inventory reads must hold zero
//!    project-scoped rows. The owner set comes from
//!    [`crate::project_catalog_migration::owner_inventory_paths`], the exact
//!    function the migration capture uses, so genesis can never prove a bundle
//!    empty by looking at stores migration would have opened;
//! 3. an owner that cannot be read is a refusal, never a zero. An unprobeable
//!    store may hold anything, and treating it as empty is precisely how a
//!    bypass would slip through.
//!
//! One owner the migration inventory reads is deliberately NOT censused: the
//! legacy publisher-ref store. It is Phase 6 deletion inventory, a publisher
//! pin binds a published scope and so cannot exist on a bundle carrying no
//! published project, and every bundle that carries one is already refused by
//! the rows above. Taking a new dependency on a store scheduled for removal
//! would buy nothing and add to what the ownership ratchet must eventually
//! delete.
//!
//! The one legacy artifact genesis tolerates is a version-1 project store
//! registering zero projects: a bridge daemon that booted and registered
//! nothing leaves exactly that file, it carries no identity for a migration to
//! carry across, and refusing it would leave hand-deleting a state file as the
//! only way forward. It is re-read under the exclusive claim before the write
//! (see `ProjectCatalogStore::initialize_empty_over_fresh_bundle`).

use std::collections::BTreeSet;

use bbox_corpus_core::project_catalog::CatalogOriginV2;
use bbox_corpus_core::project_catalog_snapshot::{
    OwnerSnapshotStateV1, OwnerSnapshotV1, capture_legacy_proposal_owner_snapshot,
    capture_legacy_task_owner_snapshot,
};
use bbox_corpus_index::index::migration_inventory as corpus_inventory;
use bbox_edge_sidecar::migration_inventory as edge_inventory;
use bbox_vectors::migration_inventory as vector_inventory;
use serde::Serialize;

use crate::project_catalog_inventory_adapters::ProjectCatalogOwnerInventoryLimitsV1;
use crate::project_catalog_migration::{
    ProjectCatalogMigrationResolvedLayoutV1, owner_inventory_paths,
};
use crate::project_catalog_store::{
    ProjectCatalogStore, ProjectStoreProbe, probe_project_store_mode,
};

const RECEIPT_VERSION_V1: u32 = 1;

/// One path-redacted error boundary for the genesis operation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectCatalogGenesisError {
    pub code: &'static str,
    pub message: String,
}

impl std::fmt::Display for ProjectCatalogGenesisError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}: {}", self.code, self.message)
    }
}

impl std::error::Error for ProjectCatalogGenesisError {}

impl From<crate::project_catalog_migration::ProjectCatalogMigrationError>
    for ProjectCatalogGenesisError
{
    fn from(error: crate::project_catalog_migration::ProjectCatalogMigrationError) -> Self {
        Self {
            code: error.code,
            message: error.message,
        }
    }
}

impl From<crate::project_catalog_store::ProjectCatalogStoreError> for ProjectCatalogGenesisError {
    fn from(error: crate::project_catalog_store::ProjectCatalogStoreError) -> Self {
        Self {
            code: error.code(),
            message: error.to_string(),
        }
    }
}

type GenesisResult<T> = Result<T, ProjectCatalogGenesisError>;

fn error(code: &'static str, message: impl std::fmt::Display) -> ProjectCatalogGenesisError {
    ProjectCatalogGenesisError {
        code,
        message: message.to_string(),
    }
}

/// What one owner census entry observed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "disposition", rename_all = "snake_case")]
pub enum GenesisOwnerDispositionV1 {
    /// The store is absent, or present and holding no project-scoped rows.
    Empty,
    /// The store holds rows a migration would have to carry across.
    NotEmpty { row_count: u64 },
    /// The store exists but could not be read as evidence. Never a zero.
    Unprobeable { diagnostic_code: String },
}

/// One legacy owner store, named so a refusal is actionable.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct GenesisOwnerCensusRowV1 {
    pub owner_id: &'static str,
    #[serde(flatten)]
    pub disposition: GenesisOwnerDispositionV1,
}

/// The successful genesis receipt.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ProjectCatalogGenesisReceiptV1 {
    pub version: u32,
    /// Always `fresh_v2`. Recorded rather than implied so an operator reading
    /// the receipt can see which origin the store was born with.
    pub catalog_origin: &'static str,
    pub epoch: u64,
    pub catalog_sha256: String,
    pub attachments_sha256: String,
    /// True when a version-1 project store registering zero projects was set
    /// aside under a `.pre-genesis` sibling rather than found absent.
    pub set_aside_empty_legacy_store: bool,
    pub owner_census: Vec<GenesisOwnerCensusRowV1>,
}

/// Genesis operates on ONE resolved layout, the same authority bundle the
/// migration facade takes. There is no rehearsal/configured pair here: genesis
/// has nothing to protect a second layout from, because it reads the owner
/// stores of exactly the bundle it initializes and writes nowhere else.
pub struct ProjectCatalogGenesisRequestV1 {
    pub target_layout: ProjectCatalogMigrationResolvedLayoutV1,
}

pub struct ProjectCatalogGenesisResultV1 {
    pub receipt: ProjectCatalogGenesisReceiptV1,
}

/// The only public genesis authority.
pub struct ProjectCatalogGenesisFacadeV1;

impl ProjectCatalogGenesisFacadeV1 {
    pub fn initialize(
        request: ProjectCatalogGenesisRequestV1,
    ) -> GenesisResult<ProjectCatalogGenesisResultV1> {
        let layout = request.target_layout;
        layout.validate()?;

        let set_aside_empty_legacy_store = classify_catalog_pre_state(&layout)?;
        refuse_catalog_family_artifacts(&layout)?;
        let census = census_owner_stores(&layout);
        refuse_non_fresh_census(&census)?;

        // The parent directory of the projects path must exist before the
        // lifetime lock file can be created beside it. Creating it is the one
        // filesystem write genesis performs before the pair transaction, and
        // it is idempotent.
        if let Some(parent) = layout.projects_path().parent() {
            std::fs::create_dir_all(parent).map_err(|io| {
                error(
                    "error.project_catalog_genesis_state_root_unwritable",
                    format!("the projects-store directory could not be created: {io}"),
                )
            })?;
        }

        let store =
            ProjectCatalogStore::initialize_empty_over_fresh_bundle(layout.projects_path())?;
        let state = store.snapshot()?;
        // Asserted rather than assumed: the receipt claims a fresh origin, and
        // a receipt that could claim it over migrated bytes would be worthless
        // as onboarding evidence.
        if !matches!(state.catalog().origin, CatalogOriginV2::FreshV2 {}) {
            return Err(error(
                "error.project_catalog_genesis_origin_unexpected",
                "the initialized catalog does not carry the fresh-v2 origin",
            ));
        }

        Ok(ProjectCatalogGenesisResultV1 {
            receipt: ProjectCatalogGenesisReceiptV1 {
                version: RECEIPT_VERSION_V1,
                catalog_origin: "fresh_v2",
                epoch: state.epoch(),
                catalog_sha256: state.catalog_sha256().to_string(),
                attachments_sha256: state.attachments_sha256().to_string(),
                set_aside_empty_legacy_store,
                owner_census: census,
            },
        })
    }
}

/// Decide whether the catalog path already carries authority, and report
/// whether an empty legacy store will be replaced.
///
/// The version probe is the same one daemon startup runs, so genesis and the
/// daemon can never disagree about what is at the projects path.
fn classify_catalog_pre_state(
    layout: &ProjectCatalogMigrationResolvedLayoutV1,
) -> GenesisResult<bool> {
    match probe_project_store_mode(layout.projects_path()) {
        Ok(ProjectStoreProbe::AbsentBridge) => Ok(false),
        Ok(ProjectStoreProbe::LegacyV1) => Ok(true),
        Ok(ProjectStoreProbe::CatalogV2) => Err(error(
            "error.project_catalog_genesis_catalog_exists",
            "a version-2 project catalog already exists at the configured projects path; \
             genesis initializes a new store and never replaces an existing catalog",
        )),
        Err(store_error) => Err(store_error.into()),
    }
}

/// Every artifact only catalog authority ever writes must be absent.
///
/// Presence is decided by `symlink_metadata`, so a dangling symlink at one of
/// these roles counts as present. That is the fail-closed direction: a symlink
/// there is state genesis cannot vouch for either way.
fn refuse_catalog_family_artifacts(
    layout: &ProjectCatalogMigrationResolvedLayoutV1,
) -> GenesisResult<()> {
    let roles = layout.catalog_family_artifacts_for_genesis();
    let present = roles
        .iter()
        .filter(|(_, path)| path.symlink_metadata().is_ok())
        .map(|(role, _)| *role)
        .collect::<Vec<_>>();
    if present.is_empty() {
        return Ok(());
    }
    Err(error(
        "error.project_catalog_genesis_catalog_state_present",
        format!(
            "the bundle already carries catalog-owned state ({}); genesis requires a bundle \
             that has never held catalog authority",
            present.join(", ")
        ),
    ))
}

fn refuse_non_fresh_census(census: &[GenesisOwnerCensusRowV1]) -> GenesisResult<()> {
    let unprobeable = census
        .iter()
        .filter_map(|row| match &row.disposition {
            GenesisOwnerDispositionV1::Unprobeable { diagnostic_code } => {
                Some(format!("{} ({diagnostic_code})", row.owner_id))
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    if !unprobeable.is_empty() {
        return Err(error(
            "error.project_catalog_genesis_owner_unprobeable",
            format!(
                "these owner stores could not be read and may hold project state: {}; \
                 an unreadable store is never counted as empty",
                unprobeable.join(", ")
            ),
        ));
    }
    let occupied = census
        .iter()
        .filter_map(|row| match &row.disposition {
            GenesisOwnerDispositionV1::NotEmpty { row_count } => {
                Some(format!("{} ({row_count} row(s))", row.owner_id))
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    if !occupied.is_empty() {
        return Err(error(
            "error.project_catalog_genesis_owner_not_empty",
            format!(
                "these legacy owner stores hold project-scoped rows: {}; that state is \
                 migration input, so run `project-catalog migrate` instead of genesis",
                occupied.join(", ")
            ),
        ));
    }
    Ok(())
}

/// The complete owner census, in a stable order so two runs against the same
/// bundle produce byte-identical receipts.
fn census_owner_stores(
    layout: &ProjectCatalogMigrationResolvedLayoutV1,
) -> Vec<GenesisOwnerCensusRowV1> {
    let owners = owner_inventory_paths(layout);
    let limits = ProjectCatalogOwnerInventoryLimitsV1::default();
    let mut census = Vec::new();

    census.push(row("legacy-projects", legacy_projects(layout)));

    // The durable coordination owners, each read through the SAME no-create
    // capture the migration inventory calls. `row_count` counts rows carrying
    // a legacy project selector, which is exactly the set a migration would
    // have to stamp; a store holding only unscoped rows is empty here, and
    // correctly so, because unscoped rows need no project identity.
    for (owner_id, snapshot) in [
        (
            "knowledge-rows",
            bbox_knowledge::knowledge::capture_project_catalog_owner_snapshot(
                &owners.knowledge_store_path,
                limits.durable_owners,
            ),
        ),
        (
            "gap-rows",
            bbox_gaps::gaps::capture_project_catalog_owner_snapshot(
                &owners.gap_store_path,
                limits.durable_owners,
            ),
        ),
        (
            "thread-rows",
            bbox_threads::threads::capture_project_catalog_owner_snapshot(
                &owners.thread_store_path,
                limits.durable_owners,
            ),
        ),
        (
            "note-rows",
            bbox_threads::notes::capture_project_catalog_owner_snapshot(
                &owners.note_store_path,
                limits.durable_owners,
            ),
        ),
        (
            "pin-rows",
            bbox_stores::pins::capture_project_catalog_owner_snapshot(
                &owners.pin_store_path,
                limits.durable_owners,
            ),
        ),
        (
            "roadmap-rows",
            bbox_stores::roadmap::capture_project_catalog_owner_snapshot(
                &owners.roadmap_store_path,
                limits.durable_owners,
            ),
        ),
        (
            "packet-rows",
            bbox_packets::capture_project_catalog_owner_snapshot(
                &owners.packet_root,
                limits.durable_owners,
            ),
        ),
        (
            "task-rows",
            capture_legacy_task_owner_snapshot(&owners.task_store_path, limits.durable_owners),
        ),
        (
            "proposal-rows",
            capture_legacy_proposal_owner_snapshot(&owners.proposal_root, limits.durable_owners),
        ),
        (
            "slack-channel-binding-rows",
            bbox_slack::slack_channel_bindings::capture_project_catalog_owner_snapshot(
                &owners.slack_store_root,
                limits.durable_owners,
            ),
        ),
        (
            "slack-proposal-link-rows",
            bbox_slack::slack_proposal_links::capture_project_catalog_owner_snapshot(
                &owners.slack_store_root,
                limits.durable_owners,
            ),
        ),
        (
            "whiteboard-rows",
            bbox_whiteboards::whiteboards::capture_project_catalog_owner_snapshot(
                &owners.whiteboard_root,
                limits.durable_owners,
            ),
        ),
        (
            "artifact-rows",
            bbox_artifacts::artifacts::capture_project_catalog_owner_snapshot(
                &owners.artifact_root,
                limits.durable_owners,
            ),
        ),
        (
            "transcript-edge-rows",
            bbox_edge_sidecar::edge_sidecar::capture_project_catalog_owner_snapshot(
                &owners.edge_root,
                limits.durable_owners,
            ),
        ),
    ] {
        census.push(row(owner_id, owner_snapshot_disposition(snapshot)));
    }

    let corpus = corpus_inventory::capture_owner_migration_snapshot_no_create(
        &owners.corpus_index_root,
        &owners.git_cursor_root,
        limits.corpus,
    );
    census.push(row(
        "index-project-scoped-refs",
        corpus_disposition(&corpus.index.state, || {
            corpus.index.project_scoped_refs.len() as u64
        }),
    ));
    census.push(row(
        "index-code-metadata-rows",
        corpus_disposition(&corpus.code_metadata.state, || {
            corpus.code_metadata.rows.len() as u64
        }),
    ));
    census.push(row(
        "git-ingest-cursors",
        corpus_disposition(&corpus.git_cursors.state, || corpus.git_cursors.row_count),
    ));

    let vectors =
        vector_inventory::capture_migration_snapshot_no_create(&owners.vector_root, limits.vectors);
    census.push(row(
        "vector-project-scoped-refs",
        match &vectors.state {
            vector_inventory::VectorMigrationSourceStateV1::Missing => {
                GenesisOwnerDispositionV1::Empty
            }
            vector_inventory::VectorMigrationSourceStateV1::Present => {
                count_disposition(vectors.project_scoped_refs.len() as u64)
            }
            vector_inventory::VectorMigrationSourceStateV1::Corrupt { diagnostic_code }
            | vector_inventory::VectorMigrationSourceStateV1::Unavailable { diagnostic_code } => {
                GenesisOwnerDispositionV1::Unprobeable {
                    diagnostic_code: (*diagnostic_code).to_string(),
                }
            }
        },
    ));

    let edges =
        edge_inventory::capture_migration_snapshot_no_create(&owners.edge_root, limits.edges);
    census.push(row(
        "edge-workspaces",
        match &edges.state {
            edge_inventory::EdgeMigrationSourceStateV1::Missing => GenesisOwnerDispositionV1::Empty,
            edge_inventory::EdgeMigrationSourceStateV1::Present => {
                count_disposition(edges.workspace_count)
            }
            edge_inventory::EdgeMigrationSourceStateV1::Corrupt { diagnostic_code }
            | edge_inventory::EdgeMigrationSourceStateV1::Unavailable { diagnostic_code } => {
                GenesisOwnerDispositionV1::Unprobeable {
                    diagnostic_code: (*diagnostic_code).to_string(),
                }
            }
        },
    ));

    census.push(row("code-source-generations", code_sources(layout)));
    census
}

fn row(owner_id: &'static str, disposition: GenesisOwnerDispositionV1) -> GenesisOwnerCensusRowV1 {
    GenesisOwnerCensusRowV1 {
        owner_id,
        disposition,
    }
}

fn count_disposition(row_count: u64) -> GenesisOwnerDispositionV1 {
    if row_count == 0 {
        GenesisOwnerDispositionV1::Empty
    } else {
        GenesisOwnerDispositionV1::NotEmpty { row_count }
    }
}

fn owner_snapshot_disposition(
    snapshot: Result<
        OwnerSnapshotV1,
        bbox_corpus_core::project_catalog_snapshot::OwnerSnapshotError,
    >,
) -> GenesisOwnerDispositionV1 {
    let snapshot = match snapshot {
        Ok(snapshot) => snapshot,
        Err(snapshot_error) => {
            return GenesisOwnerDispositionV1::Unprobeable {
                diagnostic_code: snapshot_error.code.to_string(),
            };
        }
    };
    // A subsource that failed to read makes the whole owner unprobeable even
    // when the aggregate row list came back empty: the empty list is then a
    // consequence of the failure, not evidence of an empty store.
    if let Some(diagnostic_code) = std::iter::once(&snapshot.state)
        .chain(snapshot.subsources.iter().map(|subsource| &subsource.state))
        .find_map(|state| match state {
            OwnerSnapshotStateV1::Corrupt {
                diagnostic_code, ..
            } => Some(diagnostic_code.clone()),
            _ => None,
        })
    {
        return GenesisOwnerDispositionV1::Unprobeable { diagnostic_code };
    }
    count_disposition(snapshot.row_count)
}

fn corpus_disposition(
    state: &corpus_inventory::CorpusMigrationSourceStateV1,
    count: impl FnOnce() -> u64,
) -> GenesisOwnerDispositionV1 {
    match state {
        corpus_inventory::CorpusMigrationSourceStateV1::Missing => GenesisOwnerDispositionV1::Empty,
        corpus_inventory::CorpusMigrationSourceStateV1::Present => count_disposition(count()),
        corpus_inventory::CorpusMigrationSourceStateV1::Corrupt { diagnostic_code }
        | corpus_inventory::CorpusMigrationSourceStateV1::Unavailable { diagnostic_code } => {
            GenesisOwnerDispositionV1::Unprobeable {
                diagnostic_code: (*diagnostic_code).to_string(),
            }
        }
    }
}

/// The legacy project registry itself, which is the identity source migration
/// exists to carry across.
///
/// Read through the store's own strict decoder rather than a local parse: a
/// file that decodes as neither an absent store nor a valid legacy store is
/// unprobeable, and must not be read as "no projects registered".
fn legacy_projects(layout: &ProjectCatalogMigrationResolvedLayoutV1) -> GenesisOwnerDispositionV1 {
    let path = layout.projects_path();
    match std::fs::symlink_metadata(path) {
        Err(io) if io.kind() == std::io::ErrorKind::NotFound => {
            return GenesisOwnerDispositionV1::Empty;
        }
        Err(_) => {
            return GenesisOwnerDispositionV1::Unprobeable {
                diagnostic_code: "legacy_project_store_metadata_unavailable".to_string(),
            };
        }
        Ok(metadata) if !metadata.is_file() => {
            return GenesisOwnerDispositionV1::Unprobeable {
                diagnostic_code: "legacy_project_store_not_regular_file".to_string(),
            };
        }
        Ok(_) => {}
    }
    let Ok(bytes) = std::fs::read(path) else {
        return GenesisOwnerDispositionV1::Unprobeable {
            diagnostic_code: "legacy_project_store_unreadable".to_string(),
        };
    };
    match bbox_corpus_core::project_catalog::decode_legacy_project_store(&bytes) {
        Ok(store) => count_disposition(store.projects.len() as u64),
        Err(_) => GenesisOwnerDispositionV1::Unprobeable {
            diagnostic_code: "legacy_project_store_undecodable".to_string(),
        },
    }
}

/// Collected code-source generations and activations.
///
/// An absent store root is empty: `open_existing_for_migration` never creates
/// one, so `None` means the collector plane has never written here.
fn code_sources(layout: &ProjectCatalogMigrationResolvedLayoutV1) -> GenesisOwnerDispositionV1 {
    let opened = bbox_code_source_store::CodeSourceStore::open_existing_for_migration(
        &layout.code_source_root,
        layout.store_limits.clone(),
    );
    let store = match opened {
        Ok(None) => return GenesisOwnerDispositionV1::Empty,
        Ok(Some(store)) => store,
        Err(_) => {
            return GenesisOwnerDispositionV1::Unprobeable {
                diagnostic_code: "code_source_store_existing_open_failed".to_string(),
            };
        }
    };
    // The scope filter bounds which rows migration would have to PRESERVE; the
    // counts below are whole-store totals, which is the question genesis asks.
    match store.snapshot_legacy_migration_for_scopes(&BTreeSet::new()) {
        Ok(owned) => count_disposition(
            owned.inventory.generation_count
                + owned.inventory.activations.len() as u64
                + owned.inventory.collision_pending.len() as u64,
        ),
        Err(_) => GenesisOwnerDispositionV1::Unprobeable {
            diagnostic_code: "code_source_inventory_invalid".to_string(),
        },
    }
}
