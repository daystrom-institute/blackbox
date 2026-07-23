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

use bbox_code_source_store::StoreLimits;
use bbox_config::config::Config;
use bbox_corpus_core::json_store::{NofollowDirectory, canonical_store_lock_path};
use bbox_corpus_core::project_catalog::{
    AttachmentKind, AttachmentStatus, CatalogOriginV2, ProjectCatalogTransactionId,
    validate_catalog_attachments,
};
use bbox_corpus_core::project_record::ProjectRecord;
use serde::Serialize;

use crate::project_catalog_inventory::{
    MAX_PROJECT_CATALOG_REPORT_BYTES, MAX_PROJECT_CATALOG_RESOLUTION_BYTES,
    ProjectCatalogMigrationReportV1, ProjectCatalogMigrationResolutionV1,
    ProjectCatalogMigrationStatusV1, Sha256ValueV1, decode_migration_report_v1,
    decode_migration_resolution_v1,
};
use crate::project_catalog_migration_lock::project_catalog_migration_lock_path;

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
    fn new(
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

    fn no_mutation(code: &'static str, message: impl Into<String>) -> Self {
        Self::new(
            code,
            message,
            ProjectCatalogMigrationMutationDispositionV1::NoDurableMutation,
        )
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
                    vector_root: config.paths.state_dir.join("vectors"),
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
            provenance_notes_ref: format!("refs/notes/{notes_namespace}"),
            bro_home,
            state_dir,
            store_limits,
        })
    }

    fn validate(&self) -> Result<(), ProjectCatalogMigrationError> {
        for path in self.all_paths() {
            validate_absolute_path(path)?;
        }
        validated_notes_ref(
            self.provenance_notes_ref
                .strip_prefix("refs/notes/")
                .unwrap_or_default(),
        )?;
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
        let catalog_backup_dir = parent.join("project-catalog-backups");
        Ok(Self {
            attachments_path: parent.join("project-attachments.json"),
            transaction_journal_path: parent.join("project-catalog-transaction.json"),
            migration_marker_path: parent.join("project-catalog-migration.json"),
            transaction_stage_dir: parent.join("project-catalog-stage"),
            catalog_immutable_root: catalog_backup_dir.join("immutable"),
            catalog_backup_dir,
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
        || namespace.contains("..")
        || namespace
            .bytes()
            .any(|byte| byte.is_ascii_control() || byte.is_ascii_whitespace())
    {
        return Err(unsafe_layout("provenance notes namespace is invalid"));
    }
    Ok(format!("refs/notes/{namespace}"))
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
        let prepared = self.integration.prepare_preflight(
            &request.layout,
            existing_resolution.as_deref(),
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
        if report.status != ProjectCatalogMigrationStatusV1::Clean {
            return Err(ProjectCatalogMigrationError::no_mutation(
                "error.project_catalog_migration_report_not_clean",
                "rehearsal apply requires a clean report",
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

/// Typed, crate-internal connection point for strict owner snapshots.
///
/// This staging implementation must remain fail-closed. In particular it must
/// not call `CodeSourceStore::open`, which creates missing directories, and it
/// must not replace the closed ten-lane census with empty observations.
struct CurrentClosedMigrationIntegrationV1;

impl ClosedMigrationIntegrationV1 for CurrentClosedMigrationIntegrationV1 {
    fn prepare_preflight(
        &self,
        _layout: &ProjectCatalogMigrationResolvedLayoutV1,
        _existing_resolution: Option<&[u8]>,
        _include_sensitive_paths: bool,
    ) -> Result<PreparedPreflightV1, ProjectCatalogMigrationError> {
        Err(ProjectCatalogMigrationIntegrationBlockerV1::OwnerLaneSnapshots.into_error())
    }

    fn apply_rehearsal(
        &self,
        _layout: &ProjectCatalogMigrationResolvedLayoutV1,
        _report_bytes: &[u8],
        _report: &ProjectCatalogMigrationReportV1,
        _resolution_bytes: &[u8],
        _resolution: &ProjectCatalogMigrationResolutionV1,
    ) -> Result<ProjectCatalogMigrationApplyResultV1, ProjectCatalogMigrationError> {
        Err(ProjectCatalogMigrationIntegrationBlockerV1::TransactionAssembly.into_error())
    }

    fn verify(
        &self,
        _layout: &ProjectCatalogMigrationResolvedLayoutV1,
    ) -> Result<ProjectCatalogMigrationVerifyResultV1, ProjectCatalogMigrationError> {
        Err(ProjectCatalogMigrationIntegrationBlockerV1::VerificationBootstrap.into_error())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ProjectCatalogMigrationIntegrationBlockerV1 {
    OwnerLaneSnapshots,
    TransactionAssembly,
    VerificationBootstrap,
}

impl ProjectCatalogMigrationIntegrationBlockerV1 {
    fn into_error(self) -> ProjectCatalogMigrationError {
        let message = match self {
            Self::OwnerLaneSnapshots => {
                "one or more required migration owners lack a strict no-create snapshot adapter"
            }
            Self::TransactionAssembly => {
                "closed transaction assembly does not yet bind exact reviewed artifacts"
            }
            Self::VerificationBootstrap => {
                "fresh verification bootstrap is unavailable for one or more owners"
            }
        };
        ProjectCatalogMigrationError::no_mutation(
            "error.project_catalog_migration_owner_adapter_missing",
            message,
        )
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
        || prepared.receipt.required_resolution_count
            != u64::try_from(report.required_resolutions.len()).unwrap_or(u64::MAX)
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
        return Err(ProjectCatalogMigrationError::no_mutation(
            "error.project_catalog_migration_invalid_verify_output",
            "closed verification returned inconsistent installed state",
        ));
    }
    Ok(())
}

fn verification_receipt_observations_match(receipt: &MigrationVerificationReceiptV1) -> bool {
    receipt.expected_catalog_hash == receipt.observed_catalog_hash
        && receipt.expected_attachment_hash == receipt.observed_attachment_hash
        && receipt.expected_participant_hashes == receipt.observed_participant_hashes
        && receipt.expected_immutable_asset_hashes == receipt.observed_immutable_asset_hashes
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

    use bbox_config::config;
    use tempfile::tempdir;

    use super::*;

    fn test_config(root: &Path) -> Config {
        let _guard = bbox_util::util::test_env_lock();
        let config_path = root.join("config.toml");
        fs::write(
            &config_path,
            format!("[paths]\nstate_dir = {:?}\n", root.join("live")),
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
        assert!(
            layout
                .all_paths()
                .into_iter()
                .all(|path| path.starts_with(&rehearsal))
        );
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
    fn current_public_preflight_fails_before_writing_when_owner_is_missing() {
        let dir = tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let config = test_config(&root);
        let layout = ProjectCatalogMigrationResolvedLayoutV1::from_rehearsal_root(
            root.join("rehearsal"),
            &config,
        )
        .unwrap();
        let report = root.join("review").join("report.json");
        let resolution = root.join("review").join("resolution.json");
        let error =
            ProjectCatalogMigrationFacadeV1::preflight(ProjectCatalogMigrationPreflightRequestV1 {
                layout,
                report_path: report.clone(),
                resolution_path: resolution.clone(),
                sensitive_report_path: None,
            })
            .unwrap_err();
        assert_eq!(
            error.code,
            "error.project_catalog_migration_owner_adapter_missing"
        );
        assert_eq!(
            error.mutation_disposition,
            ProjectCatalogMigrationMutationDispositionV1::NoDurableMutation
        );
        assert!(!report.exists());
        assert!(!resolution.exists());
    }
}
