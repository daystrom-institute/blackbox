//! Offline Git transport cutover preflight, apply, and verification.
//!
//! Preflight captures catalog, transport, P3, overlay, provenance, and
//! checkout-observation evidence into a reviewable artifact pair. Apply binds
//! those exact bytes to the current predecessor marker and installs one
//! checksummed, atomically selected marker. Runtime readers classify each row
//! independently, so one stale repository never invalidates another and a
//! repository that was ever covered can never silently regain checkout
//! fallback by disappearing from a later cutover report.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use bbox_code_source_store::{ActivationRecordV2, CodeSourceStorePaths, StoredGenerationV2};
use bbox_config::config::Config;
use bbox_corpus_core::entity_ref::EntityRef;
use bbox_corpus_core::git_overlay::GitOverlaySelector;
use bbox_corpus_core::git_transport_cutover::{
    RepoTransportBlockedReason, RepoTransportGrant, RepoTransportGrantProjection,
    RepoTransportGrantState, derive_repo_transport_grants,
};
use bbox_corpus_core::identity::PublishedScope;
use bbox_corpus_core::json_store::{
    NofollowDirectory, acquire_store_lock_nofollow, atomic_write_bytes_locked,
};
use bbox_corpus_core::project_catalog::{
    CatalogOriginV2, ProjectId, ProjectScope, RepoHistoryAuthority, RepoHistoryId,
    RepoHistoryMaterialization,
};
use bbox_corpus_index::index::history_generations::{
    HistoryGenerationRecordV1, HistoryGenerationStore, generations_root_for_index,
};
use bbox_edge_sidecar::edge_sidecar::{
    edge_import_key, explicit_edge_lane_version, visit_explicit_edge_lane,
};
use bbox_edge_sidecar::manifest::ManifestIndex;
use bbox_git_source::GitSourceLimits;
use bbox_git_source_store::{
    GitSourceStore, HistoryActivationJournalV1, HistoryActivationStageV1,
    ProvenanceImportJournalV1, ProvenanceImportStageV1, StoreLimits,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::checkout_access::{
    CheckoutAccessCounter, CheckoutAccessKind, CheckoutAccessObservations,
    CheckoutAccessTargetCounter,
};
use crate::project_catalog_inventory::{
    MAX_PROJECT_CATALOG_REPORT_BYTES, MAX_PROJECT_CATALOG_RESOLUTION_BYTES, Sha256ValueV1,
};
use crate::project_catalog_migration::{
    ProjectCatalogMigrationResolvedLayoutV1, read_artifact_optional, read_artifact_required,
    validate_artifact_set, write_artifact_if_absent, write_artifact_replacing,
};
use crate::project_catalog_store::ProjectCatalogStore;

const REPORT_VERSION: u32 = 1;
const RESOLUTION_VERSION: u32 = 1;
const MARKER_VERSION: u32 = 1;
const RECEIPT_VERSION: u32 = 1;
const MAX_EXPLICIT_EDGE_LINE_BYTES: usize = 16 * 1024 * 1024;
const MAX_ACTIVE_SIDECAR_INPUT_BYTES: u64 = 4 * 1024 * 1024 * 1024;
pub const MAX_GIT_TRANSPORT_CUTOVER_MARKER_BYTES: usize = 16 * 1024 * 1024;
pub const MAX_GIT_TRANSPORT_CUTOVER_RECEIPT_BYTES: usize = 1024 * 1024;
pub const GIT_TRANSPORT_CUTOVER_MARKER_FILE: &str = "git-transport-cutover-marker.json";
pub const GIT_TRANSPORT_CUTOVER_RECEIPT_FILE: &str = "git-transport-cutover-receipt.json";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GitTransportCutoverStatusV1 {
    Clean,
    Refused,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GitTransportCutoverCoverageStatusV1 {
    Proposed,
    CarriedForwardCurrent,
    BlockedPublishedNeverCovered,
    CoveredProducerRemoved,
    CoveredBlockedPendingRecutover,
    CoverageStalePendingRecutover,
    Refused,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GitTransportParityStatusV1 {
    Equal,
    VacuousFreshV2,
    Missing,
    Mismatch,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GitTransportObservationCategoryV1 {
    OverlapWindow,
    HistoryTransportCurrentPreCutover,
    TransportCoveredPostBoundary,
    BlockedPublishedNeverCovered,
    LegacyLocalLocalProject,
    LegacyLocalLegacyNamespace,
    CoveredProducerRemoved,
    CoveredBlockedPendingRecutover,
    CoverageStalePendingRecutover,
}

/// Runtime authority state for one Published repository.
///
/// `Uncovered` is the only state that permits the pre-cutover attachment
/// adapter. Every other variant proves that a durable marker row exists and
/// therefore closes checkout fallback even when current producer authority is
/// unavailable.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GitTransportRuntimeCoverageV1 {
    Uncovered,
    Current,
    CoveredProducerRemoved,
    CoveredBlockedPendingRecutover,
    CoverageStalePendingRecutover,
}

impl GitTransportRuntimeCoverageV1 {
    pub fn transport_governed(self) -> bool {
        !matches!(self, Self::Uncovered)
    }

    pub fn current(self) -> bool {
        matches!(self, Self::Current)
    }

    pub fn observation_category(self) -> Option<GitTransportObservationCategoryV1> {
        match self {
            Self::Uncovered => None,
            Self::Current => Some(GitTransportObservationCategoryV1::TransportCoveredPostBoundary),
            Self::CoveredProducerRemoved => {
                Some(GitTransportObservationCategoryV1::CoveredProducerRemoved)
            }
            Self::CoveredBlockedPendingRecutover => {
                Some(GitTransportObservationCategoryV1::CoveredBlockedPendingRecutover)
            }
            Self::CoverageStalePendingRecutover => {
                Some(GitTransportObservationCategoryV1::CoverageStalePendingRecutover)
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GitTransportCapabilityBaselineV1 {
    pub capability: CheckoutAccessKind,
    pub active_category: GitTransportObservationCategoryV1,
    pub observation_sequence: u64,
    pub overlap_window_granted_baseline: u64,
    pub overlap_window_denied_baseline: u64,
    pub target_granted_baseline: u64,
    pub target_denied_baseline: u64,
}

impl GitTransportParityStatusV1 {
    fn accepted(&self) -> bool {
        matches!(self, Self::Equal | Self::VacuousFreshV2)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GitTransportHistoryEvidenceV1 {
    pub source_generation_id: String,
    pub source_head: String,
    pub p3_generation_id: String,
    pub commit_document_count: u64,
    pub commit_document_commitment_sha256: String,
    pub vector_input_count: u64,
    pub vector_input_commitment_sha256: String,
    pub parity: GitTransportParityStatusV1,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GitTransportProvenanceEvidenceV1 {
    pub import_generation_id: String,
    pub code_selector: String,
    pub notes_ref: String,
    pub notes_tip: String,
    pub manifest_sha256: String,
    pub v1_document_count: u64,
    pub v2_document_count: u64,
    pub explicit_lane_version_token: String,
    pub explicit_lane_sha256: String,
    pub legacy_edge_key_count: u64,
    pub legacy_edge_keys_sha256: String,
    pub typed_edge_key_count: u64,
    pub typed_edge_keys_sha256: String,
    pub imported_edge_key_count: u64,
    pub imported_edge_keys_sha256: String,
    pub typed_matches_import_journal: bool,
    pub typed_covers_legacy: bool,
    pub export_receipt_generation: String,
    pub export_receipt_notes_tip: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GitTransportCodeHeadEvidenceV1 {
    pub generation_id: String,
    pub selector: String,
    pub head_commit: String,
    pub manifest_sha256: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GitTransportProjectEvidenceV1 {
    pub project_id: ProjectId,
    pub scope: PublishedScope,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub code_head: Option<GitTransportCodeHeadEvidenceV1>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub overlay: Option<GitOverlaySelector>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provenance: Option<GitTransportProvenanceEvidenceV1>,
    pub ready: bool,
    pub defects: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GitTransportRepoEvidenceV1 {
    pub repo_history_id: RepoHistoryId,
    pub membership_generation: u64,
    pub coverage_status: GitTransportCutoverCoverageStatusV1,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub blocked_reason: Option<RepoTransportBlockedReason>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub grant: Option<RepoTransportGrant>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub history: Option<GitTransportHistoryEvidenceV1>,
    pub projects: Vec<GitTransportProjectEvidenceV1>,
    pub capability_baselines: Vec<GitTransportCapabilityBaselineV1>,
    pub defects: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GitTransportObservationBaselineV1 {
    pub sequence: u64,
    pub counters: Vec<CheckoutAccessCounter>,
    pub target_counters: Vec<CheckoutAccessTargetCounter>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GitTransportLegacyLocalRepoEvidenceV1 {
    pub repo_history_id: RepoHistoryId,
    pub membership_generation: u64,
    pub authority: RepoHistoryAuthority,
    pub project_ids: Vec<ProjectId>,
    pub capability_baselines: Vec<GitTransportCapabilityBaselineV1>,
    pub valid_authority_shape: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PredictedGitTransportCutoverRowV1 {
    pub repo_history_id: RepoHistoryId,
    pub grant_commitment: String,
    pub membership_generation: u64,
    pub source_generation_id: String,
    pub p3_generation_id: String,
    pub history_parity_commitment: Sha256ValueV1,
    pub provenance_import_generations: BTreeMap<ProjectId, String>,
    pub provenance_export_generations: BTreeMap<ProjectId, String>,
    pub provenance_parity_commitments: BTreeMap<ProjectId, Sha256ValueV1>,
    pub capability_baselines: Vec<GitTransportCapabilityBaselineV1>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PredictedGitTransportCutoverMarkerV1 {
    pub version: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub predecessor_marker_checksum: Option<Sha256ValueV1>,
    pub predecessor_catalog_epoch: u64,
    pub inventory_hash: Sha256ValueV1,
    pub resolution_artifact_hash: Sha256ValueV1,
    pub aggregate_grant_hash: Sha256ValueV1,
    pub zero_prepared_history_journals: bool,
    pub zero_prepared_provenance_journals: bool,
    pub rows: Vec<PredictedGitTransportCutoverRowV1>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GitTransportCutoverReportV1 {
    pub version: u32,
    pub generated_at: String,
    pub status: GitTransportCutoverStatusV1,
    pub catalog_epoch: u64,
    pub catalog_sha256: String,
    pub catalog_origin: CatalogOriginV2,
    pub inventory_hash: Sha256ValueV1,
    pub resolution_artifact_hash: Sha256ValueV1,
    pub observation_baseline: GitTransportObservationBaselineV1,
    pub prepared_history_journal_count: u64,
    pub prepared_provenance_journal_count: u64,
    pub repos: Vec<GitTransportRepoEvidenceV1>,
    pub legacy_local_repos: Vec<GitTransportLegacyLocalRepoEvidenceV1>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub carried_forward_rows: Vec<PredictedGitTransportCutoverRowV1>,
    pub predicted_marker: PredictedGitTransportCutoverMarkerV1,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GitTransportCutoverResolutionV1 {
    pub version: u32,
    pub inventory_hash: Sha256ValueV1,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub blocked_repo_acknowledgements: BTreeMap<RepoHistoryId, String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GitTransportCutoverPreflightReceiptV1 {
    pub version: u32,
    pub status: GitTransportCutoverStatusV1,
    pub catalog_epoch: u64,
    pub inventory_hash: Sha256ValueV1,
    pub report_artifact_hash: Sha256ValueV1,
    pub resolution_artifact_hash: Sha256ValueV1,
    pub proposed_repo_count: u64,
    pub blocked_repo_count: u64,
    pub refused_repo_count: u64,
}

pub struct GitTransportCutoverPreflightRequestV1 {
    pub layout: ProjectCatalogMigrationResolvedLayoutV1,
    pub config: Config,
    pub report_path: PathBuf,
    pub resolution_path: PathBuf,
    pub generated_at: String,
}

pub struct GitTransportCutoverApplyRequestV1 {
    pub layout: ProjectCatalogMigrationResolvedLayoutV1,
    pub config: Config,
    pub report_path: PathBuf,
    pub resolution_path: PathBuf,
    pub applied_at: String,
}

pub struct GitTransportCutoverVerifyRequestV1 {
    pub layout: ProjectCatalogMigrationResolvedLayoutV1,
    pub config: Config,
    pub verified_at: String,
}

/// Atomically selected current cutover artifact.
///
/// The checksum covers every preceding field through a separate canonical
/// body projection. A fixed filename selects the current artifact; atomic
/// replacement means a predecessor can never be mistaken for current.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GitTransportCutoverMarkerV1 {
    pub version: u32,
    pub applied_at: String,
    pub report_artifact_hash: Sha256ValueV1,
    pub resolution_artifact_hash: Sha256ValueV1,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub predecessor_marker_checksum: Option<Sha256ValueV1>,
    pub predecessor_catalog_epoch: u64,
    pub inventory_hash: Sha256ValueV1,
    pub aggregate_grant_hash: Sha256ValueV1,
    pub zero_prepared_history_journals: bool,
    pub zero_prepared_provenance_journals: bool,
    pub rows: Vec<PredictedGitTransportCutoverRowV1>,
    pub checksum_sha256: Sha256ValueV1,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GitTransportCutoverReceiptV1 {
    pub version: u32,
    pub applied_at: String,
    pub verified_at: String,
    pub marker_checksum_sha256: Sha256ValueV1,
    pub report_artifact_hash: Sha256ValueV1,
    pub resolution_artifact_hash: Sha256ValueV1,
    pub covered_repo_count: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GitTransportCutoverVerificationRowV1 {
    pub repo_history_id: RepoHistoryId,
    pub coverage: GitTransportRuntimeCoverageV1,
    pub capability_observations: Vec<GitTransportCapabilityObservationV1>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GitTransportCapabilityObservationV1 {
    pub capability: CheckoutAccessKind,
    pub category: GitTransportObservationCategoryV1,
    pub granted_baseline: u64,
    pub granted_current: u64,
    pub denied_baseline: u64,
    pub denied_current: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GitTransportCutoverVerificationReceiptV1 {
    pub version: u32,
    pub marker_checksum_sha256: Sha256ValueV1,
    pub covered_repo_count: u64,
    pub current_repo_count: u64,
    pub rows: Vec<GitTransportCutoverVerificationRowV1>,
}

/// Strict runtime view of the current marker. Absence is the pre-cutover
/// state; once a row exists it is retained by every later marker.
#[derive(Debug, Clone, Default)]
pub struct GitTransportCutoverRuntimeV1 {
    marker: Option<GitTransportCutoverMarkerV1>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitTransportCutoverError {
    pub code: &'static str,
    pub message: String,
}

impl std::fmt::Display for GitTransportCutoverError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}: {}", self.code, self.message)
    }
}

impl std::error::Error for GitTransportCutoverError {}

type CutoverResult<T> = Result<T, GitTransportCutoverError>;

fn cutover_error(code: &'static str, error: impl std::fmt::Display) -> GitTransportCutoverError {
    GitTransportCutoverError {
        code,
        message: error.to_string(),
    }
}

fn derived_report_status(
    prepared_history_journal_count: u64,
    prepared_provenance_journal_count: u64,
    repos: &[GitTransportRepoEvidenceV1],
    legacy_local_repos: &[GitTransportLegacyLocalRepoEvidenceV1],
) -> GitTransportCutoverStatusV1 {
    if prepared_history_journal_count == 0
        && prepared_provenance_journal_count == 0
        && repos.iter().all(|repo| {
            !matches!(
                repo.coverage_status,
                GitTransportCutoverCoverageStatusV1::Refused
            )
        })
        && legacy_local_repos
            .iter()
            .all(|repo| repo.valid_authority_shape)
    {
        GitTransportCutoverStatusV1::Clean
    } else {
        GitTransportCutoverStatusV1::Refused
    }
}

fn preflight_repo_counts(repos: &[GitTransportRepoEvidenceV1]) -> (u64, u64, u64) {
    let count = |status| {
        repos
            .iter()
            .filter(|repo| repo.coverage_status == status)
            .count() as u64
    };
    (
        count(GitTransportCutoverCoverageStatusV1::Proposed),
        count(GitTransportCutoverCoverageStatusV1::BlockedPublishedNeverCovered),
        count(GitTransportCutoverCoverageStatusV1::Refused),
    )
}

#[allow(clippy::too_many_arguments)]
fn cutover_inventory_hash(
    catalog_epoch: u64,
    catalog_sha256: &str,
    catalog_origin: &CatalogOriginV2,
    observation_baseline: &GitTransportObservationBaselineV1,
    prepared_history_journal_count: u64,
    prepared_provenance_journal_count: u64,
    repos: &[GitTransportRepoEvidenceV1],
    legacy_local_repos: &[GitTransportLegacyLocalRepoEvidenceV1],
    carried_forward_rows: &[PredictedGitTransportCutoverRowV1],
) -> CutoverResult<Sha256ValueV1> {
    #[derive(Serialize)]
    struct Inventory<'a> {
        catalog_epoch: u64,
        catalog_sha256: &'a str,
        catalog_origin: &'a CatalogOriginV2,
        observation_baseline: &'a GitTransportObservationBaselineV1,
        prepared_history_journal_count: u64,
        prepared_provenance_journal_count: u64,
        repos: &'a [GitTransportRepoEvidenceV1],
        legacy_local_repos: &'a [GitTransportLegacyLocalRepoEvidenceV1],
        carried_forward_rows: &'a [PredictedGitTransportCutoverRowV1],
    }
    serde_json::to_vec(&Inventory {
        catalog_epoch,
        catalog_sha256,
        catalog_origin,
        observation_baseline,
        prepared_history_journal_count,
        prepared_provenance_journal_count,
        repos,
        legacy_local_repos,
        carried_forward_rows,
    })
    .map(|bytes| Sha256ValueV1::digest(&bytes))
    .map_err(|error| cutover_error("error.git_transport_cutover_artifact", error))
}

pub fn decode_git_transport_cutover_report_v1(
    bytes: &[u8],
) -> Result<GitTransportCutoverReportV1, GitTransportCutoverError> {
    if bytes.is_empty() || bytes.len() > MAX_PROJECT_CATALOG_REPORT_BYTES {
        return Err(cutover_error(
            "error.git_transport_cutover_artifact",
            "Git transport cutover report is empty or oversized",
        ));
    }
    let report: GitTransportCutoverReportV1 = serde_json::from_slice(bytes)
        .map_err(|error| cutover_error("error.git_transport_cutover_artifact", error))?;
    if report.version != REPORT_VERSION {
        return Err(cutover_error(
            "error.git_transport_cutover_artifact",
            "Git transport cutover report has an unsupported version",
        ));
    }
    let inventory_hash = cutover_inventory_hash(
        report.catalog_epoch,
        &report.catalog_sha256,
        &report.catalog_origin,
        &report.observation_baseline,
        report.prepared_history_journal_count,
        report.prepared_provenance_journal_count,
        &report.repos,
        &report.legacy_local_repos,
        &report.carried_forward_rows,
    )?;
    let predicted = predicted_marker(
        report.predicted_marker.predecessor_marker_checksum.clone(),
        report.catalog_epoch,
        report.inventory_hash.clone(),
        report.resolution_artifact_hash.clone(),
        report.prepared_history_journal_count,
        report.prepared_provenance_journal_count,
        &report.repos,
        &report.carried_forward_rows,
    );
    let status = derived_report_status(
        report.prepared_history_journal_count,
        report.prepared_provenance_journal_count,
        &report.repos,
        &report.legacy_local_repos,
    );
    if inventory_hash != report.inventory_hash
        || predicted != report.predicted_marker
        || status != report.status
    {
        return Err(cutover_error(
            "error.git_transport_cutover_artifact_identity",
            "Git transport cutover report identity does not match its contents",
        ));
    }
    Ok(report)
}

pub fn decode_git_transport_cutover_resolution_v1(
    bytes: &[u8],
) -> Result<GitTransportCutoverResolutionV1, GitTransportCutoverError> {
    if bytes.is_empty() || bytes.len() > MAX_PROJECT_CATALOG_RESOLUTION_BYTES {
        return Err(cutover_error(
            "error.git_transport_cutover_resolution",
            "Git transport cutover resolution is empty or oversized",
        ));
    }
    let resolution: GitTransportCutoverResolutionV1 = serde_json::from_slice(bytes)
        .map_err(|error| cutover_error("error.git_transport_cutover_resolution", error))?;
    if resolution.version != RESOLUTION_VERSION
        || resolution
            .blocked_repo_acknowledgements
            .values()
            .any(|reason| reason.trim().is_empty() || reason.len() > 512)
    {
        return Err(cutover_error(
            "error.git_transport_cutover_resolution",
            "Git transport cutover resolution is invalid",
        ));
    }
    Ok(resolution)
}

pub fn git_transport_cutover_marker_path(state_dir: &Path) -> PathBuf {
    state_dir.join(GIT_TRANSPORT_CUTOVER_MARKER_FILE)
}

pub fn git_transport_cutover_receipt_path(state_dir: &Path) -> PathBuf {
    state_dir.join(GIT_TRANSPORT_CUTOVER_RECEIPT_FILE)
}

fn marker_checksum(marker: &GitTransportCutoverMarkerV1) -> CutoverResult<Sha256ValueV1> {
    #[derive(Serialize)]
    struct Body<'a> {
        version: u32,
        applied_at: &'a str,
        report_artifact_hash: &'a Sha256ValueV1,
        resolution_artifact_hash: &'a Sha256ValueV1,
        predecessor_marker_checksum: &'a Option<Sha256ValueV1>,
        predecessor_catalog_epoch: u64,
        inventory_hash: &'a Sha256ValueV1,
        aggregate_grant_hash: &'a Sha256ValueV1,
        zero_prepared_history_journals: bool,
        zero_prepared_provenance_journals: bool,
        rows: &'a [PredictedGitTransportCutoverRowV1],
    }
    serde_json::to_vec(&Body {
        version: marker.version,
        applied_at: &marker.applied_at,
        report_artifact_hash: &marker.report_artifact_hash,
        resolution_artifact_hash: &marker.resolution_artifact_hash,
        predecessor_marker_checksum: &marker.predecessor_marker_checksum,
        predecessor_catalog_epoch: marker.predecessor_catalog_epoch,
        inventory_hash: &marker.inventory_hash,
        aggregate_grant_hash: &marker.aggregate_grant_hash,
        zero_prepared_history_journals: marker.zero_prepared_history_journals,
        zero_prepared_provenance_journals: marker.zero_prepared_provenance_journals,
        rows: &marker.rows,
    })
    .map(|bytes| Sha256ValueV1::digest(&bytes))
    .map_err(|error| cutover_error("error.git_transport_cutover_marker", error))
}

fn marker_rows_are_canonical(rows: &[PredictedGitTransportCutoverRowV1]) -> bool {
    rows.windows(2)
        .all(|pair| pair[0].repo_history_id < pair[1].repo_history_id)
        && rows.iter().all(|row| {
            let capabilities = row
                .capability_baselines
                .iter()
                .map(|baseline| baseline.capability)
                .collect::<Vec<_>>();
            capabilities
                == vec![
                    CheckoutAccessKind::GitHistory,
                    CheckoutAccessKind::ProvenanceNoteIo,
                ]
                && row.capability_baselines.iter().all(|baseline| {
                    baseline.target_granted_baseline <= baseline.overlap_window_granted_baseline
                        && baseline.target_denied_baseline
                            <= baseline.overlap_window_denied_baseline
                })
                && row
                    .provenance_import_generations
                    .keys()
                    .eq(row.provenance_export_generations.keys())
                && row
                    .provenance_import_generations
                    .keys()
                    .eq(row.provenance_parity_commitments.keys())
        })
}

pub fn decode_git_transport_cutover_marker_v1(
    bytes: &[u8],
) -> CutoverResult<GitTransportCutoverMarkerV1> {
    if bytes.is_empty() || bytes.len() > MAX_GIT_TRANSPORT_CUTOVER_MARKER_BYTES {
        return Err(cutover_error(
            "error.git_transport_cutover_marker",
            "Git transport cutover marker is empty or oversized",
        ));
    }
    let marker: GitTransportCutoverMarkerV1 = serde_json::from_slice(bytes)
        .map_err(|error| cutover_error("error.git_transport_cutover_marker", error))?;
    if marker.version != MARKER_VERSION
        || marker.applied_at.trim().is_empty()
        || marker.applied_at.len() > 128
        || !marker.zero_prepared_history_journals
        || !marker.zero_prepared_provenance_journals
        || !marker_rows_are_canonical(&marker.rows)
        || marker_checksum(&marker)? != marker.checksum_sha256
    {
        return Err(cutover_error(
            "error.git_transport_cutover_marker_identity",
            "Git transport cutover marker identity does not match its contents",
        ));
    }
    Ok(marker)
}

fn decode_git_transport_cutover_receipt_v1(
    bytes: &[u8],
) -> CutoverResult<GitTransportCutoverReceiptV1> {
    if bytes.is_empty() || bytes.len() > MAX_GIT_TRANSPORT_CUTOVER_RECEIPT_BYTES {
        return Err(cutover_error(
            "error.git_transport_cutover_receipt",
            "Git transport cutover receipt is empty or oversized",
        ));
    }
    let receipt: GitTransportCutoverReceiptV1 = serde_json::from_slice(bytes)
        .map_err(|error| cutover_error("error.git_transport_cutover_receipt", error))?;
    if receipt.version != RECEIPT_VERSION
        || receipt.applied_at.trim().is_empty()
        || receipt.applied_at.len() > 128
        || receipt.verified_at.trim().is_empty()
        || receipt.verified_at.len() > 128
    {
        return Err(cutover_error(
            "error.git_transport_cutover_receipt",
            "Git transport cutover receipt is invalid",
        ));
    }
    Ok(receipt)
}

fn read_cutover_state_file(
    path: &Path,
    max_bytes: usize,
    label: &'static str,
) -> CutoverResult<Option<Vec<u8>>> {
    let parent = path.parent().ok_or_else(|| {
        cutover_error(
            "error.git_transport_cutover_unsafe_layout",
            format!("{label} has no parent directory"),
        )
    })?;
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| {
            cutover_error(
                "error.git_transport_cutover_unsafe_layout",
                format!("{label} has an invalid filename"),
            )
        })?;
    let Some(directory) = NofollowDirectory::open_existing(parent)
        .map_err(|error| cutover_error("error.git_transport_cutover_unsafe_layout", error))?
    else {
        return Ok(None);
    };
    let bytes = directory
        .read_regular(name, max_bytes, label)
        .map_err(|error| cutover_error("error.git_transport_cutover_io", error))?;
    directory
        .ensure_still_current()
        .map_err(|error| cutover_error("error.git_transport_cutover_unsafe_layout", error))?;
    Ok(bytes)
}

pub fn load_git_transport_cutover_marker_optional(
    state_dir: &Path,
) -> CutoverResult<Option<GitTransportCutoverMarkerV1>> {
    read_cutover_state_file(
        &git_transport_cutover_marker_path(state_dir),
        MAX_GIT_TRANSPORT_CUTOVER_MARKER_BYTES,
        "Git transport cutover marker",
    )?
    .map(|bytes| decode_git_transport_cutover_marker_v1(&bytes))
    .transpose()
}

fn load_current_marker_with_receipt(
    state_dir: &Path,
) -> CutoverResult<Option<GitTransportCutoverMarkerV1>> {
    let marker = load_git_transport_cutover_marker_optional(state_dir)?;
    let receipt = read_cutover_state_file(
        &git_transport_cutover_receipt_path(state_dir),
        MAX_GIT_TRANSPORT_CUTOVER_RECEIPT_BYTES,
        "Git transport cutover receipt",
    )?
    .map(|bytes| decode_git_transport_cutover_receipt_v1(&bytes))
    .transpose()?;
    match (marker, receipt) {
        (None, None) => Ok(None),
        (Some(marker), Some(receipt))
            if receipt.marker_checksum_sha256 == marker.checksum_sha256
                && receipt.report_artifact_hash == marker.report_artifact_hash
                && receipt.resolution_artifact_hash == marker.resolution_artifact_hash
                && receipt.applied_at == marker.applied_at
                && receipt.covered_repo_count == marker.rows.len() as u64 =>
        {
            Ok(Some(marker))
        }
        (Some(_), None) => Err(cutover_error(
            "error.git_transport_cutover_verify_required",
            "the current marker has no matching receipt; run cutover --verify before startup",
        )),
        (None, Some(_)) => Err(cutover_error(
            "error.git_transport_cutover_marker_missing",
            "a cutover receipt exists but the current marker is missing",
        )),
        (Some(_), Some(_)) => Err(cutover_error(
            "error.git_transport_cutover_current_identity",
            "the current marker and receipt identify different cutover artifacts",
        )),
    }
}

impl GitTransportCutoverRuntimeV1 {
    pub fn open(state_dir: &Path) -> CutoverResult<Self> {
        Ok(Self {
            marker: load_current_marker_with_receipt(state_dir)?,
        })
    }

    pub fn from_marker(marker: Option<GitTransportCutoverMarkerV1>) -> Self {
        Self { marker }
    }

    pub fn marker(&self) -> Option<&GitTransportCutoverMarkerV1> {
        self.marker.as_ref()
    }

    pub fn covers_repo(&self, repo_history_id: &RepoHistoryId) -> bool {
        self.marker.as_ref().is_some_and(|marker| {
            marker
                .rows
                .binary_search_by(|row| row.repo_history_id.cmp(repo_history_id))
                .is_ok()
        })
    }

    pub fn classify_repo(
        &self,
        catalog: &bbox_corpus_core::project_catalog::CatalogSnapshotV2,
        assignments: &BTreeMap<PublishedScope, String>,
        repo_history_id: &RepoHistoryId,
    ) -> GitTransportRuntimeCoverageV1 {
        let Some(marker) = &self.marker else {
            return GitTransportRuntimeCoverageV1::Uncovered;
        };
        let Ok(index) = marker
            .rows
            .binary_search_by(|row| row.repo_history_id.cmp(repo_history_id))
        else {
            return GitTransportRuntimeCoverageV1::Uncovered;
        };
        let row = &marker.rows[index];
        let Some(history) = catalog.repo_histories.get(repo_history_id) else {
            return GitTransportRuntimeCoverageV1::CoverageStalePendingRecutover;
        };
        let projection = derive_repo_transport_grants(catalog, assignments);
        let grant_state = projection.grants.get(repo_history_id);
        if history.membership_generation != row.membership_generation {
            let current_members = catalog
                .projects
                .values()
                .filter(|project| project.repo_history.as_ref() == Some(repo_history_id))
                .filter(|project| matches!(project.scope, ProjectScope::Published(_)))
                .map(|project| &project.project_id)
                .collect::<BTreeSet<_>>();
            let committed_member_addition = current_members
                .iter()
                .any(|project_id| !row.provenance_import_generations.contains_key(*project_id));
            return if committed_member_addition
                && matches!(grant_state, Some(RepoTransportGrantState::Blocked { .. }))
            {
                GitTransportRuntimeCoverageV1::CoveredBlockedPendingRecutover
            } else {
                GitTransportRuntimeCoverageV1::CoverageStalePendingRecutover
            };
        }
        match grant_state {
            Some(RepoTransportGrantState::Granted { grant })
                if grant.commitment == row.grant_commitment =>
            {
                GitTransportRuntimeCoverageV1::Current
            }
            Some(RepoTransportGrantState::Blocked {
                reason: RepoTransportBlockedReason::MissingAssignment,
            }) if catalog
                .projects
                .values()
                .filter(|project| project.repo_history.as_ref() == Some(repo_history_id))
                .filter_map(|project| match &project.scope {
                    ProjectScope::Published(scope) => Some(scope),
                    ProjectScope::LegacyLocal => None,
                })
                .all(|scope| !assignments.contains_key(scope)) =>
            {
                GitTransportRuntimeCoverageV1::CoveredProducerRemoved
            }
            _ => GitTransportRuntimeCoverageV1::CoverageStalePendingRecutover,
        }
    }
}

fn capture_observation_baseline(
    layout: &ProjectCatalogMigrationResolvedLayoutV1,
) -> CutoverResult<GitTransportObservationBaselineV1> {
    let observations =
        CheckoutAccessObservations::open(layout.bro_home.join("checkout-access-observations.json"))
            .map_err(|error| cutover_error("error.git_transport_cutover_observations", error))?
            .health();
    Ok(GitTransportObservationBaselineV1 {
        sequence: observations.sequence,
        counters: observations
            .counters
            .into_iter()
            .filter(|counter| {
                matches!(
                    counter.kind,
                    CheckoutAccessKind::GitHistory | CheckoutAccessKind::ProvenanceNoteIo
                )
            })
            .collect(),
        target_counters: observations.target_counters,
    })
}

fn covered_report_status(
    coverage: GitTransportRuntimeCoverageV1,
) -> GitTransportCutoverCoverageStatusV1 {
    match coverage {
        GitTransportRuntimeCoverageV1::Uncovered => {
            GitTransportCutoverCoverageStatusV1::BlockedPublishedNeverCovered
        }
        GitTransportRuntimeCoverageV1::Current => {
            GitTransportCutoverCoverageStatusV1::CarriedForwardCurrent
        }
        GitTransportRuntimeCoverageV1::CoveredProducerRemoved => {
            GitTransportCutoverCoverageStatusV1::CoveredProducerRemoved
        }
        GitTransportRuntimeCoverageV1::CoveredBlockedPendingRecutover => {
            GitTransportCutoverCoverageStatusV1::CoveredBlockedPendingRecutover
        }
        GitTransportRuntimeCoverageV1::CoverageStalePendingRecutover => {
            GitTransportCutoverCoverageStatusV1::CoverageStalePendingRecutover
        }
    }
}

fn carried_repo_evidence(
    history_record: &bbox_corpus_core::project_catalog::RepoHistoryRecord,
    grant: Option<&RepoTransportGrant>,
    blocked_reason: Option<RepoTransportBlockedReason>,
    coverage_status: GitTransportCutoverCoverageStatusV1,
    catalog: &bbox_corpus_core::project_catalog::CatalogSnapshotV2,
) -> GitTransportRepoEvidenceV1 {
    let mut projects = catalog
        .projects
        .values()
        .filter(|project| project.repo_history.as_ref() == Some(&history_record.repo_history_id))
        .filter_map(|project| match &project.scope {
            ProjectScope::Published(scope) => Some(GitTransportProjectEvidenceV1 {
                project_id: project.project_id.clone(),
                scope: scope.clone(),
                code_head: None,
                overlay: None,
                provenance: None,
                ready: true,
                defects: Vec::new(),
            }),
            ProjectScope::LegacyLocal => None,
        })
        .collect::<Vec<_>>();
    projects.sort_by(|left, right| left.project_id.cmp(&right.project_id));
    GitTransportRepoEvidenceV1 {
        repo_history_id: history_record.repo_history_id.clone(),
        membership_generation: history_record.membership_generation,
        coverage_status,
        blocked_reason,
        grant: grant.cloned(),
        history: None,
        projects,
        capability_baselines: Vec::new(),
        defects: Vec::new(),
    }
}

fn carried_forward_marker_rows(
    predecessor_marker: Option<&GitTransportCutoverMarkerV1>,
    repos: &[GitTransportRepoEvidenceV1],
) -> Vec<PredictedGitTransportCutoverRowV1> {
    let replacement_repo_ids = repos
        .iter()
        .filter(|repo| {
            matches!(
                repo.coverage_status,
                GitTransportCutoverCoverageStatusV1::Proposed
            )
        })
        .map(|repo| repo.repo_history_id.clone())
        .collect::<BTreeSet<_>>();
    predecessor_marker
        .map(|marker| {
            marker
                .rows
                .iter()
                .filter(|row| !replacement_repo_ids.contains(&row.repo_history_id))
                .cloned()
                .collect()
        })
        .unwrap_or_default()
}

pub struct ProjectCatalogGitTransportCutoverFacadeV1;

impl ProjectCatalogGitTransportCutoverFacadeV1 {
    pub fn preflight(
        request: GitTransportCutoverPreflightRequestV1,
    ) -> CutoverResult<GitTransportCutoverPreflightReceiptV1> {
        validate_artifact_set(
            &request.layout,
            &request.report_path,
            &request.resolution_path,
            None,
        )
        .map_err(|error| cutover_error("error.git_transport_cutover_unsafe_layout", error))?;
        if !request.config.code_collection.enabled
            || !request.config.code_collection.git_transport_enabled
        {
            return Err(cutover_error(
                "error.git_transport_cutover_disabled",
                "code collection and Git transport must both be enabled",
            ));
        }
        let predecessor_marker = load_current_marker_with_receipt(&request.layout.state_dir)?;
        let assignments = configured_assignments(&request.config)?;
        let store = ProjectCatalogStore::open_existing(request.layout.projects_path())
            .map_err(|error| cutover_error("error.git_transport_cutover_catalog", error))?;
        let state = store
            .snapshot()
            .map_err(|error| cutover_error("error.git_transport_cutover_catalog", error))?;
        let catalog = state.catalog().clone();
        validate_assignments_resolve(&catalog, &assignments)?;
        let projection = derive_repo_transport_grants(&catalog, &assignments);
        let predecessor_runtime =
            GitTransportCutoverRuntimeV1::from_marker(predecessor_marker.clone());
        let git_store = open_git_store_if_present(&request.layout, &request.config)?;
        let history_store = open_history_store_if_present(&request.layout)?;
        let manifest = ManifestIndex::load_or_new(&request.layout.edge_root)
            .map_err(|error| cutover_error("error.git_transport_cutover_overlay", error))?;
        let observation_baseline = capture_observation_baseline(&request.layout)?;
        let history_journals = git_store
            .as_ref()
            .map(GitSourceStore::list_activation_journals)
            .transpose()
            .map_err(|error| cutover_error("error.git_transport_cutover_history", error))?
            .unwrap_or_default();
        let provenance_journals = git_store
            .as_ref()
            .map(GitSourceStore::list_provenance_import_journals)
            .transpose()
            .map_err(|error| cutover_error("error.git_transport_cutover_provenance", error))?
            .unwrap_or_default();
        let prepared_history_journal_count = history_journals
            .iter()
            .filter(|journal| !journal.stage.terminal())
            .count() as u64;
        let prepared_provenance_journal_count = provenance_journals
            .iter()
            .filter(|journal| !journal.stage.terminal())
            .count() as u64;

        let mut repos = Vec::new();
        for (repo_history_id, grant_state) in &projection.grants {
            let history_record = catalog.repo_histories.get(repo_history_id).ok_or_else(|| {
                cutover_error(
                    "error.git_transport_cutover_catalog",
                    format!("catalog repo history {repo_history_id} is missing"),
                )
            })?;
            let runtime_coverage =
                predecessor_runtime.classify_repo(&catalog, &assignments, repo_history_id);
            match grant_state {
                RepoTransportGrantState::Blocked { reason } => {
                    repos.push(carried_repo_evidence(
                        history_record,
                        None,
                        Some(*reason),
                        covered_report_status(runtime_coverage),
                        &catalog,
                    ));
                }
                RepoTransportGrantState::Granted { grant } => {
                    if runtime_coverage == GitTransportRuntimeCoverageV1::Current {
                        repos.push(carried_repo_evidence(
                            history_record,
                            Some(grant),
                            None,
                            GitTransportCutoverCoverageStatusV1::CarriedForwardCurrent,
                            &catalog,
                        ));
                    } else {
                        repos.push(capture_granted_repo(
                            &request.layout,
                            &catalog.origin,
                            history_record,
                            grant,
                            git_store.as_ref(),
                            history_store.as_ref(),
                            &manifest,
                            &history_journals,
                            &provenance_journals,
                            &request.config,
                        )?);
                    }
                }
            }
        }
        repos.sort_by(|left, right| left.repo_history_id.cmp(&right.repo_history_id));
        for repo in &mut repos {
            repo.capability_baselines = capability_baselines(repo, &observation_baseline);
        }
        let legacy_local_repos = capture_legacy_local_repos(&catalog, &observation_baseline);
        let carried_forward_rows = carried_forward_marker_rows(predecessor_marker.as_ref(), &repos);
        recheck_capture(
            &request.layout,
            None,
            state.epoch(),
            state.catalog_sha256(),
            &catalog,
            &assignments,
            &projection,
            git_store.as_ref(),
            history_store.as_ref(),
            &manifest,
            &history_journals,
            &provenance_journals,
            &repos,
        )?;

        let inventory_hash = cutover_inventory_hash(
            state.epoch(),
            state.catalog_sha256(),
            &catalog.origin,
            &observation_baseline,
            prepared_history_journal_count,
            prepared_provenance_journal_count,
            &repos,
            &legacy_local_repos,
            &carried_forward_rows,
        )?;
        let (resolution, resolution_bytes) =
            load_or_create_resolution(&request.resolution_path, inventory_hash.clone(), &repos)?;
        let resolution_artifact_hash = Sha256ValueV1::digest(&resolution_bytes);
        let predicted_marker = predicted_marker(
            predecessor_marker
                .as_ref()
                .map(|marker| marker.checksum_sha256.clone()),
            state.epoch(),
            inventory_hash.clone(),
            resolution_artifact_hash.clone(),
            prepared_history_journal_count,
            prepared_provenance_journal_count,
            &repos,
            &carried_forward_rows,
        );
        let status = derived_report_status(
            prepared_history_journal_count,
            prepared_provenance_journal_count,
            &repos,
            &legacy_local_repos,
        );
        let report = GitTransportCutoverReportV1 {
            version: REPORT_VERSION,
            generated_at: request.generated_at,
            status: status.clone(),
            catalog_epoch: state.epoch(),
            catalog_sha256: state.catalog_sha256().to_string(),
            catalog_origin: catalog.origin.clone(),
            inventory_hash: inventory_hash.clone(),
            resolution_artifact_hash: resolution_artifact_hash.clone(),
            observation_baseline,
            prepared_history_journal_count,
            prepared_provenance_journal_count,
            repos,
            legacy_local_repos,
            carried_forward_rows,
            predicted_marker,
        };
        let report_bytes = serde_json::to_vec(&report)
            .map_err(|error| cutover_error("error.git_transport_cutover_artifact", error))?;
        if report_bytes.len() > MAX_PROJECT_CATALOG_REPORT_BYTES {
            return Err(cutover_error(
                "error.git_transport_cutover_artifact",
                "Git transport cutover report exceeds the artifact bound",
            ));
        }
        if decode_git_transport_cutover_report_v1(&report_bytes)? != report {
            return Err(cutover_error(
                "error.git_transport_cutover_artifact_identity",
                "Git transport cutover report changed across canonical round trip",
            ));
        }
        write_artifact_if_absent(
            &request.resolution_path,
            &resolution_bytes,
            MAX_PROJECT_CATALOG_RESOLUTION_BYTES,
            "Git transport cutover resolution",
        )
        .map_err(|error| cutover_error("error.git_transport_cutover_artifact", error))?;
        write_artifact_replacing(
            &request.report_path,
            &report_bytes,
            MAX_PROJECT_CATALOG_REPORT_BYTES,
            "Git transport cutover report",
        )
        .map_err(|error| cutover_error("error.git_transport_cutover_artifact", error))?;

        let (proposed_repo_count, blocked_repo_count, refused_repo_count) =
            preflight_repo_counts(&report.repos);
        let _ = resolution;
        Ok(GitTransportCutoverPreflightReceiptV1 {
            version: REPORT_VERSION,
            status,
            catalog_epoch: state.epoch(),
            inventory_hash,
            report_artifact_hash: Sha256ValueV1::digest(&report_bytes),
            resolution_artifact_hash,
            proposed_repo_count,
            blocked_repo_count,
            refused_repo_count,
        })
    }

    pub fn apply(
        request: GitTransportCutoverApplyRequestV1,
    ) -> CutoverResult<GitTransportCutoverVerificationReceiptV1> {
        validate_artifact_set(
            &request.layout,
            &request.report_path,
            &request.resolution_path,
            None,
        )
        .map_err(|error| cutover_error("error.git_transport_cutover_unsafe_layout", error))?;
        if request.applied_at.trim().is_empty() || request.applied_at.len() > 128 {
            return Err(cutover_error(
                "error.git_transport_cutover_apply",
                "cutover apply timestamp is invalid",
            ));
        }
        let report_bytes = read_artifact_required(
            &request.report_path,
            MAX_PROJECT_CATALOG_REPORT_BYTES,
            "Git transport cutover report",
        )
        .map_err(|error| cutover_error("error.git_transport_cutover_artifact", error))?;
        let resolution_bytes = read_artifact_required(
            &request.resolution_path,
            MAX_PROJECT_CATALOG_RESOLUTION_BYTES,
            "Git transport cutover resolution",
        )
        .map_err(|error| cutover_error("error.git_transport_cutover_artifact", error))?;
        let report = decode_git_transport_cutover_report_v1(&report_bytes)?;
        let resolution = decode_git_transport_cutover_resolution_v1(&resolution_bytes)?;
        if report.status != GitTransportCutoverStatusV1::Clean
            || resolution.inventory_hash != report.inventory_hash
            || Sha256ValueV1::digest(&resolution_bytes) != report.resolution_artifact_hash
        {
            return Err(cutover_error(
                "error.git_transport_cutover_apply_refused",
                "reviewed artifacts are not a clean, mutually bound cutover pair",
            ));
        }

        // Open before taking the mutation lock: strict store recovery uses
        // that same lock internally. The CLI's offline lifetime claim keeps
        // the open stable, then the mutation guard below closes the final
        // recheck -> marker rename window.
        let store = ProjectCatalogStore::open_existing(request.layout.projects_path())
            .map_err(|error| cutover_error("error.git_transport_cutover_catalog", error))?;
        // The CLI holds the offline lifetime claim. This second lock is the
        // catalog mutation lock and closes the report recheck -> marker rename
        // window against every catalog transaction implementation.
        let _mutation_lock = acquire_store_lock_nofollow(request.layout.projects_path())
            .map_err(|error| cutover_error("error.git_transport_cutover_lock", error))?;
        let predecessor = load_current_marker_with_receipt(&request.layout.state_dir)?;
        let predecessor_checksum = predecessor
            .as_ref()
            .map(|marker| marker.checksum_sha256.clone());
        if predecessor_checksum != report.predicted_marker.predecessor_marker_checksum {
            return Err(cutover_error(
                "error.git_transport_cutover_predecessor_changed",
                "the current cutover marker changed after preflight",
            ));
        }
        let replacement_repo_ids = report
            .repos
            .iter()
            .filter(|repo| {
                matches!(
                    repo.coverage_status,
                    GitTransportCutoverCoverageStatusV1::Proposed
                )
            })
            .map(|repo| repo.repo_history_id.clone())
            .collect::<BTreeSet<_>>();
        let expected_carried_rows = predecessor
            .as_ref()
            .map(|marker| {
                marker
                    .rows
                    .iter()
                    .filter(|row| !replacement_repo_ids.contains(&row.repo_history_id))
                    .cloned()
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        if report.carried_forward_rows != expected_carried_rows {
            return Err(cutover_error(
                "error.git_transport_cutover_predecessor_changed",
                "reviewed carry-forward rows do not match the current predecessor marker",
            ));
        }
        recheck_report_for_apply(&request.layout, &request.config, &store, &report)?;

        let mut marker = GitTransportCutoverMarkerV1 {
            version: MARKER_VERSION,
            applied_at: request.applied_at.clone(),
            report_artifact_hash: Sha256ValueV1::digest(&report_bytes),
            resolution_artifact_hash: Sha256ValueV1::digest(&resolution_bytes),
            predecessor_marker_checksum: predecessor_checksum,
            predecessor_catalog_epoch: report.predicted_marker.predecessor_catalog_epoch,
            inventory_hash: report.predicted_marker.inventory_hash.clone(),
            aggregate_grant_hash: report.predicted_marker.aggregate_grant_hash.clone(),
            zero_prepared_history_journals: report.predicted_marker.zero_prepared_history_journals,
            zero_prepared_provenance_journals: report
                .predicted_marker
                .zero_prepared_provenance_journals,
            rows: report.predicted_marker.rows.clone(),
            checksum_sha256: Sha256ValueV1::digest(b"pending"),
        };
        marker.checksum_sha256 = marker_checksum(&marker)?;
        let marker_bytes = serde_json::to_vec(&marker)
            .map_err(|error| cutover_error("error.git_transport_cutover_marker", error))?;
        if marker_bytes.len() > MAX_GIT_TRANSPORT_CUTOVER_MARKER_BYTES
            || decode_git_transport_cutover_marker_v1(&marker_bytes)? != marker
        {
            return Err(cutover_error(
                "error.git_transport_cutover_marker_identity",
                "constructed cutover marker failed its canonical identity check",
            ));
        }
        atomic_write_bytes_locked(
            &git_transport_cutover_marker_path(&request.layout.state_dir),
            &marker_bytes,
        )
        .map_err(|error| cutover_error("error.git_transport_cutover_marker_write", error))?;
        let observed = load_git_transport_cutover_marker_optional(&request.layout.state_dir)?
            .ok_or_else(|| {
                cutover_error(
                    "error.git_transport_cutover_marker_missing",
                    "cutover marker disappeared after atomic replacement",
                )
            })?;
        if observed != marker {
            return Err(cutover_error(
                "error.git_transport_cutover_marker_identity",
                "cutover marker changed after atomic replacement",
            ));
        }
        let verification = verify_marker_against_current(
            &request.layout,
            &request.config,
            &store,
            marker.clone(),
        )?;
        write_cutover_receipt(&request.layout.state_dir, &marker, &request.applied_at)?;
        Ok(verification)
    }

    pub fn verify(
        request: GitTransportCutoverVerifyRequestV1,
    ) -> CutoverResult<GitTransportCutoverVerificationReceiptV1> {
        if request.verified_at.trim().is_empty() || request.verified_at.len() > 128 {
            return Err(cutover_error(
                "error.git_transport_cutover_verify",
                "cutover verification timestamp is invalid",
            ));
        }
        let store = ProjectCatalogStore::open_existing(request.layout.projects_path())
            .map_err(|error| cutover_error("error.git_transport_cutover_catalog", error))?;
        let _mutation_lock = acquire_store_lock_nofollow(request.layout.projects_path())
            .map_err(|error| cutover_error("error.git_transport_cutover_lock", error))?;
        let marker = load_git_transport_cutover_marker_optional(&request.layout.state_dir)?
            .ok_or_else(|| {
                cutover_error(
                    "error.git_transport_cutover_marker_missing",
                    "there is no current Git transport cutover marker",
                )
            })?;
        let receipt = verify_marker_against_current(
            &request.layout,
            &request.config,
            &store,
            marker.clone(),
        )?;
        write_cutover_receipt(&request.layout.state_dir, &marker, &request.verified_at)?;
        // Re-open the selected pair after publication. This proves the receipt
        // identifies this exact current marker rather than its predecessor.
        let reopened =
            load_current_marker_with_receipt(&request.layout.state_dir)?.ok_or_else(|| {
                cutover_error(
                    "error.git_transport_cutover_marker_missing",
                    "the verified current marker disappeared",
                )
            })?;
        if reopened != marker {
            return Err(cutover_error(
                "error.git_transport_cutover_current_identity",
                "the verified marker is not the atomically selected current artifact",
            ));
        }
        Ok(receipt)
    }
}

fn validate_report_projection(
    report: &GitTransportCutoverReportV1,
    catalog: &bbox_corpus_core::project_catalog::CatalogSnapshotV2,
    projection: &RepoTransportGrantProjection,
) -> CutoverResult<()> {
    let report_ids = report
        .repos
        .iter()
        .map(|repo| repo.repo_history_id.clone())
        .collect::<BTreeSet<_>>();
    let projection_ids = projection.grants.keys().cloned().collect::<BTreeSet<_>>();
    if report_ids != projection_ids {
        return Err(cutover_error(
            "error.git_transport_cutover_capture_changed",
            "the set of Published repository histories changed after preflight",
        ));
    }
    for repo in &report.repos {
        let membership_generation = catalog
            .repo_histories
            .get(&repo.repo_history_id)
            .map(|history| history.membership_generation)
            .ok_or_else(|| {
                cutover_error(
                    "error.git_transport_cutover_capture_changed",
                    "a reviewed repository history is no longer present",
                )
            })?;
        if membership_generation != repo.membership_generation {
            return Err(cutover_error(
                "error.git_transport_cutover_capture_changed",
                "a reviewed repository membership generation changed after preflight",
            ));
        }
        let exact = projection
            .grants
            .get(&repo.repo_history_id)
            .is_some_and(|grant_state| report_repo_matches_projection(repo, grant_state));
        if !exact {
            return Err(cutover_error(
                "error.git_transport_cutover_capture_changed",
                "a reviewed repository grant changed after preflight",
            ));
        }
    }
    Ok(())
}

fn report_repo_matches_projection(
    repo: &GitTransportRepoEvidenceV1,
    grant_state: &RepoTransportGrantState,
) -> bool {
    match grant_state {
        RepoTransportGrantState::Granted { grant } => {
            matches!(
                repo.coverage_status,
                GitTransportCutoverCoverageStatusV1::Proposed
                    | GitTransportCutoverCoverageStatusV1::CarriedForwardCurrent
            ) && repo.blocked_reason.is_none()
                && repo.grant.as_ref() == Some(grant)
        }
        RepoTransportGrantState::Blocked { reason } => {
            matches!(
                repo.coverage_status,
                GitTransportCutoverCoverageStatusV1::BlockedPublishedNeverCovered
                    | GitTransportCutoverCoverageStatusV1::CoveredProducerRemoved
                    | GitTransportCutoverCoverageStatusV1::CoveredBlockedPendingRecutover
                    | GitTransportCutoverCoverageStatusV1::CoverageStalePendingRecutover
            ) && repo.blocked_reason == Some(*reason)
                && repo.grant.is_none()
        }
    }
}

fn recheck_report_for_apply(
    layout: &ProjectCatalogMigrationResolvedLayoutV1,
    config: &Config,
    store: &ProjectCatalogStore,
    report: &GitTransportCutoverReportV1,
) -> CutoverResult<()> {
    if !config.code_collection.enabled || !config.code_collection.git_transport_enabled {
        return Err(cutover_error(
            "error.git_transport_cutover_disabled",
            "code collection and Git transport must both be enabled",
        ));
    }
    if capture_observation_baseline(layout)? != report.observation_baseline {
        return Err(cutover_error(
            "error.git_transport_cutover_observations_changed",
            "checkout target observations changed after preflight; rerun preflight so the marker boundary is exact",
        ));
    }
    let assignments = configured_assignments(config)?;
    let state = store
        .snapshot()
        .map_err(|error| cutover_error("error.git_transport_cutover_catalog", error))?;
    if state.epoch() != report.catalog_epoch
        || state.catalog_sha256() != report.catalog_sha256
        || state.catalog().origin != report.catalog_origin
    {
        return Err(cutover_error(
            "error.git_transport_cutover_capture_changed",
            "catalog identity changed after preflight",
        ));
    }
    validate_assignments_resolve(state.catalog(), &assignments)?;
    let projection = derive_repo_transport_grants(state.catalog(), &assignments);
    validate_report_projection(report, state.catalog(), &projection)?;
    let git_store = open_git_store_if_present(layout, config)?;
    let history_store = open_history_store_if_present(layout)?;
    let manifest = ManifestIndex::load_or_new(&layout.edge_root)
        .map_err(|error| cutover_error("error.git_transport_cutover_overlay", error))?;
    let history_journals = git_store
        .as_ref()
        .map(GitSourceStore::list_activation_journals)
        .transpose()
        .map_err(|error| cutover_error("error.git_transport_cutover_history", error))?
        .unwrap_or_default();
    let provenance_journals = git_store
        .as_ref()
        .map(GitSourceStore::list_provenance_import_journals)
        .transpose()
        .map_err(|error| cutover_error("error.git_transport_cutover_provenance", error))?
        .unwrap_or_default();
    if history_journals
        .iter()
        .any(|journal| !journal.stage.terminal())
        || provenance_journals
            .iter()
            .any(|journal| !journal.stage.terminal())
        || report.prepared_history_journal_count != 0
        || report.prepared_provenance_journal_count != 0
    {
        return Err(cutover_error(
            "error.git_transport_cutover_prepared_journal",
            "a prepared transport journal exists at apply time",
        ));
    }
    recheck_capture(
        layout,
        Some(store),
        report.catalog_epoch,
        &report.catalog_sha256,
        state.catalog(),
        &assignments,
        &projection,
        git_store.as_ref(),
        history_store.as_ref(),
        &manifest,
        &history_journals,
        &provenance_journals,
        &report.repos,
    )?;
    for repo in report
        .repos
        .iter()
        .filter(|repo| capture_evidence_requires_recheck(&repo.coverage_status))
    {
        for project in &repo.projects {
            let selected = manifest
                .workspaces
                .get(project.project_id.as_str())
                .and_then(|entry| entry.git_overlay.as_ref());
            if selected != project.overlay.as_ref() {
                return Err(cutover_error(
                    "error.git_transport_cutover_capture_changed",
                    "a reviewed Git overlay changed after preflight",
                ));
            }
        }
    }
    Ok(())
}

fn write_cutover_receipt(
    state_dir: &Path,
    marker: &GitTransportCutoverMarkerV1,
    verified_at: &str,
) -> CutoverResult<()> {
    let receipt = GitTransportCutoverReceiptV1 {
        version: RECEIPT_VERSION,
        applied_at: marker.applied_at.clone(),
        verified_at: verified_at.to_string(),
        marker_checksum_sha256: marker.checksum_sha256.clone(),
        report_artifact_hash: marker.report_artifact_hash.clone(),
        resolution_artifact_hash: marker.resolution_artifact_hash.clone(),
        covered_repo_count: marker.rows.len() as u64,
    };
    let bytes = serde_json::to_vec(&receipt)
        .map_err(|error| cutover_error("error.git_transport_cutover_receipt", error))?;
    if bytes.len() > MAX_GIT_TRANSPORT_CUTOVER_RECEIPT_BYTES
        || decode_git_transport_cutover_receipt_v1(&bytes)? != receipt
    {
        return Err(cutover_error(
            "error.git_transport_cutover_receipt",
            "constructed cutover receipt failed its canonical identity check",
        ));
    }
    atomic_write_bytes_locked(&git_transport_cutover_receipt_path(state_dir), &bytes)
        .map_err(|error| cutover_error("error.git_transport_cutover_receipt_write", error))
}

fn verify_row_observations(
    row: &PredictedGitTransportCutoverRowV1,
    category: GitTransportObservationCategoryV1,
    current_observations: &GitTransportObservationBaselineV1,
) -> CutoverResult<Vec<GitTransportCapabilityObservationV1>> {
    let project_ids = row
        .provenance_import_generations
        .keys()
        .map(|project_id| project_id.as_str().to_string())
        .collect::<BTreeSet<_>>();
    let mut observations = Vec::with_capacity(row.capability_baselines.len());
    for baseline in &row.capability_baselines {
        let (granted_current, denied_current) =
            target_baseline_counts(current_observations, baseline.capability, &project_ids);
        if granted_current != baseline.target_granted_baseline
            || denied_current != baseline.target_denied_baseline
        {
            return Err(cutover_error(
                "error.git_transport_cutover_observation_delta",
                format!(
                    "covered repository {} recorded a post-boundary {} checkout observation",
                    row.repo_history_id,
                    baseline.capability.as_str()
                ),
            ));
        }
        observations.push(GitTransportCapabilityObservationV1 {
            capability: baseline.capability,
            category: category.clone(),
            granted_baseline: baseline.target_granted_baseline,
            granted_current,
            denied_baseline: baseline.target_denied_baseline,
            denied_current,
        });
    }
    Ok(observations)
}

fn verify_marker_against_current(
    layout: &ProjectCatalogMigrationResolvedLayoutV1,
    config: &Config,
    store: &ProjectCatalogStore,
    marker: GitTransportCutoverMarkerV1,
) -> CutoverResult<GitTransportCutoverVerificationReceiptV1> {
    let state = store
        .snapshot()
        .map_err(|error| cutover_error("error.git_transport_cutover_catalog", error))?;
    let assignments = configured_assignments(config)?;
    validate_assignments_resolve(state.catalog(), &assignments)?;
    let runtime = GitTransportCutoverRuntimeV1::from_marker(Some(marker.clone()));
    let current_observations = capture_observation_baseline(layout)?;
    let mut rows = Vec::with_capacity(marker.rows.len());
    for row in &marker.rows {
        let coverage = runtime.classify_repo(state.catalog(), &assignments, &row.repo_history_id);
        let category = coverage.observation_category().ok_or_else(|| {
            cutover_error(
                "error.git_transport_cutover_marker_identity",
                "a marker row was unexpectedly classified as uncovered",
            )
        })?;
        let capability_observations =
            verify_row_observations(row, category, &current_observations)?;
        rows.push(GitTransportCutoverVerificationRowV1 {
            repo_history_id: row.repo_history_id.clone(),
            coverage,
            capability_observations,
        });
    }
    Ok(GitTransportCutoverVerificationReceiptV1 {
        version: RECEIPT_VERSION,
        marker_checksum_sha256: marker.checksum_sha256,
        covered_repo_count: rows.len() as u64,
        current_repo_count: rows.iter().filter(|row| row.coverage.current()).count() as u64,
        rows,
    })
}

fn configured_assignments(config: &Config) -> CutoverResult<BTreeMap<PublishedScope, String>> {
    let mut assignments = BTreeMap::new();
    for producer in &config.code_collection.producers {
        if producer.producer_id.trim().is_empty() {
            return Err(cutover_error(
                "error.git_transport_cutover_config",
                "producer id must not be empty",
            ));
        }
        if producer.scopes.is_empty() {
            return Err(cutover_error(
                "error.git_transport_cutover_config",
                format!("producer {} has no configured scopes", producer.producer_id),
            ));
        }
        for scope in &producer.scopes {
            if assignments
                .insert(scope.clone(), producer.producer_id.clone())
                .is_some()
            {
                return Err(cutover_error(
                    "error.git_transport_cutover_config",
                    "a published scope is assigned to more than one producer",
                ));
            }
        }
    }
    if assignments.is_empty() {
        return Err(cutover_error(
            "error.git_transport_cutover_config",
            "Git transport cutover requires configured producer scopes",
        ));
    }
    Ok(assignments)
}

fn validate_assignments_resolve(
    catalog: &bbox_corpus_core::project_catalog::CatalogSnapshotV2,
    assignments: &BTreeMap<PublishedScope, String>,
) -> CutoverResult<()> {
    for scope in assignments.keys() {
        let matches = catalog
            .projects
            .values()
            .filter(|project| {
                matches!(&project.scope, ProjectScope::Published(candidate) if candidate == scope)
            })
            .count();
        if matches != 1 {
            return Err(cutover_error(
                "error.git_transport_cutover_config",
                format!("configured scope {scope:?} resolves to {matches} catalog projects"),
            ));
        }
    }
    Ok(())
}

fn git_store_limits(config: &Config) -> StoreLimits {
    StoreLimits {
        contract: GitSourceLimits {
            max_history_commits: config.code_collection.max_git_history_commits,
            max_history_logical_bytes: config.code_collection.max_git_history_logical_bytes,
            max_provenance_documents: config.code_collection.max_provenance_documents,
            max_provenance_logical_bytes: config.code_collection.max_provenance_logical_bytes,
        },
        max_open_uploads_per_producer: config.code_collection.max_open_uploads_per_producer,
        retained_history_generations: config.code_collection.retained_generations,
        unreferenced_record_grace_secs: config
            .code_collection
            .unreferenced_blob_grace_hours
            .saturating_mul(60 * 60),
    }
}

fn open_git_store_if_present(
    layout: &ProjectCatalogMigrationResolvedLayoutV1,
    config: &Config,
) -> CutoverResult<Option<GitSourceStore>> {
    let root = layout.state_dir.join("git-sources");
    if !path_exists_nofollow(&root)? {
        return Ok(None);
    }
    GitSourceStore::open_existing(root, git_store_limits(config))
        .map(Some)
        .map_err(|error| cutover_error("error.git_transport_cutover_git_store", error))
}

fn open_history_store_if_present(
    layout: &ProjectCatalogMigrationResolvedLayoutV1,
) -> CutoverResult<Option<HistoryGenerationStore>> {
    let root = generations_root_for_index(&layout.index_root)
        .map_err(|error| cutover_error("error.git_transport_cutover_history", error))?;
    if !path_exists_nofollow(&root)? {
        return Ok(None);
    }
    HistoryGenerationStore::open_existing_for_index(&layout.index_root)
        .map(Some)
        .map_err(|error| cutover_error("error.git_transport_cutover_history", error))
}

fn path_exists_nofollow(path: &Path) -> CutoverResult<bool> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => Err(cutover_error(
            "error.git_transport_cutover_unsafe_layout",
            format!("{} is a symlink", path.display()),
        )),
        Ok(_) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(cutover_error("error.git_transport_cutover_io", error)),
    }
}

fn capture_code_head(
    layout: &ProjectCatalogMigrationResolvedLayoutV1,
    project_id: &ProjectId,
    scope: &PublishedScope,
) -> CutoverResult<Option<GitTransportCodeHeadEvidenceV1>> {
    let paths = CodeSourceStorePaths::new(&layout.code_source_root)
        .map_err(|error| cutover_error("error.git_transport_cutover_code_source", error))?;
    let Some(activation_bytes) = read_regular_nofollow_bounded(
        &paths.activation(project_id),
        bbox_code_source_store::MAX_ACTIVATION_RECORD_BYTES as u64,
    )?
    else {
        return Ok(None);
    };
    let activation: ActivationRecordV2 = serde_json::from_slice(&activation_bytes)
        .map_err(|error| cutover_error("error.git_transport_cutover_code_source", error))?;
    if activation.project_id != *project_id || activation.published_scope != *scope {
        return Err(cutover_error(
            "error.git_transport_cutover_code_source",
            "code activation authority does not match its catalog project",
        ));
    }
    let metadata_path = paths
        .generation_metadata(scope, &activation.generation_id)
        .map_err(|error| cutover_error("error.git_transport_cutover_code_source", error))?;
    let Some(generation_bytes) = read_regular_nofollow_bounded(
        &metadata_path,
        bbox_code_source_store::MAX_GENERATION_METADATA_RECORD_BYTES as u64,
    )?
    else {
        return Ok(None);
    };
    let generation: StoredGenerationV2 = serde_json::from_slice(&generation_bytes)
        .map_err(|error| cutover_error("error.git_transport_cutover_code_source", error))?;
    generation
        .validate()
        .map_err(|error| cutover_error("error.git_transport_cutover_code_source", error))?;
    activation
        .validate_against_generation(&generation)
        .map_err(|error| cutover_error("error.git_transport_cutover_code_source", error))?;
    Ok(Some(GitTransportCodeHeadEvidenceV1 {
        generation_id: generation.generation_id,
        selector: activation.selector,
        head_commit: generation.descriptor.head_commit,
        manifest_sha256: generation.descriptor.manifest_sha256,
    }))
}

fn read_regular_nofollow_bounded(path: &Path, max_bytes: u64) -> CutoverResult<Option<Vec<u8>>> {
    use std::io::Read;

    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW);
    }
    let file = match options.open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(cutover_error(
                "error.git_transport_cutover_code_source",
                error,
            ));
        }
    };
    let metadata = file
        .metadata()
        .map_err(|error| cutover_error("error.git_transport_cutover_code_source", error))?;
    if !metadata.is_file() || metadata.len() > max_bytes {
        return Err(cutover_error(
            "error.git_transport_cutover_code_source",
            "code-source record is unsafe or oversized",
        ));
    }
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    file.take(max_bytes.saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(|error| cutover_error("error.git_transport_cutover_code_source", error))?;
    if bytes.len() as u64 != metadata.len() {
        return Err(cutover_error(
            "error.git_transport_cutover_code_source",
            "code-source record changed during cutover capture",
        ));
    }
    Ok(Some(bytes))
}

#[allow(clippy::too_many_arguments)]
fn capture_granted_repo(
    layout: &ProjectCatalogMigrationResolvedLayoutV1,
    origin: &CatalogOriginV2,
    history_record: &bbox_corpus_core::project_catalog::RepoHistoryRecord,
    grant: &RepoTransportGrant,
    git_store: Option<&GitSourceStore>,
    history_store: Option<&HistoryGenerationStore>,
    manifest: &ManifestIndex,
    history_journals: &[HistoryActivationJournalV1],
    provenance_journals: &[ProvenanceImportJournalV1],
    config: &Config,
) -> CutoverResult<GitTransportRepoEvidenceV1> {
    let mut defects = Vec::new();
    let history = capture_history(
        origin,
        history_record,
        grant,
        git_store,
        history_store,
        history_journals,
    )?;
    if history.is_none() {
        defects.push("accepted typed history and P3 parity evidence is missing".to_string());
    } else if !history.as_ref().expect("checked above").parity.accepted() {
        defects.push("typed history does not match the checkout generation".to_string());
    }

    let mut projects = Vec::new();
    for member in &grant.members {
        let code_head = capture_code_head(layout, &member.project_id, &member.scope)?;
        let overlay = manifest
            .workspaces
            .get(member.project_id.as_str())
            .and_then(|entry| entry.git_overlay.clone());
        let mut project_defects = Vec::new();
        let Some(store) = git_store else {
            project_defects.push("Git source store is missing".to_string());
            projects.push(GitTransportProjectEvidenceV1 {
                project_id: member.project_id.clone(),
                scope: member.scope.clone(),
                code_head,
                overlay,
                provenance: None,
                ready: false,
                defects: project_defects,
            });
            continue;
        };
        let provenance = capture_provenance(
            layout,
            store,
            grant,
            &member.project_id,
            &member.scope,
            provenance_journals,
            config,
        )?;
        let expected_history = history.as_ref();
        if !code_head.as_ref().is_some_and(|code| {
            expected_history.is_some_and(|history| code.head_commit == history.source_head)
        }) {
            project_defects
                .push("active code HEAD is missing or differs from typed history".to_string());
        }
        if !overlay.as_ref().is_some_and(|selector| {
            let source_matches =
                selector
                    .source
                    .producer_transport()
                    .is_some_and(|(producer, source)| {
                        producer == grant.producer_id
                            && expected_history
                                .is_some_and(|history| source == history.source_generation_id)
                    });
            source_matches
                && expected_history.is_some_and(|history| {
                    selector.repo_history_generation == history.p3_generation_id
                        && selector.repo_head == history.source_head
                })
                && code_head.as_ref().is_some_and(|code| {
                    selector.code_generation == code.generation_id
                        && code.head_commit == selector.repo_head
                })
        }) {
            project_defects
                .push("verified ProducerTransport overlay is missing or stale".to_string());
        }
        match &provenance {
            Some(evidence)
                if evidence.typed_matches_import_journal
                    && evidence.typed_covers_legacy
                    && code_head
                        .as_ref()
                        .is_some_and(|code| code.selector == evidence.code_selector) => {}
            Some(_) => project_defects.push(
                "typed provenance parity, import journal, or code selector is stale".to_string(),
            ),
            None => {
                project_defects.push("provenance import or export receipt is missing".to_string())
            }
        }
        let ready = project_defects.is_empty();
        projects.push(GitTransportProjectEvidenceV1 {
            project_id: member.project_id.clone(),
            scope: member.scope.clone(),
            code_head,
            overlay,
            provenance,
            ready,
            defects: project_defects,
        });
    }
    if projects.iter().any(|project| !project.ready) {
        defects.push("one or more published members lack complete transport evidence".to_string());
    }
    let coverage_status = if defects.is_empty() {
        GitTransportCutoverCoverageStatusV1::Proposed
    } else {
        GitTransportCutoverCoverageStatusV1::Refused
    };
    Ok(GitTransportRepoEvidenceV1 {
        repo_history_id: grant.repo_history_id.clone(),
        membership_generation: history_record.membership_generation,
        coverage_status,
        blocked_reason: None,
        grant: Some(grant.clone()),
        history,
        projects,
        capability_baselines: Vec::new(),
        defects,
    })
}

#[allow(clippy::too_many_arguments)]
fn recheck_capture(
    layout: &ProjectCatalogMigrationResolvedLayoutV1,
    already_open_store: Option<&ProjectCatalogStore>,
    catalog_epoch: u64,
    catalog_sha256: &str,
    catalog: &bbox_corpus_core::project_catalog::CatalogSnapshotV2,
    assignments: &BTreeMap<PublishedScope, String>,
    projection: &RepoTransportGrantProjection,
    git_store: Option<&GitSourceStore>,
    history_store: Option<&HistoryGenerationStore>,
    manifest: &ManifestIndex,
    history_journals: &[HistoryActivationJournalV1],
    provenance_journals: &[ProvenanceImportJournalV1],
    repos: &[GitTransportRepoEvidenceV1],
) -> CutoverResult<()> {
    let changed = |detail: &str| {
        cutover_error(
            "error.git_transport_cutover_capture_changed",
            format!("cutover source state changed during preflight: {detail}"),
        )
    };
    let opened_store;
    let store = match already_open_store {
        Some(store) => store,
        None => {
            opened_store = ProjectCatalogStore::open_existing(layout.projects_path())
                .map_err(|error| cutover_error("error.git_transport_cutover_catalog", error))?;
            &opened_store
        }
    };
    let current = store
        .snapshot()
        .map_err(|error| cutover_error("error.git_transport_cutover_catalog", error))?;
    if current.epoch() != catalog_epoch
        || current.catalog_sha256() != catalog_sha256
        || current.catalog().as_ref() != catalog
        || derive_repo_transport_grants(current.catalog(), assignments) != *projection
    {
        return Err(changed("catalog or producer grants"));
    }
    if ManifestIndex::load_or_new(&layout.edge_root)
        .map_err(|error| cutover_error("error.git_transport_cutover_overlay", error))?
        != *manifest
    {
        return Err(changed("overlay manifest"));
    }
    let git_root_present = path_exists_nofollow(&layout.state_dir.join("git-sources"))?;
    if git_root_present != git_store.is_some() {
        return Err(changed("Git source store presence"));
    }
    let history_root = generations_root_for_index(&layout.index_root)
        .map_err(|error| cutover_error("error.git_transport_cutover_history", error))?;
    if path_exists_nofollow(&history_root)? != history_store.is_some() {
        return Err(changed("P3 history store presence"));
    }
    if let Some(git_store) = git_store {
        if git_store
            .list_activation_journals()
            .map_err(|error| cutover_error("error.git_transport_cutover_history", error))?
            != history_journals
            || git_store
                .list_provenance_import_journals()
                .map_err(|error| cutover_error("error.git_transport_cutover_provenance", error))?
                != provenance_journals
        {
            return Err(changed("transport journals"));
        }
    }
    for repo in repos {
        if !capture_evidence_requires_recheck(&repo.coverage_status) {
            continue;
        }
        if let (Some(git_store), Some(history)) = (git_store, &repo.history)
            && git_store
                .current_ready_source_id(&repo.repo_history_id)
                .map_err(|error| cutover_error("error.git_transport_cutover_history", error))?
                .as_deref()
                != Some(history.source_generation_id.as_str())
        {
            return Err(changed("current history source"));
        }
        if let (Some(history_store), Some(history)) = (history_store, &repo.history) {
            let id = bbox_corpus_index::index::history_generations::HistoryGenerationIdV1::parse(
                &history.p3_generation_id,
            )
            .map_err(|error| cutover_error("error.git_transport_cutover_history", error))?;
            let generation = history_store
                .load(&id)
                .map_err(|error| cutover_error("error.git_transport_cutover_history", error))?;
            if generation.manifest.body.commit_document_count != history.commit_document_count
                || generation.manifest.body.commit_document_commitment_sha256
                    != history.commit_document_commitment_sha256
                || generation.manifest.body.vector_input_count != history.vector_input_count
                || generation.manifest.body.vector_input_commitment_sha256
                    != history.vector_input_commitment_sha256
            {
                return Err(changed("P3 history generation"));
            }
        }
        for project in &repo.projects {
            if capture_code_head(layout, &project.project_id, &project.scope)? != project.code_head
            {
                return Err(changed("active code HEAD"));
            }
            let Some(provenance) = &project.provenance else {
                continue;
            };
            let Some(git_store) = git_store else {
                return Err(changed("provenance store disappeared"));
            };
            if git_store
                .current_ready_provenance_import_id(project.project_id.as_str())
                .map_err(|error| cutover_error("error.git_transport_cutover_provenance", error))?
                .as_deref()
                != Some(provenance.import_generation_id.as_str())
            {
                return Err(changed("current provenance import"));
            }
            let receipt = git_store
                .provenance_export_receipt(project.project_id.as_str())
                .map_err(|error| cutover_error("error.git_transport_cutover_provenance", error))?
                .ok_or_else(|| changed("provenance export receipt"))?;
            let grant = repo
                .grant
                .as_ref()
                .ok_or_else(|| changed("provenance grant"))?;
            if receipt.producer_id != grant.producer_id
                || receipt.project_id != project.project_id.as_str()
                || receipt.receipt.scope != project.scope
                || receipt.receipt.notes_ref != provenance.notes_ref
                || receipt.receipt.generation != provenance.export_receipt_generation
                || receipt.receipt.local_notes_tip != provenance.export_receipt_notes_tip
            {
                return Err(changed("provenance export receipt"));
            }
            let lane = explicit_edge_lane_version(&layout.edge_root, project.project_id.as_str())
                .map_err(|error| {
                cutover_error("error.git_transport_cutover_edge_inventory", error)
            })?;
            if lane.version_token != provenance.explicit_lane_version_token {
                return Err(changed("explicit provenance edge lane"));
            }
        }
    }
    Ok(())
}

fn capture_evidence_requires_recheck(status: &GitTransportCutoverCoverageStatusV1) -> bool {
    matches!(
        status,
        GitTransportCutoverCoverageStatusV1::Proposed
            | GitTransportCutoverCoverageStatusV1::Refused
    )
}

fn capability_baselines(
    repo: &GitTransportRepoEvidenceV1,
    baseline: &GitTransportObservationBaselineV1,
) -> Vec<GitTransportCapabilityBaselineV1> {
    let project_ids = repo
        .projects
        .iter()
        .map(|project| project.project_id.as_str().to_string())
        .collect::<BTreeSet<_>>();
    [
        CheckoutAccessKind::GitHistory,
        CheckoutAccessKind::ProvenanceNoteIo,
    ]
    .into_iter()
    .map(|capability| {
        let active_category = match repo.coverage_status {
            GitTransportCutoverCoverageStatusV1::CarriedForwardCurrent => {
                GitTransportObservationCategoryV1::TransportCoveredPostBoundary
            }
            GitTransportCutoverCoverageStatusV1::BlockedPublishedNeverCovered => {
                GitTransportObservationCategoryV1::BlockedPublishedNeverCovered
            }
            GitTransportCutoverCoverageStatusV1::CoveredProducerRemoved => {
                GitTransportObservationCategoryV1::CoveredProducerRemoved
            }
            GitTransportCutoverCoverageStatusV1::CoveredBlockedPendingRecutover => {
                GitTransportObservationCategoryV1::CoveredBlockedPendingRecutover
            }
            GitTransportCutoverCoverageStatusV1::CoverageStalePendingRecutover => {
                GitTransportObservationCategoryV1::CoverageStalePendingRecutover
            }
            GitTransportCutoverCoverageStatusV1::Proposed
            | GitTransportCutoverCoverageStatusV1::Refused
                if capability == CheckoutAccessKind::GitHistory
                    && repo.history.as_ref().is_some_and(|history| {
                        repo.grant.as_ref().is_some_and(|grant| {
                            repo.projects.iter().all(|project| {
                                project.overlay.as_ref().is_some_and(|overlay| {
                                    overlay.repo_history_generation == history.p3_generation_id
                                        && overlay.repo_head == history.source_head
                                        && project.code_head.as_ref().is_some_and(|code| {
                                            code.generation_id == overlay.code_generation
                                                && code.head_commit == history.source_head
                                        })
                                        && overlay.source.producer_transport().is_some_and(
                                            |(producer, source)| {
                                                producer == grant.producer_id
                                                    && source == history.source_generation_id
                                            },
                                        )
                                })
                            })
                        })
                    }) =>
            {
                GitTransportObservationCategoryV1::HistoryTransportCurrentPreCutover
            }
            GitTransportCutoverCoverageStatusV1::Proposed
            | GitTransportCutoverCoverageStatusV1::Refused => {
                GitTransportObservationCategoryV1::OverlapWindow
            }
        };
        let (granted, denied) = baseline_counts(baseline, capability);
        let (target_granted, target_denied) =
            target_baseline_counts(baseline, capability, &project_ids);
        GitTransportCapabilityBaselineV1 {
            capability,
            active_category,
            observation_sequence: baseline.sequence,
            overlap_window_granted_baseline: granted,
            overlap_window_denied_baseline: denied,
            target_granted_baseline: target_granted,
            target_denied_baseline: target_denied,
        }
    })
    .collect()
}

fn capture_legacy_local_repos(
    catalog: &bbox_corpus_core::project_catalog::CatalogSnapshotV2,
    baseline: &GitTransportObservationBaselineV1,
) -> Vec<GitTransportLegacyLocalRepoEvidenceV1> {
    let mut projects_by_history = BTreeMap::<RepoHistoryId, Vec<ProjectId>>::new();
    for project in catalog.projects.values() {
        if project.scope != ProjectScope::LegacyLocal {
            continue;
        }
        if let Some(repo_history_id) = &project.repo_history {
            projects_by_history
                .entry(repo_history_id.clone())
                .or_default()
                .push(project.project_id.clone());
        }
    }
    projects_by_history
        .into_iter()
        .filter_map(|(repo_history_id, mut project_ids)| {
            let history = catalog.repo_histories.get(&repo_history_id)?;
            project_ids.sort();
            let category = match history.authority {
                RepoHistoryAuthority::LocalProject(_) => {
                    Some(GitTransportObservationCategoryV1::LegacyLocalLocalProject)
                }
                RepoHistoryAuthority::LegacyNamespace(_) => {
                    Some(GitTransportObservationCategoryV1::LegacyLocalLegacyNamespace)
                }
                RepoHistoryAuthority::Recorded(_) => None,
            };
            let target_project_ids = project_ids
                .iter()
                .map(|project_id| project_id.as_str().to_string())
                .collect::<BTreeSet<_>>();
            let capability_baselines = category
                .iter()
                .flat_map(|category| {
                    [
                        CheckoutAccessKind::GitHistory,
                        CheckoutAccessKind::ProvenanceNoteIo,
                    ]
                    .into_iter()
                    .map(|capability| {
                        let (granted, denied) = baseline_counts(baseline, capability);
                        let (target_granted, target_denied) =
                            target_baseline_counts(baseline, capability, &target_project_ids);
                        GitTransportCapabilityBaselineV1 {
                            capability,
                            active_category: category.clone(),
                            observation_sequence: baseline.sequence,
                            overlap_window_granted_baseline: granted,
                            overlap_window_denied_baseline: denied,
                            target_granted_baseline: target_granted,
                            target_denied_baseline: target_denied,
                        }
                    })
                })
                .collect();
            Some(GitTransportLegacyLocalRepoEvidenceV1 {
                repo_history_id,
                membership_generation: history.membership_generation,
                authority: history.authority.clone(),
                project_ids,
                capability_baselines,
                valid_authority_shape: category.is_some(),
            })
        })
        .collect()
}

fn baseline_counts(
    baseline: &GitTransportObservationBaselineV1,
    capability: CheckoutAccessKind,
) -> (u64, u64) {
    let mut granted = 0_u64;
    let mut denied = 0_u64;
    for counter in baseline
        .counters
        .iter()
        .filter(|counter| counter.kind == capability)
    {
        match counter.outcome {
            crate::checkout_access::CheckoutAccessOutcome::Granted => {
                granted = granted.saturating_add(counter.count);
            }
            crate::checkout_access::CheckoutAccessOutcome::Denied => {
                denied = denied.saturating_add(counter.count);
            }
        }
    }
    (granted, denied)
}

fn target_baseline_counts(
    baseline: &GitTransportObservationBaselineV1,
    capability: CheckoutAccessKind,
    project_ids: &BTreeSet<String>,
) -> (u64, u64) {
    let mut granted = 0_u64;
    let mut denied = 0_u64;
    for counter in baseline
        .target_counters
        .iter()
        .filter(|counter| counter.kind == capability && project_ids.contains(&counter.project_id))
    {
        match counter.outcome {
            crate::checkout_access::CheckoutAccessOutcome::Granted => {
                granted = granted.saturating_add(counter.count);
            }
            crate::checkout_access::CheckoutAccessOutcome::Denied => {
                denied = denied.saturating_add(counter.count);
            }
        }
    }
    (granted, denied)
}

fn capture_history(
    origin: &CatalogOriginV2,
    history_record: &bbox_corpus_core::project_catalog::RepoHistoryRecord,
    grant: &RepoTransportGrant,
    git_store: Option<&GitSourceStore>,
    history_store: Option<&HistoryGenerationStore>,
    journals: &[HistoryActivationJournalV1],
) -> CutoverResult<Option<GitTransportHistoryEvidenceV1>> {
    let (Some(git_store), Some(history_store)) = (git_store, history_store) else {
        return Ok(None);
    };
    let Some(journal) = journals
        .iter()
        .find(|journal| journal.repo_history_id == grant.repo_history_id)
    else {
        return Ok(None);
    };
    if journal.stage != HistoryActivationStageV1::Committed
        || journal.producer_id != grant.producer_id
        || journal.grant_commitment != grant.commitment
    {
        return Ok(None);
    }
    let source = git_store
        .verified_history_source(&grant.producer_id, &journal.source_generation_id)
        .map_err(|error| cutover_error("error.git_transport_cutover_history", error))?;
    if git_store
        .current_ready_source_id(&grant.repo_history_id)
        .map_err(|error| cutover_error("error.git_transport_cutover_history", error))?
        .as_deref()
        != Some(journal.source_generation_id.as_str())
        || source.repo_history_id != grant.repo_history_id
        || source.primary_namespace != grant.primary_namespace
        || source.authority_scope != grant.authority_scope
    {
        return Ok(None);
    }
    let RepoHistoryMaterialization::Ready { generation_id } = &history_record.materialization
    else {
        return Ok(None);
    };
    if generation_id.as_str() != journal.planned_p3_generation_id {
        return Ok(None);
    }
    let parsed = bbox_corpus_index::index::history_generations::HistoryGenerationIdV1::parse(
        generation_id.as_str(),
    )
    .map_err(|error| cutover_error("error.git_transport_cutover_history", error))?;
    let generation = history_store
        .load(&parsed)
        .map_err(|error| cutover_error("error.git_transport_cutover_history", error))?;
    if !history_generation_matches_journal(&generation, journal) {
        return Ok(None);
    }
    let parity = history_parity_status(
        origin,
        journal.prior_p3_generation_id.as_deref(),
        &journal.planned_p3_generation_id,
    );
    Ok(Some(GitTransportHistoryEvidenceV1 {
        source_generation_id: source.source_generation_id,
        source_head: source.repo_head,
        p3_generation_id: generation.id.as_str().to_string(),
        commit_document_count: generation.manifest.body.commit_document_count,
        commit_document_commitment_sha256: generation
            .manifest
            .body
            .commit_document_commitment_sha256,
        vector_input_count: generation.manifest.body.vector_input_count,
        vector_input_commitment_sha256: generation.manifest.body.vector_input_commitment_sha256,
        parity,
    }))
}

fn history_parity_status(
    origin: &CatalogOriginV2,
    prior_p3_generation_id: Option<&str>,
    planned_p3_generation_id: &str,
) -> GitTransportParityStatusV1 {
    match origin {
        CatalogOriginV2::FreshV2 {} if prior_p3_generation_id.is_none() => {
            GitTransportParityStatusV1::VacuousFreshV2
        }
        _ if prior_p3_generation_id == Some(planned_p3_generation_id) => {
            GitTransportParityStatusV1::Equal
        }
        _ if prior_p3_generation_id.is_none() => GitTransportParityStatusV1::Missing,
        _ => GitTransportParityStatusV1::Mismatch,
    }
}

fn history_generation_matches_journal(
    generation: &HistoryGenerationRecordV1,
    journal: &HistoryActivationJournalV1,
) -> bool {
    generation.manifest.body.commit_document_count == journal.commit_document_count
        && generation.manifest.body.commit_document_commitment_sha256
            == journal.commit_document_commitment_sha256
        && generation.manifest.body.vector_input_count == journal.vector_input_count
        && generation.manifest.body.vector_input_commitment_sha256
            == journal.vector_input_commitment_sha256
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct LegacyProvenanceEdgeMatchV1 {
    source: String,
    kind: String,
    commit: String,
    file: Option<String>,
    tool: Option<String>,
    byte_start: Option<String>,
    byte_end: Option<String>,
}

fn legacy_edge_match(edge: &bbox_edge_sidecar::edge_sidecar::Edge) -> LegacyProvenanceEdgeMatchV1 {
    LegacyProvenanceEdgeMatchV1 {
        source: edge.source.to_string(),
        kind: edge.kind.clone(),
        commit: edge
            .metadata
            .get("anchor.commit_sha_at_edit")
            .cloned()
            .unwrap_or_default(),
        file: edge.metadata.get("anchor.file_path").cloned(),
        tool: edge.metadata.get("tool.name").cloned(),
        byte_start: edge.metadata.get("anchor.byte_start").cloned(),
        byte_end: edge.metadata.get("anchor.byte_end").cloned(),
    }
}

fn call_match(
    note: &bbox_provenance::GitProvenanceNote,
    call: &bbox_provenance::NoteToolCall,
    kind: &str,
    source: &EntityRef,
) -> LegacyProvenanceEdgeMatchV1 {
    LegacyProvenanceEdgeMatchV1 {
        source: source.to_string(),
        kind: kind.to_string(),
        commit: note.commit.clone(),
        file: call.file.clone(),
        tool: Some(call.tool.clone()),
        byte_start: call.byte_range.map(|range| range[0].to_string()),
        byte_end: call.byte_range.map(|range| range[1].to_string()),
    }
}

fn reconstruct_typed_provenance_keys(
    project_id: &ProjectId,
    notes: &[bbox_provenance::GitProvenanceNote],
    legacy_matches: &BTreeMap<LegacyProvenanceEdgeMatchV1, BTreeSet<String>>,
) -> CutoverResult<BTreeSet<String>> {
    let mut keys = BTreeSet::new();
    for note in notes {
        for call in &note.tool_calls {
            let Some(kind) = bbox_provenance::authenticated_edge_kind_for_call(call) else {
                continue;
            };
            let Some(source_ref) = call.source_ref.as_deref() else {
                continue;
            };
            let Ok(source) = EntityRef::parse(source_ref) else {
                continue;
            };
            if note.schema_version == bbox_provenance::SCHEMA_VERSION_V1 {
                if let Some(matched) = legacy_matches.get(&call_match(note, call, kind, &source)) {
                    keys.extend(matched.iter().cloned());
                }
                continue;
            }
            let target = EntityRef::parse(call.target_ref.as_deref().unwrap_or_default())
                .map_err(|error| cutover_error("error.git_transport_cutover_provenance", error))?;
            let target_project = match &target {
                EntityRef::ProjectFile {
                    project_id: target_project,
                    ..
                }
                | EntityRef::ProjectFileV2 {
                    project_id: target_project,
                    ..
                } => target_project,
                _ => {
                    return Err(cutover_error(
                        "error.git_transport_cutover_provenance",
                        "authenticated V2 provenance target is not a project file",
                    ));
                }
            };
            if target_project != project_id.as_str() {
                return Err(cutover_error(
                    "error.git_transport_cutover_provenance",
                    "authenticated V2 provenance target belongs to another project",
                ));
            }
            let mut metadata = BTreeMap::new();
            metadata.insert("anchor.commit_sha_at_edit".to_string(), note.commit.clone());
            let edge = bbox_edge_sidecar::edge_sidecar::Edge {
                source,
                kind: kind.to_string(),
                target,
                provenance: bbox_chunker::EdgeProvenance::Explicit,
                confidence: bbox_chunker::EdgeConfidence::Heuristic,
                metadata,
                project_id: None,
            };
            keys.insert(edge_import_key(&edge));
        }
    }
    Ok(keys)
}

fn capture_provenance(
    layout: &ProjectCatalogMigrationResolvedLayoutV1,
    store: &GitSourceStore,
    grant: &RepoTransportGrant,
    project_id: &ProjectId,
    scope: &PublishedScope,
    journals: &[ProvenanceImportJournalV1],
    config: &Config,
) -> CutoverResult<Option<GitTransportProvenanceEvidenceV1>> {
    let Some(import_generation_id) = store
        .current_ready_provenance_import_id(project_id.as_str())
        .map_err(|error| cutover_error("error.git_transport_cutover_provenance", error))?
    else {
        return Ok(None);
    };
    let source = store
        .verified_provenance_import(&import_generation_id)
        .map_err(|error| cutover_error("error.git_transport_cutover_provenance", error))?;
    if source.producer_id != grant.producer_id
        || source.project_id != project_id.as_str()
        || source.scope != *scope
        || source.notes_ref != layout.provenance_notes_ref
    {
        return Ok(None);
    }
    let Some(journal) = journals.iter().find(|journal| {
        journal.project_id == project_id.as_str()
            && journal.import_generation_id == import_generation_id
    }) else {
        return Ok(None);
    };
    if journal.stage != ProvenanceImportStageV1::Committed
        || journal.producer_id != grant.producer_id
    {
        return Ok(None);
    }
    let Some(receipt) = store
        .provenance_export_receipt(project_id.as_str())
        .map_err(|error| cutover_error("error.git_transport_cutover_provenance", error))?
    else {
        return Ok(None);
    };
    if receipt.producer_id != grant.producer_id
        || receipt.receipt.scope != *scope
        || receipt.receipt.notes_ref != layout.provenance_notes_ref
        || receipt.receipt.local_notes_tip != source.notes_tip
    {
        return Ok(None);
    }
    let mut v1_document_count = 0_u64;
    let mut v2_document_count = 0_u64;
    let mut notes = Vec::new();
    store
        .visit_verified_provenance_documents(&source, |document| {
            let note = bbox_provenance::parse_note_document(&document.document)?;
            match note.schema_version {
                bbox_provenance::SCHEMA_VERSION_V1 => v1_document_count += 1,
                bbox_provenance::SCHEMA_VERSION_V2 => v2_document_count += 1,
                _ => unreachable!("verified parser accepts only v1 and v2"),
            }
            notes.push(note);
            Ok(())
        })
        .map_err(|error| cutover_error("error.git_transport_cutover_provenance", error))?;

    let mut legacy_keys = BTreeSet::new();
    let mut resolved_matches = BTreeMap::<LegacyProvenanceEdgeMatchV1, BTreeSet<String>>::new();
    let max_source_bytes = config
        .code_collection
        .max_provenance_logical_bytes
        .saturating_mul(4)
        .min(MAX_ACTIVE_SIDECAR_INPUT_BYTES)
        .max(1);
    let lane = visit_explicit_edge_lane(
        &layout.edge_root,
        project_id.as_str(),
        max_source_bytes,
        MAX_EXPLICIT_EDGE_LINE_BYTES,
        |edge| {
            if !matches!(edge.kind.as_str(), "READ_FILE" | "EDITED_FILE")
                || edge.metadata.get("anchor.project_id").map(String::as_str)
                    != Some(project_id.as_str())
                || !edge.metadata.contains_key("anchor.commit_sha_at_edit")
            {
                return Ok(());
            }
            let key = edge_import_key(&edge);
            resolved_matches
                .entry(legacy_edge_match(&edge))
                .or_default()
                .insert(key.clone());
            if !edge
                .metadata
                .contains_key("provenance.import_generation_id")
            {
                legacy_keys.insert(key);
            }
            Ok(())
        },
    )
    .map_err(|error| cutover_error("error.git_transport_cutover_edge_inventory", error))?;
    let typed_keys = reconstruct_typed_provenance_keys(project_id, &notes, &resolved_matches)?;
    let legacy_edge_keys_sha256 = provenance_edge_key_commitment(&legacy_keys);
    let typed_edge_keys_sha256 = provenance_edge_key_commitment(&typed_keys);
    let typed_matches_import_journal = journal.edge_count == typed_keys.len() as u64
        && journal.edge_keys_sha256 == typed_edge_keys_sha256;
    let typed_covers_legacy = legacy_keys.is_subset(&typed_keys);
    Ok(Some(GitTransportProvenanceEvidenceV1 {
        import_generation_id,
        code_selector: journal.code_selector.clone(),
        notes_ref: source.notes_ref,
        notes_tip: source.notes_tip,
        manifest_sha256: source.manifest_sha256,
        v1_document_count,
        v2_document_count,
        explicit_lane_version_token: lane.version_token,
        explicit_lane_sha256: lane.content_sha256,
        legacy_edge_key_count: legacy_keys.len() as u64,
        legacy_edge_keys_sha256,
        typed_edge_key_count: typed_keys.len() as u64,
        typed_edge_keys_sha256,
        imported_edge_key_count: journal.edge_count,
        imported_edge_keys_sha256: journal.edge_keys_sha256.clone(),
        typed_matches_import_journal,
        typed_covers_legacy,
        export_receipt_generation: receipt.receipt.generation,
        export_receipt_notes_tip: receipt.receipt.local_notes_tip,
    }))
}

fn provenance_edge_key_commitment(keys: &BTreeSet<String>) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"bbox-provenance-import-edge-keys-v1\0");
    for key in keys {
        hasher.update((key.len() as u64).to_be_bytes());
        hasher.update(key.as_bytes());
    }
    hex::encode(hasher.finalize())
}

fn load_or_create_resolution(
    path: &Path,
    inventory_hash: Sha256ValueV1,
    repos: &[GitTransportRepoEvidenceV1],
) -> CutoverResult<(GitTransportCutoverResolutionV1, Vec<u8>)> {
    let existing = read_artifact_optional(
        path,
        MAX_PROJECT_CATALOG_RESOLUTION_BYTES,
        "Git transport cutover resolution",
    )
    .map_err(|error| cutover_error("error.git_transport_cutover_artifact", error))?;
    let (resolution, bytes) = match existing {
        Some(bytes) => {
            let resolution = decode_git_transport_cutover_resolution_v1(&bytes)?;
            (resolution, bytes)
        }
        None => {
            let resolution = GitTransportCutoverResolutionV1 {
                version: RESOLUTION_VERSION,
                inventory_hash: inventory_hash.clone(),
                blocked_repo_acknowledgements: BTreeMap::new(),
            };
            let bytes = serde_json::to_vec(&resolution)
                .map_err(|error| cutover_error("error.git_transport_cutover_resolution", error))?;
            (resolution, bytes)
        }
    };
    if resolution.version != RESOLUTION_VERSION || resolution.inventory_hash != inventory_hash {
        return Err(cutover_error(
            "error.git_transport_cutover_resolution",
            "resolution is bound to a different inventory",
        ));
    }
    let blocked = repos
        .iter()
        .filter(|repo| {
            matches!(
                repo.coverage_status,
                GitTransportCutoverCoverageStatusV1::BlockedPublishedNeverCovered
            )
        })
        .map(|repo| repo.repo_history_id.clone())
        .collect::<BTreeSet<_>>();
    if resolution
        .blocked_repo_acknowledgements
        .keys()
        .any(|repo| !blocked.contains(repo))
    {
        return Err(cutover_error(
            "error.git_transport_cutover_resolution",
            "resolution acknowledges a repository that is not blocked in this inventory",
        ));
    }
    Ok((resolution, bytes))
}

fn predicted_marker(
    predecessor_marker_checksum: Option<Sha256ValueV1>,
    catalog_epoch: u64,
    inventory_hash: Sha256ValueV1,
    resolution_artifact_hash: Sha256ValueV1,
    prepared_history_journal_count: u64,
    prepared_provenance_journal_count: u64,
    repos: &[GitTransportRepoEvidenceV1],
    carried_forward_rows: &[PredictedGitTransportCutoverRowV1],
) -> PredictedGitTransportCutoverMarkerV1 {
    let mut rows = repos
        .iter()
        .filter_map(|repo| {
            if !matches!(
                repo.coverage_status,
                GitTransportCutoverCoverageStatusV1::Proposed
            ) {
                return None;
            }
            let grant = repo.grant.as_ref()?;
            let history = repo.history.as_ref()?;
            Some(PredictedGitTransportCutoverRowV1 {
                repo_history_id: repo.repo_history_id.clone(),
                grant_commitment: grant.commitment.clone(),
                membership_generation: repo.membership_generation,
                source_generation_id: history.source_generation_id.clone(),
                p3_generation_id: history.p3_generation_id.clone(),
                history_parity_commitment: Sha256ValueV1::digest(
                    &serde_json::to_vec(history).expect("history evidence is serializable"),
                ),
                provenance_import_generations: repo
                    .projects
                    .iter()
                    .filter_map(|project| {
                        project.provenance.as_ref().map(|provenance| {
                            (
                                project.project_id.clone(),
                                provenance.import_generation_id.clone(),
                            )
                        })
                    })
                    .collect(),
                provenance_export_generations: repo
                    .projects
                    .iter()
                    .filter_map(|project| {
                        project.provenance.as_ref().map(|provenance| {
                            (
                                project.project_id.clone(),
                                provenance.export_receipt_generation.clone(),
                            )
                        })
                    })
                    .collect(),
                provenance_parity_commitments: repo
                    .projects
                    .iter()
                    .filter_map(|project| {
                        project.provenance.as_ref().map(|provenance| {
                            (
                                project.project_id.clone(),
                                Sha256ValueV1::digest(
                                    &serde_json::to_vec(provenance)
                                        .expect("provenance evidence is serializable"),
                                ),
                            )
                        })
                    })
                    .collect(),
                capability_baselines: repo.capability_baselines.clone(),
            })
        })
        .collect::<Vec<_>>();
    rows.extend(carried_forward_rows.iter().cloned());
    rows.sort_by(|left, right| left.repo_history_id.cmp(&right.repo_history_id));
    let aggregate_grant_hash = Sha256ValueV1::digest(
        &serde_json::to_vec(
            &repos
                .iter()
                .map(|repo| {
                    (
                        &repo.repo_history_id,
                        repo.grant.as_ref().map(|grant| &grant.commitment),
                        repo.blocked_reason,
                        repo.membership_generation,
                    )
                })
                .collect::<Vec<_>>(),
        )
        .expect("grant projection is serializable"),
    );
    PredictedGitTransportCutoverMarkerV1 {
        version: MARKER_VERSION,
        predecessor_marker_checksum,
        predecessor_catalog_epoch: catalog_epoch,
        inventory_hash,
        resolution_artifact_hash,
        aggregate_grant_hash,
        zero_prepared_history_journals: prepared_history_journal_count == 0,
        zero_prepared_provenance_journals: prepared_provenance_journal_count == 0,
        rows,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bbox_chunker::{EdgeConfidence, EdgeProvenance};
    use bbox_corpus_core::project_catalog::{
        CatalogSnapshotV2, CommitNamespace, CorpusProject, RecordedRepoAuthority,
        RepoHistoryAuthority, RepoHistoryRecord,
    };
    use bbox_edge_sidecar::edge_sidecar::Edge;
    use bbox_provenance::{GitProvenanceNote, NoteToolCall, ProducedBy};

    fn project_id() -> ProjectId {
        ProjectId::parse("p_00000000000000000000000000000001").unwrap()
    }

    fn coverage_fixture() -> (
        CatalogSnapshotV2,
        RepoHistoryId,
        PublishedScope,
        PredictedGitTransportCutoverRowV1,
    ) {
        let repo_history_id = RepoHistoryId::parse("rh_00000000000000000000000000000001").unwrap();
        let scope = PublishedScope::try_new("neutral-repo", ".").unwrap();
        let project_id = project_id();
        let mut catalog = CatalogSnapshotV2::empty(1).unwrap();
        catalog.repo_histories.insert(
            repo_history_id.clone(),
            RepoHistoryRecord {
                repo_history_id: repo_history_id.clone(),
                membership_generation: 7,
                authority: RepoHistoryAuthority::Recorded(
                    RecordedRepoAuthority::parse("neutral-repo").unwrap(),
                ),
                primary_namespace: CommitNamespace::parse("neutral-repo").unwrap(),
                compatibility_namespaces: BTreeSet::new(),
                materialization: RepoHistoryMaterialization::NotBuilt,
            },
        );
        catalog.projects.insert(
            project_id.clone(),
            CorpusProject {
                project_id: project_id.clone(),
                scope: ProjectScope::Published(scope.clone()),
                operator_aliases: BTreeSet::new(),
                nominated_aliases: BTreeSet::new(),
                display_name: "Neutral fixture".to_string(),
                created_at: "unix:1".to_string(),
                registered_at_compat: None,
                repo_history: Some(repo_history_id.clone()),
                languages: BTreeSet::new(),
            },
        );
        catalog.validate().unwrap();
        let assignments = BTreeMap::from([(scope.clone(), "producer-a".to_string())]);
        let projection = derive_repo_transport_grants(&catalog, &assignments);
        let RepoTransportGrantState::Granted { grant } = &projection.grants[&repo_history_id]
        else {
            panic!("fixture grant must be complete")
        };
        let row = PredictedGitTransportCutoverRowV1 {
            repo_history_id: repo_history_id.clone(),
            grant_commitment: grant.commitment.clone(),
            membership_generation: 7,
            source_generation_id: "source-one".to_string(),
            p3_generation_id: format!("rhg_{}", "a".repeat(64)),
            history_parity_commitment: Sha256ValueV1::digest(b"history"),
            provenance_import_generations: BTreeMap::from([(
                project_id.clone(),
                "import-one".to_string(),
            )]),
            provenance_export_generations: BTreeMap::from([(
                project_id.clone(),
                "export-one".to_string(),
            )]),
            provenance_parity_commitments: BTreeMap::from([(
                project_id,
                Sha256ValueV1::digest(b"provenance"),
            )]),
            capability_baselines: [
                CheckoutAccessKind::GitHistory,
                CheckoutAccessKind::ProvenanceNoteIo,
            ]
            .into_iter()
            .map(|capability| GitTransportCapabilityBaselineV1 {
                capability,
                active_category: GitTransportObservationCategoryV1::OverlapWindow,
                observation_sequence: 0,
                overlap_window_granted_baseline: 0,
                overlap_window_denied_baseline: 0,
                target_granted_baseline: 0,
                target_denied_baseline: 0,
            })
            .collect(),
        };
        (catalog, repo_history_id, scope, row)
    }

    fn marker_with_row(row: PredictedGitTransportCutoverRowV1) -> GitTransportCutoverMarkerV1 {
        let mut marker = GitTransportCutoverMarkerV1 {
            version: MARKER_VERSION,
            applied_at: "unix:10".to_string(),
            report_artifact_hash: Sha256ValueV1::digest(b"report"),
            resolution_artifact_hash: Sha256ValueV1::digest(b"resolution"),
            predecessor_marker_checksum: None,
            predecessor_catalog_epoch: 8,
            inventory_hash: Sha256ValueV1::digest(b"inventory"),
            aggregate_grant_hash: Sha256ValueV1::digest(b"grants"),
            zero_prepared_history_journals: true,
            zero_prepared_provenance_journals: true,
            rows: vec![row],
            checksum_sha256: Sha256ValueV1::digest(b"pending"),
        };
        marker.checksum_sha256 = marker_checksum(&marker).unwrap();
        marker
    }

    fn grant_for(
        catalog: &CatalogSnapshotV2,
        scope: &PublishedScope,
        repo_history_id: &RepoHistoryId,
    ) -> RepoTransportGrant {
        let assignments = BTreeMap::from([(scope.clone(), "producer-a".to_string())]);
        let projection = derive_repo_transport_grants(catalog, &assignments);
        let RepoTransportGrantState::Granted { grant } = &projection.grants[repo_history_id] else {
            panic!("fixture grant must be complete")
        };
        grant.clone()
    }

    fn minimal_repo_evidence(
        repo_history_id: RepoHistoryId,
        coverage_status: GitTransportCutoverCoverageStatusV1,
    ) -> GitTransportRepoEvidenceV1 {
        GitTransportRepoEvidenceV1 {
            repo_history_id,
            membership_generation: 7,
            coverage_status,
            blocked_reason: None,
            grant: None,
            history: None,
            projects: Vec::new(),
            capability_baselines: Vec::new(),
            defects: Vec::new(),
        }
    }

    fn call(target_ref: Option<String>, file: &str) -> NoteToolCall {
        NoteToolCall {
            tool: "Edit".to_string(),
            edge_kind: Some("EDITED_FILE".to_string()),
            source_ref: Some("transcript:test:session:10:0".to_string()),
            target_ref,
            file: Some(file.to_string()),
            byte_range: Some([10, 20]),
            turn: Some(10),
        }
    }

    #[test]
    fn reconstructed_typed_superset_covers_untagged_dedup_row() {
        let project_id = project_id();
        let target_one = format!("project_file:{project_id}:path:{}:0", "a".repeat(64));
        let target_two = format!("project_file:{project_id}:path:{}:0", "b".repeat(64));
        let legacy_call = call(None, "src/legacy.rs");
        let v1 = GitProvenanceNote {
            schema_version: bbox_provenance::SCHEMA_VERSION_V1,
            commit: "1".repeat(40),
            part: None,
            produced_by: ProducedBy::default(),
            tool_calls: vec![legacy_call.clone()],
            knowledge_writes: Vec::new(),
        };
        let v2 = GitProvenanceNote::new_v2(
            "2".repeat(40),
            ProducedBy::default(),
            vec![call(Some(target_two), "src/new.rs")],
            Vec::new(),
        );
        let mut metadata = BTreeMap::from([
            ("anchor.project_id".to_string(), project_id.to_string()),
            ("anchor.file_path".to_string(), "src/legacy.rs".to_string()),
            ("anchor.commit_sha_at_edit".to_string(), "1".repeat(40)),
            ("tool.name".to_string(), "Edit".to_string()),
            ("anchor.byte_start".to_string(), "10".to_string()),
            ("anchor.byte_end".to_string(), "20".to_string()),
        ]);
        let legacy_edge = Edge {
            source: EntityRef::parse("transcript:test:session:10:0").unwrap(),
            kind: "EDITED_FILE".to_string(),
            target: EntityRef::parse(&target_one).unwrap(),
            provenance: EdgeProvenance::Explicit,
            confidence: EdgeConfidence::Heuristic,
            metadata: std::mem::take(&mut metadata),
            project_id: None,
        };
        let legacy_key = edge_import_key(&legacy_edge);
        let matches = BTreeMap::from([(
            legacy_edge_match(&legacy_edge),
            BTreeSet::from([legacy_key.clone()]),
        )]);
        let typed = reconstruct_typed_provenance_keys(&project_id, &[v1, v2], &matches).unwrap();
        assert_eq!(typed.len(), 2);
        assert!(typed.contains(&legacy_key));
        assert_eq!(provenance_edge_key_commitment(&typed).len(), 64);
    }

    #[test]
    fn provenance_reconstruction_refuses_cross_project_v2_and_leaves_unresolved_v1_unmatched() {
        let project_id = project_id();
        let other_target = format!(
            "project_file:{}:path:{}:0",
            "p_00000000000000000000000000000002",
            "a".repeat(64)
        );
        let cross_project = GitProvenanceNote::new_v2(
            "2".repeat(40),
            ProducedBy::default(),
            vec![call(Some(other_target), "src/other.rs")],
            Vec::new(),
        );
        assert!(
            reconstruct_typed_provenance_keys(&project_id, &[cross_project], &BTreeMap::new())
                .is_err()
        );

        let unmatched_v1 = GitProvenanceNote {
            schema_version: bbox_provenance::SCHEMA_VERSION_V1,
            commit: "1".repeat(40),
            part: None,
            produced_by: ProducedBy::default(),
            tool_calls: vec![call(None, "src/missing.rs")],
            knowledge_writes: Vec::new(),
        };
        assert!(
            reconstruct_typed_provenance_keys(&project_id, &[unmatched_v1], &BTreeMap::new())
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn history_parity_matrix_distinguishes_fresh_missing_equal_and_mismatch() {
        let fresh = CatalogOriginV2::FreshV2 {};
        let migrated = CatalogOriginV2::MigratedV1 {
            transaction_id: bbox_corpus_core::project_catalog::ProjectCatalogTransactionId::mint(),
        };
        assert_eq!(
            history_parity_status(&fresh, None, "planned"),
            GitTransportParityStatusV1::VacuousFreshV2
        );
        assert_eq!(
            history_parity_status(&migrated, None, "planned"),
            GitTransportParityStatusV1::Missing
        );
        assert_eq!(
            history_parity_status(&migrated, Some("planned"), "planned"),
            GitTransportParityStatusV1::Equal
        );
        assert_eq!(
            history_parity_status(&migrated, Some("stale"), "planned"),
            GitTransportParityStatusV1::Mismatch
        );
    }

    #[test]
    fn marker_checksum_and_current_receipt_identity_fail_closed() {
        let (_catalog, _repo_history_id, _scope, row) = coverage_fixture();
        let marker = marker_with_row(row);
        let bytes = serde_json::to_vec(&marker).unwrap();
        assert_eq!(
            decode_git_transport_cutover_marker_v1(&bytes).unwrap(),
            marker
        );

        let mut corrupt: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        corrupt["applied_at"] = serde_json::json!("unix:11");
        let error = decode_git_transport_cutover_marker_v1(&serde_json::to_vec(&corrupt).unwrap())
            .unwrap_err();
        assert_eq!(error.code, "error.git_transport_cutover_marker_identity");

        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        assert!(
            GitTransportCutoverRuntimeV1::open(&root)
                .unwrap()
                .marker()
                .is_none()
        );
        atomic_write_bytes_locked(
            &git_transport_cutover_marker_path(&root),
            &serde_json::to_vec(&marker).unwrap(),
        )
        .unwrap();
        let error = GitTransportCutoverRuntimeV1::open(&root).unwrap_err();
        assert_eq!(error.code, "error.git_transport_cutover_verify_required");
        write_cutover_receipt(&root, &marker, "unix:12").unwrap();
        assert_eq!(
            GitTransportCutoverRuntimeV1::open(&root)
                .unwrap()
                .marker()
                .unwrap(),
            &marker
        );

        let receipt_only = tempfile::tempdir().unwrap();
        let receipt_only = receipt_only.path().canonicalize().unwrap();
        write_cutover_receipt(&receipt_only, &marker, "unix:13").unwrap();
        assert_eq!(
            GitTransportCutoverRuntimeV1::open(&receipt_only)
                .unwrap_err()
                .code,
            "error.git_transport_cutover_marker_missing"
        );

        let mismatch = tempfile::tempdir().unwrap();
        let mismatch = mismatch.path().canonicalize().unwrap();
        atomic_write_bytes_locked(
            &git_transport_cutover_marker_path(&mismatch),
            &serde_json::to_vec(&marker).unwrap(),
        )
        .unwrap();
        let mut other = marker.clone();
        other.applied_at = "unix:14".to_string();
        other.checksum_sha256 = marker_checksum(&other).unwrap();
        write_cutover_receipt(&mismatch, &other, "unix:14").unwrap();
        assert_eq!(
            GitTransportCutoverRuntimeV1::open(&mismatch)
                .unwrap_err()
                .code,
            "error.git_transport_cutover_current_identity"
        );

        let corrupt_receipt = tempfile::tempdir().unwrap();
        let corrupt_receipt = corrupt_receipt.path().canonicalize().unwrap();
        atomic_write_bytes_locked(
            &git_transport_cutover_marker_path(&corrupt_receipt),
            &serde_json::to_vec(&marker).unwrap(),
        )
        .unwrap();
        atomic_write_bytes_locked(&git_transport_cutover_receipt_path(&corrupt_receipt), b"{}")
            .unwrap();
        assert_eq!(
            GitTransportCutoverRuntimeV1::open(&corrupt_receipt)
                .unwrap_err()
                .code,
            "error.git_transport_cutover_receipt"
        );
    }

    #[test]
    fn covered_runtime_matrix_never_reclassifies_stale_as_uncovered() {
        let (mut catalog, repo_history_id, scope, row) = coverage_fixture();
        let runtime = GitTransportCutoverRuntimeV1::from_marker(Some(marker_with_row(row)));
        let assigned = BTreeMap::from([(scope.clone(), "producer-a".to_string())]);
        assert_eq!(
            runtime.classify_repo(&catalog, &assigned, &repo_history_id),
            GitTransportRuntimeCoverageV1::Current
        );
        catalog
            .projects
            .get_mut(&project_id())
            .unwrap()
            .display_name = "Routine metadata change".to_string();
        assert_eq!(
            runtime.classify_repo(&catalog, &assigned, &repo_history_id),
            GitTransportRuntimeCoverageV1::Current
        );
        let reassigned = BTreeMap::from([(scope.clone(), "producer-b".to_string())]);
        assert_eq!(
            runtime.classify_repo(&catalog, &reassigned, &repo_history_id),
            GitTransportRuntimeCoverageV1::CoverageStalePendingRecutover
        );
        assert_eq!(
            runtime.classify_repo(&catalog, &BTreeMap::new(), &repo_history_id),
            GitTransportRuntimeCoverageV1::CoveredProducerRemoved
        );
        assert_eq!(
            runtime.classify_repo(&catalog, &assigned, &repo_history_id),
            GitTransportRuntimeCoverageV1::Current,
            "exact config-only assignment restoration reuses the marker row"
        );

        catalog
            .repo_histories
            .get_mut(&repo_history_id)
            .unwrap()
            .membership_generation = 8;
        assert_eq!(
            runtime.classify_repo(&catalog, &BTreeMap::new(), &repo_history_id),
            GitTransportRuntimeCoverageV1::CoverageStalePendingRecutover,
            "a watermark change without a member addition is generic staleness"
        );
        assert_eq!(
            runtime.classify_repo(&catalog, &assigned, &repo_history_id),
            GitTransportRuntimeCoverageV1::CoverageStalePendingRecutover
        );
        assert!(
            runtime
                .classify_repo(&catalog, &assigned, &repo_history_id)
                .transport_governed()
        );

        let added_project_id = ProjectId::parse("p_00000000000000000000000000000002").unwrap();
        let added_scope = PublishedScope::try_new("neutral-repo", "member").unwrap();
        catalog.projects.insert(
            added_project_id.clone(),
            CorpusProject {
                project_id: added_project_id,
                scope: ProjectScope::Published(added_scope.clone()),
                operator_aliases: BTreeSet::new(),
                nominated_aliases: BTreeSet::new(),
                display_name: "Added member".to_string(),
                created_at: "unix:2".to_string(),
                registered_at_compat: None,
                repo_history: Some(repo_history_id.clone()),
                languages: BTreeSet::new(),
            },
        );
        assert_eq!(
            runtime.classify_repo(&catalog, &assigned, &repo_history_id),
            GitTransportRuntimeCoverageV1::CoveredBlockedPendingRecutover
        );
        let all_assigned = BTreeMap::from([
            (scope, "producer-a".to_string()),
            (added_scope, "producer-a".to_string()),
        ]);
        assert_eq!(
            runtime.classify_repo(&catalog, &all_assigned, &repo_history_id),
            GitTransportRuntimeCoverageV1::CoverageStalePendingRecutover,
            "assignment repair cannot erase a committed membership watermark"
        );
        catalog.repo_histories.remove(&repo_history_id);
        assert_eq!(
            runtime.classify_repo(&catalog, &assigned, &repo_history_id),
            GitTransportRuntimeCoverageV1::CoverageStalePendingRecutover
        );
    }

    #[test]
    fn scope_round_trip_stays_stale_until_recutover_and_does_not_touch_an_unrelated_repo() {
        let (mut catalog, first_repo, first_scope, first_row) = coverage_fixture();
        let second_repo = RepoHistoryId::parse("rh_00000000000000000000000000000003").unwrap();
        let second_project = ProjectId::parse("p_00000000000000000000000000000003").unwrap();
        let second_scope = PublishedScope::try_new("unrelated-repo", ".").unwrap();
        catalog.repo_histories.insert(
            second_repo.clone(),
            RepoHistoryRecord {
                repo_history_id: second_repo.clone(),
                membership_generation: 3,
                authority: RepoHistoryAuthority::Recorded(
                    RecordedRepoAuthority::parse("unrelated-repo").unwrap(),
                ),
                primary_namespace: CommitNamespace::parse("unrelated-repo").unwrap(),
                compatibility_namespaces: BTreeSet::new(),
                materialization: RepoHistoryMaterialization::NotBuilt,
            },
        );
        catalog.projects.insert(
            second_project.clone(),
            CorpusProject {
                project_id: second_project.clone(),
                scope: ProjectScope::Published(second_scope.clone()),
                operator_aliases: BTreeSet::new(),
                nominated_aliases: BTreeSet::new(),
                display_name: "Unrelated fixture".to_string(),
                created_at: "unix:3".to_string(),
                registered_at_compat: None,
                repo_history: Some(second_repo.clone()),
                languages: BTreeSet::new(),
            },
        );
        catalog.validate().unwrap();
        let original_assignments = BTreeMap::from([
            (first_scope.clone(), "producer-a".to_string()),
            (second_scope.clone(), "producer-b".to_string()),
        ]);
        let projection = derive_repo_transport_grants(&catalog, &original_assignments);
        let RepoTransportGrantState::Granted {
            grant: second_grant,
        } = &projection.grants[&second_repo]
        else {
            panic!("the unrelated fixture grant must be complete")
        };
        let second_row = PredictedGitTransportCutoverRowV1 {
            repo_history_id: second_repo.clone(),
            grant_commitment: second_grant.commitment.clone(),
            membership_generation: 3,
            source_generation_id: "source-three".to_string(),
            p3_generation_id: format!("rhg_{}", "c".repeat(64)),
            history_parity_commitment: Sha256ValueV1::digest(b"history-three"),
            provenance_import_generations: BTreeMap::from([(
                second_project.clone(),
                "import-three".to_string(),
            )]),
            provenance_export_generations: BTreeMap::from([(
                second_project.clone(),
                "export-three".to_string(),
            )]),
            provenance_parity_commitments: BTreeMap::from([(
                second_project,
                Sha256ValueV1::digest(b"provenance-three"),
            )]),
            capability_baselines: first_row.capability_baselines.clone(),
        };
        let mut marker = marker_with_row(first_row.clone());
        marker.rows.push(second_row.clone());
        marker.rows.sort_by(|left, right| {
            left.repo_history_id
                .as_str()
                .cmp(right.repo_history_id.as_str())
        });
        marker.checksum_sha256 = marker_checksum(&marker).unwrap();
        let runtime = GitTransportCutoverRuntimeV1::from_marker(Some(marker));
        assert_eq!(
            runtime.classify_repo(&catalog, &original_assignments, &first_repo),
            GitTransportRuntimeCoverageV1::Current
        );
        assert_eq!(
            runtime.classify_repo(&catalog, &original_assignments, &second_repo),
            GitTransportRuntimeCoverageV1::Current
        );

        let migrated_scope = PublishedScope::try_new("neutral-repo", "migrated").unwrap();
        catalog.projects.get_mut(&project_id()).unwrap().scope =
            ProjectScope::Published(migrated_scope.clone());
        catalog
            .repo_histories
            .get_mut(&first_repo)
            .unwrap()
            .membership_generation = 8;
        let migrated_assignments = BTreeMap::from([
            (migrated_scope, "producer-a".to_string()),
            (second_scope.clone(), "producer-b".to_string()),
        ]);
        assert_eq!(
            runtime.classify_repo(&catalog, &migrated_assignments, &first_repo),
            GitTransportRuntimeCoverageV1::CoverageStalePendingRecutover
        );
        assert_eq!(
            runtime.classify_repo(&catalog, &migrated_assignments, &second_repo),
            GitTransportRuntimeCoverageV1::Current,
            "a source-repo migration cannot stale an unrelated marker row"
        );

        catalog.projects.get_mut(&project_id()).unwrap().scope =
            ProjectScope::Published(first_scope.clone());
        catalog
            .repo_histories
            .get_mut(&first_repo)
            .unwrap()
            .membership_generation = 9;
        assert_eq!(
            runtime.classify_repo(&catalog, &original_assignments, &first_repo),
            GitTransportRuntimeCoverageV1::CoverageStalePendingRecutover,
            "restoring assignment bytes cannot restore an advanced membership watermark"
        );
        assert_eq!(
            runtime.classify_repo(&catalog, &original_assignments, &second_repo),
            GitTransportRuntimeCoverageV1::Current
        );

        let restored_projection = derive_repo_transport_grants(&catalog, &original_assignments);
        let RepoTransportGrantState::Granted {
            grant: restored_grant,
        } = &restored_projection.grants[&first_repo]
        else {
            panic!("the restored fixture grant must be complete")
        };
        let mut replacement_row = first_row;
        replacement_row.membership_generation = 9;
        replacement_row.grant_commitment = restored_grant.commitment.clone();
        let mut replacement_marker = marker_with_row(replacement_row);
        replacement_marker.rows.push(second_row);
        replacement_marker.rows.sort_by(|left, right| {
            left.repo_history_id
                .as_str()
                .cmp(right.repo_history_id.as_str())
        });
        replacement_marker.checksum_sha256 = marker_checksum(&replacement_marker).unwrap();
        let replacement_runtime =
            GitTransportCutoverRuntimeV1::from_marker(Some(replacement_marker));
        assert_eq!(
            replacement_runtime.classify_repo(&catalog, &original_assignments, &first_repo),
            GitTransportRuntimeCoverageV1::Current
        );
        assert_eq!(
            replacement_runtime.classify_repo(&catalog, &original_assignments, &second_repo),
            GitTransportRuntimeCoverageV1::Current,
            "the newer marker must carry the unrelated row unchanged"
        );
    }

    #[test]
    fn predicted_marker_carries_predecessor_rows_byte_for_byte() {
        let (_catalog, _repo_history_id, _scope, row) = coverage_fixture();
        let predecessor = Sha256ValueV1::digest(b"predecessor");
        let predicted = predicted_marker(
            Some(predecessor.clone()),
            41,
            Sha256ValueV1::digest(b"inventory"),
            Sha256ValueV1::digest(b"resolution"),
            0,
            0,
            &[],
            std::slice::from_ref(&row),
        );
        assert_eq!(predicted.version, MARKER_VERSION);
        assert_eq!(predicted.predecessor_marker_checksum, Some(predecessor));
        assert_eq!(predicted.rows, vec![row]);
    }

    #[test]
    fn recutover_preserves_current_rows_and_replaces_only_proposed_rows() {
        let (catalog, first_repo, scope, first_old_row) = coverage_fixture();
        let second_repo = RepoHistoryId::parse("rh_00000000000000000000000000000002").unwrap();
        let mut second_row = first_old_row.clone();
        second_row.repo_history_id = second_repo.clone();
        second_row.source_generation_id = "source-two-stable".to_string();
        let mut predecessor = marker_with_row(first_old_row.clone());
        predecessor.rows.push(second_row.clone());
        predecessor.rows.sort_by(|left, right| {
            left.repo_history_id
                .as_str()
                .cmp(right.repo_history_id.as_str())
        });
        predecessor.checksum_sha256 = marker_checksum(&predecessor).unwrap();

        let grant = grant_for(&catalog, &scope, &first_repo);
        let mut replacement = minimal_repo_evidence(
            first_repo.clone(),
            GitTransportCutoverCoverageStatusV1::Proposed,
        );
        replacement.grant = Some(grant);
        replacement.history = Some(GitTransportHistoryEvidenceV1 {
            source_generation_id: "source-one-recutover".to_string(),
            source_head: "a".repeat(40),
            p3_generation_id: format!("rhg_{}", "b".repeat(64)),
            commit_document_count: 1,
            commit_document_commitment_sha256: "c".repeat(64),
            vector_input_count: 1,
            vector_input_commitment_sha256: "d".repeat(64),
            parity: GitTransportParityStatusV1::Equal,
        });
        let carried = minimal_repo_evidence(
            second_repo.clone(),
            GitTransportCutoverCoverageStatusV1::CarriedForwardCurrent,
        );
        let repos = vec![replacement, carried];

        let carried_rows = carried_forward_marker_rows(Some(&predecessor), &repos);
        assert_eq!(carried_rows, vec![second_row.clone()]);
        let predicted = predicted_marker(
            Some(predecessor.checksum_sha256.clone()),
            42,
            Sha256ValueV1::digest(b"recutover-inventory"),
            Sha256ValueV1::digest(b"recutover-resolution"),
            0,
            0,
            &repos,
            &carried_rows,
        );
        assert_eq!(predicted.rows.len(), 2);
        assert_eq!(
            predicted
                .rows
                .iter()
                .find(|row| row.repo_history_id == second_repo)
                .unwrap(),
            &second_row,
            "the unaffected marker row must remain byte-identical"
        );
        let first_new_row = predicted
            .rows
            .iter()
            .find(|row| row.repo_history_id == first_repo)
            .unwrap();
        assert_ne!(first_new_row, &first_old_row);
        assert_eq!(first_new_row.source_generation_id, "source-one-recutover");
    }

    #[test]
    fn carry_forward_statuses_keep_projection_taxonomy_and_receipt_counts_exact() {
        let (catalog, repo_history_id, scope, _row) = coverage_fixture();
        let record = &catalog.repo_histories[&repo_history_id];
        let grant = grant_for(&catalog, &scope, &repo_history_id);
        let baseline = GitTransportObservationBaselineV1 {
            sequence: 11,
            counters: Vec::new(),
            target_counters: Vec::new(),
        };
        for (runtime, report_status, category) in [
            (
                GitTransportRuntimeCoverageV1::Current,
                GitTransportCutoverCoverageStatusV1::CarriedForwardCurrent,
                GitTransportObservationCategoryV1::TransportCoveredPostBoundary,
            ),
            (
                GitTransportRuntimeCoverageV1::CoveredProducerRemoved,
                GitTransportCutoverCoverageStatusV1::CoveredProducerRemoved,
                GitTransportObservationCategoryV1::CoveredProducerRemoved,
            ),
            (
                GitTransportRuntimeCoverageV1::CoveredBlockedPendingRecutover,
                GitTransportCutoverCoverageStatusV1::CoveredBlockedPendingRecutover,
                GitTransportObservationCategoryV1::CoveredBlockedPendingRecutover,
            ),
            (
                GitTransportRuntimeCoverageV1::CoverageStalePendingRecutover,
                GitTransportCutoverCoverageStatusV1::CoverageStalePendingRecutover,
                GitTransportObservationCategoryV1::CoverageStalePendingRecutover,
            ),
        ] {
            assert_eq!(covered_report_status(runtime), report_status);
            let is_current =
                report_status == GitTransportCutoverCoverageStatusV1::CarriedForwardCurrent;
            let mut repo = carried_repo_evidence(
                record,
                is_current.then_some(&grant),
                (!is_current).then_some(RepoTransportBlockedReason::MissingAssignment),
                report_status.clone(),
                &catalog,
            );
            repo.capability_baselines = capability_baselines(&repo, &baseline);
            assert!(
                repo.capability_baselines
                    .iter()
                    .all(|row| row.active_category == category)
            );
            let state = if is_current {
                RepoTransportGrantState::Granted {
                    grant: grant.clone(),
                }
            } else {
                RepoTransportGrantState::Blocked {
                    reason: RepoTransportBlockedReason::MissingAssignment,
                }
            };
            assert!(report_repo_matches_projection(&repo, &state));
            assert!(!capture_evidence_requires_recheck(&report_status));
            assert_eq!(preflight_repo_counts(&[repo]), (0, 0, 0));
        }
        assert_eq!(
            covered_report_status(GitTransportRuntimeCoverageV1::Uncovered),
            GitTransportCutoverCoverageStatusV1::BlockedPublishedNeverCovered
        );
    }

    #[test]
    fn covered_row_observation_gate_rejects_granted_and_denied_deltas() {
        let (_catalog, _repo_history_id, _scope, row) = coverage_fixture();
        let empty = GitTransportObservationBaselineV1 {
            sequence: 0,
            counters: Vec::new(),
            target_counters: Vec::new(),
        };
        let observations = verify_row_observations(
            &row,
            GitTransportObservationCategoryV1::TransportCoveredPostBoundary,
            &empty,
        )
        .unwrap();
        assert_eq!(observations.len(), 2);

        for outcome in [
            crate::checkout_access::CheckoutAccessOutcome::Granted,
            crate::checkout_access::CheckoutAccessOutcome::Denied,
        ] {
            let current = GitTransportObservationBaselineV1 {
                sequence: 1,
                counters: Vec::new(),
                target_counters: vec![CheckoutAccessTargetCounter {
                    project_id: project_id().as_str().to_string(),
                    kind: CheckoutAccessKind::GitHistory,
                    source_lane: crate::checkout_access::CheckoutAccessSourceLane::NativeAttachment,
                    outcome,
                    count: 1,
                    last_sequence: 1,
                    last_unix_secs: 1,
                }],
            };
            assert_eq!(
                verify_row_observations(
                    &row,
                    GitTransportObservationCategoryV1::TransportCoveredPostBoundary,
                    &current,
                )
                .unwrap_err()
                .code,
                "error.git_transport_cutover_observation_delta"
            );
        }
    }
}
