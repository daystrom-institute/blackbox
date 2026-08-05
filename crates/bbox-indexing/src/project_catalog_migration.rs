//! Closed public authority boundary for project-catalog migration.
//!
//! The public facade owns path validation, exact reviewed-artifact identity,
//! result redaction, and the only executable migration entry points exported
//! by `bbox-indexing`. Owner snapshots and transaction assembly remain
//! crate-private. The current owner integration deliberately fails closed
//! until every required owner exposes a strict no-create snapshot API.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::path::{Component, Path, PathBuf};

use bbox_code_source_store::{
    ActivationRecordV2, CollisionRetirementEntryV1, CollisionRetirementLifecycleStateV1,
    CollisionRetirementLifecycleV1, CollisionRetirementSelectorEvidenceV1,
    MigrationEffectiveSourceManifestV1, MigrationEffectiveSourceSelectionV1, StoreLimits,
    StoredGenerationV2, encode_activation_v2_for_migration,
    encode_collision_retirement_pending_for_migration,
    encode_migration_effective_source_manifest_v1, encode_stored_generation_v2_for_migration,
};
use bbox_config::config::Config;
use bbox_corpus_core::git::{
    StableGitRepository, list_verified_committed_dir_bounded,
    read_verified_committed_file_bytes_bounded,
    read_verified_committed_file_bytes_optional_bounded,
};
use bbox_corpus_core::identity::{PublishedScope, resolve_recorded_repo_id};
use bbox_corpus_core::json_store::{NofollowDirectory, canonical_store_lock_path};
use bbox_corpus_core::project_catalog::{
    AmbiguousNamespaceRecord, AmbiguousNamespaceStatus, AttachmentCapabilities, AttachmentId,
    AttachmentKind, AttachmentSnapshotV1, AttachmentStatus, CatalogOriginV2, CatalogSnapshotV2,
    CheckoutAttachment, CommitNamespace, CorpusProject, LegacyPathBindingId,
    LegacyPathBindingStatus, LegacyPathLedgerEntry, LegacyPathRelationship,
    MAX_PROJECT_CATALOG_ENTRIES, ProjectCatalogTransactionId, ProjectId, ProjectScope,
    RecordedRepoAuthority, RepoHistoryAuthority, RepoHistoryId, RepoHistoryMaterialization,
    RepoHistoryQuarantineMaterialization, RepoHistoryRecord, encode_attachment_snapshot,
    encode_catalog_snapshot, validate_catalog_attachments,
};
use bbox_corpus_core::project_record::ProjectRecord;
use serde::{Deserialize, Serialize};

use crate::accepted_publication_store::{
    AcceptedGapSourceV1, AcceptedKnowledgeSourceV1, AcceptedPublicationBuildInputV1,
    AcceptedPublicationLimits, FullPublisherRef, GitObjectId, PreparedAcceptedPublicationV1,
    prepare_accepted_publication_v1,
};
use crate::project_catalog_inventory::{
    AttachmentMigrationReportRowV1, AttachmentPostImageInputV1, CheckoutIdentityActionV1,
    ConflictReportV1, DeterministicPostImageInputV1, DeterministicRepoHistoryGroupV1,
    ImmutableInventoryOwnerKindV1, InventorySourceStateV1, LegacyCommitNamespaceInventoryV1,
    LegacyPathBindingPostImageInputV1, LegacyPathBindingReportV1, LegacyPathBindingStatusV1,
    LegacyPathRelationshipV1, MAX_PROJECT_CATALOG_REPORT_BYTES,
    MAX_PROJECT_CATALOG_RESOLUTION_BYTES, MigrationRefusalOriginV1, MigrationRefusalReportV1,
    MissingPathReportV1, MutableInventorySourceKindV1, PlannedRepoHistoryIdentityV1,
    PredictedAssetV1, PredictedPostImageHashesV1, ProjectCatalogMigrationPlanKindV1,
    ProjectCatalogMigrationReportV1, ProjectCatalogMigrationResolutionV1,
    ProjectCatalogMigrationStatusV1, ProjectMigrationReportRowV1, PublicationPayloadHashesV1,
    PublisherBindingDispositionV1, PublisherBindingReportStatusV1, PublisherBindingReportV1,
    QuarantinePostImageInputV1, RequiredResolutionKindV1, RequiredResolutionV1,
    ResolvedProjectScopeInputV1, SensitiveLocalPathReportV1, SensitiveLocalPathRowV1,
    Sha256ValueV1, V1ProjectCatalogInventory, build_deterministic_repo_history_groups,
    canonical_plan_hash, canonical_scope_conflicts, canonical_scope_resolution_id,
    decode_migration_report_v1, decode_migration_resolution_v1,
    deterministic_repo_history_group_ids, deterministic_repo_history_group_memberships,
    digest_path, digest_published_scope, digest_publisher_full_ref, encode_migration_report_v1,
    encode_migration_resolution_v1, project_authority_scope, resolved_publisher_pins,
    validated_quarantine_bindings,
};
use crate::project_catalog_inventory_adapters::{
    AttachmentCandidateIdentityPlanV1, AttachmentCandidateKeyV1,
    ProjectCatalogAttachmentCandidateDiscoveryRequestV1, ProjectCatalogMigrationInventoryFacadeV1,
    ProjectCatalogMigrationInventoryRequestV1, ProjectCatalogOwnerInventoryLimitsV1,
    ProjectCatalogOwnerInventoryPathsV1, aggregate_inventory_states, attachment_observation_id,
    corpus_source_state, vector_source_state,
};
use crate::project_catalog_migration_lock::project_catalog_migration_lock_path;
use crate::project_catalog_store::{
    ImmutableAssetRoleV1, LEGACY_COMMIT_NAMESPACE_INVENTORY_ASSET_MAX_BYTES,
    MigrationCheckoutIdentityActionDraftV1, MigrationCheckoutRegistryBootstrapV1,
    MigrationCodeSourceActivationDraftV1, MigrationCodeSourceDispositionV1,
    MigrationCodeSourceGenerationDraftV1, MigrationCodeSourceSnapshotDraftV1,
    MigrationImmutableAssetDraftV1, MigrationLegacyProjectSourceDraftV1,
    MigrationMutationDispositionV1, MigrationParticipantDraftV1, MigrationParticipantRegistry,
    MigrationPlanDraftV1, MigrationPublisherSourceDraftV1, MigrationStoreOpenOutcomeV1,
    ParticipantRoleV1, PublisherDispositionEvidenceV1, PublisherPinEvidenceV1, Sha256Hex,
    ValidatedMigrationPlanV1, begin_migration_checkout_registry_bootstrap,
    legacy_commit_namespace_inventory_asset_location, sha256, transact_migration_classified,
    validate_migration_plan,
};
use crate::publisher::PublisherRefStore;

const FACADE_VERSION_V1: u32 = 1;
const MAX_FACADE_DIAGNOSTIC_BYTES: usize = 512;
const MAX_SENSITIVE_REVIEW_BYTES: usize = MAX_PROJECT_CATALOG_REPORT_BYTES;

/// Stable mutation classification carried by every facade error.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProjectCatalogMigrationMutationDispositionV1 {
    NoDurableMutation,
    RecoveredToOldState,
    RecoveredToCommittedState,
    RetryExactPlanRequired,
}

/// One path-redacted error boundary for all public migration operations.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ProjectCatalogMigrationError {
    pub code: &'static str,
    pub message: String,
    pub mutation_disposition: ProjectCatalogMigrationMutationDispositionV1,
}

impl ProjectCatalogMigrationError {
    /// Visible crate-wide so the Phase 6 durable-backfill and path-free-rebuild
    /// facades share this ONE error boundary. A second boundary would mint a
    /// parallel code vocabulary, which section 7 forbids.
    pub(crate) fn new(
        code: &'static str,
        message: impl Into<String>,
        mutation_disposition: ProjectCatalogMigrationMutationDispositionV1,
    ) -> Self {
        let message: String = message.into();
        let mut message_out = String::new();
        for ch in message.chars() {
            let ch = if ch.is_control() { ' ' } else { ch };
            if message_out.len() + ch.len_utf8() > MAX_FACADE_DIAGNOSTIC_BYTES {
                break;
            }
            message_out.push(ch);
        }
        Self {
            code,
            message: message_out,
            mutation_disposition,
        }
    }

    pub(crate) fn no_mutation(code: &'static str, message: impl Into<String>) -> Self {
        Self::new(
            code,
            message,
            ProjectCatalogMigrationMutationDispositionV1::NoDurableMutation,
        )
    }

    /// Reclassify a refusal raised while stamping durable-store rows.
    ///
    /// A stamping failure is never `NoDurableMutation`: by the time the stamper
    /// runs, any appended supersession has already committed to the pair and an
    /// unknown prefix of the stamp set has landed. The recovery section 3.3
    /// specifies is a fresh preflight and re-apply against the new predecessor,
    /// which is exactly what `RetryExactPlanRequired` tells the operator, and
    /// mislabelling it as no-mutation would invite a retry of the same stale
    /// plan.
    pub(crate) fn with_backfill_stamping_disposition(self) -> Self {
        self.with_mutation_disposition(
            ProjectCatalogMigrationMutationDispositionV1::RetryExactPlanRequired,
        )
    }

    fn with_mutation_disposition(
        self,
        mutation_disposition: ProjectCatalogMigrationMutationDispositionV1,
    ) -> Self {
        Self {
            mutation_disposition,
            ..self
        }
    }
}

impl fmt::Display for ProjectCatalogMigrationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}: {}", self.code, self.message)
    }
}

impl std::error::Error for ProjectCatalogMigrationError {}

/// Optional path overrides accepted by the shared-config layout constructor.
#[derive(Clone, Default)]
pub struct ProjectCatalogMigrationLayoutOverridesV1 {
    pub projects_path: Option<PathBuf>,
    pub state_dir: Option<PathBuf>,
}

/// Exact configured code-source limits used by daemon and migration layouts.
pub fn project_catalog_migration_store_limits(config: &Config) -> StoreLimits {
    StoreLimits {
        max_manifest_files: config.code_collection.max_manifest_files,
        max_manifest_logical_bytes: config.code_collection.max_manifest_logical_bytes,
        max_open_uploads_per_producer: config.code_collection.max_open_uploads_per_producer,
        retained_generations: config.code_collection.retained_generations,
        unreferenced_blob_grace_hours: config.code_collection.unreferenced_blob_grace_hours,
        max_migration_survivor_rows: config.code_collection.max_migration_survivor_rows,
        max_migration_survivor_bytes: config.code_collection.max_migration_survivor_bytes,
    }
}

/// Opaque, validated, non-serializable owner and transaction layout.
///
/// Paths intentionally have no public getters. Callers choose one constructor
/// and hand the resulting authority bundle back to the facade.
#[derive(Clone)]
pub struct ProjectCatalogMigrationResolvedLayoutV1 {
    pub(crate) rehearsal_root: Option<PathBuf>,
    pub(crate) state_dir: PathBuf,
    pub(crate) projects_path: PathBuf,
    pub(crate) attachments_path: PathBuf,
    pub(crate) transaction_journal_path: PathBuf,
    pub(crate) migration_marker_path: PathBuf,
    pub(crate) migration_receipt_path: PathBuf,
    pub(crate) transaction_stage_dir: PathBuf,
    pub(crate) catalog_backup_dir: PathBuf,
    pub(crate) catalog_mutation_lock_path: PathBuf,
    pub(crate) catalog_lifetime_lock_path: PathBuf,
    pub(crate) code_source_root: PathBuf,
    pub(crate) accepted_publications_anchor: PathBuf,
    pub(crate) accepted_publications_root: PathBuf,
    pub(crate) accepted_publication_pointers: PathBuf,
    pub(crate) accepted_publication_generations: PathBuf,
    pub(crate) accepted_publications_lock_path: PathBuf,
    pub(crate) catalog_immutable_root: PathBuf,
    pub(crate) publisher_refs_path: PathBuf,
    pub(crate) index_root: PathBuf,
    pub(crate) vector_root: PathBuf,
    pub(crate) edge_root: PathBuf,
    pub(crate) git_meta_root: PathBuf,
    pub(crate) knowledge_path: PathBuf,
    pub(crate) gaps_path: PathBuf,
    pub(crate) threads_path: PathBuf,
    pub(crate) notes_path: PathBuf,
    pub(crate) pins_path: PathBuf,
    pub(crate) roadmap_path: PathBuf,
    pub(crate) packets_dir: PathBuf,
    pub(crate) artifacts_dir: PathBuf,
    pub(crate) bro_home: PathBuf,
    pub(crate) backup_dir: PathBuf,
    pub(crate) checkout_replicas_root: Option<PathBuf>,
    pub(crate) provenance_notes_ref: String,
    pub(crate) store_limits: StoreLimits,
}

impl ProjectCatalogMigrationResolvedLayoutV1 {
    /// The resolved `projects.json` path this layout administers.
    ///
    /// Exposed for the offline CLI, which needs the CONFIGURED path to take
    /// the lifetime lock (`--configured` apply, and the
    /// `--require-exclusive-availability` bridge-down proof on verify) before
    /// any store is opened. Read-only: the layout stays the single resolver.
    pub fn projects_path(&self) -> &Path {
        &self.projects_path
    }

    /// The state directory the Phase 6 backfill places its completion journal
    /// beside (section 3.3). Crate-visible rather than public: the journal path
    /// is code-owned, and a caller that could choose it could place the
    /// rebuild's predecessor binding somewhere the rebuild never looks.
    pub(crate) fn state_dir_for_backfill(&self) -> &Path {
        &self.state_dir
    }

    /// The accepted-publication pointer root the D-040 per-disposition proof
    /// reads. Read-only: this facade verifies publisher evidence and never
    /// seeds it.
    pub(crate) fn accepted_publication_pointers_for_backfill(&self) -> PathBuf {
        self.accepted_publication_pointers.clone()
    }

    /// The accepted-publication generation root a `SeedG1` disposition's G1
    /// evidence must be present in.
    pub(crate) fn accepted_publication_generations_for_backfill(&self) -> PathBuf {
        self.accepted_publication_generations.clone()
    }

    /// The index root the Phase 6 path-free rebuild scans and replaces.
    pub(crate) fn index_root_for_rebuild(&self) -> &Path {
        &self.index_root
    }

    /// The RESOLVED vector-store root. The materializer's equality proof
    /// recomputes a fingerprint over exactly this store, so deriving it
    /// anywhere else would compare against a different store and could never
    /// reach `Equality` (R33F1).
    pub(crate) fn vector_root_for_rebuild(&self) -> &Path {
        &self.vector_root
    }

    /// Resolve configured paths without reading environment or opening stores.
    pub fn from_config(
        config: &Config,
        overrides: ProjectCatalogMigrationLayoutOverridesV1,
    ) -> Result<Self, ProjectCatalogMigrationError> {
        let layout = match overrides.state_dir {
            Some(state_dir) => {
                let projects_path = overrides
                    .projects_path
                    .unwrap_or_else(|| state_dir.join("projects.json"));
                Self::conventional(
                    None,
                    state_dir,
                    projects_path,
                    config.provenance.git_notes_namespace.clone(),
                    project_catalog_migration_store_limits(config),
                )?
            }
            None => {
                let projects_path = overrides
                    .projects_path
                    .unwrap_or_else(|| config.paths.projects_path.clone());
                let projects_parent = projects_path
                    .parent()
                    .ok_or_else(|| {
                        unsafe_layout("configured projects path has no parent directory")
                    })?
                    .to_path_buf();
                let edge_root = projects_parent.join("edges");
                let git_meta_root = projects_parent.join("git_meta");
                let catalog_paths = CatalogDerivedPathsV1::derive(&projects_path)?;
                Self {
                    rehearsal_root: None,
                    state_dir: config.paths.state_dir.clone(),
                    projects_path,
                    attachments_path: catalog_paths.attachments_path,
                    transaction_journal_path: catalog_paths.transaction_journal_path,
                    migration_marker_path: catalog_paths.migration_marker_path,
                    migration_receipt_path: catalog_paths.migration_receipt_path,
                    transaction_stage_dir: catalog_paths.transaction_stage_dir,
                    catalog_backup_dir: catalog_paths.catalog_backup_dir,
                    catalog_mutation_lock_path: catalog_paths.catalog_mutation_lock_path,
                    catalog_lifetime_lock_path: catalog_paths.catalog_lifetime_lock_path,
                    code_source_root: config.paths.state_dir.join("code-sources"),
                    accepted_publications_anchor: catalog_paths.accepted_publications_anchor,
                    accepted_publications_root: catalog_paths.accepted_publications_root,
                    accepted_publication_pointers: catalog_paths.accepted_publication_pointers,
                    accepted_publication_generations: catalog_paths
                        .accepted_publication_generations,
                    accepted_publications_lock_path: catalog_paths.accepted_publications_lock_path,
                    catalog_immutable_root: catalog_paths.catalog_immutable_root,
                    publisher_refs_path: config.paths.bro_home.join("publisher-refs.json"),
                    index_root: config.paths.index_path.clone(),
                    // R33F1: the RESOLVED vector root, not a state-directory
                    // derivation. The runtime store opens at this path, so an
                    // inventory captured here observes the rows retirement
                    // must discharge.
                    vector_root: config.paths.vectors_path.clone(),
                    edge_root,
                    git_meta_root,
                    knowledge_path: config.paths.knowledge_path.clone(),
                    gaps_path: config.paths.gaps_path.clone(),
                    threads_path: config.paths.threads_path.clone(),
                    notes_path: config.paths.notes_path.clone(),
                    pins_path: config.paths.pins_path.clone(),
                    roadmap_path: config.paths.roadmap_path.clone(),
                    packets_dir: config.paths.packets_dir.clone(),
                    artifacts_dir: config.paths.artifacts_dir.clone(),
                    bro_home: config.paths.bro_home.clone(),
                    backup_dir: config.paths.backup_dir.clone(),
                    checkout_replicas_root: None,
                    provenance_notes_ref: validated_notes_ref(
                        &config.provenance.git_notes_namespace,
                    )?,
                    store_limits: project_catalog_migration_store_limits(config),
                }
            }
        };
        layout.validate()?;
        Ok(layout)
    }

    /// Derive the one Phase-1 rehearsal layout below an explicit root.
    pub fn from_rehearsal_root(
        root: impl Into<PathBuf>,
        config: &Config,
    ) -> Result<Self, ProjectCatalogMigrationError> {
        let root = root.into();
        let state_dir = root.join("state");
        let layout = Self::conventional(
            Some(root.clone()),
            state_dir.clone(),
            state_dir.join("projects.json"),
            config.provenance.git_notes_namespace.clone(),
            project_catalog_migration_store_limits(config),
        )?;
        layout.validate()?;
        Ok(layout)
    }

    fn conventional(
        rehearsal_root: Option<PathBuf>,
        state_dir: PathBuf,
        projects_path: PathBuf,
        notes_namespace: String,
        store_limits: StoreLimits,
    ) -> Result<Self, ProjectCatalogMigrationError> {
        let bro_home = state_dir.join("bro");
        let catalog_paths = CatalogDerivedPathsV1::derive(&projects_path)?;
        Ok(Self {
            rehearsal_root: rehearsal_root.clone(),
            projects_path,
            attachments_path: catalog_paths.attachments_path,
            transaction_journal_path: catalog_paths.transaction_journal_path,
            migration_marker_path: catalog_paths.migration_marker_path,
            migration_receipt_path: catalog_paths.migration_receipt_path,
            transaction_stage_dir: catalog_paths.transaction_stage_dir,
            catalog_backup_dir: catalog_paths.catalog_backup_dir,
            catalog_mutation_lock_path: catalog_paths.catalog_mutation_lock_path,
            catalog_lifetime_lock_path: catalog_paths.catalog_lifetime_lock_path,
            code_source_root: state_dir.join("code-sources"),
            accepted_publications_anchor: catalog_paths.accepted_publications_anchor,
            accepted_publications_root: catalog_paths.accepted_publications_root,
            accepted_publication_pointers: catalog_paths.accepted_publication_pointers,
            accepted_publication_generations: catalog_paths.accepted_publication_generations,
            accepted_publications_lock_path: catalog_paths.accepted_publications_lock_path,
            catalog_immutable_root: catalog_paths.catalog_immutable_root,
            publisher_refs_path: bro_home.join("publisher-refs.json"),
            index_root: state_dir.join("index"),
            vector_root: state_dir.join("vectors"),
            edge_root: state_dir.join("edges"),
            git_meta_root: state_dir.join("git_meta"),
            knowledge_path: state_dir.join("blackbox-knowledge.json"),
            gaps_path: state_dir.join("blackbox-gaps.json"),
            threads_path: state_dir.join("blackbox-threads.json"),
            notes_path: state_dir.join("blackbox-notes.json"),
            pins_path: state_dir.join("blackbox-pins.json"),
            roadmap_path: state_dir.join("blackbox-roadmap.json"),
            packets_dir: state_dir.join("packets"),
            artifacts_dir: state_dir.join("artifacts"),
            backup_dir: state_dir.join("backups"),
            checkout_replicas_root: rehearsal_root.map(|root| root.join("checkouts")),
            provenance_notes_ref: validated_notes_ref(&notes_namespace)?,
            bro_home,
            state_dir,
            store_limits,
        })
    }

    fn validate(&self) -> Result<(), ProjectCatalogMigrationError> {
        for path in self.all_paths() {
            validate_absolute_path(path)?;
        }
        bbox_provenance::validate_notes_ref(&self.provenance_notes_ref)
            .map_err(|_| unsafe_layout("provenance notes ref is invalid"))?;
        if let Some(root) = &self.rehearsal_root {
            for path in self.all_paths() {
                if !path.starts_with(root) {
                    return Err(unsafe_layout(
                        "rehearsal layout contains a path outside its root",
                    ));
                }
            }
        }
        let exact_roles = [
            &self.projects_path,
            &self.attachments_path,
            &self.transaction_journal_path,
            &self.migration_marker_path,
            &self.migration_receipt_path,
            &self.catalog_mutation_lock_path,
            &self.catalog_lifetime_lock_path,
            &self.accepted_publications_anchor,
            &self.accepted_publications_lock_path,
            &self.publisher_refs_path,
            &self.knowledge_path,
            &self.gaps_path,
            &self.threads_path,
            &self.notes_path,
            &self.pins_path,
            &self.roadmap_path,
        ];
        if exact_roles.iter().copied().collect::<BTreeSet<_>>().len() != exact_roles.len() {
            return Err(unsafe_layout(
                "migration layout contains colliding file authorities",
            ));
        }
        let all_paths = self.all_paths();
        if all_paths.iter().copied().collect::<BTreeSet<_>>().len() != all_paths.len() {
            return Err(unsafe_layout(
                "migration layout contains colliding authority roles",
            ));
        }
        let owner_roots = self.owner_directory_roots();
        for (index, left) in owner_roots.iter().enumerate() {
            if owner_roots
                .iter()
                .skip(index + 1)
                .any(|right| paths_overlap(left, right))
            {
                return Err(unsafe_layout(
                    "migration layout contains overlapping owner roots",
                ));
            }
        }
        Ok(())
    }

    fn all_paths(&self) -> Vec<&Path> {
        let mut paths = vec![
            self.state_dir.as_path(),
            self.projects_path.as_path(),
            self.attachments_path.as_path(),
            self.transaction_journal_path.as_path(),
            self.migration_marker_path.as_path(),
            self.migration_receipt_path.as_path(),
            self.transaction_stage_dir.as_path(),
            self.catalog_backup_dir.as_path(),
            self.catalog_mutation_lock_path.as_path(),
            self.catalog_lifetime_lock_path.as_path(),
            self.code_source_root.as_path(),
            self.accepted_publications_anchor.as_path(),
            self.accepted_publications_root.as_path(),
            self.accepted_publication_pointers.as_path(),
            self.accepted_publication_generations.as_path(),
            self.accepted_publications_lock_path.as_path(),
            self.catalog_immutable_root.as_path(),
            self.publisher_refs_path.as_path(),
            self.index_root.as_path(),
            self.vector_root.as_path(),
            self.edge_root.as_path(),
            self.git_meta_root.as_path(),
            self.knowledge_path.as_path(),
            self.gaps_path.as_path(),
            self.threads_path.as_path(),
            self.notes_path.as_path(),
            self.pins_path.as_path(),
            self.roadmap_path.as_path(),
            self.packets_dir.as_path(),
            self.artifacts_dir.as_path(),
            self.bro_home.as_path(),
            self.backup_dir.as_path(),
        ];
        if let Some(root) = &self.checkout_replicas_root {
            paths.push(root);
        }
        paths
    }

    fn owner_directory_roots(&self) -> Vec<&Path> {
        let mut paths = vec![
            self.transaction_stage_dir.as_path(),
            self.catalog_backup_dir.as_path(),
            self.catalog_immutable_root.as_path(),
            self.code_source_root.as_path(),
            self.accepted_publications_root.as_path(),
            self.index_root.as_path(),
            self.vector_root.as_path(),
            self.edge_root.as_path(),
            self.git_meta_root.as_path(),
            self.packets_dir.as_path(),
            self.artifacts_dir.as_path(),
            self.bro_home.as_path(),
            self.backup_dir.as_path(),
        ];
        if let Some(root) = &self.checkout_replicas_root {
            paths.push(root);
        }
        paths
    }
}

struct CatalogDerivedPathsV1 {
    attachments_path: PathBuf,
    transaction_journal_path: PathBuf,
    migration_marker_path: PathBuf,
    migration_receipt_path: PathBuf,
    transaction_stage_dir: PathBuf,
    catalog_backup_dir: PathBuf,
    catalog_mutation_lock_path: PathBuf,
    catalog_lifetime_lock_path: PathBuf,
    accepted_publications_anchor: PathBuf,
    accepted_publications_root: PathBuf,
    accepted_publication_pointers: PathBuf,
    accepted_publication_generations: PathBuf,
    accepted_publications_lock_path: PathBuf,
    catalog_immutable_root: PathBuf,
}

impl CatalogDerivedPathsV1 {
    fn derive(projects_path: &Path) -> Result<Self, ProjectCatalogMigrationError> {
        validate_absolute_path(projects_path)?;
        let parent = projects_path
            .parent()
            .ok_or_else(|| unsafe_layout("configured projects path has no parent directory"))?;
        let basename = artifact_name(projects_path)?;
        if matches!(
            basename,
            "project-attachments.json"
                | "project-catalog-transaction.json"
                | "project-catalog-migration.json"
                | "project-catalog-migration-receipt.json"
                | "project-catalog-migration-assets"
                | "project-catalog-stage"
                | "project-catalog-backups"
                | "project-catalog-migration.lock"
                | "accepted-publications.json"
                | "accepted-publications"
        ) {
            return Err(unsafe_layout(
                "configured projects filename collides with a fixed migration role",
            ));
        }
        let accepted_publications_anchor = parent.join("accepted-publications.json");
        let accepted_publications_root = parent.join("accepted-publications");
        Ok(Self {
            attachments_path: parent.join("project-attachments.json"),
            transaction_journal_path: parent.join("project-catalog-transaction.json"),
            migration_marker_path: parent.join("project-catalog-migration.json"),
            migration_receipt_path: parent.join("project-catalog-migration-receipt.json"),
            transaction_stage_dir: parent.join("project-catalog-stage"),
            catalog_immutable_root: parent.join("project-catalog-migration-assets"),
            catalog_backup_dir: parent.join("project-catalog-backups"),
            catalog_mutation_lock_path: canonical_store_lock_path(projects_path),
            catalog_lifetime_lock_path: project_catalog_migration_lock_path(projects_path),
            accepted_publication_pointers: accepted_publications_root.join("pointers"),
            accepted_publication_generations: accepted_publications_root.join("generations"),
            accepted_publications_lock_path: canonical_store_lock_path(
                &accepted_publications_anchor,
            ),
            accepted_publications_anchor,
            accepted_publications_root,
        })
    }
}

fn validated_notes_ref(namespace: &str) -> Result<String, ProjectCatalogMigrationError> {
    if namespace.is_empty()
        || namespace.len() > 256
        || namespace.starts_with('/')
        || namespace.ends_with('/')
        || namespace.contains('/')
        || namespace.contains("..")
        || namespace
            .bytes()
            .any(|byte| byte.is_ascii_control() || byte.is_ascii_whitespace())
    {
        return Err(unsafe_layout("provenance notes namespace is invalid"));
    }
    let notes_ref = format!("refs/notes/{namespace}/provenance");
    bbox_provenance::validate_notes_ref(&notes_ref)
        .map_err(|_| unsafe_layout("provenance notes namespace is invalid"))?;
    Ok(notes_ref)
}

fn validate_absolute_path(path: &Path) -> Result<(), ProjectCatalogMigrationError> {
    if !path.is_absolute()
        || path.components().any(|component| {
            matches!(
                component,
                Component::ParentDir | Component::CurDir | Component::Prefix(_)
            )
        })
    {
        return Err(unsafe_layout(
            "migration layout requires normalized absolute paths",
        ));
    }
    Ok(())
}

fn unsafe_layout(message: &'static str) -> ProjectCatalogMigrationError {
    ProjectCatalogMigrationError::no_mutation(
        "error.project_catalog_migration_unsafe_layout",
        message,
    )
}

/// Successful preflight status is domain state, not an API error.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ProjectCatalogMigrationPreflightReceiptV1 {
    pub version: u32,
    pub status: ProjectCatalogMigrationStatusV1,
    pub transaction_id: ProjectCatalogTransactionId,
    pub inventory_hash: Sha256ValueV1,
    pub plan_hash: Sha256ValueV1,
    pub report_artifact_hash: Sha256ValueV1,
    pub resolution_artifact_hash: Sha256ValueV1,
    pub predicted_catalog_hash: Sha256ValueV1,
    pub predicted_attachment_hash: Sha256ValueV1,
    pub predicted_participant_hashes: BTreeMap<String, Sha256ValueV1>,
    pub predicted_immutable_asset_hashes: BTreeMap<String, Sha256ValueV1>,
    pub predicted_marker_hash: Option<Sha256ValueV1>,
    pub required_resolution_count: u64,
    pub refusal_count: u64,
    pub checkout_action_count: u64,
    pub publisher_pin_count: u64,
    pub quarantine_root_count: u64,
    pub attached_project_count: u64,
    pub omitted_catalog_count: u64,
    pub sensitive_review: Option<SensitiveReviewReceiptV1>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ProjectCatalogMigrationPreflightResultV1 {
    pub receipt: ProjectCatalogMigrationPreflightReceiptV1,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProjectCatalogMigrationApplyOutcomeV1 {
    Applied,
    AlreadyApplied,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct MigrationVerificationReceiptV1 {
    pub version: u32,
    pub transaction_id: ProjectCatalogTransactionId,
    pub inventory_hash: Sha256ValueV1,
    pub plan_hash: Sha256ValueV1,
    pub report_artifact_hash: Sha256ValueV1,
    pub resolution_artifact_hash: Sha256ValueV1,
    pub expected_catalog_hash: Sha256ValueV1,
    pub observed_catalog_hash: Sha256ValueV1,
    pub expected_attachment_hash: Sha256ValueV1,
    pub observed_attachment_hash: Sha256ValueV1,
    pub expected_participant_hashes: BTreeMap<String, Sha256ValueV1>,
    pub observed_participant_hashes: BTreeMap<String, Sha256ValueV1>,
    pub expected_immutable_asset_hashes: BTreeMap<String, Sha256ValueV1>,
    pub observed_immutable_asset_hashes: BTreeMap<String, Sha256ValueV1>,
    pub predicted_marker_hash: Sha256ValueV1,
    pub observed_marker_hash: Sha256ValueV1,
    pub backup_hashes: BTreeMap<String, Sha256ValueV1>,
    pub epoch: u64,
    pub checkout_action_count: u64,
    pub publisher_pin_count: u64,
    pub quarantine_root_count: u64,
    pub attached_project_count: u64,
    pub omitted_catalog_count: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ProjectCatalogMigrationApplyReceiptV1 {
    pub version: u32,
    pub outcome: ProjectCatalogMigrationApplyOutcomeV1,
    pub verification: MigrationVerificationReceiptV1,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ProjectCatalogMigrationApplyResultV1 {
    pub receipt: ProjectCatalogMigrationApplyReceiptV1,
}

/// Host-local path-bearing parity projection. Deliberately not serializable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectCatalogCompatibilityProjectionV1 {
    records: Vec<ProjectRecord>,
    omitted_catalog_count: u64,
}

impl ProjectCatalogCompatibilityProjectionV1 {
    pub fn records(&self) -> &[ProjectRecord] {
        &self.records
    }

    pub fn omitted_catalog_count(&self) -> u64 {
        self.omitted_catalog_count
    }
}

/// Verify result separates serializable receipt from host-local paths.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectCatalogMigrationVerifyResultV1 {
    receipt: MigrationVerificationReceiptV1,
    compatibility: ProjectCatalogCompatibilityProjectionV1,
    mutation_disposition: ProjectCatalogMigrationMutationDispositionV1,
}

impl ProjectCatalogMigrationVerifyResultV1 {
    pub fn receipt(&self) -> &MigrationVerificationReceiptV1 {
        &self.receipt
    }

    pub fn compatibility(&self) -> &ProjectCatalogCompatibilityProjectionV1 {
        &self.compatibility
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SensitiveReviewReceiptV1 {
    pub artifact_hash: Sha256ValueV1,
    pub legacy_path_row_count: u64,
    pub attachment_path_row_count: u64,
}

pub struct ProjectCatalogMigrationPreflightRequestV1 {
    pub layout: ProjectCatalogMigrationResolvedLayoutV1,
    pub report_path: PathBuf,
    pub resolution_path: PathBuf,
    pub sensitive_report_path: Option<PathBuf>,
}

pub struct ProjectCatalogMigrationApplyRequestV1 {
    pub rehearsal_layout: ProjectCatalogMigrationResolvedLayoutV1,
    pub protected_layout: ProjectCatalogMigrationResolvedLayoutV1,
    pub report_path: PathBuf,
    pub resolution_path: PathBuf,
}

pub struct ProjectCatalogMigrationVerifyRequestV1 {
    pub rehearsal_layout: ProjectCatalogMigrationResolvedLayoutV1,
}

/// The only public executable migration authority.
pub struct ProjectCatalogMigrationFacadeV1;

impl ProjectCatalogMigrationFacadeV1 {
    pub fn preflight(
        request: ProjectCatalogMigrationPreflightRequestV1,
    ) -> Result<ProjectCatalogMigrationPreflightResultV1, ProjectCatalogMigrationError> {
        FacadeCoreV1::new(CurrentClosedMigrationIntegrationV1).preflight(request)
    }

    pub fn apply_rehearsal(
        request: ProjectCatalogMigrationApplyRequestV1,
    ) -> Result<ProjectCatalogMigrationApplyResultV1, ProjectCatalogMigrationError> {
        FacadeCoreV1::new(CurrentClosedMigrationIntegrationV1).apply_rehearsal(request)
    }

    pub fn verify(
        request: ProjectCatalogMigrationVerifyRequestV1,
    ) -> Result<ProjectCatalogMigrationVerifyResultV1, ProjectCatalogMigrationError> {
        FacadeCoreV1::new(CurrentClosedMigrationIntegrationV1).verify(request)
    }
}

struct FacadeCoreV1<I> {
    integration: I,
}

impl<I> FacadeCoreV1<I>
where
    I: ClosedMigrationIntegrationV1,
{
    fn new(integration: I) -> Self {
        Self { integration }
    }

    fn preflight(
        &self,
        request: ProjectCatalogMigrationPreflightRequestV1,
    ) -> Result<ProjectCatalogMigrationPreflightResultV1, ProjectCatalogMigrationError> {
        request.layout.validate()?;
        validate_artifact_set(
            &request.layout,
            &request.report_path,
            &request.resolution_path,
            request.sensitive_report_path.as_deref(),
        )?;
        let existing_resolution = read_artifact_optional(
            &request.resolution_path,
            MAX_PROJECT_CATALOG_RESOLUTION_BYTES,
            "resolution",
        )?;
        if let Some(bytes) = &existing_resolution {
            decode_migration_resolution_v1(bytes).map_err(|_| {
                ProjectCatalogMigrationError::no_mutation(
                    "error.project_catalog_migration_invalid_resolution_artifact",
                    "resolution artifact is not a strict nonempty v1 document",
                )
            })?;
        }
        let existing_report = read_artifact_optional(
            &request.report_path,
            MAX_PROJECT_CATALOG_REPORT_BYTES,
            "report",
        )?;
        if let Some(bytes) = &existing_report {
            decode_migration_report_v1(bytes).map_err(|_| {
                ProjectCatalogMigrationError::no_mutation(
                    "error.project_catalog_migration_invalid_report_artifact",
                    "existing report artifact is not a strict nonempty v1 document",
                )
            })?;
        }
        let prepared = self.integration.prepare_preflight(
            &request.layout,
            existing_resolution.as_deref(),
            existing_report.as_deref(),
            request.sensitive_report_path.is_some(),
        )?;
        validate_prepared_preflight(
            &prepared,
            existing_resolution.as_deref(),
            request.sensitive_report_path.is_some(),
        )?;

        if existing_resolution.is_none() {
            write_artifact_if_absent(
                &request.resolution_path,
                &prepared.resolution_bytes,
                MAX_PROJECT_CATALOG_RESOLUTION_BYTES,
                "resolution",
            )?;
        }
        if let Some(path) = request.sensitive_report_path.as_deref() {
            let sensitive = prepared.sensitive_review.as_ref().ok_or_else(|| {
                ProjectCatalogMigrationError::no_mutation(
                    "error.project_catalog_migration_sensitive_review_missing",
                    "sensitive review was requested but complete path bindings are unavailable",
                )
            })?;
            write_artifact_atomic(
                path,
                &sensitive.bytes,
                MAX_SENSITIVE_REVIEW_BYTES,
                "sensitive review",
            )?;
        }
        write_artifact_atomic(
            &request.report_path,
            &prepared.report_bytes,
            MAX_PROJECT_CATALOG_REPORT_BYTES,
            "report",
        )?;
        Ok(ProjectCatalogMigrationPreflightResultV1 {
            receipt: prepared.receipt,
        })
    }

    fn apply_rehearsal(
        &self,
        request: ProjectCatalogMigrationApplyRequestV1,
    ) -> Result<ProjectCatalogMigrationApplyResultV1, ProjectCatalogMigrationError> {
        request.rehearsal_layout.validate()?;
        request.protected_layout.validate()?;
        validate_rehearsal_separation(&request.rehearsal_layout, &request.protected_layout)?;
        validate_artifact_set(
            &request.rehearsal_layout,
            &request.report_path,
            &request.resolution_path,
            None,
        )?;
        validate_artifact_target(&request.protected_layout, &request.report_path)?;
        validate_artifact_target(&request.protected_layout, &request.resolution_path)?;
        let report_bytes = read_artifact_required(
            &request.report_path,
            MAX_PROJECT_CATALOG_REPORT_BYTES,
            "report",
        )?;
        let resolution_bytes = read_artifact_required(
            &request.resolution_path,
            MAX_PROJECT_CATALOG_RESOLUTION_BYTES,
            "resolution",
        )?;
        let report = decode_migration_report_v1(&report_bytes).map_err(|_| {
            ProjectCatalogMigrationError::no_mutation(
                "error.project_catalog_migration_invalid_report_artifact",
                "report artifact is not a strict nonempty v1 document",
            )
        })?;
        let resolution = decode_migration_resolution_v1(&resolution_bytes).map_err(|_| {
            ProjectCatalogMigrationError::no_mutation(
                "error.project_catalog_migration_invalid_resolution_artifact",
                "resolution artifact is not a strict nonempty v1 document",
            )
        })?;
        if report.status != ProjectCatalogMigrationStatusV1::Clean
            || report.plan_kind != ProjectCatalogMigrationPlanKindV1::Executable
        {
            return Err(ProjectCatalogMigrationError::no_mutation(
                "error.project_catalog_migration_report_not_clean",
                "rehearsal apply requires a clean executable report",
            ));
        }
        if report.resolution_artifact_hash != Sha256ValueV1::digest(&resolution_bytes) {
            return Err(ProjectCatalogMigrationError::no_mutation(
                "error.project_catalog_migration_artifact_identity",
                "report is bound to different resolution artifact bytes",
            ));
        }
        if resolution.inventory_hash != report.inventory_hash {
            return Err(ProjectCatalogMigrationError::no_mutation(
                "error.project_catalog_migration_artifact_identity",
                "report and resolution are bound to different inventories",
            ));
        }
        let result = self.integration.apply_rehearsal(
            &request.rehearsal_layout,
            &report_bytes,
            &report,
            &resolution_bytes,
            &resolution,
        )?;
        validate_apply_result(&result, &report_bytes, &report, &resolution_bytes)?;
        Ok(result)
    }

    fn verify(
        &self,
        request: ProjectCatalogMigrationVerifyRequestV1,
    ) -> Result<ProjectCatalogMigrationVerifyResultV1, ProjectCatalogMigrationError> {
        request.rehearsal_layout.validate()?;
        if request.rehearsal_layout.rehearsal_root.is_none() {
            return Err(unsafe_layout(
                "migration verify requires a rehearsal-root layout",
            ));
        }
        let result = self.integration.verify(&request.rehearsal_layout)?;
        validate_verify_result(&result)?;
        Ok(result)
    }
}

pub(crate) struct PreparedPreflightV1 {
    pub(crate) report_bytes: Vec<u8>,
    pub(crate) resolution_bytes: Vec<u8>,
    pub(crate) receipt: ProjectCatalogMigrationPreflightReceiptV1,
    pub(crate) sensitive_review: Option<PreparedSensitiveReviewV1>,
}

pub(crate) struct PreparedSensitiveReviewV1 {
    pub(crate) bytes: Vec<u8>,
}

pub(crate) trait ClosedMigrationIntegrationV1 {
    fn prepare_preflight(
        &self,
        layout: &ProjectCatalogMigrationResolvedLayoutV1,
        existing_resolution: Option<&[u8]>,
        existing_report: Option<&[u8]>,
        include_sensitive_paths: bool,
    ) -> Result<PreparedPreflightV1, ProjectCatalogMigrationError>;

    fn apply_rehearsal(
        &self,
        layout: &ProjectCatalogMigrationResolvedLayoutV1,
        report_bytes: &[u8],
        report: &ProjectCatalogMigrationReportV1,
        resolution_bytes: &[u8],
        resolution: &ProjectCatalogMigrationResolutionV1,
    ) -> Result<ProjectCatalogMigrationApplyResultV1, ProjectCatalogMigrationError>;

    fn verify(
        &self,
        layout: &ProjectCatalogMigrationResolvedLayoutV1,
    ) -> Result<ProjectCatalogMigrationVerifyResultV1, ProjectCatalogMigrationError>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct MigrationPersistedIdentityPlanV1 {
    transaction_id: ProjectCatalogTransactionId,
    repo_history_groups: Vec<DeterministicRepoHistoryGroupV1>,
    checkout_identity_actions: Vec<CheckoutIdentityActionV1>,
    legacy_path_binding_ids: BTreeMap<String, LegacyPathBindingId>,
    attachment_ids: BTreeMap<String, AttachmentId>,
}

#[derive(Debug, Clone)]
struct MigrationRuntimeBindingsViewV1 {
    legacy_project_store_bytes: Vec<u8>,
    legacy_project_store_was_missing: bool,
    legacy_project_paths: BTreeMap<String, PathBuf>,
    checkout_paths: BTreeMap<String, PathBuf>,
    checkout_repositories: BTreeMap<String, StableGitRepository>,
    legacy_selectors: BTreeMap<String, String>,
}

#[derive(Debug, Clone)]
struct MigrationBasePostImagesV1 {
    catalog: CatalogSnapshotV2,
    attachments: AttachmentSnapshotV1,
    post_image_attachments: Vec<AttachmentPostImageInputV1>,
    post_image_legacy_bindings: Vec<LegacyPathBindingPostImageInputV1>,
    legacy_binding_report: Vec<LegacyPathBindingReportV1>,
    sensitive_report: SensitiveLocalPathReportV1,
    missing_paths: Vec<MissingPathReportV1>,
    unscoped_legacy_counts: BTreeMap<crate::project_catalog_inventory::LegacyPathStoreKindV1, u64>,
}

#[derive(Debug, Clone)]
struct ClassifiedLegacyPathV1 {
    observation_id: String,
    planned_binding_id: LegacyPathBindingId,
    literal_selector: String,
    relationship: LegacyPathRelationshipV1,
    mapped_project_id: Option<ProjectId>,
}

#[derive(Debug, Clone)]
struct ClassifiedLegacyPathsV1 {
    paths: Vec<ClassifiedLegacyPathV1>,
    report_rows: Vec<LegacyPathBindingReportV1>,
    sensitive_report: SensitiveLocalPathReportV1,
    unscoped_counts: BTreeMap<crate::project_catalog_inventory::LegacyPathStoreKindV1, u64>,
    refusals: Vec<MigrationRefusalReportV1>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum LateMigrationDomainRefusalV1 {
    ConflictingPublishedAuthorities {
        affected_record_ids: BTreeSet<String>,
    },
    MultipleBaseAttachments {
        affected_record_ids: BTreeSet<String>,
    },
    MissingBaseAttachment {
        affected_record_ids: BTreeSet<String>,
    },
}

#[derive(Debug)]
enum MigrationBasePostImagesFailureV1 {
    Refused(LateMigrationDomainRefusalV1),
    Error(ProjectCatalogMigrationError),
}

impl From<ProjectCatalogMigrationError> for MigrationBasePostImagesFailureV1 {
    fn from(error: ProjectCatalogMigrationError) -> Self {
        Self::Error(error)
    }
}

#[derive(Debug, Clone)]
struct PreparedPublisherPlanV1 {
    dispositions: Vec<PublisherBindingDispositionV1>,
    prepared: BTreeMap<ProjectId, PreparedAcceptedPublicationV1>,
}

#[derive(Debug, Clone)]
struct MigrationStorePlanPartsV1 {
    participants: Vec<MigrationParticipantDraftV1>,
    immutable_assets: Vec<MigrationImmutableAssetDraftV1>,
    code_source_snapshot: MigrationCodeSourceSnapshotDraftV1,
    publisher_pins: Vec<PublisherPinEvidenceV1>,
    publisher_dispositions: Vec<PublisherDispositionEvidenceV1>,
}

#[derive(Debug, Serialize)]
struct FacadeSensitiveReviewV1<'a> {
    version: u32,
    inventory_hash: &'a Sha256ValueV1,
    local_paths_included: bool,
    warning: &'static str,
    legacy_paths: &'a SensitiveLocalPathReportV1,
    attachment_paths: Vec<FacadeSensitiveAttachmentPathV1>,
}

#[derive(Debug, Serialize)]
struct FacadeSensitiveAttachmentPathV1 {
    observation_id: String,
    attachment_id: AttachmentId,
    checkout_observation_id: String,
    checkout_root: String,
    checkout_root_digest: Sha256ValueV1,
    project_path: String,
    project_path_digest: Sha256ValueV1,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct MigrationSemanticAssessmentV1 {
    resolved_project_scopes: Vec<ResolvedProjectScopeInputV1>,
    namespace_conflicts: Vec<ConflictReportV1>,
    scope_conflicts: Vec<ConflictReportV1>,
    alias_conflicts: Vec<ConflictReportV1>,
    activation_conflicts: Vec<ConflictReportV1>,
    publisher_bindings: Vec<PublisherBindingReportV1>,
    publisher_binding_conflicts: Vec<ConflictReportV1>,
    retained_attachment_ids: BTreeSet<AttachmentId>,
    required_resolutions: Vec<RequiredResolutionV1>,
    unresolved_resolution_ids: BTreeSet<String>,
    refusals: Vec<MigrationRefusalReportV1>,
}

impl MigrationSemanticAssessmentV1 {
    fn status(&self) -> ProjectCatalogMigrationStatusV1 {
        if !self.refusals.is_empty() {
            ProjectCatalogMigrationStatusV1::Refused
        } else if !self.unresolved_resolution_ids.is_empty() {
            ProjectCatalogMigrationStatusV1::ResolutionRequired
        } else {
            ProjectCatalogMigrationStatusV1::Clean
        }
    }
}

fn migration_refusal(
    origin: MigrationRefusalOriginV1,
    diagnostic_code: impl Into<String>,
    affected_record_ids: impl IntoIterator<Item = String>,
) -> MigrationRefusalReportV1 {
    MigrationRefusalReportV1 {
        origin,
        diagnostic_code: diagnostic_code.into(),
        affected_record_ids: affected_record_ids.into_iter().collect(),
    }
}

fn semantic_refusal(
    diagnostic_code: impl Into<String>,
    affected_record_ids: impl IntoIterator<Item = String>,
) -> MigrationRefusalReportV1 {
    migration_refusal(
        MigrationRefusalOriginV1::Semantic,
        diagnostic_code,
        affected_record_ids,
    )
}

fn canonicalize_refusals(refusals: &mut Vec<MigrationRefusalReportV1>) {
    refusals.sort_by(|left, right| {
        left.origin
            .cmp(&right.origin)
            .then_with(|| left.diagnostic_code.cmp(&right.diagnostic_code))
            .then_with(|| left.affected_record_ids.cmp(&right.affected_record_ids))
    });
    refusals.dedup();
}

fn late_domain_refusal_row(refusal: LateMigrationDomainRefusalV1) -> MigrationRefusalReportV1 {
    match refusal {
        LateMigrationDomainRefusalV1::ConflictingPublishedAuthorities {
            affected_record_ids,
        } => semantic_refusal("conflicting_published_authorities", affected_record_ids),
        LateMigrationDomainRefusalV1::MultipleBaseAttachments {
            affected_record_ids,
        } => semantic_refusal("multiple_base_attachments", affected_record_ids),
        LateMigrationDomainRefusalV1::MissingBaseAttachment {
            affected_record_ids,
        } => semantic_refusal("missing_base_attachment", affected_record_ids),
    }
}

impl MigrationPersistedIdentityPlanV1 {
    #[cfg(test)]
    fn transaction_id(&self) -> &ProjectCatalogTransactionId {
        &self.transaction_id
    }
}

fn assess_migration_semantics(
    inventory: &V1ProjectCatalogInventory,
    resolution: &ProjectCatalogMigrationResolutionV1,
) -> Result<MigrationSemanticAssessmentV1, ProjectCatalogMigrationError> {
    inventory.validate().map_err(inventory_error)?;
    let inventory_hash = inventory.inventory_hash().map_err(inventory_error)?;
    if resolution.inventory_hash != inventory_hash {
        return Err(ProjectCatalogMigrationError::no_mutation(
            "error.project_catalog_inventory_stale_resolution",
            "resolution artifact belongs to a different captured inventory",
        ));
    }

    let mut refusals = inventory
        .hard_refusals()
        .into_iter()
        .map(|row| {
            migration_refusal(
                MigrationRefusalOriginV1::Inventory,
                row.diagnostic_code,
                [row.record_id],
            )
        })
        .collect::<Vec<_>>();
    let mut resolved_project_scopes = Vec::with_capacity(inventory.legacy_projects.len());
    for project in &inventory.legacy_projects {
        let project_id = ProjectId::parse(project.record.project_id.clone())
            .map_err(|_| planner_error("legacy project id is invalid"))?;
        let published_scope = match project_authority_scope(inventory, &project_id) {
            Ok(scope) => scope.cloned(),
            Err(_) => {
                refusals.push(semantic_refusal(
                    "project_authority_ambiguous",
                    [project.observation_id.clone()],
                ));
                None
            }
        };
        resolved_project_scopes.push(ResolvedProjectScopeInputV1 {
            project_id,
            published_scope,
            created_at: project.record.registered_at.clone(),
        });
    }
    resolved_project_scopes.sort_by(|left, right| left.project_id.cmp(&right.project_id));

    let scope_projects = canonical_scope_conflicts(inventory).map_err(inventory_error)?;
    let selected_owners = resolution
        .selected_scope_owners
        .iter()
        .map(|selection| (selection.scope.clone(), selection))
        .collect::<BTreeMap<_, _>>();
    if selected_owners.len() != resolution.selected_scope_owners.len() {
        return Err(invalid_resolution_artifact(
            "resolution repeats a selected scope owner",
        ));
    }

    let mut scope_conflicts = Vec::new();
    let mut required_resolutions = Vec::new();
    let mut unresolved_resolution_ids = BTreeSet::new();
    let mut canonical_conflict_scopes = BTreeSet::new();
    for (scope, candidates) in scope_projects
        .iter()
        .filter(|(_, candidates)| candidates.len() > 1)
    {
        canonical_conflict_scopes.insert(scope.clone());
        let resolution_id =
            canonical_scope_resolution_id(scope, candidates).map_err(inventory_error)?;
        let candidate_record_ids = candidates
            .iter()
            .map(ToString::to_string)
            .collect::<BTreeSet<_>>();
        scope_conflicts.push(ConflictReportV1 {
            conflict_id: resolution_id.clone(),
            affected_record_ids: candidate_record_ids.clone(),
            diagnostic_code: "duplicate_published_scope".to_string(),
        });
        required_resolutions.push(RequiredResolutionV1 {
            resolution_id: resolution_id.clone(),
            kind: RequiredResolutionKindV1::ScopeOwner,
            candidate_record_ids,
        });
        let Some(selection) = selected_owners.get(scope).copied() else {
            unresolved_resolution_ids.insert(resolution_id);
            for project in &mut resolved_project_scopes {
                if candidates.contains(&project.project_id) {
                    project.published_scope = None;
                }
            }
            continue;
        };
        let mut selected_candidates = selection.losing_project_ids.clone();
        selected_candidates.insert(selection.owner_project_id.clone());
        if selection.resolution_id != resolution_id
            || selected_candidates != *candidates
            || !candidates.contains(&selection.owner_project_id)
        {
            return Err(invalid_resolution_artifact(
                "selected scope owner does not exactly match its conflict",
            ));
        }
        for project in &mut resolved_project_scopes {
            if candidates.contains(&project.project_id) {
                if project.project_id == selection.owner_project_id {
                    project.published_scope = Some(scope.clone());
                } else {
                    project.published_scope = None;
                }
            }
        }
    }
    if selected_owners
        .keys()
        .any(|scope| !canonical_conflict_scopes.contains(scope))
    {
        return Err(invalid_resolution_artifact(
            "resolution contains an unknown scope owner disposition",
        ));
    }

    let resolved_alias_owners = resolution
        .selected_scope_owners
        .iter()
        .flat_map(|selection| {
            selection
                .owned_aliases
                .iter()
                .map(move |alias| (alias.as_str(), &selection.owner_project_id))
        })
        .collect::<BTreeMap<_, _>>();
    let resolved_alias_count = resolution
        .selected_scope_owners
        .iter()
        .map(|selection| selection.owned_aliases.len())
        .sum::<usize>();
    if resolved_alias_owners.len() != resolved_alias_count {
        return Err(invalid_resolution_artifact(
            "resolution repeats an alias owner disposition",
        ));
    }
    let mut aliases = BTreeMap::<String, BTreeSet<ProjectId>>::new();
    for project in &inventory.legacy_projects {
        let project_id = ProjectId::parse(project.record.project_id.clone())
            .map_err(|_| planner_error("legacy project id is invalid"))?;
        for alias in &project.record.aliases {
            aliases
                .entry(alias.clone())
                .or_default()
                .insert(project_id.clone());
        }
    }
    for alias in &inventory.materialized_aliases {
        aliases
            .entry(alias.alias.clone())
            .or_default()
            .insert(alias.project_id.clone());
    }
    let mut alias_conflicts = Vec::new();
    for (alias, candidates) in aliases
        .iter()
        .filter(|(_, candidates)| candidates.len() > 1)
    {
        let candidate_record_ids = candidates
            .iter()
            .map(ToString::to_string)
            .collect::<BTreeSet<_>>();
        alias_conflicts.push(ConflictReportV1 {
            conflict_id: stable_conflict_id("alias_conflict", &(alias, candidates))?,
            affected_record_ids: candidate_record_ids,
            diagnostic_code: "duplicate_materialized_alias".to_string(),
        });
        let exact_owner = resolution.selected_scope_owners.iter().any(|selection| {
            if !selection.owned_aliases.contains(alias) {
                return false;
            }
            let mut selected_candidates = selection.losing_project_ids.clone();
            selected_candidates.insert(selection.owner_project_id.clone());
            selected_candidates == *candidates && candidates.contains(&selection.owner_project_id)
        });
        if !exact_owner {
            // V1 has no free-standing alias-owner disposition. An alias may
            // only ride on the exact duplicate-scope owner selection that
            // already names it; otherwise it is a non-overridable conflict.
            refusals.push(semantic_refusal(
                "independent_alias_conflict",
                candidates.iter().map(ToString::to_string),
            ));
        }
    }
    if resolved_alias_owners
        .iter()
        .any(|(alias, owner)| !aliases.get(*alias).is_some_and(|set| set.contains(*owner)))
    {
        return Err(invalid_resolution_artifact(
            "resolution contains an unknown alias owner disposition",
        ));
    }

    let resolved_scopes = resolved_project_scopes
        .iter()
        .map(|row| (row.project_id.clone(), row.published_scope.as_ref()))
        .collect::<BTreeMap<_, _>>();
    let exclusion_rows = resolution
        .excluded_attachments
        .iter()
        .map(|row| (row.attachment_id.clone(), row))
        .collect::<BTreeMap<_, _>>();
    if exclusion_rows.len() != resolution.excluded_attachments.len() {
        return Err(invalid_resolution_artifact(
            "resolution repeats an attachment exclusion",
        ));
    }
    let duplicate_attachment_keys = inventory
        .attachment_candidates
        .iter()
        .fold(
            BTreeMap::<(&str, &str), Vec<&str>>::new(),
            |mut groups, attachment| {
                groups
                    .entry((
                        attachment.checkout_observation_id.as_str(),
                        attachment.base_relpath.as_str(),
                    ))
                    .or_default()
                    .push(attachment.observation_id.as_str());
                groups
            },
        )
        .into_iter()
        .filter(|(_, observations)| observations.len() > 1)
        .flat_map(|(_, observations)| observations)
        .collect::<BTreeSet<_>>();
    let mut canonical_exclusions = BTreeSet::new();
    let mut retained_attachment_ids = BTreeSet::new();
    for attachment in &inventory.attachment_candidates {
        let scope_mismatch = resolved_scopes
            .get(&attachment.project_id)
            .map_or(true, |resolved| {
                *resolved != attachment.observed_scope.as_ref()
            })
            || attachment
                .observed_scope
                .as_ref()
                .is_some_and(|scope| scope.bbox_root_relpath() != attachment.base_relpath);
        let authorized_scope_downgrade = resolution.selected_scope_owners.iter().any(|selection| {
            selection
                .losing_project_ids
                .contains(&attachment.project_id)
                && attachment.observed_scope.as_ref() == Some(&selection.scope)
                && resolved_scopes.get(&attachment.project_id) == Some(&None)
        });
        let requires_exclusion = scope_mismatch && !authorized_scope_downgrade
            || duplicate_attachment_keys.contains(attachment.observation_id.as_str());
        if !requires_exclusion {
            retained_attachment_ids.insert(attachment.attachment_id.clone());
            continue;
        }
        canonical_exclusions.insert(attachment.attachment_id.clone());
        let resolution_id = stable_conflict_id("attachment_conflict", &attachment.observation_id)?;
        let candidate_record_ids = BTreeSet::from([attachment.observation_id.clone()]);
        required_resolutions.push(RequiredResolutionV1 {
            resolution_id: resolution_id.clone(),
            kind: RequiredResolutionKindV1::ExcludeAttachment,
            candidate_record_ids,
        });
        match exclusion_rows.get(&attachment.attachment_id).copied() {
            None => {
                unresolved_resolution_ids.insert(resolution_id);
            }
            Some(row) if row.resolution_id != resolution_id => {
                return Err(invalid_resolution_artifact(
                    "attachment exclusion does not match its conflict",
                ));
            }
            Some(_) => {}
        }
    }
    if exclusion_rows
        .keys()
        .any(|attachment_id| !canonical_exclusions.contains(attachment_id))
    {
        return Err(invalid_resolution_artifact(
            "resolution contains an unknown attachment exclusion",
        ));
    }

    let group_memberships =
        deterministic_repo_history_group_memberships(inventory, &resolved_project_scopes)
            .map_err(inventory_error)?
            .into_iter()
            .collect::<BTreeMap<_, _>>();
    let split_rows = resolution
        .repo_history_group_splits
        .iter()
        .map(|row| (row.source_cluster_id.as_str(), row))
        .collect::<BTreeMap<_, _>>();
    if split_rows.len() != resolution.repo_history_group_splits.len() {
        return Err(invalid_resolution_artifact(
            "resolution repeats a repository history split",
        ));
    }
    let mut namespace_conflicts = Vec::new();
    for cluster in &inventory.legacy_namespace_clusters {
        let resolution_id = cluster.observation_id.clone();
        let candidate_record_ids = cluster
            .project_ids
            .iter()
            .map(ToString::to_string)
            .collect::<BTreeSet<_>>();
        namespace_conflicts.push(ConflictReportV1 {
            conflict_id: resolution_id.clone(),
            affected_record_ids: candidate_record_ids.clone(),
            diagnostic_code: "ambiguous_legacy_namespace".to_string(),
        });
        required_resolutions.push(RequiredResolutionV1 {
            resolution_id: resolution_id.clone(),
            kind: RequiredResolutionKindV1::RepoHistoryGroupSplit,
            candidate_record_ids,
        });
        let Some(split) = split_rows.get(cluster.cluster_id.as_str()).copied() else {
            unresolved_resolution_ids.insert(resolution_id);
            continue;
        };
        let partitioned = split
            .partitions
            .iter()
            .flat_map(|partition| partition.project_ids.iter().cloned())
            .collect::<BTreeSet<_>>();
        let partitions_are_exact = partitioned == cluster.project_ids
            && split.resolution_id == resolution_id
            && split.partitions.iter().all(|partition| {
                group_memberships.get(&partition.target_group_id) == Some(&partition.project_ids)
            });
        if !partitions_are_exact {
            return Err(invalid_resolution_artifact(
                "repository history split does not match its conflict",
            ));
        }
    }
    if split_rows.keys().any(|cluster_id| {
        !inventory
            .legacy_namespace_clusters
            .iter()
            .any(|cluster| cluster.cluster_id == **cluster_id)
    }) {
        return Err(invalid_resolution_artifact(
            "resolution contains an unknown repository history split",
        ));
    }
    if !resolution.repo_history_group_merges.is_empty() {
        return Err(invalid_resolution_artifact(
            "repository history merge dispositions are unsupported",
        ));
    }

    let quarantine_rows = resolution
        .quarantine_collected
        .iter()
        .map(|row| ((row.project_id.clone(), row.generation_id.as_str()), row))
        .collect::<BTreeMap<_, _>>();
    if quarantine_rows.len() != resolution.quarantine_collected.len() {
        return Err(invalid_resolution_artifact(
            "resolution repeats a collected quarantine",
        ));
    }
    let mut expected_quarantines = BTreeSet::new();
    let mut activation_conflicts = Vec::new();
    for selection in &resolution.selected_scope_owners {
        for source in &inventory.code_sources {
            if !selection.losing_project_ids.contains(&source.project_id) {
                continue;
            }
            for generation in &source.generations {
                let key = (
                    generation.project_id.clone(),
                    generation.generation_id.as_str(),
                );
                expected_quarantines.insert((key.0.clone(), key.1.to_string()));
                let resolution_id =
                    stable_conflict_id("activation_conflict", &generation.observation_id)?;
                let candidate_record_ids = BTreeSet::from([generation.observation_id.clone()]);
                activation_conflicts.push(ConflictReportV1 {
                    conflict_id: resolution_id.clone(),
                    affected_record_ids: candidate_record_ids.clone(),
                    diagnostic_code: "losing_collected_generation".to_string(),
                });
                required_resolutions.push(RequiredResolutionV1 {
                    resolution_id: resolution_id.clone(),
                    kind: RequiredResolutionKindV1::QuarantineCollected,
                    candidate_record_ids,
                });
                match quarantine_rows.get(&key).copied() {
                    None => {
                        unresolved_resolution_ids.insert(resolution_id);
                    }
                    Some(row) if row.resolution_id != resolution_id => {
                        return Err(invalid_resolution_artifact(
                            "collected quarantine does not match its conflict",
                        ));
                    }
                    Some(_) => {}
                }
            }
        }
    }
    if quarantine_rows.keys().any(|(project_id, generation_id)| {
        !expected_quarantines.contains(&(project_id.clone(), (*generation_id).to_string()))
    }) {
        return Err(invalid_resolution_artifact(
            "resolution contains an unknown collected quarantine",
        ));
    }

    let resolution_publishers = resolution
        .publisher_binding_dispositions
        .iter()
        .map(|row| {
            (
                (
                    row.project_id().clone(),
                    row.expected_scope().clone(),
                    row.full_ref().to_string(),
                ),
                row,
            )
        })
        .collect::<BTreeMap<_, _>>();
    if resolution_publishers.len() != resolution.publisher_binding_dispositions.len() {
        return Err(invalid_resolution_artifact(
            "resolution repeats a publisher binding disposition",
        ));
    }
    let mut publisher_bindings = Vec::new();
    let mut publisher_binding_conflicts = Vec::new();
    for pin in &inventory.unbound_publisher_pins {
        if pin.reason
            == crate::project_catalog_inventory::UnboundPublisherPinReasonV1::DuplicateScopeOwners
            && !resolution
                .selected_scope_owners
                .iter()
                .any(|selection| selection.scope == pin.expected_scope)
        {
            publisher_bindings.push(PublisherBindingReportV1 {
                pin_observation_id: pin.observation_id.clone(),
                project_id: None,
                expected_scope_digest: digest_published_scope(&pin.expected_scope)
                    .map_err(inventory_error)?,
                full_ref_digest: digest_publisher_full_ref(&pin.full_ref)
                    .map_err(inventory_error)?,
                status: PublisherBindingReportStatusV1::ResolutionRequired,
            });
            publisher_binding_conflicts.push(ConflictReportV1 {
                conflict_id: stable_conflict_id("publisher_scope_owner", &pin.observation_id)?,
                affected_record_ids: std::iter::once(pin.observation_id.clone())
                    .chain(pin.candidate_project_ids.iter().map(ToString::to_string))
                    .collect(),
                diagnostic_code: "publisher_scope_owner_ambiguous".to_string(),
            });
        } else if pin.reason
            == crate::project_catalog_inventory::UnboundPublisherPinReasonV1::OwnerlessScope
        {
            publisher_bindings.push(PublisherBindingReportV1 {
                pin_observation_id: pin.observation_id.clone(),
                project_id: None,
                expected_scope_digest: digest_published_scope(&pin.expected_scope)
                    .map_err(inventory_error)?,
                full_ref_digest: digest_publisher_full_ref(&pin.full_ref)
                    .map_err(inventory_error)?,
                status: PublisherBindingReportStatusV1::Refused,
            });
            publisher_binding_conflicts.push(ConflictReportV1 {
                conflict_id: stable_conflict_id("publisher_scope_owner", &pin.observation_id)?,
                affected_record_ids: BTreeSet::from([pin.observation_id.clone()]),
                diagnostic_code: "publisher_scope_owner_missing".to_string(),
            });
        }
    }
    let effective_pins = resolved_publisher_pins(inventory, resolution).map_err(inventory_error)?;
    let git_lane_complete = inventory
        .immutable_lane_evidence
        .iter()
        .find(|lane| {
            lane.lane_kind
                == crate::project_catalog_inventory::ImmutableInventoryLaneKindV1::GitMetadata
        })
        .is_some_and(|lane| {
            lane.completeness
                == crate::project_catalog_inventory::ImmutableInventoryLaneCompletenessV1::Complete
        });
    for pin in &effective_pins {
        let key = (
            pin.project_id.clone(),
            pin.expected_scope.clone(),
            pin.full_ref.clone(),
        );
        let retained_pin_candidates = pin
            .candidate_attachment_ids
            .intersection(&retained_attachment_ids)
            .count();
        let automatic = retained_pin_candidates == 1
            && pin.resolved_commit.is_some()
            && pin.resolved_scope.as_ref() == Some(&pin.expected_scope);
        let status = if !git_lane_complete {
            publisher_binding_conflicts.push(ConflictReportV1 {
                conflict_id: stable_conflict_id("publisher_git_lane", &pin.observation_id)?,
                affected_record_ids: BTreeSet::from([pin.observation_id.clone()]),
                diagnostic_code: "publisher_git_lane_incomplete".to_string(),
            });
            PublisherBindingReportStatusV1::Refused
        } else if automatic {
            if resolution_publishers.contains_key(&key) {
                return Err(invalid_resolution_artifact(
                    "resolution overrides an unambiguous publisher binding",
                ));
            }
            PublisherBindingReportStatusV1::SeedG1Predicted
        } else {
            let resolution_id = stable_conflict_id("publisher_binding", &pin.observation_id)?;
            let mut candidate_record_ids = BTreeSet::from([pin.observation_id.clone()]);
            candidate_record_ids.extend(
                inventory
                    .attachment_candidates
                    .iter()
                    .filter(|attachment| {
                        pin.candidate_attachment_ids
                            .contains(&attachment.attachment_id)
                    })
                    .map(|attachment| attachment.observation_id.clone()),
            );
            publisher_binding_conflicts.push(ConflictReportV1 {
                conflict_id: resolution_id.clone(),
                affected_record_ids: candidate_record_ids.clone(),
                diagnostic_code: "publisher_binding_ambiguous".to_string(),
            });
            required_resolutions.push(RequiredResolutionV1 {
                resolution_id: resolution_id.clone(),
                kind: RequiredResolutionKindV1::PublisherBindingDisposition,
                candidate_record_ids,
            });
            match resolution_publishers.get(&key) {
                None => {
                    unresolved_resolution_ids.insert(resolution_id);
                    PublisherBindingReportStatusV1::ResolutionRequired
                }
                Some(PublisherBindingDispositionV1::SeedG1 { .. }) => {
                    PublisherBindingReportStatusV1::ResolutionRequired
                }
                Some(PublisherBindingDispositionV1::NoPublishedContentAcknowledged { .. }) => {
                    PublisherBindingReportStatusV1::ResolutionRequired
                }
            }
        };
        publisher_bindings.push(PublisherBindingReportV1 {
            pin_observation_id: pin.observation_id.clone(),
            project_id: Some(pin.project_id.clone()),
            expected_scope_digest: digest_published_scope(&pin.expected_scope)
                .map_err(inventory_error)?,
            full_ref_digest: digest_publisher_full_ref(&pin.full_ref).map_err(inventory_error)?,
            status,
        });
    }
    if resolution_publishers.keys().any(|key| {
        !effective_pins.iter().any(|pin| {
            pin.project_id == key.0 && pin.expected_scope == key.1 && pin.full_ref == key.2
        })
    }) {
        return Err(invalid_resolution_artifact(
            "resolution contains an unknown publisher binding disposition",
        ));
    }

    for checkout in &inventory.checkouts {
        if matches!(
            &checkout.marker_state,
            crate::project_catalog_inventory::CheckoutMarkerStateV1::Malformed { .. }
                | crate::project_catalog_inventory::CheckoutMarkerStateV1::Unreadable { .. }
                | crate::project_catalog_inventory::CheckoutMarkerStateV1::Symlinked
        ) {
            refusals.push(semantic_refusal(
                "unsafe_checkout_marker",
                [checkout.observation_id.clone()],
            ));
        }
    }
    for namespace in &inventory.legacy_commit_namespaces {
        use crate::project_catalog_inventory::LegacyCommitNamespaceAttributionV1 as Attribution;
        let supported = match &namespace.attribution {
            Attribution::Proved { .. } => true,
            Attribution::Ambiguous {
                candidate_project_ids,
            } => inventory.legacy_namespace_clusters.iter().any(|cluster| {
                cluster.materialized_namespace == namespace.namespace.as_str()
                    && &cluster.project_ids == candidate_project_ids
            }),
            Attribution::Unclaimed => false,
        };
        if !supported {
            refusals.push(semantic_refusal(
                "unsupported_legacy_namespace",
                [namespace.observation_id.clone()],
            ));
        }
    }
    namespace_conflicts.sort_by(|left, right| left.conflict_id.cmp(&right.conflict_id));
    scope_conflicts.sort_by(|left, right| left.conflict_id.cmp(&right.conflict_id));
    alias_conflicts.sort_by(|left, right| left.conflict_id.cmp(&right.conflict_id));
    activation_conflicts.sort_by(|left, right| left.conflict_id.cmp(&right.conflict_id));
    publisher_bindings
        .sort_by(|left, right| left.pin_observation_id.cmp(&right.pin_observation_id));
    publisher_binding_conflicts.sort_by(|left, right| left.conflict_id.cmp(&right.conflict_id));
    required_resolutions.sort_by(|left, right| left.resolution_id.cmp(&right.resolution_id));
    canonicalize_refusals(&mut refusals);
    Ok(MigrationSemanticAssessmentV1 {
        resolved_project_scopes,
        namespace_conflicts,
        scope_conflicts,
        alias_conflicts,
        activation_conflicts,
        publisher_bindings,
        publisher_binding_conflicts,
        retained_attachment_ids,
        required_resolutions,
        unresolved_resolution_ids,
        refusals,
    })
}

fn build_persisted_identity_plan(
    inventory: &V1ProjectCatalogInventory,
    resolved_project_scopes: &[ResolvedProjectScopeInputV1],
    retained_attachment_ids: &BTreeSet<AttachmentId>,
    prior_report: Option<&ProjectCatalogMigrationReportV1>,
) -> Result<MigrationPersistedIdentityPlanV1, ProjectCatalogMigrationError> {
    inventory.validate().map_err(inventory_error)?;
    let inventory_hash = inventory.inventory_hash().map_err(inventory_error)?;
    let prior_report = match prior_report {
        Some(report) if report.inventory_hash == inventory_hash => {
            report
                .validate_against_inventory(inventory)
                .map_err(inventory_error)?;
            Some(report)
        }
        Some(_) | None => None,
    };

    let transaction_id = prior_report
        .map(|report| report.transaction_id.clone())
        .unwrap_or_else(ProjectCatalogTransactionId::mint);
    let prior_groups = prior_report
        .into_iter()
        .flat_map(|report| &report.repo_history_groups)
        .map(|group| (group.group_id.as_str(), group))
        .collect::<BTreeMap<_, _>>();
    let mut planned_groups = BTreeMap::new();
    let mut used_history_ids = prior_report
        .into_iter()
        .flat_map(|report| &report.repo_history_groups)
        .map(|group| group.planned_history_id.clone())
        .collect::<BTreeSet<_>>();
    let mut used_namespaces = prior_report
        .into_iter()
        .flat_map(|report| &report.repo_history_groups)
        .flat_map(|group| {
            std::iter::once(group.planned_primary_namespace.clone())
                .chain(group.planned_compatibility_namespaces.iter().cloned())
        })
        .collect::<BTreeSet<_>>();
    for group_id in deterministic_repo_history_group_ids(inventory, resolved_project_scopes)
        .map_err(inventory_error)?
    {
        let prior = prior_groups.get(group_id.as_str()).copied();
        let planned_history_id = match prior {
            Some(group) => group.planned_history_id.clone(),
            None => mint_unique_repo_history_id(&mut used_history_ids),
        };
        used_history_ids.insert(planned_history_id.clone());

        let inventoried_namespaces =
            inventoried_group_namespaces(inventory, resolved_project_scopes, &group_id)?;
        let (planned_primary_namespace, planned_compatibility_namespaces) =
            if inventoried_namespaces.is_empty() {
                let namespace = prior
                    .map(|group| group.planned_primary_namespace.clone())
                    .unwrap_or_else(|| mint_local_namespace(&mut used_namespaces));
                (namespace, BTreeSet::new())
            } else {
                let primary = prior
                    .filter(|group| {
                        inventoried_namespaces.contains(&group.planned_primary_namespace)
                    })
                    .map(|group| group.planned_primary_namespace.clone())
                    .unwrap_or_else(|| {
                        inventoried_namespaces
                            .first()
                            .expect("nonempty inventoried namespace set")
                            .clone()
                    });
                let mut compatibility = inventoried_namespaces.clone();
                compatibility.remove(&primary);
                (primary, compatibility)
            };
        used_namespaces.insert(planned_primary_namespace.clone());
        used_namespaces.extend(planned_compatibility_namespaces.iter().cloned());
        planned_groups.insert(
            group_id,
            PlannedRepoHistoryIdentityV1 {
                planned_history_id,
                planned_primary_namespace,
                planned_compatibility_namespaces,
            },
        );
    }
    let repo_history_groups = build_deterministic_repo_history_groups(
        inventory,
        resolved_project_scopes,
        &planned_groups,
    )
    .map_err(inventory_error)?;

    let prior_actions = prior_report
        .into_iter()
        .flat_map(|report| &report.checkout_identity_actions)
        .map(|action| (action.observation_id.as_str(), action))
        .collect::<BTreeMap<_, _>>();
    let used_checkout_observations = inventory
        .attachment_candidates
        .iter()
        .filter(|attachment| retained_attachment_ids.contains(&attachment.attachment_id))
        .map(|attachment| attachment.checkout_observation_id.as_str())
        .collect::<BTreeSet<_>>();
    let checkout_identity_actions = inventory
        .checkouts
        .iter()
        .filter(|checkout| used_checkout_observations.contains(checkout.observation_id.as_str()))
        .filter_map(|checkout| match &checkout.marker_state {
            crate::project_catalog_inventory::CheckoutMarkerStateV1::MissingOrEmpty => {
                let planned_checkout_id = prior_actions
                    .get(checkout.observation_id.as_str())
                    .map(|action| action.planned_checkout_id.clone())
                    .unwrap_or_else(mint_checkout_id);
                Some(CheckoutIdentityActionV1 {
                    observation_id: checkout.observation_id.clone(),
                    canonical_root_digest: checkout.canonical_root_digest.clone(),
                    planned_checkout_id,
                })
            }
            crate::project_catalog_inventory::CheckoutMarkerStateV1::Valid { .. }
            | crate::project_catalog_inventory::CheckoutMarkerStateV1::Malformed { .. }
            | crate::project_catalog_inventory::CheckoutMarkerStateV1::Unreadable { .. }
            | crate::project_catalog_inventory::CheckoutMarkerStateV1::Symlinked => None,
        })
        .collect();

    let prior_bindings = prior_report
        .into_iter()
        .flat_map(|report| &report.legacy_path_bindings)
        .map(|binding| {
            (
                binding.observation_id.as_str(),
                binding.planned_binding_id.clone(),
            )
        })
        .collect::<BTreeMap<_, _>>();
    let mut used_binding_ids = prior_bindings.values().cloned().collect::<BTreeSet<_>>();
    let legacy_path_binding_ids = inventory
        .legacy_path_observations
        .iter()
        .map(|observation| {
            let binding_id = prior_bindings
                .get(observation.observation_id.as_str())
                .cloned()
                .unwrap_or_else(|| mint_unique_binding_id(&mut used_binding_ids));
            used_binding_ids.insert(binding_id.clone());
            (observation.observation_id.clone(), binding_id)
        })
        .collect();

    let prior_attachments = prior_report
        .into_iter()
        .flat_map(|report| &report.attachments)
        .map(|attachment| {
            (
                attachment.observation_id.as_str(),
                attachment.attachment_id.clone(),
            )
        })
        .collect::<BTreeMap<_, _>>();
    let attachment_ids = inventory
        .attachment_candidates
        .iter()
        .map(|attachment| {
            if let Some(prior_id) = prior_attachments.get(attachment.observation_id.as_str())
                && *prior_id != attachment.attachment_id
            {
                return Err(ProjectCatalogMigrationError::no_mutation(
                    "error.project_catalog_migration_identity_remint",
                    "recaptured attachment candidate changed its persisted strong identity",
                ));
            }
            Ok((
                attachment.observation_id.clone(),
                attachment.attachment_id.clone(),
            ))
        })
        .collect::<Result<_, _>>()?;

    Ok(MigrationPersistedIdentityPlanV1 {
        transaction_id,
        repo_history_groups,
        checkout_identity_actions,
        legacy_path_binding_ids,
        attachment_ids,
    })
}

fn classify_legacy_paths(
    inventory: &V1ProjectCatalogInventory,
    runtime: &MigrationRuntimeBindingsViewV1,
    identities: &MigrationPersistedIdentityPlanV1,
) -> Result<ClassifiedLegacyPathsV1, ProjectCatalogMigrationError> {
    let mut paths = Vec::new();
    let mut report_rows = Vec::new();
    let mut sensitive_rows = Vec::new();
    let mut unscoped_counts = BTreeMap::new();
    let mut refusals = Vec::new();
    for observed in &inventory.legacy_path_observations {
        let literal = runtime
            .legacy_selectors
            .get(&observed.observation_id)
            .ok_or_else(|| planner_error("legacy selector runtime binding is missing"))?;
        let literal_path = Path::new(literal);
        let selector_is_unsafe = !literal_path.is_absolute()
            || literal_path.components().any(|component| {
                matches!(
                    component,
                    Component::CurDir | Component::ParentDir | Component::Prefix(_)
                )
            });
        if selector_is_unsafe {
            refusals.push(semantic_refusal(
                "unsafe_legacy_selector",
                [observed.observation_id.clone()],
            ));
            let planned_binding_id = identities
                .legacy_path_binding_ids
                .get(&observed.observation_id)
                .ok_or_else(|| planner_error("legacy path binding identity is missing"))?
                .clone();
            paths.push(ClassifiedLegacyPathV1 {
                observation_id: observed.observation_id.clone(),
                planned_binding_id: planned_binding_id.clone(),
                literal_selector: literal.clone(),
                relationship: LegacyPathRelationshipV1::UnsafeSelector,
                mapped_project_id: None,
            });
            report_rows.push(LegacyPathBindingReportV1 {
                observation_id: observed.observation_id.clone(),
                planned_binding_id,
                store_kind: observed.store_kind,
                relationship: LegacyPathRelationshipV1::UnsafeSelector,
                status: LegacyPathBindingStatusV1::Refused,
                path_digest: digest_path(literal),
            });
            sensitive_rows.push(SensitiveLocalPathRowV1 {
                observation_id: observed.observation_id.clone(),
                store_kind: observed.store_kind,
                stable_row_id: observed.stable_row_id.clone(),
                literal_selector: literal.clone(),
            });
            continue;
        }
        let mut matching_projects = inventory
            .legacy_projects
            .iter()
            .filter_map(|project| {
                runtime
                    .legacy_project_paths
                    .get(&project.observation_id)
                    .filter(|root| literal_path.starts_with(root))
                    .map(|root| (project, root.components().count()))
            })
            .collect::<Vec<_>>();
        matching_projects.sort_by(|(left, left_depth), (right, right_depth)| {
            right_depth
                .cmp(left_depth)
                .then_with(|| left.observation_id.cmp(&right.observation_id))
        });
        let deepest_tied = matching_projects
            .first()
            .map(|(_, depth)| {
                matching_projects
                    .iter()
                    .take_while(|(_, candidate_depth)| candidate_depth == depth)
                    .count()
            })
            .unwrap_or(0);
        let (relationship, status, mapped_project_id) =
            match (matching_projects.first(), deepest_tied) {
                (None, _) => {
                    *unscoped_counts.entry(observed.store_kind).or_default() += 1;
                    (
                        LegacyPathRelationshipV1::Unscoped,
                        LegacyPathBindingStatusV1::UnscopedPreserved,
                        None,
                    )
                }
                (Some((project, _)), 1)
                    if project.path_status
                        == crate::project_catalog_inventory::LegacyProjectPathStatusV1::Missing =>
                {
                    refusals.push(semantic_refusal(
                        "legacy_selector_targets_missing_project",
                        [
                            observed.observation_id.clone(),
                            project.observation_id.clone(),
                        ],
                    ));
                    (
                        LegacyPathRelationshipV1::MissingProject,
                        LegacyPathBindingStatusV1::Refused,
                        None,
                    )
                }
                (Some((project, _)), 1)
                    if observed.selector_kind
                        == crate::project_catalog_inventory::LegacySelectorKindV1::Project
                        && literal_path
                            != runtime
                                .legacy_project_paths
                                .get(&project.observation_id)
                                .expect("matched legacy project path") =>
                {
                    refusals.push(semantic_refusal(
                        "project_selector_is_not_exact_root",
                        [
                            observed.observation_id.clone(),
                            project.observation_id.clone(),
                        ],
                    ));
                    (
                        LegacyPathRelationshipV1::UnsafeSelector,
                        LegacyPathBindingStatusV1::Refused,
                        None,
                    )
                }
                (Some((project, _)), 1) => {
                    let project_id = ProjectId::parse(project.record.project_id.clone())
                        .map_err(|_| planner_error("legacy project id is invalid"))?;
                    let root = runtime
                        .legacy_project_paths
                        .get(&project.observation_id)
                        .expect("matched legacy project path");
                    (
                        if literal_path == root {
                            LegacyPathRelationshipV1::ExactRoot
                        } else {
                            LegacyPathRelationshipV1::Contained
                        },
                        LegacyPathBindingStatusV1::Planned,
                        Some(project_id),
                    )
                }
                (Some(_), _) => {
                    refusals.push(semantic_refusal(
                        "ambiguous_legacy_selector",
                        std::iter::once(observed.observation_id.clone()).chain(
                            matching_projects
                                .iter()
                                .take(deepest_tied)
                                .map(|(project, _)| project.observation_id.clone()),
                        ),
                    ));
                    (
                        LegacyPathRelationshipV1::Ambiguous,
                        LegacyPathBindingStatusV1::Refused,
                        None,
                    )
                }
            };
        let planned_binding_id = identities
            .legacy_path_binding_ids
            .get(&observed.observation_id)
            .ok_or_else(|| planner_error("legacy path binding identity is missing"))?
            .clone();
        paths.push(ClassifiedLegacyPathV1 {
            observation_id: observed.observation_id.clone(),
            planned_binding_id: planned_binding_id.clone(),
            literal_selector: literal.clone(),
            relationship,
            mapped_project_id,
        });
        report_rows.push(LegacyPathBindingReportV1 {
            observation_id: observed.observation_id.clone(),
            planned_binding_id,
            store_kind: observed.store_kind,
            relationship,
            status,
            path_digest: digest_path(literal),
        });
        sensitive_rows.push(SensitiveLocalPathRowV1 {
            observation_id: observed.observation_id.clone(),
            store_kind: observed.store_kind,
            stable_row_id: observed.stable_row_id.clone(),
            literal_selector: literal.clone(),
        });
    }
    paths.sort_by(|left, right| left.observation_id.cmp(&right.observation_id));
    report_rows.sort_by(|left, right| left.observation_id.cmp(&right.observation_id));
    let sensitive_report = SensitiveLocalPathReportV1::from_runtime_rows(inventory, sensitive_rows)
        .map_err(inventory_error)?;
    canonicalize_refusals(&mut refusals);
    Ok(ClassifiedLegacyPathsV1 {
        paths,
        report_rows,
        sensitive_report,
        unscoped_counts,
        refusals,
    })
}

fn build_base_post_images(
    inventory: &V1ProjectCatalogInventory,
    runtime: &MigrationRuntimeBindingsViewV1,
    assessment: &MigrationSemanticAssessmentV1,
    identities: &MigrationPersistedIdentityPlanV1,
    resolution: &ProjectCatalogMigrationResolutionV1,
    classified_legacy_paths: &ClassifiedLegacyPathsV1,
) -> Result<MigrationBasePostImagesV1, MigrationBasePostImagesFailureV1> {
    let resolved_scopes = assessment
        .resolved_project_scopes
        .iter()
        .map(|row| (row.project_id.clone(), row))
        .collect::<BTreeMap<_, _>>();
    let project_history_ids = identities
        .repo_history_groups
        .iter()
        .flat_map(|group| {
            group
                .project_ids
                .iter()
                .map(move |project_id| (project_id.clone(), group.planned_history_id.clone()))
        })
        .collect::<BTreeMap<_, _>>();

    let mut repo_histories = BTreeMap::new();
    for group in &identities.repo_history_groups {
        let published_repo_ids = group
            .project_ids
            .iter()
            .filter_map(|project_id| {
                resolved_scopes
                    .get(project_id)
                    .and_then(|row| row.published_scope.as_ref())
                    .map(|scope| scope.repo_id().to_string())
            })
            .collect::<BTreeSet<_>>();
        let authority = match published_repo_ids.len() {
            1 => RepoHistoryAuthority::Recorded(
                RecordedRepoAuthority::parse(
                    published_repo_ids
                        .first()
                        .expect("one published repository authority")
                        .clone(),
                )
                .map_err(|_| planner_error("resolved repository authority is invalid"))?,
            ),
            0 if group.project_ids.len() == 1
                && group
                    .planned_primary_namespace
                    .as_str()
                    .starts_with("local_") =>
            {
                RepoHistoryAuthority::LocalProject(
                    group
                        .project_ids
                        .first()
                        .expect("single-project history group")
                        .clone(),
                )
            }
            0 => RepoHistoryAuthority::LegacyNamespace(group.planned_primary_namespace.clone()),
            _ => {
                let affected_record_ids = std::iter::once(group.group_id.clone())
                    .chain(group.project_ids.iter().map(ToString::to_string))
                    .collect();
                return Err(MigrationBasePostImagesFailureV1::Refused(
                    LateMigrationDomainRefusalV1::ConflictingPublishedAuthorities {
                        affected_record_ids,
                    },
                ));
            }
        };
        repo_histories.insert(
            group.planned_history_id.clone(),
            RepoHistoryRecord {
                repo_history_id: group.planned_history_id.clone(),
                authority,
                primary_namespace: group.planned_primary_namespace.clone(),
                compatibility_namespaces: group.planned_compatibility_namespaces.clone(),
                // Phase 1 never reads commit-document bodies or creates
                // history generations; the v1 importer emits this field
                // explicitly as typed NotBuilt (Phase 3 plan section 4.1).
                materialization: RepoHistoryMaterialization::NotBuilt,
            },
        );
    }

    let alias_candidates = inventory
        .legacy_projects
        .iter()
        .flat_map(|project| {
            let project_id = ProjectId::parse(project.record.project_id.clone())
                .expect("validated inventory project id");
            project
                .record
                .aliases
                .iter()
                .cloned()
                .map(move |alias| (alias, project_id.clone()))
        })
        .chain(
            inventory
                .materialized_aliases
                .iter()
                .map(|row| (row.alias.clone(), row.project_id.clone())),
        )
        .fold(
            BTreeMap::<String, BTreeSet<ProjectId>>::new(),
            |mut owners, (alias, project_id)| {
                owners.entry(alias).or_default().insert(project_id);
                owners
            },
        );
    let selected_alias_owners = resolution
        .selected_scope_owners
        .iter()
        .flat_map(|selection| {
            selection
                .owned_aliases
                .iter()
                .cloned()
                .map(move |alias| (alias, selection.owner_project_id.clone()))
        })
        .collect::<BTreeMap<_, _>>();
    let mut accepted_aliases = alias_candidates
        .into_iter()
        .filter_map(|(alias, owners)| {
            if owners.len() == 1 {
                Some((alias, owners.into_iter().next().expect("one alias owner")))
            } else {
                selected_alias_owners
                    .get(&alias)
                    .filter(|owner| owners.contains(*owner))
                    .cloned()
                    .map(|owner| (alias, owner))
            }
        })
        .fold(
            BTreeMap::<ProjectId, BTreeSet<String>>::new(),
            |mut by_project, (alias, owner)| {
                by_project.entry(owner).or_default().insert(alias);
                by_project
            },
        );

    let mut projects = BTreeMap::new();
    let mut missing_paths = Vec::new();
    for observed in &inventory.legacy_projects {
        let project_id = ProjectId::parse(observed.record.project_id.clone())
            .map_err(|_| planner_error("legacy project id is invalid"))?;
        let resolved = resolved_scopes
            .get(&project_id)
            .ok_or_else(|| planner_error("resolved project scope is incomplete"))?;
        let scope = resolved
            .published_scope
            .clone()
            .map(ProjectScope::Published)
            .unwrap_or(ProjectScope::LegacyLocal);
        if observed.path_status
            == crate::project_catalog_inventory::LegacyProjectPathStatusV1::Missing
        {
            missing_paths.push(MissingPathReportV1 {
                project_id: project_id.clone(),
                path_digest: observed.record.canonical_path_digest.clone(),
            });
        }
        projects.insert(
            project_id.clone(),
            CorpusProject {
                project_id: project_id.clone(),
                scope,
                operator_aliases: accepted_aliases.remove(&project_id).unwrap_or_default(),
                nominated_aliases: BTreeSet::new(),
                display_name: project_id.to_string(),
                created_at: observed.record.registered_at.clone(),
                registered_at_compat: Some(observed.record.registered_at.clone()),
                repo_history: project_history_ids.get(&project_id).cloned(),
                languages: observed.record.languages.clone(),
            },
        );
    }
    missing_paths.sort_by(|left, right| left.project_id.cmp(&right.project_id));

    let mut ambiguous_namespaces = BTreeMap::new();
    for cluster in &inventory.legacy_namespace_clusters {
        let namespace = CommitNamespace::parse(cluster.materialized_namespace.clone())
            .map_err(|_| planner_error("legacy namespace cluster is invalid"))?;
        let candidates = cluster
            .project_ids
            .iter()
            .filter_map(|project_id| project_history_ids.get(project_id).cloned())
            .collect::<BTreeSet<_>>();
        if candidates.len() < 2 {
            return Err(planner_error(
                "legacy namespace ambiguity lacks two planned history candidates",
            )
            .into());
        }
        ambiguous_namespaces.insert(
            namespace.clone(),
            AmbiguousNamespaceRecord {
                namespace,
                candidate_repo_history_ids: candidates,
                status: AmbiguousNamespaceStatus::Quarantined,
                // Same emission rule as repo_histories above: explicit typed
                // NotBuilt, not a relied-upon serde default.
                materialization: RepoHistoryQuarantineMaterialization::NotBuilt,
            },
        );
    }
    let catalog = CatalogSnapshotV2 {
        version: 2,
        epoch: 1,
        origin: CatalogOriginV2::MigratedV1 {
            transaction_id: identities.transaction_id.clone(),
        },
        projects,
        repo_histories,
        ambiguous_namespaces,
        scope_migrations: BTreeMap::new(),
    };

    let checkout_actions = identities
        .checkout_identity_actions
        .iter()
        .map(|action| {
            (
                action.observation_id.as_str(),
                action.planned_checkout_id.as_str(),
            )
        })
        .collect::<BTreeMap<_, _>>();
    let retained_checkout_observation_ids = inventory
        .attachment_candidates
        .iter()
        .filter(|candidate| {
            assessment
                .retained_attachment_ids
                .contains(&candidate.attachment_id)
        })
        .map(|candidate| candidate.checkout_observation_id.as_str())
        .collect::<BTreeSet<_>>();
    let checkout_ids = inventory
        .checkouts
        .iter()
        .filter(|checkout| {
            retained_checkout_observation_ids.contains(checkout.observation_id.as_str())
        })
        .map(|checkout| {
            let checkout_id = match &checkout.marker_state {
                crate::project_catalog_inventory::CheckoutMarkerStateV1::Valid { checkout_id } => {
                    checkout_id.as_str()
                }
                crate::project_catalog_inventory::CheckoutMarkerStateV1::MissingOrEmpty => {
                    checkout_actions
                        .get(checkout.observation_id.as_str())
                        .copied()
                        .ok_or_else(|| {
                            planner_error("missing checkout marker lacks its persisted planned id")
                        })?
                }
                crate::project_catalog_inventory::CheckoutMarkerStateV1::Malformed { .. }
                | crate::project_catalog_inventory::CheckoutMarkerStateV1::Unreadable { .. }
                | crate::project_catalog_inventory::CheckoutMarkerStateV1::Symlinked => {
                    return Err(
                        planner_error("unsafe checkout marker cannot form a post-image").into(),
                    );
                }
            };
            Ok((checkout.observation_id.as_str(), checkout_id))
        })
        .collect::<Result<BTreeMap<_, _>, ProjectCatalogMigrationError>>()?;
    let registered_at = inventory
        .legacy_projects
        .iter()
        .map(|project| {
            Ok((
                ProjectId::parse(project.record.project_id.clone())
                    .map_err(|_| planner_error("legacy project id is invalid"))?,
                project.record.registered_at.as_str(),
            ))
        })
        .collect::<Result<BTreeMap<_, _>, ProjectCatalogMigrationError>>()?;

    let mut post_image_attachments = Vec::new();
    let mut base_attachment_projects = BTreeSet::new();
    let mut attachment_snapshot_rows = BTreeMap::new();
    for observed in inventory.attachment_candidates.iter().filter(|row| {
        assessment
            .retained_attachment_ids
            .contains(&row.attachment_id)
    }) {
        let checkout_root = runtime
            .checkout_paths
            .get(&observed.checkout_observation_id)
            .ok_or_else(|| planner_error("attachment checkout runtime binding is missing"))?;
        let checkout_dir = checkout_root
            .to_str()
            .ok_or_else(|| planner_error("attachment checkout runtime path is not utf8"))?
            .to_string();
        let project_dir = if observed.base_relpath == "." {
            checkout_root.clone()
        } else {
            checkout_root.join(&observed.base_relpath)
        };
        let checkout_project_dir = project_dir
            .to_str()
            .ok_or_else(|| planner_error("attachment project runtime path is not utf8"))?
            .to_string();
        let checkout_id = checkout_ids
            .get(observed.checkout_observation_id.as_str())
            .copied()
            .ok_or_else(|| planner_error("attachment checkout identity is missing"))?
            .to_string();
        let attached_at = registered_at
            .get(&observed.project_id)
            .copied()
            .ok_or_else(|| planner_error("attachment project timestamp is missing"))?
            .to_string();
        let legacy_project = inventory
            .legacy_projects
            .iter()
            .find(|project| project.record.project_id == observed.project_id.as_str())
            .ok_or_else(|| planner_error("attachment project is absent from legacy inventory"))?;
        let legacy_project_path = runtime
            .legacy_project_paths
            .get(&legacy_project.observation_id)
            .ok_or_else(|| planner_error("legacy project runtime binding is missing"))?;
        let kind = if &project_dir == legacy_project_path {
            if !base_attachment_projects.insert(observed.project_id.clone()) {
                let affected_record_ids = inventory
                    .attachment_candidates
                    .iter()
                    .filter(|candidate| {
                        candidate.project_id == observed.project_id
                            && assessment
                                .retained_attachment_ids
                                .contains(&candidate.attachment_id)
                    })
                    .map(|candidate| candidate.observation_id.clone())
                    .chain(std::iter::once(legacy_project.observation_id.clone()))
                    .collect();
                return Err(MigrationBasePostImagesFailureV1::Refused(
                    LateMigrationDomainRefusalV1::MultipleBaseAttachments {
                        affected_record_ids,
                    },
                ));
            }
            AttachmentKind::Base
        } else {
            AttachmentKind::Worktree
        };
        let validated_scope = resolved_scopes
            .get(&observed.project_id)
            .ok_or_else(|| planner_error("resolved project scope is incomplete"))?
            .published_scope
            .clone();
        let has_history = project_history_ids.contains_key(&observed.project_id);
        attachment_snapshot_rows.insert(
            observed.attachment_id.clone(),
            CheckoutAttachment {
                attachment_id: observed.attachment_id.clone(),
                project_id: observed.project_id.clone(),
                checkout_id: checkout_id.clone(),
                checkout_dir,
                checkout_project_dir,
                project_root_relpath: observed.base_relpath.clone(),
                kind,
                validated_scope: validated_scope.clone(),
                computed_repo_hint: None,
                branch_ref: None,
                capabilities: AttachmentCapabilities {
                    local_code_source: true,
                    git_history: has_history,
                    blame: has_history,
                    repo_knowledge: true,
                    repo_mutation: true,
                    render_output: true,
                    provenance_note_io: true,
                    artifact_watching: true,
                },
                status: AttachmentStatus::Attached,
                attached_at: attached_at.clone(),
                detached_at: None,
            },
        );
        post_image_attachments.push(AttachmentPostImageInputV1 {
            attachment_id: observed.attachment_id.clone(),
            project_id: observed.project_id.clone(),
            checkout_observation_id: observed.checkout_observation_id.clone(),
            checkout_id,
            expected_scope: validated_scope,
            attached_at,
        });
    }
    for project in &inventory.legacy_projects {
        if project.path_status
            != crate::project_catalog_inventory::LegacyProjectPathStatusV1::Present
        {
            continue;
        }
        let project_id = ProjectId::parse(project.record.project_id.clone())
            .map_err(|_| planner_error("legacy project id is invalid"))?;
        let base_count = attachment_snapshot_rows
            .values()
            .filter(|attachment| {
                attachment.project_id == project_id && attachment.kind == AttachmentKind::Base
            })
            .count();
        if base_count != 1 {
            let affected_record_ids = std::iter::once(project.observation_id.clone())
                .chain(
                    inventory
                        .attachment_candidates
                        .iter()
                        .filter(|candidate| {
                            candidate.project_id == project_id
                                && assessment
                                    .retained_attachment_ids
                                    .contains(&candidate.attachment_id)
                        })
                        .map(|candidate| candidate.observation_id.clone()),
                )
                .collect();
            return Err(MigrationBasePostImagesFailureV1::Refused(
                LateMigrationDomainRefusalV1::MissingBaseAttachment {
                    affected_record_ids,
                },
            ));
        }
    }
    post_image_attachments.sort_by(|left, right| left.attachment_id.cmp(&right.attachment_id));

    let mut post_image_legacy_bindings = Vec::new();
    let mut legacy_path_bindings = BTreeMap::new();
    for classified in &classified_legacy_paths.paths {
        let source = inventory
            .legacy_path_observations
            .iter()
            .find(|row| row.observation_id == classified.observation_id)
            .ok_or_else(|| planner_error("classified legacy path source is missing"))?;
        let (ledger_status, attachment_id) = match &classified.mapped_project_id {
            Some(project_id) => {
                let ledger_relationship =
                    if classified.relationship == LegacyPathRelationshipV1::ExactRoot {
                        LegacyPathRelationship::Root
                    } else {
                        LegacyPathRelationship::ContainedSubdirectory
                    };
                let literal_path = Path::new(&classified.literal_selector);
                let attachment_id = attachment_snapshot_rows
                    .values()
                    .filter(|attachment| {
                        &attachment.project_id == project_id
                            && literal_path.starts_with(&attachment.checkout_project_dir)
                    })
                    .max_by_key(|attachment| {
                        Path::new(&attachment.checkout_project_dir)
                            .components()
                            .count()
                    })
                    .map(|attachment| attachment.attachment_id.clone());
                (
                    LegacyPathBindingStatus::Mapped {
                        project_id: project_id.clone(),
                        relationship: ledger_relationship,
                    },
                    attachment_id,
                )
            }
            None if classified.relationship == LegacyPathRelationshipV1::Unscoped => {
                (LegacyPathBindingStatus::Unscoped {}, None)
            }
            None => (LegacyPathBindingStatus::Quarantined {}, None),
        };
        post_image_legacy_bindings.push(LegacyPathBindingPostImageInputV1 {
            observation_id: classified.observation_id.clone(),
            planned_binding_id: classified.planned_binding_id.clone(),
            attachment_id,
            literal_selector: classified.literal_selector.clone(),
            relationship: classified.relationship,
        });
        legacy_path_bindings.insert(
            classified.planned_binding_id.clone(),
            LegacyPathLedgerEntry {
                legacy_path_binding_id: classified.planned_binding_id.clone(),
                historical_path: classified.literal_selector.clone(),
                source_store: legacy_store_kind_token(source.store_kind).to_string(),
                source_row_id: source.stable_row_id.clone(),
                inventory_epoch: 1,
                status: ledger_status,
            },
        );
    }
    post_image_legacy_bindings
        .sort_by(|left, right| left.observation_id.cmp(&right.observation_id));
    let attachments = AttachmentSnapshotV1 {
        version: 1,
        epoch: 1,
        attachments: attachment_snapshot_rows,
        scope_migration_proofs: BTreeMap::new(),
        legacy_path_bindings,
        default_attachments: BTreeMap::new(),
    };
    validate_catalog_attachments(&catalog, &attachments)
        .map_err(|_| planner_error("catalog and attachment post-images are inconsistent"))?;
    Ok(MigrationBasePostImagesV1 {
        catalog,
        attachments,
        post_image_attachments,
        post_image_legacy_bindings,
        legacy_binding_report: classified_legacy_paths.report_rows.clone(),
        sensitive_report: classified_legacy_paths.sensitive_report.clone(),
        missing_paths,
        unscoped_legacy_counts: classified_legacy_paths.unscoped_counts.clone(),
    })
}

fn legacy_store_kind_token(
    kind: crate::project_catalog_inventory::LegacyPathStoreKindV1,
) -> &'static str {
    use crate::project_catalog_inventory::LegacyPathStoreKindV1 as Kind;
    match kind {
        Kind::Knowledge => "knowledge",
        Kind::Gap => "gap",
        Kind::Thread => "thread",
        Kind::Note => "note",
        Kind::Pin => "pin",
        Kind::Roadmap => "roadmap",
        Kind::Packet => "packet",
        Kind::Task => "task",
        Kind::Proposal => "proposal",
        Kind::SlackBinding => "slack_binding",
        Kind::Whiteboard => "whiteboard",
        Kind::Artifact => "artifact",
        Kind::Provenance => "provenance",
        Kind::TranscriptEdge => "transcript_edge",
    }
}

fn prepare_publisher_plan(
    inventory: &V1ProjectCatalogInventory,
    runtime: &MigrationRuntimeBindingsViewV1,
    assessment: &MigrationSemanticAssessmentV1,
    resolution: &ProjectCatalogMigrationResolutionV1,
) -> Result<PreparedPublisherPlanV1, ProjectCatalogMigrationError> {
    let requested = resolution
        .publisher_binding_dispositions
        .iter()
        .map(|row| {
            (
                (
                    row.project_id().clone(),
                    row.expected_scope().clone(),
                    row.full_ref().to_string(),
                ),
                row,
            )
        })
        .collect::<BTreeMap<_, _>>();
    let mut dispositions = Vec::new();
    let mut prepared_by_project = BTreeMap::new();
    let effective_pins = resolved_publisher_pins(inventory, resolution).map_err(inventory_error)?;
    for pin in &effective_pins {
        let key = (
            pin.project_id.clone(),
            pin.expected_scope.clone(),
            pin.full_ref.clone(),
        );
        let retained_candidates = pin
            .candidate_attachment_ids
            .intersection(&assessment.retained_attachment_ids)
            .cloned()
            .collect::<BTreeSet<_>>();
        let automatic = retained_candidates.len() == 1
            && pin.resolved_commit.is_some()
            && pin.resolved_scope.as_ref() == Some(&pin.expected_scope);
        let disposition = if automatic {
            let attachment_id = retained_candidates
                .first()
                .expect("one automatic publisher attachment")
                .clone();
            let accepted_commit = pin
                .resolved_commit
                .as_deref()
                .expect("automatic publisher commit");
            let prepared = prepare_publisher_generation(
                inventory,
                runtime,
                &pin.project_id,
                &attachment_id,
                &pin.expected_scope,
                &pin.full_ref,
                accepted_commit,
            )?;
            let disposition =
                publisher_seed_disposition(&pin.project_id, &attachment_id, &prepared)?;
            prepared_by_project.insert(pin.project_id.clone(), prepared);
            disposition
        } else {
            match requested.get(&key).copied().ok_or_else(|| {
                planner_error("clean publisher binding lacks its reviewed disposition")
            })? {
                PublisherBindingDispositionV1::SeedG1 {
                    attachment_id,
                    accepted_commit,
                    ..
                } => {
                    if !assessment.retained_attachment_ids.contains(attachment_id) {
                        return Err(planner_error(
                            "publisher disposition selects an excluded attachment",
                        ));
                    }
                    let prepared = prepare_publisher_generation(
                        inventory,
                        runtime,
                        &pin.project_id,
                        attachment_id,
                        &pin.expected_scope,
                        &pin.full_ref,
                        accepted_commit,
                    )?;
                    let exact =
                        publisher_seed_disposition(&pin.project_id, attachment_id, &prepared)?;
                    if requested.get(&key).copied() != Some(&exact) {
                        return Err(ProjectCatalogMigrationError::no_mutation(
                            "error.project_catalog_migration_publisher_prediction_mismatch",
                            "reviewed publisher seed does not match exact committed source bytes",
                        ));
                    }
                    prepared_by_project.insert(pin.project_id.clone(), prepared);
                    exact
                }
                disposition @ PublisherBindingDispositionV1::NoPublishedContentAcknowledged {
                    ..
                } => disposition.clone(),
            }
        };
        dispositions.push(disposition);
    }
    dispositions.sort_by(|left, right| {
        (left.project_id(), left.expected_scope(), left.full_ref()).cmp(&(
            right.project_id(),
            right.expected_scope(),
            right.full_ref(),
        ))
    });
    Ok(PreparedPublisherPlanV1 {
        dispositions,
        prepared: prepared_by_project,
    })
}

fn prepare_publisher_generation(
    inventory: &V1ProjectCatalogInventory,
    runtime: &MigrationRuntimeBindingsViewV1,
    project_id: &ProjectId,
    attachment_id: &AttachmentId,
    scope: &bbox_corpus_core::identity::PublishedScope,
    full_ref: &str,
    accepted_commit: &str,
) -> Result<PreparedAcceptedPublicationV1, ProjectCatalogMigrationError> {
    let attachment = inventory
        .attachment_candidates
        .iter()
        .find(|row| &row.attachment_id == attachment_id && &row.project_id == project_id)
        .ok_or_else(|| planner_error("publisher attachment is not inventoried for the project"))?;
    let repository = runtime
        .checkout_repositories
        .get(&attachment.checkout_observation_id)
        .ok_or_else(|| planner_error("publisher checkout repository authority is missing"))?;
    let commit = repository
        .verify_commit_oid(accepted_commit)
        .map_err(|_| planner_error("publisher accepted commit cannot be verified exactly"))?;
    let scope_root = scope.bbox_root_relpath();
    let config_relpath = repo_relative_lane_root(scope_root, "config.toml");
    let config_bytes =
        read_verified_committed_file_bytes_optional_bounded(&commit, &config_relpath, 1024 * 1024)
            .map_err(|_| planner_error("publisher accepted commit config cannot be read exactly"))?
            .ok_or_else(|| planner_error("publisher accepted commit does not declare its scope"))?;
    let config_source = std::str::from_utf8(&config_bytes)
        .map_err(|_| planner_error("publisher accepted commit config is not UTF-8"))?;
    let config_root = repository.repository_root().join(
        (scope_root != ".")
            .then_some(scope_root)
            .unwrap_or_default(),
    );
    let inputs =
        bbox_config::config::repo_id_inputs_from_project_config_source(&config_root, config_source)
            .map_err(|_| planner_error("publisher accepted commit config is invalid"))?;
    let recorded = resolve_recorded_repo_id(&inputs)
        .ok_or_else(|| planner_error("publisher accepted commit lacks recorded authority"))?;
    let declared_scope = PublishedScope::try_new(recorded, scope_root)
        .map_err(|_| planner_error("publisher accepted commit scope is invalid"))?;
    if &declared_scope != scope {
        return Err(planner_error(
            "publisher accepted commit declares a different scope",
        ));
    }
    let knowledge_root = repo_relative_lane_root(scope_root, "knowledge");
    let gap_root = repo_relative_lane_root(scope_root, "gaps");
    let knowledge = read_publication_lane(&commit, &knowledge_root)?
        .into_iter()
        .map(
            |(repository_relative_filename, source_bytes)| AcceptedKnowledgeSourceV1 {
                repository_relative_filename,
                source_bytes,
            },
        )
        .collect();
    let gaps = read_publication_lane(&commit, &gap_root)?
        .into_iter()
        .map(
            |(repository_relative_filename, source_bytes)| AcceptedGapSourceV1 {
                repository_relative_filename,
                source_bytes,
            },
        )
        .collect();
    let full_ref = FullPublisherRef::parse(full_ref.to_string())
        .map_err(|_| planner_error("publisher full ref is invalid"))?;
    let accepted_commit = GitObjectId::parse(accepted_commit.to_string())
        .map_err(|_| planner_error("publisher accepted commit is invalid"))?;
    prepare_accepted_publication_v1(
        AcceptedPublicationBuildInputV1 {
            project_id: project_id.clone(),
            attachment_id: attachment_id.clone(),
            scope: scope.clone(),
            full_ref,
            accepted_commit,
            knowledge,
            gaps,
            prior_pointer: None,
        },
        &AcceptedPublicationLimits::default(),
    )
    .map_err(|error| {
        ProjectCatalogMigrationError::no_mutation(
            error.code(),
            "accepted publication G1 preparation failed",
        )
    })
}

fn repo_relative_lane_root(scope_root: &str, lane: &str) -> String {
    if scope_root == "." {
        format!(".bbox/{lane}")
    } else {
        format!("{scope_root}/.bbox/{lane}")
    }
}

fn read_publication_lane(
    commit: &bbox_corpus_core::git::VerifiedCommit,
    root: &str,
) -> Result<Vec<(String, Vec<u8>)>, ProjectCatalogMigrationError> {
    use crate::accepted_publication_store::{
        MAX_ACCEPTED_PUBLICATION_ENTRIES_PER_LANE, MAX_ACCEPTED_PUBLICATION_SOURCE_BYTES_PER_LANE,
        MAX_ACCEPTED_PUBLICATION_SOURCE_FILE_BYTES,
    };
    let max_listing_bytes =
        usize::try_from(MAX_ACCEPTED_PUBLICATION_SOURCE_BYTES_PER_LANE).unwrap_or(usize::MAX);
    let paths = list_verified_committed_dir_bounded(
        commit,
        root,
        MAX_ACCEPTED_PUBLICATION_ENTRIES_PER_LANE,
        max_listing_bytes,
    )
    .map_err(|_| planner_error("publisher committed lane cannot be enumerated safely"))?;
    paths
        .into_iter()
        .map(|path| {
            let bytes = read_verified_committed_file_bytes_bounded(
                commit,
                &path,
                usize::try_from(MAX_ACCEPTED_PUBLICATION_SOURCE_FILE_BYTES).unwrap_or(usize::MAX),
            )
            .map_err(|_| planner_error("publisher committed source cannot be read safely"))?;
            Ok((path, bytes))
        })
        .collect()
}

fn publisher_seed_disposition(
    project_id: &ProjectId,
    attachment_id: &AttachmentId,
    prepared: &PreparedAcceptedPublicationV1,
) -> Result<PublisherBindingDispositionV1, ProjectCatalogMigrationError> {
    let hashes = &prepared.generation.hashes;
    Ok(PublisherBindingDispositionV1::SeedG1 {
        project_id: project_id.clone(),
        attachment_id: attachment_id.clone(),
        expected_scope: prepared.generation.scope.clone(),
        full_ref: prepared.generation.full_ref.as_str().to_string(),
        accepted_commit: prepared.generation.accepted_commit.as_str().to_string(),
        generation_id: prepared.generation_id.as_str().to_string(),
        payload_hashes: PublicationPayloadHashesV1 {
            knowledge_manifest_hash: Sha256ValueV1::parse(
                hashes.knowledge_file_manifest_sha256.as_str().to_string(),
            )
            .map_err(inventory_error)?,
            gap_manifest_hash: Sha256ValueV1::parse(
                hashes.gap_file_manifest_sha256.as_str().to_string(),
            )
            .map_err(inventory_error)?,
            knowledge_payload_hash: Sha256ValueV1::parse(
                hashes.normalized_knowledge_sha256.as_str().to_string(),
            )
            .map_err(inventory_error)?,
            gap_payload_hash: Sha256ValueV1::parse(
                hashes.normalized_gaps_sha256.as_str().to_string(),
            )
            .map_err(inventory_error)?,
        },
        pointer_hash: Sha256ValueV1::parse(prepared.pointer_hash.as_str().to_string())
            .map_err(inventory_error)?,
    })
}

fn prepare_store_plan_parts(
    inventory: &V1ProjectCatalogInventory,
    resolution: &ProjectCatalogMigrationResolutionV1,
    legacy_code_source: &bbox_code_source_store::MigrationLegacyInventoryV1,
    post_image: &DeterministicPostImageInputV1,
    plan_hash: &Sha256ValueV1,
    publisher: &PreparedPublisherPlanV1,
) -> Result<MigrationStorePlanPartsV1, ProjectCatalogMigrationError> {
    let quarantined = post_image
        .quarantined_collected
        .iter()
        .map(|row| (row.project_id.clone(), row.generation_id.as_str()))
        .collect::<BTreeSet<_>>();
    let legacy_activations = legacy_code_source
        .activations
        .iter()
        .map(|row| (row.project_id.clone(), row))
        .collect::<BTreeMap<_, _>>();
    let mut generation_plans = Vec::new();
    let mut generation_dispositions = BTreeMap::new();
    let mut participants = Vec::new();
    let mut immutable_assets = Vec::new();
    for generation in &legacy_code_source.generations {
        let (project_id, observation_id) =
            resolved_generation_owner(inventory, post_image, generation)?;
        let disposition = if quarantined
            .contains(&(project_id.clone(), generation.generation_id.as_str()))
        {
            MigrationCodeSourceDispositionV1::QuarantinedCollision
        } else if legacy_activations
            .get(&project_id)
            .is_some_and(|activation| activation.record.generation_id == generation.generation_id)
        {
            MigrationCodeSourceDispositionV1::SurvivingActive
        } else {
            MigrationCodeSourceDispositionV1::SurvivingRetained
        };
        let generation_id = store_sha256(&generation.generation_id)?;
        let stored = StoredGenerationV2::from_v1_for_migration(
            generation.record.clone(),
            generation.published_scope.clone(),
        )
        .map_err(|_| planner_error("legacy stored generation cannot convert to v2"))?;
        let stored_bytes = encode_stored_generation_v2_for_migration(&stored)
            .map_err(|_| planner_error("v2 stored generation cannot be encoded"))?;
        participants.push(MigrationParticipantDraftV1::new(
            ParticipantRoleV1::StoredGenerationMetadata {
                project_id: project_id.clone(),
                published_scope: generation.published_scope.clone(),
                generation_id: generation_id.clone(),
            },
            Some(store_sha256(&generation.metadata_sha256)?),
            Some(stored_bytes),
        ));
        immutable_assets.push(MigrationImmutableAssetDraftV1::pinned_existing(
            ImmutableAssetRoleV1::CollectedGenerationManifest {
                published_scope: generation.published_scope.clone(),
                generation_id: generation_id.clone(),
            },
            store_sha256(&generation.manifest_sha256)?,
        ));
        generation_plans.push(MigrationCodeSourceGenerationDraftV1 {
            observation_id,
            project_id: project_id.clone(),
            generation_id: generation_id.clone(),
            disposition,
        });
        generation_dispositions.insert(
            generation.generation_id.as_str(),
            (project_id, disposition, generation),
        );
    }

    let mut activation_plans = Vec::new();
    let mut effective_selections = Vec::new();
    for activation in &legacy_code_source.activations {
        let (project_id, disposition, generation) = generation_dispositions
            .get(activation.record.generation_id.as_str())
            .map(|(project_id, disposition, generation)| {
                (project_id.clone(), *disposition, *generation)
            })
            .ok_or_else(|| planner_error("legacy activation generation is not protected"))?;
        if project_id != activation.project_id {
            return Err(planner_error(
                "legacy activation and generation ownership disagree",
            ));
        }
        let observation_id = inventory
            .code_sources
            .iter()
            .find(|source| source.project_id == project_id)
            .map(|source| source.observation_id.clone())
            .ok_or_else(|| planner_error("activation observation is missing"))?;
        activation_plans.push(MigrationCodeSourceActivationDraftV1 {
            observation_id,
            project_id: project_id.clone(),
            disposition,
        });
        let role = ParticipantRoleV1::Activation {
            project_id: project_id.clone(),
        };
        let post_bytes = if disposition == MigrationCodeSourceDispositionV1::SurvivingActive {
            let stored = StoredGenerationV2::from_v1_for_migration(
                generation.record.clone(),
                generation.published_scope.clone(),
            )
            .map_err(|_| planner_error("legacy active generation cannot convert to v2"))?;
            let converted =
                ActivationRecordV2::from_v1_for_migration(activation.record.clone(), &stored)
                    .map_err(|_| planner_error("legacy activation cannot convert to v2"))?;
            effective_selections.push(MigrationEffectiveSourceSelectionV1 {
                project_id: project_id.clone(),
                published_scope: generation.published_scope.clone(),
                generation_id: generation.generation_id.clone(),
                selector: activation.record.selector.clone(),
            });
            Some(
                encode_activation_v2_for_migration(&converted)
                    .map_err(|_| planner_error("v2 activation cannot be encoded"))?,
            )
        } else {
            None
        };
        participants.push(MigrationParticipantDraftV1::new(
            role,
            Some(store_sha256(&activation.sha256)?),
            post_bytes,
        ));
    }
    effective_selections.sort_by(|left, right| left.project_id.cmp(&right.project_id));
    let effective_bytes =
        encode_migration_effective_source_manifest_v1(&MigrationEffectiveSourceManifestV1 {
            version: 1,
            selections: effective_selections,
        })
        .map_err(|_| planner_error("effective source manifest cannot be encoded"))?;
    let effective_old = match &legacy_code_source.anchor {
        bbox_code_source_store::MigrationLegacyAnchorEvidenceV1::Missing => None,
        bbox_code_source_store::MigrationLegacyAnchorEvidenceV1::Present { sha256, .. } => {
            Some(store_sha256(sha256)?)
        }
    };
    participants.push(MigrationParticipantDraftV1::new(
        ParticipantRoleV1::EffectiveSourceManifest,
        effective_old,
        Some(effective_bytes),
    ));

    let collision_projects = generation_dispositions
        .values()
        .filter(|(_, disposition, _)| {
            *disposition == MigrationCodeSourceDispositionV1::QuarantinedCollision
        })
        .map(|(project_id, _, _)| project_id.clone())
        .collect::<BTreeSet<_>>();
    for project_id in collision_projects {
        let mut entries = BTreeMap::new();
        for (generation_id, (owner, disposition, generation)) in &generation_dispositions {
            if owner != &project_id
                || *disposition != MigrationCodeSourceDispositionV1::QuarantinedCollision
            {
                continue;
            }
            let activation = legacy_activations
                .get(&project_id)
                .filter(|activation| activation.record.generation_id == **generation_id);
            let selector_evidence = activation.map_or(
                CollisionRetirementSelectorEvidenceV1::NoDurableSelector,
                |activation| {
                    CollisionRetirementSelectorEvidenceV1::ExactMaterialized(
                        activation.record.selector.clone(),
                    )
                },
            );
            let snapshot_id = activation.map_or_else(
                || format!("collected-{}", &generation.generation_id[..32]),
                |activation| activation.record.snapshot_id.clone(),
            );
            entries.insert(
                (*generation_id).to_string(),
                CollisionRetirementEntryV1 {
                    state: CollisionRetirementLifecycleStateV1::Pending,
                    former_scope: generation.published_scope.clone(),
                    selector_evidence,
                    snapshot_id,
                    manifest_sha256: generation.record.descriptor.manifest_sha256.clone(),
                    inventory_hash: post_image.inventory_hash.to_string(),
                    plan_hash: plan_hash.to_string(),
                },
            );
        }
        let lifecycle = CollisionRetirementLifecycleV1 {
            version: 1,
            project_id: project_id.clone(),
            entries,
        };
        let bytes = encode_collision_retirement_pending_for_migration(&lifecycle)
            .map_err(|_| planner_error("collision retirement lifecycle cannot be encoded"))?;
        let old_hash = legacy_code_source
            .collision_pending
            .iter()
            .find(|row| row.project_id == project_id)
            .map(|row| store_sha256(&row.sha256))
            .transpose()?;
        participants.push(MigrationParticipantDraftV1::new(
            ParticipantRoleV1::CollisionRetirement { project_id },
            old_hash,
            Some(bytes),
        ));
    }

    let (publisher_pins, publisher_dispositions) =
        prepare_publisher_store_evidence(inventory, publisher, resolution)?;
    for (project_id, prepared) in &publisher.prepared {
        participants.push(MigrationParticipantDraftV1::new(
            ParticipantRoleV1::AcceptedPublicationPointer {
                project_id: project_id.clone(),
            },
            None,
            Some(prepared.pointer_bytes.clone()),
        ));
        immutable_assets.push(MigrationImmutableAssetDraftV1::new(
            ImmutableAssetRoleV1::AcceptedPublicationGeneration {
                project_id: project_id.clone(),
                generation_id: prepared.generation_id.clone(),
            },
            prepared.generation_bytes.clone(),
        ));
    }
    Ok(MigrationStorePlanPartsV1 {
        participants,
        immutable_assets,
        code_source_snapshot: MigrationCodeSourceSnapshotDraftV1 {
            legacy_inventory: legacy_code_source.clone(),
            activations: activation_plans,
            generations: generation_plans,
        },
        publisher_pins,
        publisher_dispositions,
    })
}

fn resolved_generation_owner<'a>(
    inventory: &V1ProjectCatalogInventory,
    post_image: &DeterministicPostImageInputV1,
    generation: &bbox_code_source_store::MigrationLegacyGenerationEvidenceV1,
) -> Result<(ProjectId, String), ProjectCatalogMigrationError> {
    for source in &inventory.code_sources {
        if let Some(observed) = source
            .generations
            .iter()
            .find(|row| row.generation_id == generation.generation_id)
        {
            return Ok((observed.project_id.clone(), observed.observation_id.clone()));
        }
        if let Some(observed) = source
            .quarantine
            .iter()
            .find(|row| row.generation_id == generation.generation_id)
        {
            return Ok((observed.project_id.clone(), observed.observation_id.clone()));
        }
    }
    let retained = inventory
        .retained_owner_resolutions
        .iter()
        .find(|row| row.generation_id == generation.generation_id)
        .ok_or_else(|| planner_error("protected generation lacks an inventory observation"))?;
    if let Some(quarantine) = post_image
        .quarantined_collected
        .iter()
        .find(|row| row.generation_id == generation.generation_id)
    {
        if retained
            .candidate_project_ids
            .contains(&quarantine.project_id)
        {
            return Ok((
                quarantine.project_id.clone(),
                retained.observation_id.clone(),
            ));
        }
    }
    let owners = post_image
        .resolved_project_scopes
        .iter()
        .filter(|row| {
            retained.candidate_project_ids.contains(&row.project_id)
                && row.published_scope.as_ref() == Some(&retained.published_scope)
        })
        .map(|row| row.project_id.clone())
        .collect::<BTreeSet<_>>();
    if owners.len() != 1 {
        return Err(planner_error(
            "retained generation lacks one reviewed canonical owner",
        ));
    }
    Ok((
        owners.into_iter().next().expect("one retained owner"),
        retained.observation_id.clone(),
    ))
}

fn prepare_publisher_store_evidence(
    inventory: &V1ProjectCatalogInventory,
    publisher: &PreparedPublisherPlanV1,
    resolution: &ProjectCatalogMigrationResolutionV1,
) -> Result<
    (
        Vec<PublisherPinEvidenceV1>,
        Vec<PublisherDispositionEvidenceV1>,
    ),
    ProjectCatalogMigrationError,
> {
    let effective_pins = resolved_publisher_pins(inventory, resolution).map_err(inventory_error)?;
    let pins = effective_pins
        .iter()
        .map(|pin| {
            Ok(PublisherPinEvidenceV1 {
                observation_id: pin.observation_id.clone(),
                project_id: pin.project_id.clone(),
                expected_scope: pin.expected_scope.clone(),
                full_ref: FullPublisherRef::parse(pin.full_ref.clone())
                    .map_err(|_| planner_error("publisher full ref is invalid"))?,
                candidate_attachment_ids: pin.candidate_attachment_ids.clone(),
                resolved_commit: pin
                    .resolved_commit
                    .as_ref()
                    .map(|commit| {
                        GitObjectId::parse(commit.clone())
                            .map_err(|_| planner_error("publisher commit is invalid"))
                    })
                    .transpose()?,
                resolved_scope: pin.resolved_scope.clone(),
                source_observation_ids: pin.source_observation_ids.clone(),
            })
        })
        .collect::<Result<Vec<_>, ProjectCatalogMigrationError>>()?;
    let observations = inventory
        .publisher_pins
        .iter()
        .map(|pin| {
            (
                (
                    pin.project_id.clone(),
                    pin.expected_scope.clone(),
                    pin.full_ref.as_str(),
                ),
                pin.observation_id.as_str(),
            )
        })
        .collect::<BTreeMap<_, _>>();
    let dispositions = publisher
        .dispositions
        .iter()
        .map(|disposition| {
            let observation_id = observations
                .get(&(
                    disposition.project_id().clone(),
                    disposition.expected_scope().clone(),
                    disposition.full_ref(),
                ))
                .copied()
                .ok_or_else(|| planner_error("publisher disposition lacks an inventory pin"))?
                .to_string();
            match disposition {
                PublisherBindingDispositionV1::SeedG1 {
                    project_id,
                    attachment_id,
                    expected_scope,
                    full_ref,
                    accepted_commit,
                    ..
                } => {
                    let prepared = publisher.prepared.get(project_id).ok_or_else(|| {
                        planner_error("publisher seed lacks its prepared generation")
                    })?;
                    Ok(PublisherDispositionEvidenceV1::SeedG1 {
                        observation_id,
                        project_id: project_id.clone(),
                        attachment_id: attachment_id.clone(),
                        expected_scope: expected_scope.clone(),
                        full_ref: FullPublisherRef::parse(full_ref.clone())
                            .map_err(|_| planner_error("publisher full ref is invalid"))?,
                        accepted_commit: GitObjectId::parse(accepted_commit.clone())
                            .map_err(|_| planner_error("publisher commit is invalid"))?,
                        generation_id: prepared.generation_id.clone(),
                        generation_sha256: store_sha256(prepared.generation_hash.as_str())?,
                        pointer_sha256: store_sha256(prepared.pointer_hash.as_str())?,
                    })
                }
                PublisherBindingDispositionV1::NoPublishedContentAcknowledged {
                    project_id,
                    expected_scope,
                    full_ref,
                    bounded_reason,
                } => Ok(
                    PublisherDispositionEvidenceV1::NoPublishedContentAcknowledged {
                        observation_id,
                        project_id: project_id.clone(),
                        expected_scope: expected_scope.clone(),
                        full_ref: FullPublisherRef::parse(full_ref.clone())
                            .map_err(|_| planner_error("publisher full ref is invalid"))?,
                        bounded_reason: bounded_reason.clone(),
                    },
                ),
            }
        })
        .collect::<Result<Vec<_>, ProjectCatalogMigrationError>>()?;
    Ok((pins, dispositions))
}

fn store_sha256(value: &str) -> Result<Sha256Hex, ProjectCatalogMigrationError> {
    Sha256Hex::parse(value.to_string())
        .map_err(|_| planner_error("planned SHA-256 identity is invalid"))
}

fn build_migration_report(
    inventory: &V1ProjectCatalogInventory,
    resolution_bytes: &[u8],
    assessment: &MigrationSemanticAssessmentV1,
    identities: &MigrationPersistedIdentityPlanV1,
    plan_hash: Sha256ValueV1,
    predicted: PredictedPostImageHashesV1,
    predicted_immutable_asset_hashes: BTreeMap<String, Sha256ValueV1>,
    legacy_path_bindings: Vec<LegacyPathBindingReportV1>,
    missing_paths: Vec<MissingPathReportV1>,
    unscoped_legacy_counts: BTreeMap<crate::project_catalog_inventory::LegacyPathStoreKindV1, u64>,
    status: ProjectCatalogMigrationStatusV1,
) -> Result<ProjectCatalogMigrationReportV1, ProjectCatalogMigrationError> {
    let generated_at = inventory
        .legacy_projects
        .iter()
        .map(|project| project.record.registered_at.as_str())
        .max()
        .unwrap_or("1970-01-01T00:00:00Z")
        .to_string();
    let projects = inventory
        .legacy_projects
        .iter()
        .map(|row| {
            Ok(ProjectMigrationReportRowV1 {
                observation_id: row.observation_id.clone(),
                project_id: ProjectId::parse(row.record.project_id.clone())
                    .map_err(|_| planner_error("legacy project id is invalid"))?,
                path_status: row.path_status,
                path_digest: row.record.canonical_path_digest.clone(),
                committed_authority_present: row.committed_authority.is_some(),
            })
        })
        .collect::<Result<Vec<_>, ProjectCatalogMigrationError>>()?;
    let attachments = inventory
        .attachment_candidates
        .iter()
        .map(|row| {
            Ok(AttachmentMigrationReportRowV1 {
                observation_id: row.observation_id.clone(),
                attachment_id: identities
                    .attachment_ids
                    .get(&row.observation_id)
                    .ok_or_else(|| planner_error("attachment identity plan is incomplete"))?
                    .clone(),
                project_id: row.project_id.clone(),
                checkout_observation_id: row.checkout_observation_id.clone(),
                scope_digest: row
                    .observed_scope
                    .as_ref()
                    .map(digest_published_scope)
                    .transpose()
                    .map_err(inventory_error)?,
            })
        })
        .collect::<Result<Vec<_>, ProjectCatalogMigrationError>>()?;
    let refusals = assessment.refusals.clone();
    let report = ProjectCatalogMigrationReportV1 {
        version: 1,
        transaction_id: identities.transaction_id.clone(),
        inventory_hash: inventory.inventory_hash().map_err(inventory_error)?,
        plan_hash,
        resolution_artifact_hash: Sha256ValueV1::digest(resolution_bytes),
        source_store_hash: inventory.source_store_hash.clone(),
        publisher_ref_source_hash: inventory.publisher_ref_source_hash.clone(),
        generated_at,
        status,
        plan_kind: if status == ProjectCatalogMigrationStatusV1::Clean {
            ProjectCatalogMigrationPlanKindV1::Executable
        } else {
            ProjectCatalogMigrationPlanKindV1::AssessmentOnly
        },
        projects,
        repo_history_groups: identities.repo_history_groups.clone(),
        attachments,
        checkout_identity_actions: identities.checkout_identity_actions.clone(),
        legacy_path_bindings,
        namespace_conflicts: assessment.namespace_conflicts.clone(),
        scope_conflicts: assessment.scope_conflicts.clone(),
        alias_conflicts: assessment.alias_conflicts.clone(),
        activation_conflicts: assessment.activation_conflicts.clone(),
        publisher_bindings: assessment.publisher_bindings.clone(),
        publisher_binding_conflicts: assessment.publisher_binding_conflicts.clone(),
        predicted_g1_assets: predicted.g1_assets.clone(),
        predicted_accepted_pointer_hashes: predicted.accepted_pointer_hashes.clone(),
        missing_paths,
        unscoped_legacy_counts,
        required_resolutions: assessment.required_resolutions.clone(),
        refusals,
        predicted_catalog_hash: predicted.catalog_hash.clone(),
        predicted_attachment_hash: predicted.attachment_hash.clone(),
        predicted_participant_hashes: predicted.participant_hashes.clone(),
        predicted_immutable_asset_hashes,
    };
    report
        .validate_against_inventory(inventory)
        .map_err(inventory_error)?;
    Ok(report)
}

fn missing_project_rows(
    inventory: &V1ProjectCatalogInventory,
) -> Result<Vec<MissingPathReportV1>, ProjectCatalogMigrationError> {
    inventory
        .legacy_projects
        .iter()
        .filter(|project| {
            project.path_status
                == crate::project_catalog_inventory::LegacyProjectPathStatusV1::Missing
        })
        .map(|project| {
            Ok(MissingPathReportV1 {
                project_id: ProjectId::parse(project.record.project_id.clone())
                    .map_err(|_| planner_error("legacy project id is invalid"))?,
                path_digest: project.record.canonical_path_digest.clone(),
            })
        })
        .collect()
}

fn non_executable_assessment_hash(
    inventory: &V1ProjectCatalogInventory,
    resolution_bytes: &[u8],
    identities: &MigrationPersistedIdentityPlanV1,
    status: ProjectCatalogMigrationStatusV1,
) -> Result<Sha256ValueV1, ProjectCatalogMigrationError> {
    let bytes = serde_json::to_vec(&(
        "blackbox.project-catalog.non-executable-assessment.v1",
        inventory.inventory_hash().map_err(inventory_error)?,
        Sha256ValueV1::digest(resolution_bytes),
        identities.transaction_id.as_str(),
        status,
    ))
    .map_err(|_| planner_error("non-executable assessment identity cannot be encoded"))?;
    Ok(Sha256ValueV1::digest(&bytes))
}

fn encode_facade_sensitive_review(
    inventory: &V1ProjectCatalogInventory,
    runtime: &MigrationRuntimeBindingsViewV1,
    legacy_paths: &SensitiveLocalPathReportV1,
) -> Result<(Vec<u8>, u64), ProjectCatalogMigrationError> {
    let checkouts = inventory
        .checkouts
        .iter()
        .map(|row| (row.observation_id.as_str(), row))
        .collect::<BTreeMap<_, _>>();
    let mut attachment_paths = Vec::new();
    for attachment in &inventory.attachment_candidates {
        let checkout = checkouts
            .get(attachment.checkout_observation_id.as_str())
            .ok_or_else(|| planner_error("sensitive attachment checkout is missing"))?;
        let checkout_root = runtime
            .checkout_paths
            .get(&attachment.checkout_observation_id)
            .ok_or_else(|| planner_error("sensitive attachment runtime root is missing"))?;
        let checkout_root_text = checkout_root
            .to_str()
            .ok_or_else(|| planner_error("sensitive attachment runtime root is not utf8"))?
            .to_string();
        if digest_path(&checkout_root_text) != checkout.canonical_root_digest {
            return Err(planner_error(
                "sensitive attachment checkout digest disagrees with inventory",
            ));
        }
        let project_path = checkout_root.join(&attachment.base_relpath);
        let project_path_text = project_path
            .to_str()
            .ok_or_else(|| planner_error("sensitive attachment project path is not utf8"))?
            .to_string();
        attachment_paths.push(FacadeSensitiveAttachmentPathV1 {
            observation_id: attachment.observation_id.clone(),
            attachment_id: attachment.attachment_id.clone(),
            checkout_observation_id: attachment.checkout_observation_id.clone(),
            checkout_root: checkout_root_text,
            checkout_root_digest: checkout.canonical_root_digest.clone(),
            project_path_digest: digest_path(&project_path_text),
            project_path: project_path_text,
        });
    }
    attachment_paths.sort_by(|left, right| left.observation_id.cmp(&right.observation_id));
    let inventory_hash = inventory.inventory_hash().map_err(inventory_error)?;
    let value = FacadeSensitiveReviewV1 {
        version: 1,
        inventory_hash: &inventory_hash,
        local_paths_included: true,
        warning: "host_local_sensitive_do_not_commit",
        legacy_paths,
        attachment_paths,
    };
    let mut bytes = serde_json::to_vec_pretty(&value)
        .map_err(|_| planner_error("sensitive review cannot be encoded"))?;
    bytes.push(b'\n');
    if bytes.len() > MAX_SENSITIVE_REVIEW_BYTES {
        return Err(ProjectCatalogMigrationError::no_mutation(
            "error.project_catalog_migration_artifact_limit",
            "sensitive review exceeds its byte limit",
        ));
    }
    Ok((
        bytes,
        u64::try_from(value.attachment_paths.len()).unwrap_or(u64::MAX),
    ))
}

fn inventoried_group_namespaces(
    inventory: &V1ProjectCatalogInventory,
    resolved_project_scopes: &[ResolvedProjectScopeInputV1],
    group_id: &str,
) -> Result<BTreeSet<CommitNamespace>, ProjectCatalogMigrationError> {
    let memberships =
        deterministic_repo_history_group_memberships(inventory, resolved_project_scopes)
            .map_err(inventory_error)?;
    let project_ids = memberships
        .iter()
        .find(|(candidate, _)| candidate == group_id)
        .map(|(_, projects)| projects)
        .ok_or_else(|| planner_error("repository-history group disappeared during planning"))?;
    let ambiguous = inventory
        .legacy_namespace_clusters
        .iter()
        .map(|cluster| {
            CommitNamespace::parse(cluster.materialized_namespace.clone())
                .map_err(|_| planner_error("legacy namespace cluster is invalid"))
        })
        .collect::<Result<BTreeSet<_>, _>>()?;
    Ok(inventory
        .git_metadata
        .iter()
        .filter(|row| project_ids.contains(&row.project_id))
        .flat_map(|row| &row.materialized_commit_namespaces)
        .map(|namespace| {
            CommitNamespace::parse(namespace.clone())
                .map_err(|_| planner_error("inventoried commit namespace is invalid"))
        })
        .collect::<Result<BTreeSet<_>, _>>()?
        .difference(&ambiguous)
        .cloned()
        .collect())
}

fn mint_unique_repo_history_id(used: &mut BTreeSet<RepoHistoryId>) -> RepoHistoryId {
    loop {
        let candidate = RepoHistoryId::mint();
        if !used.contains(&candidate) {
            return candidate;
        }
    }
}

fn mint_unique_binding_id(used: &mut BTreeSet<LegacyPathBindingId>) -> LegacyPathBindingId {
    loop {
        let candidate = LegacyPathBindingId::mint();
        if !used.contains(&candidate) {
            return candidate;
        }
    }
}

fn mint_local_namespace(used: &mut BTreeSet<CommitNamespace>) -> CommitNamespace {
    loop {
        let random = RepoHistoryId::mint();
        let suffix = random
            .as_str()
            .strip_prefix("rh_")
            .expect("minted repository-history id has its code-owned prefix");
        let candidate = CommitNamespace::parse(format!("local_{suffix}"))
            .expect("code-owned local namespace must validate");
        if !used.contains(&candidate) {
            return candidate;
        }
    }
}

fn mint_checkout_id() -> String {
    ProjectCatalogTransactionId::mint()
        .as_str()
        .strip_prefix("pct_")
        .expect("minted transaction id has its code-owned prefix")
        .to_string()
}

fn stable_conflict_id(
    prefix: &'static str,
    value: &impl Serialize,
) -> Result<String, ProjectCatalogMigrationError> {
    let mut bytes = format!("blackbox.project-catalog.{prefix}.v1\0").into_bytes();
    bytes.extend_from_slice(
        &serde_json::to_vec(value)
            .map_err(|_| planner_error("canonical conflict identity cannot be encoded"))?,
    );
    let digest = Sha256ValueV1::digest(&bytes);
    Ok(format!("{prefix}_{}", &digest.as_str()[..32]))
}

fn inventory_error(
    error: crate::project_catalog_inventory::ProjectCatalogInventoryError,
) -> ProjectCatalogMigrationError {
    ProjectCatalogMigrationError::no_mutation(error.code(), "migration inventory contract failed")
}

fn planner_error(message: &'static str) -> ProjectCatalogMigrationError {
    ProjectCatalogMigrationError::no_mutation("error.project_catalog_migration_planner", message)
}

fn invalid_resolution_artifact(message: &'static str) -> ProjectCatalogMigrationError {
    ProjectCatalogMigrationError::no_mutation(
        "error.project_catalog_migration_artifact_identity",
        message,
    )
}

struct CurrentClosedMigrationIntegrationV1;

impl ClosedMigrationIntegrationV1 for CurrentClosedMigrationIntegrationV1 {
    fn prepare_preflight(
        &self,
        layout: &ProjectCatalogMigrationResolvedLayoutV1,
        existing_resolution: Option<&[u8]>,
        existing_report: Option<&[u8]>,
        include_sensitive_paths: bool,
    ) -> Result<PreparedPreflightV1, ProjectCatalogMigrationError> {
        Ok(prepare_closed_migration(
            layout,
            existing_resolution,
            existing_report,
            include_sensitive_paths,
        )?
        .preflight)
    }

    fn apply_rehearsal(
        &self,
        layout: &ProjectCatalogMigrationResolvedLayoutV1,
        report_bytes: &[u8],
        report: &ProjectCatalogMigrationReportV1,
        resolution_bytes: &[u8],
        _resolution: &ProjectCatalogMigrationResolutionV1,
    ) -> Result<ProjectCatalogMigrationApplyResultV1, ProjectCatalogMigrationError> {
        if let Some(result) =
            verify_exact_installed_review(layout, report_bytes, report, resolution_bytes)?
        {
            return Ok(ProjectCatalogMigrationApplyResultV1 {
                receipt: ProjectCatalogMigrationApplyReceiptV1 {
                    version: FACADE_VERSION_V1,
                    outcome: ProjectCatalogMigrationApplyOutcomeV1::AlreadyApplied,
                    verification: result.receipt,
                },
            });
        }
        let prepared =
            prepare_closed_migration(layout, Some(resolution_bytes), Some(report_bytes), false)?;
        if prepared.preflight.report_bytes != report_bytes
            || prepared.preflight.resolution_bytes != resolution_bytes
        {
            return Err(ProjectCatalogMigrationError::no_mutation(
                "error.project_catalog_migration_artifact_identity",
                "recaptured executable plan does not match exact reviewed artifacts",
            ));
        }
        let predicted_marker_hash = prepared
            .preflight
            .receipt
            .predicted_marker_hash
            .clone()
            .ok_or_else(|| {
                ProjectCatalogMigrationError::no_mutation(
                    "error.project_catalog_migration_artifact_identity",
                    "recaptured executable plan lacks its marker prediction",
                )
            })?;
        let plan = prepared.plan.ok_or_else(|| {
            ProjectCatalogMigrationError::no_mutation(
                "error.project_catalog_migration_report_not_clean",
                "reviewed migration is not executable",
            )
        })?;
        transact_migration_classified(&layout.projects_path, plan).map_err(|failure| {
            ProjectCatalogMigrationError::new(
                failure.error.code(),
                "migration transaction failed after exact-plan validation",
                facade_mutation_disposition(failure.disposition),
            )
        })?;
        git_meta_backup_copy_if_needed(layout).map_err(post_commit_verification_error)?;
        let verified = verify_installed(layout).map_err(post_commit_verification_error)?;
        if verified.receipt.predicted_marker_hash != predicted_marker_hash {
            return Err(ProjectCatalogMigrationError::new(
                "error.project_catalog_migration_artifact_identity",
                "installed marker disagrees with the recaptured executable plan",
                ProjectCatalogMigrationMutationDispositionV1::RecoveredToCommittedState,
            ));
        }
        Ok(ProjectCatalogMigrationApplyResultV1 {
            receipt: ProjectCatalogMigrationApplyReceiptV1 {
                version: FACADE_VERSION_V1,
                outcome: ProjectCatalogMigrationApplyOutcomeV1::Applied,
                verification: verified.receipt,
            },
        })
    }

    fn verify(
        &self,
        layout: &ProjectCatalogMigrationResolvedLayoutV1,
    ) -> Result<ProjectCatalogMigrationVerifyResultV1, ProjectCatalogMigrationError> {
        verify_installed(layout)
    }
}

/// Versioned canonical-JSON namespace-inventory asset persisted through the
/// migration facade's existing immutable-asset mechanism (Phase 3 plan
/// section 4.2, governing section 11). Phase 1 computed but never persisted
/// `V1ProjectCatalogInventory.legacy_commit_namespaces`; this asset is the
/// durable proof surface the pre-replacement materializer proves observed
/// namespace sets against, so its hash is bound in
/// `predicted_immutable_asset_hashes` and verified by the receipt exactly
/// like every other immutable asset.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LegacyCommitNamespaceInventoryAssetV1 {
    pub version: u32,
    pub inventory_hash: Sha256ValueV1,
    pub source_index_fingerprint: Sha256ValueV1,
    pub rows: Vec<LegacyCommitNamespaceInventoryV1>,
}

const LEGACY_COMMIT_NAMESPACE_INVENTORY_ASSET_VERSION_V1: u32 = 1;
const LEGACY_COMMIT_NAMESPACE_SOURCE_FINGERPRINT_DOMAIN: &[u8] =
    b"blackbox.project-catalog.legacy-commit-namespace-source.v1\0";

impl LegacyCommitNamespaceInventoryAssetV1 {
    fn from_inventory(
        inventory: &V1ProjectCatalogInventory,
    ) -> Result<Self, ProjectCatalogMigrationError> {
        let mut rows = inventory.legacy_commit_namespaces.clone();
        rows.sort_by(|left, right| left.namespace.cmp(&right.namespace));
        Ok(Self {
            version: LEGACY_COMMIT_NAMESPACE_INVENTORY_ASSET_VERSION_V1,
            inventory_hash: inventory.inventory_hash().map_err(inventory_error)?,
            source_index_fingerprint: legacy_commit_namespace_source_fingerprint(inventory),
            rows,
        })
    }

    fn canonical_json(&self) -> Result<Vec<u8>, ProjectCatalogMigrationError> {
        serde_json::to_vec(self)
            .map_err(|_| planner_error("legacy commit namespace inventory asset cannot be encoded"))
    }
}

/// Read back the installed legacy commit-namespace inventory asset for a
/// migrated store, or `None` when the store predates the asset.
///
/// The Phase 3 history materializer is the only consumer: it proves the
/// namespaces it observes in the live index against these recorded rows
/// before any of them may authorize a destructive replacement.
///
/// The asset filename embeds its own content hash. This reader recomputes
/// that hash from the bytes it read and refuses a mismatch, so a truncated
/// or edited asset can never pass itself off as recorded evidence. Exactly
/// one asset may exist for a transaction (the role is a singleton), and more
/// than one candidate file refuses rather than picking.
pub fn load_legacy_commit_namespace_inventory_asset(
    projects_path: &Path,
    transaction_id: &ProjectCatalogTransactionId,
) -> Result<Option<LegacyCommitNamespaceInventoryAssetV1>, ProjectCatalogMigrationError> {
    let (assets_dir, prefix) =
        legacy_commit_namespace_inventory_asset_location(projects_path, transaction_id).map_err(
            |error| {
                ProjectCatalogMigrationError::no_mutation(
                    "error.project_catalog_migration_artifact_unreadable",
                    format!("cannot derive the migration asset root: {}", error.code()),
                )
            },
        )?;
    let entries = match std::fs::read_dir(&assets_dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(_) => {
            return Err(ProjectCatalogMigrationError::no_mutation(
                "error.project_catalog_migration_artifact_unreadable",
                "migration asset root cannot be listed",
            ));
        }
    };
    let mut candidates = Vec::new();
    for entry in entries {
        let Ok(entry) = entry else {
            return Err(ProjectCatalogMigrationError::no_mutation(
                "error.project_catalog_migration_artifact_unreadable",
                "migration asset root entry cannot be read",
            ));
        };
        let Some(name) = entry.file_name().to_str().map(str::to_string) else {
            continue;
        };
        if name.starts_with(&prefix) && name.ends_with(".immutable") {
            candidates.push(name);
        }
    }
    if candidates.len() > 1 {
        return Err(ProjectCatalogMigrationError::no_mutation(
            "error.project_catalog_migration_artifact_identity",
            "more than one legacy commit-namespace inventory asset is installed",
        ));
    }
    let Some(name) = candidates.pop() else {
        return Ok(None);
    };
    let expected_hash = name
        .strip_prefix(&prefix)
        .and_then(|rest| rest.strip_suffix(".immutable"))
        .map(str::to_string)
        .ok_or_else(|| {
            ProjectCatalogMigrationError::no_mutation(
                "error.project_catalog_migration_artifact_identity",
                "legacy commit-namespace inventory asset has an unreadable name",
            )
        })?;
    let path = assets_dir.join(&name);
    let metadata = std::fs::symlink_metadata(&path).map_err(|_| {
        ProjectCatalogMigrationError::no_mutation(
            "error.project_catalog_migration_artifact_unreadable",
            "legacy commit-namespace inventory asset cannot be stat'ed",
        )
    })?;
    if !metadata.is_file()
        || metadata.len() > LEGACY_COMMIT_NAMESPACE_INVENTORY_ASSET_MAX_BYTES as u64
    {
        return Err(ProjectCatalogMigrationError::no_mutation(
            "error.project_catalog_migration_artifact_unreadable",
            "legacy commit-namespace inventory asset is not a bounded regular file",
        ));
    }
    let bytes = std::fs::read(&path).map_err(|_| {
        ProjectCatalogMigrationError::no_mutation(
            "error.project_catalog_migration_artifact_unreadable",
            "legacy commit-namespace inventory asset cannot be read",
        )
    })?;
    if sha256(&bytes).as_str() != expected_hash {
        return Err(ProjectCatalogMigrationError::no_mutation(
            "error.project_catalog_migration_artifact_identity",
            "legacy commit-namespace inventory asset disagrees with its content hash",
        ));
    }
    let asset: LegacyCommitNamespaceInventoryAssetV1 =
        serde_json::from_slice(&bytes).map_err(|_| {
            ProjectCatalogMigrationError::no_mutation(
                "error.project_catalog_migration_artifact_identity",
                "legacy commit-namespace inventory asset cannot be decoded",
            )
        })?;
    if asset.version != LEGACY_COMMIT_NAMESPACE_INVENTORY_ASSET_VERSION_V1 {
        return Err(ProjectCatalogMigrationError::no_mutation(
            "error.project_catalog_migration_artifact_identity",
            "legacy commit-namespace inventory asset version is not supported",
        ));
    }
    Ok(Some(asset))
}

/// Folds the captured tantivy and vector source states the namespace rows
/// were counted from (the "git-metadata" lane's owner subsources), so the
/// materializer can detect drift against a re-derived inventory. Absent
/// owner evidence folds in as an empty field rather than failing: the asset
/// is still emitted (possibly with zero rows) when the source index was not
/// present at capture time.
fn legacy_commit_namespace_source_fingerprint(
    inventory: &V1ProjectCatalogInventory,
) -> Sha256ValueV1 {
    let tantivy = git_metadata_owner_fingerprint(inventory, ImmutableInventoryOwnerKindV1::Tantivy);
    let vectors =
        git_metadata_owner_fingerprint(inventory, ImmutableInventoryOwnerKindV1::VectorMetadata);
    fold_legacy_commit_namespace_source_fingerprint(tantivy.as_ref(), vectors.as_ref())
}

/// The fold itself, shared by the asset WRITE path above and the
/// materializer's READ-BACK recomputation below.
///
/// It exists as its own function so the recorded fingerprint and the
/// recomputed one can never drift apart through two copies of one recipe.
/// Do not inline it back into either caller.
fn fold_legacy_commit_namespace_source_fingerprint(
    tantivy: Option<&Sha256ValueV1>,
    vectors: Option<&Sha256ValueV1>,
) -> Sha256ValueV1 {
    let mut bytes = Vec::new();
    bytes.extend_from_slice(LEGACY_COMMIT_NAMESPACE_SOURCE_FINGERPRINT_DOMAIN);
    for fingerprint in [tantivy, vectors] {
        let value = fingerprint
            .map(|value| value.as_str().to_string())
            .unwrap_or_default();
        bytes.extend_from_slice(&(value.len() as u64).to_be_bytes());
        bytes.extend_from_slice(value.as_bytes());
    }
    Sha256ValueV1::digest(&bytes)
}

/// Recompute the asset's `source_index_fingerprint` over the CURRENT owner
/// state, so the history materializer can tell "index unchanged since
/// migration" from "index has been live-indexed since migration".
///
/// This runs the same Phase 1 capture recipe against the live paths and
/// folds it through the same helper the asset was written with: the
/// aggregate of the corpus index and code-metadata source fingerprints, plus
/// the vector source fingerprint. It deliberately invents nothing.
///
/// Returns `None` when the recipe cannot produce a comparable value, which
/// the caller must treat as drift (the safe direction) rather than as
/// equality. Two cases reach it: an `Unavailable` owner state, which the
/// migration path rejects before lane projection and which the shared state
/// helpers therefore refuse to fold; and a capture that panicked nothing but
/// produced no fingerprint at all.
///
/// Note for consumers requiring equality mode: `capture_index` treats a
/// schema marker that is not the RUNNING `INDEX_SCHEMA_VERSION` as corrupt,
/// so an index observed across a schema bump can never fold to the recorded
/// fingerprint. Equality mode is therefore reachable only at the same schema
/// the migration captured, which is the offline-rebuild shape, not a live
/// bump.
pub fn recompute_legacy_commit_namespace_source_fingerprint(
    index_path: &Path,
    git_meta_dir: &Path,
    vector_root: &Path,
) -> Option<Sha256ValueV1> {
    let corpus =
        bbox_corpus_index::index::migration_inventory::capture_owner_migration_snapshot_no_create(
            index_path,
            git_meta_dir,
            Default::default(),
        );
    let vectors = bbox_vectors::migration_inventory::capture_migration_snapshot_no_create(
        vector_root,
        Default::default(),
    );
    if matches!(
        corpus.index.state,
        bbox_corpus_index::index::migration_inventory::CorpusMigrationSourceStateV1::Unavailable { .. }
    ) || matches!(
        corpus.code_metadata.state,
        bbox_corpus_index::index::migration_inventory::CorpusMigrationSourceStateV1::Unavailable { .. }
    ) || matches!(
        vectors.state,
        bbox_vectors::migration_inventory::VectorMigrationSourceStateV1::Unavailable { .. }
    ) {
        return None;
    }
    let index_state = corpus_source_state(
        "tantivy",
        &corpus.index.state,
        corpus.index.source_fingerprint_sha256.as_deref(),
    );
    let code_metadata_state = corpus_source_state(
        "tantivy-code-metadata",
        &corpus.code_metadata.state,
        corpus.code_metadata.source_fingerprint_sha256.as_deref(),
    );
    let tantivy_state = aggregate_inventory_states("tantivy", &[index_state, code_metadata_state]);
    let vector_state = vector_source_state(&vectors);
    Some(fold_legacy_commit_namespace_source_fingerprint(
        Some(source_state_fingerprint(&tantivy_state)),
        Some(source_state_fingerprint(&vector_state)),
    ))
}

fn git_metadata_owner_fingerprint(
    inventory: &V1ProjectCatalogInventory,
    owner_kind: ImmutableInventoryOwnerKindV1,
) -> Option<Sha256ValueV1> {
    inventory
        .immutable_lane_evidence
        .iter()
        .find(|lane| lane.source_id == "git-metadata")?
        .owner_subsources
        .iter()
        .find(|owner| owner.owner_kind == owner_kind)
        .map(|owner| source_state_fingerprint(&owner.source_state).clone())
}

fn source_state_fingerprint(state: &InventorySourceStateV1) -> &Sha256ValueV1 {
    match state {
        InventorySourceStateV1::Present { fingerprint, .. }
        | InventorySourceStateV1::Missing { fingerprint }
        | InventorySourceStateV1::Corrupt { fingerprint, .. } => fingerprint,
    }
}

struct PreparedClosedMigrationV1 {
    preflight: PreparedPreflightV1,
    plan: Option<ValidatedMigrationPlanV1>,
}

fn prepare_closed_migration(
    layout: &ProjectCatalogMigrationResolvedLayoutV1,
    existing_resolution: Option<&[u8]>,
    existing_report: Option<&[u8]>,
    include_sensitive_paths: bool,
) -> Result<PreparedClosedMigrationV1, ProjectCatalogMigrationError> {
    let checkout_roots = discover_checkout_roots(layout)?;
    validate_rehearsal_runtime_authorities(layout, &checkout_roots)?;
    let prior_report = existing_report
        .map(decode_migration_report_v1)
        .transpose()
        .map_err(inventory_error)?;
    let candidate_keys =
        ProjectCatalogMigrationInventoryFacadeV1::discover_attachment_candidate_keys(
            ProjectCatalogAttachmentCandidateDiscoveryRequestV1 {
                rehearsal_root: layout.rehearsal_root.clone(),
                legacy_project_store_path: layout.projects_path.clone(),
                checkout_roots: checkout_roots.clone(),
            },
        )
        .map_err(adapter_error)?;
    let attachment_identity_plan =
        prepare_attachment_identity_plan(&candidate_keys, prior_report.as_ref())?;
    let publisher_ref_store =
        PublisherRefStore::migration_source(layout.publisher_refs_path.clone());
    let captured = ProjectCatalogMigrationInventoryFacadeV1::capture(
        ProjectCatalogMigrationInventoryRequestV1 {
            rehearsal_root: layout.rehearsal_root.clone(),
            legacy_project_store_path: layout.projects_path.clone(),
            publisher_ref_store: &publisher_ref_store,
            code_source_store_root: layout.code_source_root.clone(),
            code_source_store_limits: layout.store_limits.clone(),
            checkout_roots,
            owner_paths: owner_inventory_paths(layout),
            owner_limits: ProjectCatalogOwnerInventoryLimitsV1::default(),
            attachment_identity_plan: &attachment_identity_plan,
        },
    )
    .map_err(adapter_error)?;
    let runtime = MigrationRuntimeBindingsViewV1 {
        legacy_project_store_bytes: captured
            .runtime_bindings()
            .legacy_project_store_bytes()
            .to_vec(),
        legacy_project_store_was_missing: captured
            .runtime_bindings()
            .legacy_project_store_was_missing(),
        legacy_project_paths: captured
            .runtime_bindings()
            .legacy_project_paths()
            .map(|(id, path)| (id.to_string(), path.to_path_buf()))
            .collect(),
        checkout_paths: captured
            .runtime_bindings()
            .checkout_paths()
            .map(|(id, path)| (id.to_string(), path.to_path_buf()))
            .collect(),
        checkout_repositories: captured
            .runtime_bindings()
            .checkout_repositories()
            .map(|(id, repository)| (id.to_string(), repository.clone()))
            .collect(),
        legacy_selectors: captured
            .runtime_bindings()
            .legacy_selectors()
            .map(|(id, value)| (id.to_string(), value.to_string()))
            .collect(),
    };
    let inventory = &captured.inventory;
    let inventory_hash = inventory.inventory_hash().map_err(inventory_error)?;
    let resolution = match existing_resolution {
        Some(bytes) => decode_migration_resolution_v1(bytes).map_err(inventory_error)?,
        None => ProjectCatalogMigrationResolutionV1::empty(inventory_hash.clone()),
    };
    let resolution_bytes = match existing_resolution {
        Some(bytes) => bytes.to_vec(),
        None => encode_migration_resolution_v1(&resolution).map_err(inventory_error)?,
    };
    let mut assessment = assess_migration_semantics(inventory, &resolution)?;
    let identities = build_persisted_identity_plan(
        inventory,
        &assessment.resolved_project_scopes,
        &assessment.retained_attachment_ids,
        prior_report.as_ref(),
    )?;
    let classified_legacy_paths = classify_legacy_paths(inventory, &runtime, &identities)?;
    assessment
        .refusals
        .extend(classified_legacy_paths.refusals.clone());
    canonicalize_refusals(&mut assessment.refusals);
    if assessment.status() != ProjectCatalogMigrationStatusV1::Clean {
        return prepare_assessment_only_with_rows(
            inventory,
            &runtime,
            &resolution_bytes,
            &assessment,
            &identities,
            include_sensitive_paths,
            classified_legacy_paths.report_rows,
            missing_project_rows(inventory)?,
            classified_legacy_paths.unscoped_counts,
            classified_legacy_paths.sensitive_report,
        );
    }

    let base = match build_base_post_images(
        inventory,
        &runtime,
        &assessment,
        &identities,
        &resolution,
        &classified_legacy_paths,
    ) {
        Ok(base) => base,
        Err(MigrationBasePostImagesFailureV1::Refused(refusal)) => {
            assessment.refusals.push(late_domain_refusal_row(refusal));
            canonicalize_refusals(&mut assessment.refusals);
            return prepare_assessment_only_with_rows(
                inventory,
                &runtime,
                &resolution_bytes,
                &assessment,
                &identities,
                include_sensitive_paths,
                classified_legacy_paths.report_rows,
                missing_project_rows(inventory)?,
                classified_legacy_paths.unscoped_counts,
                classified_legacy_paths.sensitive_report,
            );
        }
        Err(MigrationBasePostImagesFailureV1::Error(error)) => return Err(error),
    };
    let publisher = prepare_publisher_plan(inventory, &runtime, &assessment, &resolution)?;
    let catalog_bytes = encode_catalog_snapshot(&base.catalog)
        .map_err(|_| planner_error("catalog post-image cannot be encoded"))?;
    let attachment_bytes = encode_attachment_snapshot(&base.attachments)
        .map_err(|_| planner_error("attachment post-image cannot be encoded"))?;
    let mut predicted = PredictedPostImageHashesV1 {
        catalog_hash: Sha256ValueV1::digest(&catalog_bytes),
        attachment_hash: Sha256ValueV1::digest(&attachment_bytes),
        participant_hashes: BTreeMap::new(),
        g1_assets: publisher
            .prepared
            .values()
            .map(|prepared| {
                Ok(PredictedAssetV1 {
                    asset_id: prepared.generation_id.as_str().to_string(),
                    content_hash: Sha256ValueV1::parse(
                        prepared.generation_hash.as_str().to_string(),
                    )
                    .map_err(inventory_error)?,
                })
            })
            .collect::<Result<Vec<_>, ProjectCatalogMigrationError>>()?,
        accepted_pointer_hashes: publisher
            .prepared
            .iter()
            .map(|(project_id, prepared)| {
                Ok((
                    project_id.clone(),
                    Sha256ValueV1::parse(prepared.pointer_hash.as_str().to_string())
                        .map_err(inventory_error)?,
                ))
            })
            .collect::<Result<_, ProjectCatalogMigrationError>>()?,
    };
    let mut post_image = DeterministicPostImageInputV1 {
        version: 1,
        transaction_id: identities.transaction_id.clone(),
        inventory_hash: inventory_hash.clone(),
        resolved_project_scopes: assessment.resolved_project_scopes.clone(),
        repo_history_groups: identities.repo_history_groups.clone(),
        attachments: base.post_image_attachments.clone(),
        checkout_identity_actions: identities.checkout_identity_actions.clone(),
        legacy_path_bindings: base.post_image_legacy_bindings.clone(),
        quarantined_collected: resolution
            .quarantine_collected
            .iter()
            .map(|row| QuarantinePostImageInputV1 {
                project_id: row.project_id.clone(),
                generation_id: row.generation_id.clone(),
            })
            .collect(),
        publisher_binding_dispositions: publisher.dispositions.clone(),
        predicted_hashes: predicted.clone(),
    };
    let plan_hash =
        canonical_plan_hash(inventory, &resolution, &post_image).map_err(inventory_error)?;
    let mut store_parts = prepare_store_plan_parts(
        inventory,
        &resolution,
        &captured.code_source_owner_inventory,
        &post_image,
        &plan_hash,
        &publisher,
    )?;
    let (legacy_source, publisher_source, source_assets) = prepare_source_drafts(
        inventory,
        &runtime,
        captured.publisher_ref_source_was_missing,
    )?;
    store_parts.immutable_assets.extend(source_assets);
    let namespace_inventory_asset =
        LegacyCommitNamespaceInventoryAssetV1::from_inventory(inventory)?;
    store_parts
        .immutable_assets
        .push(MigrationImmutableAssetDraftV1::new(
            ImmutableAssetRoleV1::LegacyCommitNamespaceInventory,
            namespace_inventory_asset.canonical_json()?,
        ));
    predicted.participant_hashes = store_parts
        .participants
        .iter()
        .filter_map(MigrationParticipantDraftV1::predicted_post_image)
        .map(|(token, hash)| {
            Ok((
                token,
                Sha256ValueV1::parse(hash.to_string()).map_err(inventory_error)?,
            ))
        })
        .collect::<Result<_, ProjectCatalogMigrationError>>()?;
    post_image.predicted_hashes = predicted.clone();
    let immutable_predictions = store_parts
        .immutable_assets
        .iter()
        .map(MigrationImmutableAssetDraftV1::predicted_identity)
        .map(|(token, hash)| {
            Ok((
                token,
                Sha256ValueV1::parse(hash.to_string()).map_err(inventory_error)?,
            ))
        })
        .collect::<Result<BTreeMap<_, _>, ProjectCatalogMigrationError>>()?;
    let report = build_migration_report(
        inventory,
        &resolution_bytes,
        &assessment,
        &identities,
        plan_hash.clone(),
        predicted,
        immutable_predictions.clone(),
        base.legacy_binding_report.clone(),
        base.missing_paths.clone(),
        base.unscoped_legacy_counts.clone(),
        ProjectCatalogMigrationStatusV1::Clean,
    )?;
    let report_bytes = encode_migration_report_v1(&report, inventory).map_err(inventory_error)?;
    let quarantine_authority =
        validated_quarantine_bindings(inventory, &report, &resolution, &post_image)
            .map_err(inventory_error)?;
    let retained_checkout_observation_ids = inventory
        .attachment_candidates
        .iter()
        .filter(|candidate| {
            assessment
                .retained_attachment_ids
                .contains(&candidate.attachment_id)
        })
        .map(|candidate| candidate.checkout_observation_id.as_str())
        .collect::<BTreeSet<_>>();
    let retained_checkout_paths = runtime
        .checkout_paths
        .iter()
        .filter(|(observation_id, _)| {
            retained_checkout_observation_ids.contains(observation_id.as_str())
        })
        .map(|(observation_id, path)| (observation_id.clone(), path.clone()))
        .collect::<BTreeMap<_, _>>();
    if retained_checkout_paths.len() != retained_checkout_observation_ids.len() {
        return Err(planner_error(
            "retained attachment checkout runtime bindings are incomplete",
        ));
    }
    let registry = build_registry(layout, &retained_checkout_paths)?;
    let draft = MigrationPlanDraftV1 {
        transaction_id: identities.transaction_id.clone(),
        plan_hash: store_sha256(plan_hash.as_str())?,
        report_artifact_sha256: store_sha256(Sha256ValueV1::digest(&report_bytes).as_str())?,
        resolution_artifact_sha256: store_sha256(
            Sha256ValueV1::digest(&resolution_bytes).as_str(),
        )?,
        legacy_project_source: legacy_source,
        publisher_ref_source: publisher_source,
        inventory_sha256: store_sha256(inventory_hash.as_str())?,
        code_source_inventory_sha256: store_sha256(captured.code_source_canonical_sha256.as_str())?,
        catalog: base.catalog.clone(),
        attachments: base.attachments.clone(),
        participants: store_parts.participants,
        immutable_assets: store_parts.immutable_assets,
        code_source_snapshot: store_parts.code_source_snapshot,
        quarantine_authority,
        publisher_pins: store_parts.publisher_pins,
        publisher_dispositions: store_parts.publisher_dispositions,
        checkout_identity_actions: identities
            .checkout_identity_actions
            .iter()
            .map(|action| {
                MigrationCheckoutIdentityActionDraftV1::new(
                    action.observation_id.clone(),
                    action.planned_checkout_id.clone(),
                )
            })
            .collect(),
    };
    let plan = validate_migration_plan(&layout.projects_path, registry, draft)
        .map_err(store_validation_error)?;
    let identity_projections = validate_planned_identity(
        &plan,
        &report_bytes,
        &resolution_bytes,
        &report,
        &immutable_predictions,
    )?;
    let sensitive_review = prepare_sensitive_review(
        inventory,
        &runtime,
        &base.sensitive_report,
        include_sensitive_paths,
    )?;
    let receipt = preflight_receipt(
        &report,
        &report_bytes,
        &resolution_bytes,
        Some(identity_projections.marker_hash.clone()),
        sensitive_review.as_ref(),
        u64::try_from(
            base.attachments
                .attachments
                .values()
                .filter(|row| row.kind == AttachmentKind::Base)
                .count(),
        )
        .unwrap_or(u64::MAX),
        u64::try_from(base.catalog.projects.len()).unwrap_or(u64::MAX),
    );
    Ok(PreparedClosedMigrationV1 {
        preflight: PreparedPreflightV1 {
            report_bytes,
            resolution_bytes,
            receipt,
            sensitive_review,
        },
        plan: Some(plan),
    })
}

#[cfg(test)]
fn prepare_assessment_only(
    inventory: &V1ProjectCatalogInventory,
    runtime: &MigrationRuntimeBindingsViewV1,
    resolution_bytes: &[u8],
    assessment: &MigrationSemanticAssessmentV1,
    identities: &MigrationPersistedIdentityPlanV1,
    include_sensitive_paths: bool,
) -> Result<PreparedClosedMigrationV1, ProjectCatalogMigrationError> {
    let classified = classify_legacy_paths(inventory, runtime, identities)?;
    let mut assessment = assessment.clone();
    assessment.refusals.extend(classified.refusals);
    canonicalize_refusals(&mut assessment.refusals);
    prepare_assessment_only_with_rows(
        inventory,
        runtime,
        resolution_bytes,
        &assessment,
        identities,
        include_sensitive_paths,
        classified.report_rows,
        missing_project_rows(inventory)?,
        classified.unscoped_counts,
        classified.sensitive_report,
    )
}

#[allow(clippy::too_many_arguments)]
fn prepare_assessment_only_with_rows(
    inventory: &V1ProjectCatalogInventory,
    runtime: &MigrationRuntimeBindingsViewV1,
    resolution_bytes: &[u8],
    assessment: &MigrationSemanticAssessmentV1,
    identities: &MigrationPersistedIdentityPlanV1,
    include_sensitive_paths: bool,
    legacy_bindings: Vec<LegacyPathBindingReportV1>,
    missing_paths: Vec<MissingPathReportV1>,
    unscoped: BTreeMap<crate::project_catalog_inventory::LegacyPathStoreKindV1, u64>,
    sensitive_paths: SensitiveLocalPathReportV1,
) -> Result<PreparedClosedMigrationV1, ProjectCatalogMigrationError> {
    let status = assessment.status();
    let plan_hash =
        non_executable_assessment_hash(inventory, resolution_bytes, identities, status)?;
    let predicted = PredictedPostImageHashesV1 {
        catalog_hash: assessment_prediction(&plan_hash, "catalog"),
        attachment_hash: assessment_prediction(&plan_hash, "attachments"),
        participant_hashes: BTreeMap::new(),
        g1_assets: Vec::new(),
        accepted_pointer_hashes: BTreeMap::new(),
    };
    let report = build_migration_report(
        inventory,
        resolution_bytes,
        assessment,
        identities,
        plan_hash,
        predicted,
        BTreeMap::new(),
        legacy_bindings,
        missing_paths,
        unscoped,
        status,
    )?;
    let report_bytes = encode_migration_report_v1(&report, inventory).map_err(inventory_error)?;
    let sensitive_review = prepare_sensitive_review(
        inventory,
        runtime,
        &sensitive_paths,
        include_sensitive_paths,
    )?;
    let receipt = preflight_receipt(
        &report,
        &report_bytes,
        resolution_bytes,
        None,
        sensitive_review.as_ref(),
        0,
        u64::try_from(inventory.legacy_projects.len()).unwrap_or(u64::MAX),
    );
    Ok(PreparedClosedMigrationV1 {
        preflight: PreparedPreflightV1 {
            report_bytes,
            resolution_bytes: resolution_bytes.to_vec(),
            receipt,
            sensitive_review,
        },
        plan: None,
    })
}

fn assessment_prediction(plan_hash: &Sha256ValueV1, role: &str) -> Sha256ValueV1 {
    Sha256ValueV1::digest(
        format!(
            "blackbox.project-catalog.assessment-prediction.v1\0{}\0{}",
            plan_hash, role
        )
        .as_bytes(),
    )
}

fn prepare_attachment_identity_plan(
    keys: &[AttachmentCandidateKeyV1],
    prior_report: Option<&ProjectCatalogMigrationReportV1>,
) -> Result<AttachmentCandidateIdentityPlanV1, ProjectCatalogMigrationError> {
    let prior = prior_report
        .into_iter()
        .flat_map(|report| &report.attachments)
        .map(|row| (row.observation_id.as_str(), row))
        .collect::<BTreeMap<_, _>>();
    let mut used = BTreeSet::new();
    let mut identities = BTreeMap::new();
    for key in keys {
        let observation_id = attachment_observation_id(key).map_err(adapter_error)?;
        let attachment_id = prior
            .get(observation_id.as_str())
            .filter(|row| {
                row.project_id == key.project_id
                    && row.checkout_observation_id == key.checkout_observation_id
            })
            .map(|row| row.attachment_id.clone())
            .unwrap_or_else(AttachmentId::mint);
        if !used.insert(attachment_id.clone()) {
            return Err(ProjectCatalogMigrationError::no_mutation(
                "error.project_catalog_migration_identity_remint",
                "reviewed attachment identity is reused by another candidate",
            ));
        }
        identities.insert(key.clone(), attachment_id);
    }
    Ok(AttachmentCandidateIdentityPlanV1 { identities })
}

fn owner_inventory_paths(
    layout: &ProjectCatalogMigrationResolvedLayoutV1,
) -> ProjectCatalogOwnerInventoryPathsV1 {
    ProjectCatalogOwnerInventoryPathsV1 {
        corpus_index_root: layout.index_root.clone(),
        git_cursor_root: layout.git_meta_root.clone(),
        vector_root: layout.vector_root.clone(),
        edge_root: layout.edge_root.clone(),
        knowledge_store_path: layout.knowledge_path.clone(),
        gap_store_path: layout.gaps_path.clone(),
        thread_store_path: layout.threads_path.clone(),
        note_store_path: layout.notes_path.clone(),
        pin_store_path: layout.pins_path.clone(),
        roadmap_store_path: layout.roadmap_path.clone(),
        packet_root: layout.packets_dir.clone(),
        task_store_path: layout.bro_home.join("tasks.json"),
        proposal_root: layout.bro_home.join("badgey").join("proposals"),
        slack_store_root: layout.bro_home.clone(),
        whiteboard_root: layout.bro_home.join("whiteboards"),
        artifact_root: layout.artifacts_dir.clone(),
        provenance_notes_ref: layout.provenance_notes_ref.clone(),
    }
}

fn discover_checkout_roots(
    layout: &ProjectCatalogMigrationResolvedLayoutV1,
) -> Result<Vec<PathBuf>, ProjectCatalogMigrationError> {
    let Some(root) = &layout.checkout_replicas_root else {
        return Ok(Vec::new());
    };
    let metadata = match std::fs::symlink_metadata(root) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(_) => {
            return Err(ProjectCatalogMigrationError::no_mutation(
                "error.project_catalog_migration_owner_snapshot",
                "checkout replica root cannot be inspected",
            ));
        }
    };
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Err(ProjectCatalogMigrationError::no_mutation(
            "error.project_catalog_migration_owner_snapshot",
            "checkout replica root is not a no-follow directory",
        ));
    }
    let entries = std::fs::read_dir(root).map_err(|_| {
        ProjectCatalogMigrationError::no_mutation(
            "error.project_catalog_migration_owner_snapshot",
            "checkout replica root cannot be enumerated",
        )
    })?;
    let mut roots = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|_| {
            ProjectCatalogMigrationError::no_mutation(
                "error.project_catalog_migration_owner_snapshot",
                "checkout replica entry cannot be inspected",
            )
        })?;
        let file_type = entry.file_type().map_err(|_| {
            ProjectCatalogMigrationError::no_mutation(
                "error.project_catalog_migration_owner_snapshot",
                "checkout replica entry type is unavailable",
            )
        })?;
        if !file_type.is_dir() || file_type.is_symlink() {
            return Err(ProjectCatalogMigrationError::no_mutation(
                "error.project_catalog_migration_owner_snapshot",
                "checkout replica root contains a non-directory entry",
            ));
        }
        roots.push(entry.path());
        if roots.len() > MAX_PROJECT_CATALOG_ENTRIES {
            return Err(ProjectCatalogMigrationError::no_mutation(
                "error.project_catalog_migration_owner_snapshot",
                "checkout replica root exceeds its cardinality limit",
            ));
        }
    }
    roots.sort();
    Ok(roots)
}

fn build_registry(
    layout: &ProjectCatalogMigrationResolvedLayoutV1,
    checkout_bindings: &BTreeMap<String, PathBuf>,
) -> Result<MigrationParticipantRegistry, ProjectCatalogMigrationError> {
    let mut registry = MigrationParticipantRegistry::new(
        &layout.projects_path,
        layout.code_source_root.clone(),
        layout.publisher_refs_path.clone(),
        layout.store_limits.clone(),
    )
    .map_err(store_validation_error)?;
    for (observation_id, root) in checkout_bindings {
        registry
            .register_checkout_identity(observation_id.clone(), root.clone())
            .map_err(store_validation_error)?;
    }
    registry.validate().map_err(store_validation_error)
}

fn prepare_source_drafts(
    inventory: &V1ProjectCatalogInventory,
    runtime: &MigrationRuntimeBindingsViewV1,
    publisher_was_missing: bool,
) -> Result<
    (
        MigrationLegacyProjectSourceDraftV1,
        MigrationPublisherSourceDraftV1,
        Vec<MigrationImmutableAssetDraftV1>,
    ),
    ProjectCatalogMigrationError,
> {
    let legacy = exact_source_state(inventory, MutableInventorySourceKindV1::LegacyProjectStore)?;
    let publisher = exact_source_state(inventory, MutableInventorySourceKindV1::PublisherRefStore)?;
    let mut assets = Vec::new();
    let legacy_draft = match legacy {
        InventorySourceStateV1::Missing { .. }
            if runtime.legacy_project_store_was_missing
                && runtime.legacy_project_store_bytes.is_empty() =>
        {
            MigrationLegacyProjectSourceDraftV1::Missing
        }
        InventorySourceStateV1::Present {
            content_hash,
            byte_len,
            ..
        } if !runtime.legacy_project_store_was_missing
            && content_hash == &Sha256ValueV1::digest(&runtime.legacy_project_store_bytes)
            && *byte_len
                == u64::try_from(runtime.legacy_project_store_bytes.len()).unwrap_or(u64::MAX) =>
        {
            assets.push(MigrationImmutableAssetDraftV1::new(
                ImmutableAssetRoleV1::LegacyProjectStoreBackup,
                runtime.legacy_project_store_bytes.clone(),
            ));
            MigrationLegacyProjectSourceDraftV1::Present(runtime.legacy_project_store_bytes.clone())
        }
        _ => {
            return Err(planner_error(
                "legacy project source state and exact runtime bytes disagree",
            ));
        }
    };
    let publisher_draft = match publisher {
        InventorySourceStateV1::Missing { .. }
            if publisher_was_missing && inventory.publisher_ref_source_bytes.is_empty() =>
        {
            MigrationPublisherSourceDraftV1::Missing
        }
        InventorySourceStateV1::Present {
            content_hash,
            byte_len,
            ..
        } if !publisher_was_missing
            && content_hash == &inventory.publisher_ref_source_hash
            && *byte_len
                == u64::try_from(inventory.publisher_ref_source_bytes.len())
                    .unwrap_or(u64::MAX) =>
        {
            assets.push(MigrationImmutableAssetDraftV1::new(
                ImmutableAssetRoleV1::LegacyPublisherRefBackup,
                inventory.publisher_ref_source_bytes.clone(),
            ));
            MigrationPublisherSourceDraftV1::Present(inventory.publisher_ref_source_bytes.clone())
        }
        _ => {
            return Err(planner_error(
                "publisher source state and exact captured bytes disagree",
            ));
        }
    };
    Ok((legacy_draft, publisher_draft, assets))
}

fn exact_source_state(
    inventory: &V1ProjectCatalogInventory,
    kind: MutableInventorySourceKindV1,
) -> Result<&InventorySourceStateV1, ProjectCatalogMigrationError> {
    let mut matches = inventory
        .mutable_source_evidence
        .iter()
        .filter(|row| row.source_kind == kind);
    let state = &matches
        .next()
        .ok_or_else(|| planner_error("required mutable source evidence is missing"))?
        .state;
    if matches.next().is_some() || matches!(state, InventorySourceStateV1::Corrupt { .. }) {
        return Err(planner_error(
            "required mutable source evidence is duplicated or corrupt",
        ));
    }
    Ok(state)
}

fn prepare_sensitive_review(
    inventory: &V1ProjectCatalogInventory,
    runtime: &MigrationRuntimeBindingsViewV1,
    sensitive_paths: &SensitiveLocalPathReportV1,
    include: bool,
) -> Result<Option<PreparedSensitiveReviewV1>, ProjectCatalogMigrationError> {
    if !include {
        return Ok(None);
    }
    let (bytes, _) = encode_facade_sensitive_review(inventory, runtime, sensitive_paths)?;
    Ok(Some(PreparedSensitiveReviewV1 { bytes }))
}

fn preflight_receipt(
    report: &ProjectCatalogMigrationReportV1,
    report_bytes: &[u8],
    resolution_bytes: &[u8],
    predicted_marker_hash: Option<Sha256ValueV1>,
    sensitive: Option<&PreparedSensitiveReviewV1>,
    attached_project_count: u64,
    catalog_project_count: u64,
) -> ProjectCatalogMigrationPreflightReceiptV1 {
    let quarantine_root_count = report
        .activation_conflicts
        .iter()
        .flat_map(|row| &row.affected_record_ids)
        .count();
    ProjectCatalogMigrationPreflightReceiptV1 {
        version: FACADE_VERSION_V1,
        status: report.status,
        transaction_id: report.transaction_id.clone(),
        inventory_hash: report.inventory_hash.clone(),
        plan_hash: report.plan_hash.clone(),
        report_artifact_hash: Sha256ValueV1::digest(report_bytes),
        resolution_artifact_hash: Sha256ValueV1::digest(resolution_bytes),
        predicted_catalog_hash: report.predicted_catalog_hash.clone(),
        predicted_attachment_hash: report.predicted_attachment_hash.clone(),
        predicted_participant_hashes: report.predicted_participant_hashes.clone(),
        predicted_immutable_asset_hashes: report.predicted_immutable_asset_hashes.clone(),
        predicted_marker_hash,
        required_resolution_count: u64::try_from(report.required_resolutions.len())
            .unwrap_or(u64::MAX),
        refusal_count: u64::try_from(report.refusals.len()).unwrap_or(u64::MAX),
        checkout_action_count: u64::try_from(report.checkout_identity_actions.len())
            .unwrap_or(u64::MAX),
        publisher_pin_count: u64::try_from(report.publisher_bindings.len()).unwrap_or(u64::MAX),
        quarantine_root_count: u64::try_from(quarantine_root_count).unwrap_or(u64::MAX),
        attached_project_count,
        omitted_catalog_count: catalog_project_count.saturating_sub(attached_project_count),
        sensitive_review: sensitive.map(|review| SensitiveReviewReceiptV1 {
            artifact_hash: Sha256ValueV1::digest(&review.bytes),
            legacy_path_row_count: u64::try_from(report.legacy_path_bindings.len())
                .unwrap_or(u64::MAX),
            attachment_path_row_count: u64::try_from(report.attachments.len()).unwrap_or(u64::MAX),
        }),
    }
}

fn validate_planned_identity(
    plan: &ValidatedMigrationPlanV1,
    report_bytes: &[u8],
    resolution_bytes: &[u8],
    report: &ProjectCatalogMigrationReportV1,
    immutable_predictions: &BTreeMap<String, Sha256ValueV1>,
) -> Result<IdentityProjectionsV1, ProjectCatalogMigrationError> {
    let identity = plan.artifact_identity();
    let projections = identity_projections(&identity)?;
    if identity.transaction_id != report.transaction_id
        || identity.plan_hash.as_str() != report.plan_hash.as_str()
        || identity.inventory_sha256.as_str() != report.inventory_hash.as_str()
        || identity.report_artifact_sha256.as_str() != Sha256ValueV1::digest(report_bytes).as_str()
        || identity.resolution_artifact_sha256.as_str()
            != Sha256ValueV1::digest(resolution_bytes).as_str()
        || projections.catalog_hash != report.predicted_catalog_hash
        || projections.attachment_hash != report.predicted_attachment_hash
        || projections.participant_hashes != report.predicted_participant_hashes
        || &projections.immutable_hashes != immutable_predictions
        || projections.immutable_hashes != report.predicted_immutable_asset_hashes
    {
        return Err(ProjectCatalogMigrationError::no_mutation(
            "error.project_catalog_migration_artifact_identity",
            "validated transaction plan disagrees with reviewed predictions",
        ));
    }
    Ok(projections)
}

struct IdentityProjectionsV1 {
    catalog_hash: Sha256ValueV1,
    attachment_hash: Sha256ValueV1,
    participant_hashes: BTreeMap<String, Sha256ValueV1>,
    immutable_hashes: BTreeMap<String, Sha256ValueV1>,
    marker_hash: Sha256ValueV1,
    observed_marker_hash: Sha256ValueV1,
    backup_hashes: BTreeMap<String, Sha256ValueV1>,
}

fn identity_projections(
    identity: &crate::project_catalog_store::MigrationArtifactIdentityV1,
) -> Result<IdentityProjectionsV1, ProjectCatalogMigrationError> {
    let mut catalog_hash = None;
    let mut attachment_hash = None;
    let mut marker_hash = None;
    let mut participants = BTreeMap::new();
    let mut backups = BTreeMap::new();
    for participant in &identity.participants {
        let token = participant.role.artifact_token();
        if let Some(old) = &participant.old_sha256 {
            backups.insert(
                format!("backup-{token}"),
                Sha256ValueV1::parse(old.to_string()).map_err(inventory_error)?,
            );
        }
        let Some(new) = &participant.new_sha256 else {
            continue;
        };
        let hash = Sha256ValueV1::parse(new.to_string()).map_err(inventory_error)?;
        match participant.role {
            ParticipantRoleV1::Catalog => catalog_hash = Some(hash),
            ParticipantRoleV1::Attachments => attachment_hash = Some(hash),
            ParticipantRoleV1::MigrationMarker => marker_hash = Some(hash),
            _ => {
                participants.insert(token, hash);
            }
        }
    }
    let immutable_hashes = identity
        .immutable_assets
        .iter()
        .map(|asset| {
            Ok((
                asset.role.artifact_token(),
                Sha256ValueV1::parse(asset.sha256.to_string()).map_err(inventory_error)?,
            ))
        })
        .collect::<Result<_, ProjectCatalogMigrationError>>()?;
    Ok(IdentityProjectionsV1 {
        catalog_hash: catalog_hash.ok_or_else(|| planner_error("catalog identity is missing"))?,
        attachment_hash: attachment_hash
            .ok_or_else(|| planner_error("attachment identity is missing"))?,
        participant_hashes: participants,
        immutable_hashes,
        marker_hash: marker_hash.ok_or_else(|| planner_error("marker identity is missing"))?,
        observed_marker_hash: Sha256ValueV1::parse(identity.observed_marker_sha256.to_string())
            .map_err(inventory_error)?,
        backup_hashes: backups,
    })
}

fn verify_exact_installed_review(
    layout: &ProjectCatalogMigrationResolvedLayoutV1,
    report_bytes: &[u8],
    report: &ProjectCatalogMigrationReportV1,
    resolution_bytes: &[u8],
) -> Result<Option<ProjectCatalogMigrationVerifyResultV1>, ProjectCatalogMigrationError> {
    match verify_installed_optional(layout)? {
        InstalledMigrationVerificationV1::Installed(result) => {
            validate_exact_installed_review(&result, report_bytes, report, resolution_bytes)?;
            Ok(Some(result))
        }
        InstalledMigrationVerificationV1::NotInstalled { .. } => Ok(None),
    }
}

fn validate_exact_installed_review(
    result: &ProjectCatalogMigrationVerifyResultV1,
    report_bytes: &[u8],
    report: &ProjectCatalogMigrationReportV1,
    resolution_bytes: &[u8],
) -> Result<(), ProjectCatalogMigrationError> {
    let receipt = &result.receipt;
    if receipt.transaction_id != report.transaction_id
        || receipt.plan_hash != report.plan_hash
        || receipt.inventory_hash != report.inventory_hash
        || receipt.report_artifact_hash != Sha256ValueV1::digest(report_bytes)
        || receipt.resolution_artifact_hash != Sha256ValueV1::digest(resolution_bytes)
        || receipt.expected_catalog_hash != report.predicted_catalog_hash
        || receipt.expected_attachment_hash != report.predicted_attachment_hash
        || receipt.expected_participant_hashes != report.predicted_participant_hashes
        || receipt.expected_immutable_asset_hashes != report.predicted_immutable_asset_hashes
    {
        return Err(ProjectCatalogMigrationError::new(
            "error.project_catalog_migration_artifact_identity",
            "installed migration belongs to a different reviewed artifact set",
            result.mutation_disposition,
        ));
    }
    Ok(())
}

fn post_commit_verification_error(
    error: ProjectCatalogMigrationError,
) -> ProjectCatalogMigrationError {
    ProjectCatalogMigrationError::new(
        error.code,
        "installed migration verification failed after commit",
        ProjectCatalogMigrationMutationDispositionV1::RecoveredToCommittedState,
    )
}

fn verify_installed(
    layout: &ProjectCatalogMigrationResolvedLayoutV1,
) -> Result<ProjectCatalogMigrationVerifyResultV1, ProjectCatalogMigrationError> {
    match verify_installed_optional(layout)? {
        InstalledMigrationVerificationV1::Installed(result) => Ok(result),
        InstalledMigrationVerificationV1::NotInstalled {
            mutation_disposition,
        } => {
            let (code, message) = if mutation_disposition
                == ProjectCatalogMigrationMutationDispositionV1::NoDurableMutation
            {
                (
                    "error.project_catalog_invalid_snapshot",
                    "migration verification requires installed v2 state",
                )
            } else {
                (
                    "error.project_catalog_migration_incomplete",
                    "migration recovery restored the pre-migration state",
                )
            };
            Err(ProjectCatalogMigrationError::new(
                code,
                message,
                mutation_disposition,
            ))
        }
    }
}

enum InstalledMigrationVerificationV1 {
    NotInstalled {
        mutation_disposition: ProjectCatalogMigrationMutationDispositionV1,
    },
    Installed(ProjectCatalogMigrationVerifyResultV1),
}

const GIT_META_BACKUP_DIRNAME: &str = "git_meta";
const GIT_META_BACKUP_STAGING_DIRNAME: &str = "git_meta.tmp";
const GIT_META_BACKUP_HASH_KEY: &str = "backup-git_meta";
const GIT_META_BACKUP_DOMAIN: &[u8] = b"blackbox.project-catalog.git-meta-backup.v1\0";
const MAX_GIT_META_BACKUP_FILES: usize = MAX_PROJECT_CATALOG_ENTRIES;
const MAX_GIT_META_BACKUP_FILE_BYTES: usize = 4096;

fn git_meta_backup_error(detail: impl Into<String>) -> ProjectCatalogMigrationError {
    ProjectCatalogMigrationError::new(
        "error.project_catalog_migration_git_meta_backup",
        detail.into(),
        ProjectCatalogMigrationMutationDispositionV1::RecoveredToCommittedState,
    )
}

fn sorted_regular_basenames(path: &Path) -> Result<Vec<String>, ProjectCatalogMigrationError> {
    let mut names = Vec::new();
    for entry in
        std::fs::read_dir(path).map_err(|error| git_meta_backup_error(error.to_string()))?
    {
        let entry = entry.map_err(|error| git_meta_backup_error(error.to_string()))?;
        let file_type = entry
            .file_type()
            .map_err(|error| git_meta_backup_error(error.to_string()))?;
        if !file_type.is_file() {
            return Err(git_meta_backup_error(
                "git meta directory contains a non-regular entry",
            ));
        }
        let name = entry
            .file_name()
            .into_string()
            .map_err(|_| git_meta_backup_error("git meta directory has a non-utf8 entry"))?;
        if names.len() >= MAX_GIT_META_BACKUP_FILES {
            return Err(git_meta_backup_error(
                "git meta directory exceeds its row limit",
            ));
        }
        names.push(name);
    }
    names.sort();
    Ok(names)
}

/// One-time, idempotent copy of the legacy per-project Git cursor directory
/// into the migration backup root (governing section 11's cursor-file
/// backup promise, Phase 3 plan section 4.2). Never overwrites an existing
/// backup: the backup must reflect the state captured at the migration that
/// created it, not whatever the live directory has drifted to by a later
/// retry or reverification. A no-op when the live directory never existed
/// (a store that never walked Git history).
///
/// Copies into a sibling staging directory first, then atomically renames it
/// onto the final `git_meta` destination: `git_meta_backup_hash` (and any
/// other reader) only ever observes either no destination at all, or a
/// COMPLETE one, never a partial one written mid-copy. A leftover staging
/// directory from a crashed prior attempt is removed unconditionally before
/// staging fresh, so a retry always promotes a freshly-written, complete
/// copy rather than resuming (or exposing) a partial one.
fn git_meta_backup_copy_if_needed(
    layout: &ProjectCatalogMigrationResolvedLayoutV1,
) -> Result<(), ProjectCatalogMigrationError> {
    let destination_root = layout.catalog_backup_dir.join(GIT_META_BACKUP_DIRNAME);
    if NofollowDirectory::open_existing(&destination_root)
        .map_err(|error| git_meta_backup_error(error.to_string()))?
        .is_some()
    {
        return Ok(());
    }
    let Some(source) = NofollowDirectory::open_existing(&layout.git_meta_root)
        .map_err(|error| git_meta_backup_error(error.to_string()))?
    else {
        return Ok(());
    };
    let names = sorted_regular_basenames(&layout.git_meta_root)?;

    let staging_root = layout
        .catalog_backup_dir
        .join(GIT_META_BACKUP_STAGING_DIRNAME);
    match std::fs::remove_dir_all(&staging_root) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(git_meta_backup_error(error.to_string())),
    }
    let staging = NofollowDirectory::open_or_create(&staging_root)
        .map_err(|error| git_meta_backup_error(error.to_string()))?;
    for name in &names {
        let bytes = source
            .read_regular(name, MAX_GIT_META_BACKUP_FILE_BYTES, "git meta cursor file")
            .map_err(|error| git_meta_backup_error(error.to_string()))?
            .ok_or_else(|| {
                git_meta_backup_error("git meta cursor file disappeared during backup")
            })?;
        staging
            .atomic_replace(name, &bytes)
            .map_err(|error| git_meta_backup_error(error.to_string()))?;
    }
    staging
        .sync_all()
        .map_err(|error| git_meta_backup_error(error.to_string()))?;
    source
        .ensure_still_current()
        .map_err(|error| git_meta_backup_error(error.to_string()))?;
    // The atomic promotion: a crash on either side of this rename leaves the
    // world in a state the next retry (or a fresh copy attempt) handles
    // correctly: either the staging directory alone (cleaned up and
    // rebuilt above) or the complete destination alone (caught by the
    // existence guard at the top).
    std::fs::rename(&staging_root, &destination_root)
        .map_err(|error| git_meta_backup_error(error.to_string()))?;
    Ok(())
}

/// Read-only hash of whatever is currently at the git-meta backup
/// destination, `None` when no backup was ever made. Hashes the STORED
/// backup copy rather than the live `git_meta_root`, so repeated `verify()`
/// calls stay stable across Git activity that happens after migration.
fn git_meta_backup_hash(
    layout: &ProjectCatalogMigrationResolvedLayoutV1,
) -> Result<Option<Sha256ValueV1>, ProjectCatalogMigrationError> {
    let destination_root = layout.catalog_backup_dir.join(GIT_META_BACKUP_DIRNAME);
    let Some(directory) = NofollowDirectory::open_existing(&destination_root)
        .map_err(|error| git_meta_backup_error(error.to_string()))?
    else {
        return Ok(None);
    };
    let names = sorted_regular_basenames(&destination_root)?;
    let mut bytes = Vec::new();
    bytes.extend_from_slice(GIT_META_BACKUP_DOMAIN);
    for name in &names {
        let contents = directory
            .read_regular(name, MAX_GIT_META_BACKUP_FILE_BYTES, "git meta cursor file")
            .map_err(|error| git_meta_backup_error(error.to_string()))?
            .ok_or_else(|| {
                git_meta_backup_error("git meta backup file disappeared during hashing")
            })?;
        bytes.extend_from_slice(&(name.len() as u64).to_be_bytes());
        bytes.extend_from_slice(name.as_bytes());
        bytes.extend_from_slice(&(contents.len() as u64).to_be_bytes());
        bytes.extend_from_slice(&contents);
    }
    directory
        .ensure_still_current()
        .map_err(|error| git_meta_backup_error(error.to_string()))?;
    Ok(Some(Sha256ValueV1::digest(&bytes)))
}

fn verify_installed_optional(
    layout: &ProjectCatalogMigrationResolvedLayoutV1,
) -> Result<InstalledMigrationVerificationV1, ProjectCatalogMigrationError> {
    let bootstrap =
        begin_migration_checkout_registry_bootstrap(&layout.projects_path).map_err(|failure| {
            ProjectCatalogMigrationError::new(
                failure.error.code(),
                "migration checkout registry bootstrap failed",
                facade_mutation_disposition(failure.disposition),
            )
        })?;
    let session = match bootstrap {
        MigrationCheckoutRegistryBootstrapV1::FreshLegacyNotInstalled => {
            return Ok(InstalledMigrationVerificationV1::NotInstalled {
                mutation_disposition:
                    ProjectCatalogMigrationMutationDispositionV1::NoDurableMutation,
            });
        }
        MigrationCheckoutRegistryBootstrapV1::RolledBackNotInstalled { disposition } => {
            return Ok(InstalledMigrationVerificationV1::NotInstalled {
                mutation_disposition: facade_mutation_disposition(disposition),
            });
        }
        MigrationCheckoutRegistryBootstrapV1::RequiresRegistry(session) => session,
    };
    let bootstrap_disposition = facade_mutation_disposition(session.disposition());
    let bootstrap_registry = build_registry(layout, &BTreeMap::new())
        .map_err(|error| error.with_mutation_disposition(bootstrap_disposition))?;
    let session = session
        .bind_registry(bootstrap_registry)
        .map_err(|failure| {
            ProjectCatalogMigrationError::new(
                failure.error.code(),
                "migration checkout registry binding failed",
                facade_mutation_disposition(failure.disposition),
            )
        })?;
    let bootstrap_disposition = facade_mutation_disposition(session.disposition());
    let checkout_roots = discover_checkout_roots(layout)
        .map_err(|error| error.with_mutation_disposition(bootstrap_disposition))?;
    let checkout_bindings =
        ProjectCatalogMigrationInventoryFacadeV1::discover_checkout_observation_bindings(
            checkout_roots,
        )
        .map_err(adapter_error)
        .map_err(|error| error.with_mutation_disposition(bootstrap_disposition))?;
    let opened = session.finish_open(&checkout_bindings).map_err(|failure| {
        ProjectCatalogMigrationError::new(
            failure.error.code(),
            "migration store recovery or open failed",
            facade_mutation_disposition(failure.disposition),
        )
    })?;
    let opened = match opened {
        MigrationStoreOpenOutcomeV1::Installed(opened) => opened,
        MigrationStoreOpenOutcomeV1::RolledBackNotInstalled { disposition } => {
            return Ok(InstalledMigrationVerificationV1::NotInstalled {
                mutation_disposition: facade_mutation_disposition(disposition),
            });
        }
    };
    let mutation_disposition = facade_mutation_disposition(opened.disposition);
    let store = opened.store;
    let identity = store
        .migration_artifact_identity()
        .map_err(|error| store_error_with_disposition(error, mutation_disposition))?;
    let state = store
        .snapshot()
        .map_err(|error| store_error_with_disposition(error, mutation_disposition))?;
    let compatibility = build_compatibility_projection(&state)
        .map_err(|error| error.with_mutation_disposition(mutation_disposition))?;
    let projections = identity_projections(&identity)
        .map_err(|error| error.with_mutation_disposition(mutation_disposition))?;
    let observed_catalog_hash = if identity.migration_install_is_current {
        Sha256ValueV1::parse(state.catalog_sha256().to_string())
            .map_err(inventory_error)
            .map_err(|error| error.with_mutation_disposition(mutation_disposition))?
    } else {
        projections.catalog_hash.clone()
    };
    let observed_attachment_hash = if identity.migration_install_is_current {
        Sha256ValueV1::parse(state.attachments_sha256().to_string())
            .map_err(inventory_error)
            .map_err(|error| error.with_mutation_disposition(mutation_disposition))?
    } else {
        projections.attachment_hash.clone()
    };
    let git_meta_backup_hash_value = git_meta_backup_hash(layout)
        .map_err(|error| error.with_mutation_disposition(mutation_disposition))?;
    let receipt = (|| {
        let mut backup_hashes = projections.backup_hashes;
        if let Some(hash) = git_meta_backup_hash_value {
            backup_hashes.insert(GIT_META_BACKUP_HASH_KEY.to_string(), hash);
        }
        Ok::<_, ProjectCatalogMigrationError>(MigrationVerificationReceiptV1 {
            version: FACADE_VERSION_V1,
            transaction_id: identity.transaction_id,
            inventory_hash: Sha256ValueV1::parse(identity.inventory_sha256.to_string())
                .map_err(inventory_error)?,
            plan_hash: Sha256ValueV1::parse(identity.plan_hash.to_string())
                .map_err(inventory_error)?,
            report_artifact_hash: Sha256ValueV1::parse(identity.report_artifact_sha256.to_string())
                .map_err(inventory_error)?,
            resolution_artifact_hash: Sha256ValueV1::parse(
                identity.resolution_artifact_sha256.to_string(),
            )
            .map_err(inventory_error)?,
            expected_catalog_hash: projections.catalog_hash.clone(),
            observed_catalog_hash,
            expected_attachment_hash: projections.attachment_hash.clone(),
            observed_attachment_hash,
            expected_participant_hashes: projections.participant_hashes.clone(),
            observed_participant_hashes: projections.participant_hashes,
            expected_immutable_asset_hashes: projections.immutable_hashes.clone(),
            observed_immutable_asset_hashes: projections.immutable_hashes,
            predicted_marker_hash: projections.marker_hash,
            observed_marker_hash: projections.observed_marker_hash,
            backup_hashes,
            epoch: identity.epoch,
            checkout_action_count: identity.checkout_action_count,
            publisher_pin_count: identity.publisher_pin_count,
            quarantine_root_count: identity.quarantine_root_count,
            attached_project_count: u64::try_from(compatibility.records.len()).unwrap_or(u64::MAX),
            omitted_catalog_count: compatibility.omitted_catalog_count,
        })
    })()
    .map_err(|error| error.with_mutation_disposition(mutation_disposition))?;
    Ok(InstalledMigrationVerificationV1::Installed(
        ProjectCatalogMigrationVerifyResultV1 {
            receipt,
            compatibility,
            mutation_disposition,
        },
    ))
}

fn adapter_error(
    error: crate::project_catalog_inventory_adapters::InventoryAdapterError,
) -> ProjectCatalogMigrationError {
    ProjectCatalogMigrationError::no_mutation(
        error.code(),
        "migration owner snapshot contract failed",
    )
}

fn store_validation_error(
    error: crate::project_catalog_store::ProjectCatalogStoreError,
) -> ProjectCatalogMigrationError {
    ProjectCatalogMigrationError::no_mutation(error.code(), "migration store contract failed")
}

fn store_error_with_disposition(
    error: crate::project_catalog_store::ProjectCatalogStoreError,
    mutation_disposition: ProjectCatalogMigrationMutationDispositionV1,
) -> ProjectCatalogMigrationError {
    ProjectCatalogMigrationError::new(
        error.code(),
        "migration store contract failed after classified recovery",
        mutation_disposition,
    )
}

fn facade_mutation_disposition(
    disposition: MigrationMutationDispositionV1,
) -> ProjectCatalogMigrationMutationDispositionV1 {
    match disposition {
        MigrationMutationDispositionV1::NoDurableMutation => {
            ProjectCatalogMigrationMutationDispositionV1::NoDurableMutation
        }
        MigrationMutationDispositionV1::RecoveredToOldState => {
            ProjectCatalogMigrationMutationDispositionV1::RecoveredToOldState
        }
        MigrationMutationDispositionV1::RecoveredToCommittedState => {
            ProjectCatalogMigrationMutationDispositionV1::RecoveredToCommittedState
        }
        MigrationMutationDispositionV1::RetryExactPlanRequired => {
            ProjectCatalogMigrationMutationDispositionV1::RetryExactPlanRequired
        }
    }
}

fn validate_prepared_preflight(
    prepared: &PreparedPreflightV1,
    existing_resolution: Option<&[u8]>,
    sensitive_review_requested: bool,
) -> Result<(), ProjectCatalogMigrationError> {
    if prepared.report_bytes.is_empty() || prepared.resolution_bytes.is_empty() {
        return Err(ProjectCatalogMigrationError::no_mutation(
            "error.project_catalog_migration_invalid_preflight_output",
            "closed preflight produced an empty reviewed artifact",
        ));
    }
    let report = decode_migration_report_v1(&prepared.report_bytes).map_err(|_| {
        ProjectCatalogMigrationError::no_mutation(
            "error.project_catalog_migration_invalid_preflight_output",
            "closed preflight produced an invalid report",
        )
    })?;
    let resolution = decode_migration_resolution_v1(&prepared.resolution_bytes).map_err(|_| {
        ProjectCatalogMigrationError::no_mutation(
            "error.project_catalog_migration_invalid_preflight_output",
            "closed preflight produced an invalid resolution",
        )
    })?;
    if existing_resolution.is_some_and(|bytes| bytes != prepared.resolution_bytes)
        || report.resolution_artifact_hash != Sha256ValueV1::digest(&prepared.resolution_bytes)
        || resolution.inventory_hash != report.inventory_hash
        || prepared.receipt.version != FACADE_VERSION_V1
        || prepared.receipt.status != report.status
        || prepared.receipt.transaction_id != report.transaction_id
        || prepared.receipt.inventory_hash != report.inventory_hash
        || prepared.receipt.plan_hash != report.plan_hash
        || prepared.receipt.report_artifact_hash != Sha256ValueV1::digest(&prepared.report_bytes)
        || prepared.receipt.resolution_artifact_hash
            != Sha256ValueV1::digest(&prepared.resolution_bytes)
        || prepared.receipt.predicted_catalog_hash != report.predicted_catalog_hash
        || prepared.receipt.predicted_attachment_hash != report.predicted_attachment_hash
        || prepared.receipt.predicted_participant_hashes != report.predicted_participant_hashes
        || prepared.receipt.predicted_immutable_asset_hashes
            != report.predicted_immutable_asset_hashes
        || prepared.receipt.predicted_marker_hash.is_some()
            != (report.status == ProjectCatalogMigrationStatusV1::Clean
                && report.plan_kind == ProjectCatalogMigrationPlanKindV1::Executable)
        || prepared.receipt.required_resolution_count
            != u64::try_from(report.required_resolutions.len()).unwrap_or(u64::MAX)
        || prepared.receipt.refusal_count
            != u64::try_from(report.refusals.len()).unwrap_or(u64::MAX)
        || prepared.receipt.checkout_action_count
            != u64::try_from(report.checkout_identity_actions.len()).unwrap_or(u64::MAX)
    {
        return Err(ProjectCatalogMigrationError::no_mutation(
            "error.project_catalog_migration_invalid_preflight_output",
            "closed preflight artifact identities disagree",
        ));
    }
    match (
        sensitive_review_requested,
        &prepared.sensitive_review,
        &prepared.receipt.sensitive_review,
    ) {
        (false, None, None) => {}
        (true, Some(sensitive), Some(receipt))
            if !sensitive.bytes.is_empty()
                && receipt.artifact_hash == Sha256ValueV1::digest(&sensitive.bytes) => {}
        _ => {
            return Err(ProjectCatalogMigrationError::no_mutation(
                "error.project_catalog_migration_invalid_preflight_output",
                "closed preflight sensitive-review bytes and receipt disagree",
            ));
        }
    }
    Ok(())
}

fn validate_apply_result(
    result: &ProjectCatalogMigrationApplyResultV1,
    report_bytes: &[u8],
    report: &ProjectCatalogMigrationReportV1,
    resolution_bytes: &[u8],
) -> Result<(), ProjectCatalogMigrationError> {
    let verification = &result.receipt.verification;
    if result.receipt.version != FACADE_VERSION_V1
        || verification.version != FACADE_VERSION_V1
        || verification.transaction_id != report.transaction_id
        || verification.inventory_hash != report.inventory_hash
        || verification.plan_hash != report.plan_hash
        || verification.report_artifact_hash != Sha256ValueV1::digest(report_bytes)
        || verification.resolution_artifact_hash != Sha256ValueV1::digest(resolution_bytes)
        || verification.expected_catalog_hash != report.predicted_catalog_hash
        || verification.expected_attachment_hash != report.predicted_attachment_hash
        || verification.expected_participant_hashes != report.predicted_participant_hashes
        || verification.expected_immutable_asset_hashes != report.predicted_immutable_asset_hashes
        || !verification_receipt_observations_match(verification)
    {
        return Err(ProjectCatalogMigrationError::new(
            "error.project_catalog_migration_invalid_apply_output",
            "closed apply returned a receipt that disagrees with reviewed or installed state",
            ProjectCatalogMigrationMutationDispositionV1::RecoveredToCommittedState,
        ));
    }
    Ok(())
}

fn validate_verify_result(
    result: &ProjectCatalogMigrationVerifyResultV1,
) -> Result<(), ProjectCatalogMigrationError> {
    if result.receipt.version != FACADE_VERSION_V1
        || !verification_receipt_observations_match(&result.receipt)
        || result.receipt.omitted_catalog_count != result.compatibility.omitted_catalog_count
        || result.receipt.attached_project_count
            != u64::try_from(result.compatibility.records.len()).unwrap_or(u64::MAX)
    {
        return Err(ProjectCatalogMigrationError::new(
            "error.project_catalog_migration_invalid_verify_output",
            "closed verification returned inconsistent installed state",
            result.mutation_disposition,
        ));
    }
    Ok(())
}

fn verification_receipt_observations_match(receipt: &MigrationVerificationReceiptV1) -> bool {
    receipt.expected_catalog_hash == receipt.observed_catalog_hash
        && receipt.expected_attachment_hash == receipt.observed_attachment_hash
        && receipt.expected_participant_hashes == receipt.observed_participant_hashes
        && receipt.expected_immutable_asset_hashes == receipt.observed_immutable_asset_hashes
        && receipt.predicted_marker_hash == receipt.observed_marker_hash
}

fn validate_artifact_set(
    layout: &ProjectCatalogMigrationResolvedLayoutV1,
    report_path: &Path,
    resolution_path: &Path,
    sensitive_path: Option<&Path>,
) -> Result<(), ProjectCatalogMigrationError> {
    validate_artifact_target(layout, report_path)?;
    validate_artifact_target(layout, resolution_path)?;
    if report_path == resolution_path {
        return Err(unsafe_layout(
            "report and resolution artifacts require distinct paths",
        ));
    }
    if let Some(path) = sensitive_path {
        validate_artifact_target(layout, path)?;
        if path == report_path || path == resolution_path {
            return Err(unsafe_layout(
                "sensitive review requires a distinct artifact path",
            ));
        }
    }
    Ok(())
}

fn validate_artifact_target(
    layout: &ProjectCatalogMigrationResolvedLayoutV1,
    path: &Path,
) -> Result<(), ProjectCatalogMigrationError> {
    validate_absolute_path(path)?;
    if layout.all_paths().into_iter().any(|source| path == source)
        || layout
            .owner_directory_roots()
            .into_iter()
            .any(|root| path.starts_with(root))
    {
        return Err(unsafe_layout(
            "reviewed artifact path overlaps a migration owner",
        ));
    }
    artifact_name(path)?;
    Ok(())
}

fn validate_rehearsal_separation(
    rehearsal: &ProjectCatalogMigrationResolvedLayoutV1,
    protected: &ProjectCatalogMigrationResolvedLayoutV1,
) -> Result<(), ProjectCatalogMigrationError> {
    let root = rehearsal
        .rehearsal_root
        .as_ref()
        .ok_or_else(|| unsafe_layout("rehearsal apply requires a rehearsal-root layout"))?;
    let root_metadata = std::fs::symlink_metadata(root)
        .map_err(|_| unsafe_layout("rehearsal root must be an existing directory"))?;
    if root_metadata.file_type().is_symlink() || !root_metadata.is_dir() {
        return Err(unsafe_layout(
            "rehearsal root must be a no-follow directory",
        ));
    }
    let canonical_root = root
        .canonicalize()
        .map_err(|_| unsafe_layout("rehearsal root must exist and be canonicalizable"))?;
    let rehearsal_paths = rehearsal.all_paths();
    let protected_paths = protected.all_paths();
    for path in &rehearsal_paths {
        if !path.starts_with(root) {
            return Err(unsafe_layout(
                "rehearsal authority escapes the declared rehearsal root",
            ));
        }
        let canonical = canonicalize_existing_ancestor(path)?;
        if !canonical.starts_with(&canonical_root) {
            return Err(unsafe_layout(
                "rehearsal authority resolves outside the declared root",
            ));
        }
    }
    for path in &protected_paths {
        if paths_overlap(root, path) {
            return Err(unsafe_layout(
                "rehearsal root overlaps a protected configured authority",
            ));
        }
        let canonical = canonicalize_existing_ancestor(path)?;
        if paths_overlap(&canonical_root, &canonical) {
            return Err(unsafe_layout(
                "rehearsal root aliases a protected configured authority",
            ));
        }
    }
    for rehearsal_path in &rehearsal_paths {
        for protected_path in &protected_paths {
            if existing_paths_share_inode(rehearsal_path, protected_path)? {
                return Err(unsafe_layout(
                    "rehearsal authority aliases a protected configured inode",
                ));
            }
        }
    }
    Ok(())
}

fn validate_rehearsal_runtime_authorities(
    layout: &ProjectCatalogMigrationResolvedLayoutV1,
    checkout_roots: &[PathBuf],
) -> Result<(), ProjectCatalogMigrationError> {
    let Some(root) = layout.rehearsal_root.as_ref() else {
        return Ok(());
    };
    let canonical_root = root
        .canonicalize()
        .map_err(|_| unsafe_layout("rehearsal root must be canonicalizable"))?;
    let require_contained = |path: &Path| -> Result<PathBuf, ProjectCatalogMigrationError> {
        if !path.is_absolute()
            || path.components().any(|component| {
                matches!(
                    component,
                    Component::CurDir | Component::ParentDir | Component::Prefix(_)
                )
            })
        {
            return Err(unsafe_layout(
                "runtime migration authority is not a normalized absolute path",
            ));
        }
        let canonical = path
            .canonicalize()
            .map_err(|_| unsafe_layout("runtime migration authority is not canonicalizable"))?;
        if canonical != path || !canonical.starts_with(&canonical_root) {
            return Err(unsafe_layout(
                "runtime migration authority escapes the rehearsal root",
            ));
        }
        Ok(canonical)
    };
    for checkout in checkout_roots {
        require_contained(checkout)?;
    }
    Ok(())
}

fn canonicalize_existing_ancestor(path: &Path) -> Result<PathBuf, ProjectCatalogMigrationError> {
    let mut cursor = path;
    let mut suffix = Vec::new();
    while !cursor.exists() {
        let name = cursor
            .file_name()
            .ok_or_else(|| unsafe_layout("migration authority has no existing ancestor"))?;
        suffix.push(name.to_os_string());
        cursor = cursor
            .parent()
            .ok_or_else(|| unsafe_layout("migration authority has no existing ancestor"))?;
    }
    let mut canonical = cursor
        .canonicalize()
        .map_err(|_| unsafe_layout("migration authority ancestor cannot be canonicalized"))?;
    for name in suffix.into_iter().rev() {
        canonical.push(name);
    }
    Ok(canonical)
}

#[cfg(unix)]
fn existing_paths_share_inode(
    left: &Path,
    right: &Path,
) -> Result<bool, ProjectCatalogMigrationError> {
    use std::os::unix::fs::MetadataExt;

    let left = match std::fs::symlink_metadata(left) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(_) => return Err(unsafe_layout("migration authority metadata is unreadable")),
    };
    let right = match std::fs::symlink_metadata(right) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(_) => return Err(unsafe_layout("migration authority metadata is unreadable")),
    };
    Ok(left.dev() == right.dev() && left.ino() == right.ino())
}

#[cfg(not(unix))]
fn existing_paths_share_inode(
    _left: &Path,
    _right: &Path,
) -> Result<bool, ProjectCatalogMigrationError> {
    Ok(false)
}

fn paths_overlap(left: &Path, right: &Path) -> bool {
    left == right || left.starts_with(right) || right.starts_with(left)
}

fn read_artifact_required(
    path: &Path,
    max_bytes: usize,
    label: &'static str,
) -> Result<Vec<u8>, ProjectCatalogMigrationError> {
    read_artifact_optional(path, max_bytes, label)?.ok_or_else(|| {
        ProjectCatalogMigrationError::no_mutation(
            "error.project_catalog_migration_artifact_missing",
            format!("{label} artifact is missing"),
        )
    })
}

fn read_artifact_optional(
    path: &Path,
    max_bytes: usize,
    label: &'static str,
) -> Result<Option<Vec<u8>>, ProjectCatalogMigrationError> {
    validate_absolute_path(path)?;
    let parent = path
        .parent()
        .ok_or_else(|| unsafe_layout("reviewed artifact path has no parent"))?;
    let name = artifact_name(path)?;
    let Some(directory) = NofollowDirectory::open_existing(parent)
        .map_err(|_| artifact_io_error(label, "artifact parent is unsafe or unreadable"))?
    else {
        return Ok(None);
    };
    let bytes = directory
        .read_regular(name, max_bytes, label)
        .map_err(|_| artifact_io_error(label, "artifact is unsafe or unreadable"))?;
    directory
        .ensure_still_current()
        .map_err(|_| artifact_io_error(label, "artifact parent changed during read"))?;
    match bytes {
        Some(bytes) if bytes.is_empty() => Err(ProjectCatalogMigrationError::no_mutation(
            "error.project_catalog_migration_artifact_empty",
            format!("{label} artifact is empty"),
        )),
        other => Ok(other),
    }
}

fn write_artifact_atomic(
    path: &Path,
    bytes: &[u8],
    max_bytes: usize,
    label: &'static str,
) -> Result<(), ProjectCatalogMigrationError> {
    write_artifact(path, bytes, max_bytes, label, true)
}

fn write_artifact_if_absent(
    path: &Path,
    bytes: &[u8],
    max_bytes: usize,
    label: &'static str,
) -> Result<(), ProjectCatalogMigrationError> {
    write_artifact(path, bytes, max_bytes, label, false)
}

fn write_artifact(
    path: &Path,
    bytes: &[u8],
    max_bytes: usize,
    label: &'static str,
    replace_existing: bool,
) -> Result<(), ProjectCatalogMigrationError> {
    if bytes.is_empty() || bytes.len() > max_bytes {
        return Err(artifact_io_error(
            label,
            "artifact bytes are empty or exceed the hard limit",
        ));
    }
    validate_absolute_path(path)?;
    let parent = path
        .parent()
        .ok_or_else(|| unsafe_layout("reviewed artifact path has no parent"))?;
    let name = artifact_name(path)?;
    let directory = NofollowDirectory::open_or_create(parent)
        .map_err(|_| artifact_io_error(label, "artifact parent is unsafe or unwritable"))?;
    directory
        .lock_exclusive()
        .map_err(|_| artifact_io_error(label, "artifact parent cannot be locked"))?;
    let existing = directory
        .read_regular(name, max_bytes, label)
        .map_err(|_| artifact_io_error(label, "existing artifact is not a regular file"))?;
    if !replace_existing && let Some(existing) = existing {
        directory
            .ensure_still_current()
            .map_err(|_| artifact_io_error(label, "artifact parent changed during write"))?;
        return if existing == bytes {
            Ok(())
        } else {
            Err(ProjectCatalogMigrationError::no_mutation(
                "error.project_catalog_migration_artifact_identity",
                format!("{label} artifact appeared with different bytes during preflight"),
            ))
        };
    }
    directory
        .atomic_replace(name, bytes)
        .map_err(|_| artifact_io_error(label, "artifact atomic replacement failed"))?;
    directory
        .ensure_still_current()
        .map_err(|_| artifact_io_error(label, "artifact parent changed during write"))
}

fn artifact_name(path: &Path) -> Result<&str, ProjectCatalogMigrationError> {
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .filter(|name| {
            !name.is_empty()
                && name.len() <= 255
                && !matches!(*name, "." | "..")
                && !name.contains(['/', '\\'])
                && !name
                    .bytes()
                    .any(|byte| byte == 0 || byte.is_ascii_control())
        })
        .ok_or_else(|| unsafe_layout("reviewed artifact basename is unsafe"))?;
    Ok(name)
}

fn artifact_io_error(label: &'static str, message: &'static str) -> ProjectCatalogMigrationError {
    ProjectCatalogMigrationError::no_mutation(
        "error.project_catalog_migration_artifact_io",
        format!("{label} {message}"),
    )
}

pub(crate) fn build_compatibility_projection(
    state: &crate::project_catalog_store::ProjectCatalogState,
) -> Result<ProjectCatalogCompatibilityProjectionV1, ProjectCatalogMigrationError> {
    let catalog = state.catalog();
    let attachments = state.attachments();
    let validated = validate_catalog_attachments(catalog, attachments).map_err(|_| {
        ProjectCatalogMigrationError::no_mutation(
            "error.project_catalog_migration_compatibility_join",
            "catalog and attachment snapshots fail cross-store validation",
        )
    })?;
    let migrated = matches!(&catalog.origin, CatalogOriginV2::MigratedV1 { .. });
    let mut records = Vec::new();
    let mut omitted_catalog_count = 0_u64;
    for project in catalog.projects.values() {
        let base_ids = attachments
            .attachments
            .values()
            .filter(|attachment| {
                attachment.project_id == project.project_id
                    && attachment.status == AttachmentStatus::Attached
                    && attachment.kind == AttachmentKind::Base
            })
            .map(|attachment| attachment.attachment_id.clone())
            .collect::<Vec<_>>();
        if base_ids.is_empty() {
            omitted_catalog_count = omitted_catalog_count.saturating_add(1);
            continue;
        }
        if base_ids.len() != 1
            || migrated
                && (project.registered_at_compat.is_none()
                    || project.registered_at_compat.as_ref() != Some(&project.created_at))
        {
            return Err(ProjectCatalogMigrationError::no_mutation(
                "error.project_catalog_migration_compatibility_join",
                "migrated project lacks one exact base compatibility attachment",
            ));
        }
        let attachment = validated.attachment(&base_ids[0]).ok_or_else(|| {
            ProjectCatalogMigrationError::no_mutation(
                "error.project_catalog_migration_compatibility_join",
                "base compatibility attachment is not cross-store validated",
            )
        })?;
        records.push(
            ProjectRecord::from_catalog_attachment(project, attachment).map_err(|_| {
                ProjectCatalogMigrationError::no_mutation(
                    "error.project_catalog_migration_compatibility_join",
                    "base attachment cannot form a v1 compatibility row",
                )
            })?,
        );
    }
    records.sort_by(|left, right| {
        (&left.canonical_path, &left.project_id).cmp(&(&right.canonical_path, &right.project_id))
    });
    Ok(ProjectCatalogCompatibilityProjectionV1 {
        records,
        omitted_catalog_count,
    })
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::process::Command;

    use bbox_config::config;
    use tempfile::tempdir;

    use super::*;

    fn run_git(root: &Path, args: &[&str]) -> String {
        let output = Command::new("git")
            .arg("-C")
            .arg(root)
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap().trim().to_string()
    }

    fn test_config(root: &Path) -> Config {
        let _guard = bbox_util::util::test_env_lock();
        let config_path = root.join("config.toml");
        fs::write(
            &config_path,
            // `vectors_dir` is explicit: the vector root defaults to the
            // PLATFORM state directory (R33F1), and a fixture that omitted it
            // would inventory the host's real vector store.
            format!(
                "[paths]\nstate_dir = {:?}\nvectors_dir = {:?}\n",
                root.join("live"),
                root.join("live").join("vectors")
            ),
        )
        .unwrap();
        config::load_with(config::LoadOptions {
            config_path: Some(config_path),
            ..Default::default()
        })
        .unwrap()
    }

    #[test]
    fn rehearsal_layout_is_fixed_and_contained() {
        let dir = tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let config = test_config(&root);
        let rehearsal = root.join("rehearsal");
        let layout =
            ProjectCatalogMigrationResolvedLayoutV1::from_rehearsal_root(&rehearsal, &config)
                .unwrap();
        assert_eq!(
            layout.projects_path,
            rehearsal.join("state").join("projects.json")
        );
        assert_eq!(
            layout.attachments_path,
            rehearsal.join("state").join("project-attachments.json")
        );
        assert_eq!(
            layout.transaction_journal_path,
            rehearsal
                .join("state")
                .join("project-catalog-transaction.json")
        );
        assert_eq!(
            layout.accepted_publications_root,
            rehearsal.join("state").join("accepted-publications")
        );
        assert_eq!(
            layout.publisher_refs_path,
            rehearsal
                .join("state")
                .join("bro")
                .join("publisher-refs.json")
        );
        assert_eq!(
            layout.checkout_replicas_root,
            Some(rehearsal.join("checkouts"))
        );
        assert_eq!(layout.provenance_notes_ref, "refs/notes/bb/provenance");
        assert_eq!(
            owner_inventory_paths(&layout).whiteboard_root,
            rehearsal.join("state").join("bro").join("whiteboards")
        );
        assert!(
            layout
                .all_paths()
                .into_iter()
                .all(|path| path.starts_with(&rehearsal))
        );
    }

    #[test]
    fn git_meta_backup_is_idempotent_content_addressed_and_absence_safe() {
        let dir = tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let config = test_config(&root);
        let rehearsal = root.join("rehearsal");
        let layout =
            ProjectCatalogMigrationResolvedLayoutV1::from_rehearsal_root(&rehearsal, &config)
                .unwrap();

        // No git_meta directory at all: a no-op copy and a None hash.
        assert!(git_meta_backup_copy_if_needed(&layout).is_ok());
        assert_eq!(git_meta_backup_hash(&layout).unwrap(), None);

        fs::create_dir_all(&layout.git_meta_root).unwrap();
        fs::write(layout.git_meta_root.join("project-a"), b"sha-one").unwrap();
        fs::write(layout.git_meta_root.join("project-b"), b"sha-two").unwrap();

        // Simulate a crash mid-copy: a leftover staging directory holding
        // only a subset of files (and, for project-a, stale content a real
        // copy would never have written). The partial staging directory
        // must never be visible as a backup: the hash reader only looks at
        // the final `git_meta` name, which does not exist yet.
        let staging_dir = layout.catalog_backup_dir.join("git_meta.tmp");
        fs::create_dir_all(&staging_dir).unwrap();
        fs::write(staging_dir.join("project-a"), b"stale-partial-content").unwrap();
        let backup_dir = layout.catalog_backup_dir.join("git_meta");
        assert!(!backup_dir.exists(), "no backup exists yet");
        assert_eq!(
            git_meta_backup_hash(&layout).unwrap(),
            None,
            "a partial temp dir must never be read as the hashed backup"
        );

        git_meta_backup_copy_if_needed(&layout).unwrap();
        assert!(
            !staging_dir.exists(),
            "the staging directory is consumed by the atomic rename"
        );
        assert_eq!(
            fs::read(backup_dir.join("project-a")).unwrap(),
            b"sha-one",
            "the retry discards the stale partial content and writes a fresh, complete copy"
        );
        assert_eq!(fs::read(backup_dir.join("project-b")).unwrap(), b"sha-two");
        let first_hash = git_meta_backup_hash(&layout).unwrap().unwrap();

        // Live drift after the backup was made must not change the
        // read-only hash: it hashes the STORED backup, not the live
        // directory.
        fs::write(layout.git_meta_root.join("project-c"), b"sha-three").unwrap();
        assert_eq!(git_meta_backup_hash(&layout).unwrap().unwrap(), first_hash);

        // A second copy call is a no-op: the backup already exists, so the
        // live drift is never copied in.
        git_meta_backup_copy_if_needed(&layout).unwrap();
        assert!(!backup_dir.join("project-c").exists());
        assert_eq!(git_meta_backup_hash(&layout).unwrap().unwrap(), first_hash);
    }

    #[test]
    fn rehearsal_runtime_authority_cannot_escape_declared_root() {
        let dir = tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let config = test_config(&root);
        let rehearsal = root.join("rehearsal");
        let outside = root.join("outside");
        fs::create_dir_all(rehearsal.join("state")).unwrap();
        fs::create_dir_all(&outside).unwrap();
        let layout =
            ProjectCatalogMigrationResolvedLayoutV1::from_rehearsal_root(&rehearsal, &config)
                .unwrap();
        assert_eq!(
            validate_rehearsal_runtime_authorities(&layout, std::slice::from_ref(&outside))
                .unwrap_err()
                .code,
            "error.project_catalog_migration_unsafe_layout"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::symlink;

            let checkout = rehearsal.join("checkouts/swap");
            let held = rehearsal.join("checkouts/held");
            fs::create_dir_all(&checkout).unwrap();
            fs::rename(&checkout, &held).unwrap();
            symlink(&outside, &checkout).unwrap();
            assert_eq!(
                validate_rehearsal_runtime_authorities(&layout, std::slice::from_ref(&checkout),)
                    .unwrap_err()
                    .code,
                "error.project_catalog_migration_unsafe_layout"
            );
        }
    }

    #[test]
    fn state_override_reroots_publisher_refs_and_projects_override_wins() {
        let dir = tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let config = test_config(&root);
        let state = root.join("other-state");
        let projects = state.join("custom-projects.json");
        let layout = ProjectCatalogMigrationResolvedLayoutV1::from_config(
            &config,
            ProjectCatalogMigrationLayoutOverridesV1 {
                state_dir: Some(state.clone()),
                projects_path: Some(projects.clone()),
            },
        )
        .unwrap();
        assert_eq!(layout.projects_path, projects);
        assert_eq!(
            layout.publisher_refs_path,
            state.join("bro").join("publisher-refs.json")
        );
    }

    #[test]
    fn exact_artifact_io_rejects_empty_and_symlinked_files() {
        let dir = tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let artifact = root.join("review").join("report.json");
        write_artifact_atomic(&artifact, b"{\"ok\":true}\n", 1024, "report").unwrap();
        assert_eq!(
            read_artifact_required(&artifact, 1024, "report").unwrap(),
            b"{\"ok\":true}\n"
        );
        fs::write(&artifact, b"").unwrap();
        assert_eq!(
            read_artifact_required(&artifact, 1024, "report")
                .unwrap_err()
                .code,
            "error.project_catalog_migration_artifact_empty"
        );

        #[cfg(unix)]
        {
            use std::os::unix::fs::symlink;
            fs::remove_file(&artifact).unwrap();
            let target = root.join("target");
            fs::write(&target, b"private").unwrap();
            symlink(&target, &artifact).unwrap();
            assert_eq!(
                read_artifact_required(&artifact, 1024, "report")
                    .unwrap_err()
                    .code,
                "error.project_catalog_migration_artifact_io"
            );
        }
    }

    #[test]
    fn production_identity_planner_reuses_every_reported_random_identity() {
        let inventory = crate::project_catalog_inventory::tests::fixture_inventory();
        let resolution =
            ProjectCatalogMigrationResolutionV1::empty(inventory.inventory_hash().unwrap());
        let post_image = crate::project_catalog_inventory::tests::fixture_post_image(&inventory);
        let report = crate::project_catalog_inventory::tests::fixture_report(
            &inventory,
            &resolution,
            &post_image,
        );
        let retained_attachment_ids = post_image
            .attachments
            .iter()
            .map(|row| row.attachment_id.clone())
            .collect::<BTreeSet<_>>();

        let first = build_persisted_identity_plan(
            &inventory,
            &post_image.resolved_project_scopes,
            &retained_attachment_ids,
            Some(&report),
        )
        .unwrap();
        let second = build_persisted_identity_plan(
            &inventory,
            &post_image.resolved_project_scopes,
            &retained_attachment_ids,
            Some(&report),
        )
        .unwrap();

        assert_eq!(first, second);
        assert_eq!(first.transaction_id(), &report.transaction_id);
        assert_eq!(first.repo_history_groups, report.repo_history_groups);
        assert_eq!(
            first.checkout_identity_actions,
            report.checkout_identity_actions
        );
        assert_eq!(
            first.legacy_path_binding_ids,
            report
                .legacy_path_bindings
                .iter()
                .map(|row| { (row.observation_id.clone(), row.planned_binding_id.clone(),) })
                .collect()
        );
        assert_eq!(
            first.attachment_ids,
            report
                .attachments
                .iter()
                .map(|row| (row.observation_id.clone(), row.attachment_id.clone()))
                .collect()
        );
    }

    #[test]
    fn production_semantic_planner_returns_refused_for_unsafe_checkout_evidence() {
        let mut inventory = crate::project_catalog_inventory::tests::fixture_inventory();
        inventory.checkouts[0].marker_state =
            crate::project_catalog_inventory::CheckoutMarkerStateV1::Malformed {
                diagnostic_code: "invalid_checkout_marker".to_string(),
            };
        let resolution =
            ProjectCatalogMigrationResolutionV1::empty(inventory.inventory_hash().unwrap());

        let assessment = assess_migration_semantics(&inventory, &resolution).unwrap();

        assert_eq!(
            assessment.status(),
            ProjectCatalogMigrationStatusV1::Refused
        );
        assert_eq!(assessment.refusals.len(), 1);
        assert_eq!(
            assessment.refusals[0],
            semantic_refusal(
                "unsafe_checkout_marker",
                [inventory.checkouts[0].observation_id.clone()],
            )
        );
    }

    #[test]
    fn late_domain_refusal_is_assessment_state_but_internal_binding_failure_is_error() {
        let inventory = crate::project_catalog_inventory::tests::fixture_inventory();
        let resolution =
            ProjectCatalogMigrationResolutionV1::empty(inventory.inventory_hash().unwrap());
        let mut assessment = assess_migration_semantics(&inventory, &resolution).unwrap();
        assert_eq!(assessment.status(), ProjectCatalogMigrationStatusV1::Clean);
        let identities = build_persisted_identity_plan(
            &inventory,
            &assessment.resolved_project_scopes,
            &assessment.retained_attachment_ids,
            None,
        )
        .unwrap();
        let runtime = MigrationRuntimeBindingsViewV1 {
            legacy_project_store_bytes: Vec::new(),
            legacy_project_store_was_missing: false,
            legacy_project_paths: BTreeMap::from([
                (
                    "legacy_alpha".to_string(),
                    PathBuf::from("/workspace/acme/alpha"),
                ),
                (
                    "legacy_beta".to_string(),
                    PathBuf::from("/workspace/acme/beta"),
                ),
            ]),
            checkout_paths: BTreeMap::from([(
                "checkout_acme".to_string(),
                PathBuf::from("/workspace/acme"),
            )]),
            checkout_repositories: BTreeMap::new(),
            legacy_selectors: BTreeMap::from([(
                "legacy_path_alpha".to_string(),
                "/workspace/acme/alpha/src/Example.java".to_string(),
            )]),
        };

        let classified = classify_legacy_paths(&inventory, &runtime, &identities).unwrap();
        let refusal = build_base_post_images(
            &inventory,
            &runtime,
            &assessment,
            &identities,
            &resolution,
            &classified,
        )
        .unwrap_err();
        assert!(matches!(
            refusal,
            MigrationBasePostImagesFailureV1::Refused(
                LateMigrationDomainRefusalV1::MissingBaseAttachment { .. }
            )
        ));
        let MigrationBasePostImagesFailureV1::Refused(refusal) = refusal else {
            unreachable!("matched refusal")
        };
        assessment.refusals.push(late_domain_refusal_row(refusal));
        canonicalize_refusals(&mut assessment.refusals);
        let prepared = prepare_assessment_only(
            &inventory,
            &runtime,
            &encode_migration_resolution_v1(&resolution).unwrap(),
            &assessment,
            &identities,
            false,
        )
        .unwrap();
        assert_eq!(
            prepared.preflight.receipt.status,
            ProjectCatalogMigrationStatusV1::Refused
        );
        assert!(prepared.plan.is_none());

        let mut incomplete_runtime = runtime;
        incomplete_runtime.checkout_paths.clear();
        let failure = build_base_post_images(
            &inventory,
            &incomplete_runtime,
            &assessment,
            &identities,
            &resolution,
            &classified,
        )
        .unwrap_err();
        assert!(matches!(
            failure,
            MigrationBasePostImagesFailureV1::Error(error)
                if error.code == "error.project_catalog_migration_planner"
        ));

        let wrong_resolution =
            ProjectCatalogMigrationResolutionV1::empty(Sha256ValueV1::digest(b"wrong inventory"));
        assert!(assess_migration_semantics(&inventory, &wrong_resolution).is_err());
    }

    #[test]
    fn canonical_legacy_path_classification_is_reused_by_assessment_and_executable_paths() {
        let mut inventory = crate::project_catalog_inventory::tests::fixture_inventory();
        inventory.legacy_path_observations[0].selector_digest =
            digest_path("/workspace/acme/services/alpha/src/Example.java");
        let resolution =
            ProjectCatalogMigrationResolutionV1::empty(inventory.inventory_hash().unwrap());
        let assessment = assess_migration_semantics(&inventory, &resolution).unwrap();
        let identities = build_persisted_identity_plan(
            &inventory,
            &assessment.resolved_project_scopes,
            &assessment.retained_attachment_ids,
            None,
        )
        .unwrap();
        let mut runtime = MigrationRuntimeBindingsViewV1 {
            legacy_project_store_bytes: Vec::new(),
            legacy_project_store_was_missing: false,
            legacy_project_paths: BTreeMap::from([
                (
                    "legacy_alpha".to_string(),
                    PathBuf::from("/workspace/acme/services/alpha"),
                ),
                (
                    "legacy_beta".to_string(),
                    PathBuf::from("/workspace/acme/services/beta"),
                ),
            ]),
            checkout_paths: BTreeMap::from([(
                "checkout_acme".to_string(),
                PathBuf::from("/workspace/acme"),
            )]),
            checkout_repositories: BTreeMap::new(),
            legacy_selectors: BTreeMap::from([(
                "legacy_path_alpha".to_string(),
                "/workspace/acme/services/alpha/src/Example.java".to_string(),
            )]),
        };

        let contained = classify_legacy_paths(&inventory, &runtime, &identities).unwrap();
        assert_eq!(
            contained.report_rows[0].relationship,
            LegacyPathRelationshipV1::Contained
        );
        let base = build_base_post_images(
            &inventory,
            &runtime,
            &assessment,
            &identities,
            &resolution,
            &contained,
        )
        .unwrap();
        assert_eq!(base.legacy_binding_report, contained.report_rows);

        let mut refused_assessment = assessment.clone();
        refused_assessment.refusals.push(semantic_refusal(
            "test_semantic_refusal",
            ["legacy_alpha".to_string()],
        ));
        let prepared = prepare_assessment_only(
            &inventory,
            &runtime,
            &encode_migration_resolution_v1(&resolution).unwrap(),
            &refused_assessment,
            &identities,
            false,
        )
        .unwrap();
        let report = decode_migration_report_v1(&prepared.preflight.report_bytes).unwrap();
        assert_eq!(report.legacy_path_bindings, contained.report_rows);

        runtime.legacy_selectors.insert(
            "legacy_path_alpha".to_string(),
            "/workspace/acme/services/alpha".to_string(),
        );
        let mut exact_inventory = inventory.clone();
        exact_inventory.legacy_path_observations[0].selector_digest =
            digest_path("/workspace/acme/services/alpha");
        assert_eq!(
            classify_legacy_paths(&exact_inventory, &runtime, &identities)
                .unwrap()
                .report_rows[0]
                .relationship,
            LegacyPathRelationshipV1::ExactRoot
        );
        runtime.legacy_selectors.insert(
            "legacy_path_alpha".to_string(),
            "/outside/unscoped".to_string(),
        );
        let mut unscoped_inventory = inventory.clone();
        unscoped_inventory.legacy_path_observations[0].selector_digest =
            digest_path("/outside/unscoped");
        let unscoped = classify_legacy_paths(&unscoped_inventory, &runtime, &identities).unwrap();
        assert_eq!(
            unscoped.report_rows[0].relationship,
            LegacyPathRelationshipV1::Unscoped
        );
        assert_eq!(
            unscoped
                .unscoped_counts
                .get(&crate::project_catalog_inventory::LegacyPathStoreKindV1::Knowledge),
            Some(&1)
        );

        runtime.legacy_selectors.insert(
            "legacy_path_alpha".to_string(),
            "/workspace/acme/services/alpha/src/Example.java".to_string(),
        );
        let mut missing_inventory = inventory.clone();
        missing_inventory.legacy_projects[0].path_status =
            crate::project_catalog_inventory::LegacyProjectPathStatusV1::Missing;
        let missing = classify_legacy_paths(&missing_inventory, &runtime, &identities).unwrap();
        assert_eq!(
            missing.report_rows[0].relationship,
            LegacyPathRelationshipV1::MissingProject
        );
        assert_eq!(missing.refusals.len(), 1);

        runtime.legacy_project_paths.insert(
            "legacy_beta".to_string(),
            PathBuf::from("/workspace/acme/services/alpha"),
        );
        let ambiguous = classify_legacy_paths(&inventory, &runtime, &identities).unwrap();
        assert_eq!(
            ambiguous.report_rows[0].relationship,
            LegacyPathRelationshipV1::Ambiguous
        );
        assert_eq!(ambiguous.refusals.len(), 1);

        runtime.legacy_selectors.insert(
            "legacy_path_alpha".to_string(),
            "/workspace/acme/services/alpha/../beta".to_string(),
        );
        let mut unsafe_inventory = inventory.clone();
        unsafe_inventory.legacy_path_observations[0].selector_digest =
            digest_path("/workspace/acme/services/alpha/../beta");
        let unsafe_selector =
            classify_legacy_paths(&unsafe_inventory, &runtime, &identities).unwrap();
        assert_eq!(
            unsafe_selector.report_rows[0].relationship,
            LegacyPathRelationshipV1::UnsafeSelector
        );
        assert_eq!(unsafe_selector.refusals.len(), 1);
    }

    fn verification_validation_fixture() -> (
        ProjectCatalogMigrationReportV1,
        Vec<u8>,
        Vec<u8>,
        MigrationVerificationReceiptV1,
    ) {
        let inventory = crate::project_catalog_inventory::tests::fixture_inventory();
        let resolution =
            ProjectCatalogMigrationResolutionV1::empty(inventory.inventory_hash().unwrap());
        let post_image = crate::project_catalog_inventory::tests::fixture_post_image(&inventory);
        let report = crate::project_catalog_inventory::tests::fixture_report(
            &inventory,
            &resolution,
            &post_image,
        );
        let report_bytes = encode_migration_report_v1(&report, &inventory).unwrap();
        let resolution_bytes = encode_migration_resolution_v1(&resolution).unwrap();
        let marker_hash = Sha256ValueV1::digest(b"marker");
        let receipt = MigrationVerificationReceiptV1 {
            version: FACADE_VERSION_V1,
            transaction_id: report.transaction_id.clone(),
            inventory_hash: report.inventory_hash.clone(),
            plan_hash: report.plan_hash.clone(),
            report_artifact_hash: Sha256ValueV1::digest(&report_bytes),
            resolution_artifact_hash: Sha256ValueV1::digest(&resolution_bytes),
            expected_catalog_hash: report.predicted_catalog_hash.clone(),
            observed_catalog_hash: report.predicted_catalog_hash.clone(),
            expected_attachment_hash: report.predicted_attachment_hash.clone(),
            observed_attachment_hash: report.predicted_attachment_hash.clone(),
            expected_participant_hashes: report.predicted_participant_hashes.clone(),
            observed_participant_hashes: report.predicted_participant_hashes.clone(),
            expected_immutable_asset_hashes: report.predicted_immutable_asset_hashes.clone(),
            observed_immutable_asset_hashes: report.predicted_immutable_asset_hashes.clone(),
            predicted_marker_hash: marker_hash.clone(),
            observed_marker_hash: marker_hash,
            backup_hashes: BTreeMap::new(),
            epoch: 1,
            checkout_action_count: 0,
            publisher_pin_count: 0,
            quarantine_root_count: 0,
            attached_project_count: 0,
            omitted_catalog_count: 0,
        };
        (report, report_bytes, resolution_bytes, receipt)
    }

    #[test]
    fn post_open_verify_mismatch_inherits_recovery_disposition() {
        let (_, _, _, mut receipt) = verification_validation_fixture();
        receipt.version = 0;
        let result = ProjectCatalogMigrationVerifyResultV1 {
            receipt,
            compatibility: ProjectCatalogCompatibilityProjectionV1 {
                records: Vec::new(),
                omitted_catalog_count: 0,
            },
            mutation_disposition: ProjectCatalogMigrationMutationDispositionV1::RecoveredToOldState,
        };

        let error = validate_verify_result(&result).unwrap_err();

        assert_eq!(
            error.mutation_disposition,
            ProjectCatalogMigrationMutationDispositionV1::RecoveredToOldState
        );
    }

    #[test]
    fn exact_artifact_mismatch_inherits_recovered_committed_disposition() {
        let (report, _, resolution_bytes, receipt) = verification_validation_fixture();
        let result = ProjectCatalogMigrationVerifyResultV1 {
            receipt,
            compatibility: ProjectCatalogCompatibilityProjectionV1 {
                records: Vec::new(),
                omitted_catalog_count: 0,
            },
            mutation_disposition:
                ProjectCatalogMigrationMutationDispositionV1::RecoveredToCommittedState,
        };

        let error =
            validate_exact_installed_review(&result, b"different", &report, &resolution_bytes)
                .unwrap_err();

        assert_eq!(
            error.mutation_disposition,
            ProjectCatalogMigrationMutationDispositionV1::RecoveredToCommittedState
        );
    }

    #[test]
    fn post_commit_verification_failure_forces_committed_disposition() {
        let error = post_commit_verification_error(ProjectCatalogMigrationError::new(
            "error.test_recovery_uncertain",
            "test",
            ProjectCatalogMigrationMutationDispositionV1::RetryExactPlanRequired,
        ));

        assert_eq!(
            error.mutation_disposition,
            ProjectCatalogMigrationMutationDispositionV1::RecoveredToCommittedState
        );
    }

    #[test]
    fn post_commit_apply_mismatch_reports_committed_disposition() {
        let (report, report_bytes, resolution_bytes, mut verification) =
            verification_validation_fixture();
        verification.version = 0;
        let result = ProjectCatalogMigrationApplyResultV1 {
            receipt: ProjectCatalogMigrationApplyReceiptV1 {
                version: FACADE_VERSION_V1,
                outcome: ProjectCatalogMigrationApplyOutcomeV1::Applied,
                verification,
            },
        };

        let error =
            validate_apply_result(&result, &report_bytes, &report, &resolution_bytes).unwrap_err();

        assert_eq!(
            error.mutation_disposition,
            ProjectCatalogMigrationMutationDispositionV1::RecoveredToCommittedState
        );
    }

    #[cfg(unix)]
    #[test]
    fn publisher_generation_uses_captured_repository_after_checkout_swap() {
        use std::os::unix::fs::symlink;

        let directory = tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let rehearsal = root.join("rehearsal");
        let checkout = rehearsal.join("checkout");
        let protected = root.join("protected");
        fs::create_dir_all(&checkout).unwrap();
        fs::create_dir_all(&protected).unwrap();
        for repository in [&checkout, &protected] {
            run_git(repository, &["init", "-q"]);
            run_git(repository, &["config", "user.email", "test@example.com"]);
            run_git(repository, &["config", "user.name", "Test"]);
        }
        fs::create_dir_all(checkout.join("services/alpha/.bbox")).unwrap();
        fs::write(
            checkout.join("services/alpha/.bbox/config.toml"),
            "[project]\nrepo_id = \"acme_repo\"\n",
        )
        .unwrap();
        fs::write(checkout.join("tracked.txt"), "inside\n").unwrap();
        run_git(
            &checkout,
            &["add", "tracked.txt", "services/alpha/.bbox/config.toml"],
        );
        run_git(&checkout, &["commit", "-qm", "inside"]);
        let accepted_commit = run_git(&checkout, &["rev-parse", "HEAD"]);
        fs::write(protected.join("tracked.txt"), "outside-sentinel\n").unwrap();
        run_git(&protected, &["add", "tracked.txt"]);
        run_git(&protected, &["commit", "-qm", "outside"]);

        let checkout_authority = NofollowDirectory::open_existing(&checkout)
            .unwrap()
            .unwrap();
        let repository = bbox_corpus_core::git::open_stable_git_repository(&checkout_authority)
            .unwrap()
            .unwrap();
        let held = rehearsal.join("held-checkout");
        fs::rename(&checkout, &held).unwrap();
        symlink(&protected, &checkout).unwrap();

        let inventory = crate::project_catalog_inventory::tests::fixture_inventory();
        let attachment = inventory
            .attachment_candidates
            .iter()
            .find(|row| row.observation_id == "attachment_alpha")
            .unwrap();
        let runtime = MigrationRuntimeBindingsViewV1 {
            legacy_project_store_bytes: Vec::new(),
            legacy_project_store_was_missing: false,
            legacy_project_paths: BTreeMap::new(),
            checkout_paths: BTreeMap::from([(
                attachment.checkout_observation_id.clone(),
                checkout,
            )]),
            checkout_repositories: BTreeMap::from([(
                attachment.checkout_observation_id.clone(),
                repository,
            )]),
            legacy_selectors: BTreeMap::new(),
        };
        prepare_publisher_generation(
            &inventory,
            &runtime,
            &attachment.project_id,
            &attachment.attachment_id,
            attachment.observed_scope.as_ref().unwrap(),
            "refs/heads/main",
            &accepted_commit,
        )
        .unwrap();
    }
}
