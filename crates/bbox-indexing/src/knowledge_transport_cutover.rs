//! Offline knowledge transport overlap report, cutover, and runtime gate.
//!
//! A marker row is durable operator authority. Once present it is never
//! interpreted as permission to reopen a checkout adapter: scope, producer,
//! or accepted-source drift makes the row pending re-cutover while preserving
//! the no-fallback boundary.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use bbox_config::config::Config;
use bbox_corpus_core::identity::PublishedScope;
use bbox_corpus_core::json_store::{
    NofollowDirectory, acquire_store_lock_nofollow, atomic_write_bytes_locked,
};
use bbox_corpus_core::project_catalog::{ProjectId, ProjectScope};
use bbox_knowledge_source_store::{
    KnowledgeSourceStore, ReadyProvisionalWorkspace, ReadyPublicationCandidate,
    ReadyPublicationFile, StoreLimits,
};
use serde::{Deserialize, Serialize};

use crate::accepted_publication_runtime::{
    AcceptedPublicationRuntime, AcceptedPublicationSourceBinding, PublishRequest,
    PublishSourceFile, PublishSources, PublisherPublishMode, VerifiedAcceptedPublication,
};
use crate::checkout_access::{
    CheckoutAccessKind, CheckoutAccessObservations, CheckoutAccessOutcome,
    CheckoutAccessTargetCounter,
};
use crate::knowledge_transport_observations::KnowledgeTransportObservationSnapshotV1;
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
pub const KNOWLEDGE_TRANSPORT_CUTOVER_MARKER_FILE: &str = "knowledge-transport-cutover-marker.json";
pub const KNOWLEDGE_TRANSPORT_CUTOVER_RECEIPT_FILE: &str =
    "knowledge-transport-cutover-receipt.json";
pub const MAX_KNOWLEDGE_TRANSPORT_CUTOVER_MARKER_BYTES: usize = 16 * 1024 * 1024;
pub const MAX_KNOWLEDGE_TRANSPORT_CUTOVER_RECEIPT_BYTES: usize = 1024 * 1024;

const CUTOVER_CAPABILITIES: [CheckoutAccessKind; 4] = [
    CheckoutAccessKind::PublisherConfigTreeRead,
    CheckoutAccessKind::KnowledgeGapOverlayRead,
    CheckoutAccessKind::ArtifactWatchDiscovery,
    CheckoutAccessKind::RepositoryMutation,
];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum KnowledgeTransportCutoverStatusV1 {
    Clean,
    Refused,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum KnowledgeTransportCoverageStatusV1 {
    Proposed,
    CarriedForwardCurrent,
    BlockedPublishedNeverCovered,
    CoveredProducerRemoved,
    ScopeMigrationPendingRecutover,
    ProducerAssignmentPendingRecutover,
    AcceptedSourcePendingRecutover,
    Refused,
}

/// Runtime state for one Published project. Only `Uncovered` may use the
/// local adapter; every state backed by a marker row is transport-governed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum KnowledgeTransportRuntimeCoverageV1 {
    Uncovered,
    Current,
    CoveredProducerRemoved,
    ScopeMigrationPendingRecutover,
    ProducerAssignmentPendingRecutover,
    AcceptedSourcePendingRecutover,
}

impl KnowledgeTransportRuntimeCoverageV1 {
    pub fn transport_governed(self) -> bool {
        !matches!(self, Self::Uncovered)
    }

    pub fn current(self) -> bool {
        matches!(self, Self::Current)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct KnowledgeTransportCapabilityBaselineV1 {
    pub capability: CheckoutAccessKind,
    pub granted: u64,
    pub denied: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct KnowledgeTransportPublicationParityV1 {
    pub accepted_generation_id: String,
    pub accepted_generation_sha256: String,
    pub accepted_pointer_sha256: String,
    pub source_generation_id: String,
    pub source_generation_sha256: String,
    pub knowledge_manifest_sha256: String,
    pub gap_manifest_sha256: String,
    pub rebuilt_generation_id: String,
    pub rebuilt_generation_sha256: String,
    pub equal: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct KnowledgeTransportWorkspaceParityV1 {
    pub workspace_id: String,
    pub source_generation_id: String,
    pub sequence: u64,
    pub accepted_generation_id: String,
    pub lease_expires_unix_secs: u64,
    pub knowledge_snapshot_id: String,
    pub gap_snapshot_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct KnowledgeTransportProjectEvidenceV1 {
    pub project_id: ProjectId,
    pub scope: PublishedScope,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub producer_id: Option<String>,
    pub coverage_status: KnowledgeTransportCoverageStatusV1,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub publication_parity: Option<KnowledgeTransportPublicationParityV1>,
    pub prepared_upload_count: u64,
    pub unfinished_finalize_journal_count: u64,
    pub expired_workspace_ids: Vec<String>,
    pub workspace_parity: Vec<KnowledgeTransportWorkspaceParityV1>,
    pub shadow_comparisons:
        Vec<crate::knowledge_transport_observations::KnowledgeTransportShadowComparisonV1>,
    pub capability_baselines: Vec<KnowledgeTransportCapabilityBaselineV1>,
    pub observation_window_start_sequence: u64,
    pub observation_window_end_sequence: u64,
    pub defects: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PredictedKnowledgeTransportCutoverRowV1 {
    pub project_id: ProjectId,
    pub scope: PublishedScope,
    pub producer_id: String,
    pub grant_commitment: Sha256ValueV1,
    pub accepted_generation_id: String,
    pub accepted_generation_sha256: String,
    pub accepted_pointer_sha256: String,
    pub source_generation_id: String,
    pub source_generation_sha256: String,
    pub publication_parity_commitment: Sha256ValueV1,
    pub parity_workspace_ids: Vec<String>,
    pub workspace_parity_commitment: Sha256ValueV1,
    pub shadow_observation_commitment: Sha256ValueV1,
    pub capability_baselines: Vec<KnowledgeTransportCapabilityBaselineV1>,
    pub observation_window_start_sequence: u64,
    pub observation_window_end_sequence: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PredictedKnowledgeTransportCutoverMarkerV1 {
    pub version: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub predecessor_marker_checksum: Option<Sha256ValueV1>,
    pub predecessor_catalog_epoch: u64,
    pub inventory_hash: Sha256ValueV1,
    pub resolution_artifact_hash: Sha256ValueV1,
    pub observation_snapshot_hash: Sha256ValueV1,
    pub rows: Vec<PredictedKnowledgeTransportCutoverRowV1>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct KnowledgeTransportCutoverReportV1 {
    pub version: u32,
    pub generated_at: String,
    pub status: KnowledgeTransportCutoverStatusV1,
    pub catalog_epoch: u64,
    pub catalog_sha256: String,
    pub checkout_observation_sequence: u64,
    pub knowledge_observations: KnowledgeTransportObservationSnapshotV1,
    pub inventory_hash: Sha256ValueV1,
    pub resolution_artifact_hash: Sha256ValueV1,
    pub projects: Vec<KnowledgeTransportProjectEvidenceV1>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub carried_forward_rows: Vec<PredictedKnowledgeTransportCutoverRowV1>,
    pub predicted_marker: PredictedKnowledgeTransportCutoverMarkerV1,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct KnowledgeTransportCutoverResolutionV1 {
    pub version: u32,
    pub inventory_hash: Sha256ValueV1,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub blocked_project_acknowledgements: BTreeMap<ProjectId, String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct KnowledgeTransportCutoverPreflightReceiptV1 {
    pub version: u32,
    pub status: KnowledgeTransportCutoverStatusV1,
    pub catalog_epoch: u64,
    pub inventory_hash: Sha256ValueV1,
    pub report_artifact_hash: Sha256ValueV1,
    pub resolution_artifact_hash: Sha256ValueV1,
    pub proposed_project_count: u64,
    pub blocked_project_count: u64,
    pub refused_project_count: u64,
}

pub struct KnowledgeTransportCutoverPreflightRequestV1 {
    pub layout: ProjectCatalogMigrationResolvedLayoutV1,
    pub config: Config,
    pub report_path: PathBuf,
    pub resolution_path: PathBuf,
    pub generated_at: String,
}

pub struct KnowledgeTransportCutoverApplyRequestV1 {
    pub layout: ProjectCatalogMigrationResolvedLayoutV1,
    pub config: Config,
    pub report_path: PathBuf,
    pub resolution_path: PathBuf,
    pub applied_at: String,
}

pub struct KnowledgeTransportCutoverVerifyRequestV1 {
    pub layout: ProjectCatalogMigrationResolvedLayoutV1,
    pub config: Config,
    pub verified_at: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct KnowledgeTransportCutoverMarkerV1 {
    pub version: u32,
    pub applied_at: String,
    pub report_artifact_hash: Sha256ValueV1,
    pub resolution_artifact_hash: Sha256ValueV1,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub predecessor_marker_checksum: Option<Sha256ValueV1>,
    pub predecessor_catalog_epoch: u64,
    pub inventory_hash: Sha256ValueV1,
    pub observation_snapshot_hash: Sha256ValueV1,
    pub rows: Vec<PredictedKnowledgeTransportCutoverRowV1>,
    pub checksum_sha256: Sha256ValueV1,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct KnowledgeTransportCutoverReceiptV1 {
    pub version: u32,
    pub applied_at: String,
    pub verified_at: String,
    pub marker_checksum_sha256: Sha256ValueV1,
    pub report_artifact_hash: Sha256ValueV1,
    pub resolution_artifact_hash: Sha256ValueV1,
    pub covered_project_count: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct KnowledgeTransportCutoverVerificationRowV1 {
    pub project_id: ProjectId,
    pub coverage: KnowledgeTransportRuntimeCoverageV1,
    pub capability_observations: Vec<KnowledgeTransportCapabilityBaselineV1>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct KnowledgeTransportCutoverVerificationReceiptV1 {
    pub version: u32,
    pub marker_checksum_sha256: Sha256ValueV1,
    pub covered_project_count: u64,
    pub current_project_count: u64,
    pub rows: Vec<KnowledgeTransportCutoverVerificationRowV1>,
}

#[derive(Debug, Clone, Default)]
pub struct KnowledgeTransportCutoverRuntimeV1 {
    marker: Option<KnowledgeTransportCutoverMarkerV1>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KnowledgeTransportCutoverError {
    pub code: &'static str,
    pub message: String,
}

impl std::fmt::Display for KnowledgeTransportCutoverError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}: {}", self.code, self.message)
    }
}

impl std::error::Error for KnowledgeTransportCutoverError {}

type CutoverResult<T> = Result<T, KnowledgeTransportCutoverError>;

fn error(code: &'static str, message: impl std::fmt::Display) -> KnowledgeTransportCutoverError {
    KnowledgeTransportCutoverError {
        code,
        message: message.to_string(),
    }
}

pub fn knowledge_transport_cutover_marker_path(state_dir: &Path) -> PathBuf {
    state_dir.join(KNOWLEDGE_TRANSPORT_CUTOVER_MARKER_FILE)
}

pub fn knowledge_transport_cutover_receipt_path(state_dir: &Path) -> PathBuf {
    state_dir.join(KNOWLEDGE_TRANSPORT_CUTOVER_RECEIPT_FILE)
}

fn marker_checksum(marker: &KnowledgeTransportCutoverMarkerV1) -> CutoverResult<Sha256ValueV1> {
    #[derive(Serialize)]
    struct Body<'a> {
        version: u32,
        applied_at: &'a str,
        report_artifact_hash: &'a Sha256ValueV1,
        resolution_artifact_hash: &'a Sha256ValueV1,
        predecessor_marker_checksum: &'a Option<Sha256ValueV1>,
        predecessor_catalog_epoch: u64,
        inventory_hash: &'a Sha256ValueV1,
        observation_snapshot_hash: &'a Sha256ValueV1,
        rows: &'a [PredictedKnowledgeTransportCutoverRowV1],
    }
    serde_json::to_vec(&Body {
        version: marker.version,
        applied_at: &marker.applied_at,
        report_artifact_hash: &marker.report_artifact_hash,
        resolution_artifact_hash: &marker.resolution_artifact_hash,
        predecessor_marker_checksum: &marker.predecessor_marker_checksum,
        predecessor_catalog_epoch: marker.predecessor_catalog_epoch,
        inventory_hash: &marker.inventory_hash,
        observation_snapshot_hash: &marker.observation_snapshot_hash,
        rows: &marker.rows,
    })
    .map(|bytes| Sha256ValueV1::digest(&bytes))
    .map_err(|cause| error("error.knowledge_transport_cutover_marker", cause))
}

fn rows_are_canonical(rows: &[PredictedKnowledgeTransportCutoverRowV1]) -> bool {
    rows.windows(2)
        .all(|pair| pair[0].project_id < pair[1].project_id)
        && rows.iter().all(|row| {
            valid_producer_id(&row.producer_id)
                && is_sha256(&row.accepted_generation_id)
                && is_sha256(&row.accepted_generation_sha256)
                && is_sha256(&row.accepted_pointer_sha256)
                && bbox_knowledge_source::validate_publication_generation_id(
                    &row.source_generation_id,
                )
                .is_ok()
                && is_sha256(&row.source_generation_sha256)
                && row.observation_window_start_sequence <= row.observation_window_end_sequence
                && row
                    .capability_baselines
                    .iter()
                    .map(|baseline| baseline.capability)
                    .eq(CUTOVER_CAPABILITIES)
                && row
                    .parity_workspace_ids
                    .windows(2)
                    .all(|pair| pair[0] < pair[1])
                && row
                    .parity_workspace_ids
                    .iter()
                    .all(|workspace| bro_core::WorkspaceId::parse(workspace.clone()).is_ok())
        })
}

fn is_sha256(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn valid_producer_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
}

pub fn decode_knowledge_transport_cutover_marker_v1(
    bytes: &[u8],
) -> CutoverResult<KnowledgeTransportCutoverMarkerV1> {
    if bytes.is_empty() || bytes.len() > MAX_KNOWLEDGE_TRANSPORT_CUTOVER_MARKER_BYTES {
        return Err(error(
            "error.knowledge_transport_cutover_marker",
            "knowledge transport cutover marker is empty or oversized",
        ));
    }
    let marker: KnowledgeTransportCutoverMarkerV1 = serde_json::from_slice(bytes)
        .map_err(|cause| error("error.knowledge_transport_cutover_marker", cause))?;
    if marker.version != MARKER_VERSION
        || marker.applied_at.trim().is_empty()
        || marker.applied_at.len() > 128
        || !rows_are_canonical(&marker.rows)
        || marker_checksum(&marker)? != marker.checksum_sha256
    {
        return Err(error(
            "error.knowledge_transport_cutover_marker_identity",
            "knowledge transport cutover marker identity does not match its contents",
        ));
    }
    Ok(marker)
}

fn read_state_file(path: &Path, max_bytes: usize, label: &str) -> CutoverResult<Option<Vec<u8>>> {
    let parent = path.parent().ok_or_else(|| {
        error(
            "error.knowledge_transport_cutover_unsafe_layout",
            format!("{label} has no parent"),
        )
    })?;
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| {
            error(
                "error.knowledge_transport_cutover_unsafe_layout",
                format!("{label} has an invalid filename"),
            )
        })?;
    let Some(directory) = NofollowDirectory::open_existing(parent)
        .map_err(|cause| error("error.knowledge_transport_cutover_unsafe_layout", cause))?
    else {
        return Ok(None);
    };
    let bytes = directory
        .read_regular(name, max_bytes, label)
        .map_err(|cause| error("error.knowledge_transport_cutover_io", cause))?;
    directory
        .ensure_still_current()
        .map_err(|cause| error("error.knowledge_transport_cutover_unsafe_layout", cause))?;
    Ok(bytes)
}

pub fn load_knowledge_transport_cutover_marker_optional(
    state_dir: &Path,
) -> CutoverResult<Option<KnowledgeTransportCutoverMarkerV1>> {
    read_state_file(
        &knowledge_transport_cutover_marker_path(state_dir),
        MAX_KNOWLEDGE_TRANSPORT_CUTOVER_MARKER_BYTES,
        "knowledge transport cutover marker",
    )?
    .map(|bytes| decode_knowledge_transport_cutover_marker_v1(&bytes))
    .transpose()
}

fn decode_receipt(bytes: &[u8]) -> CutoverResult<KnowledgeTransportCutoverReceiptV1> {
    if bytes.is_empty() || bytes.len() > MAX_KNOWLEDGE_TRANSPORT_CUTOVER_RECEIPT_BYTES {
        return Err(error(
            "error.knowledge_transport_cutover_receipt",
            "knowledge transport cutover receipt is empty or oversized",
        ));
    }
    let receipt: KnowledgeTransportCutoverReceiptV1 = serde_json::from_slice(bytes)
        .map_err(|cause| error("error.knowledge_transport_cutover_receipt", cause))?;
    if receipt.version != RECEIPT_VERSION
        || receipt.applied_at.trim().is_empty()
        || receipt.verified_at.trim().is_empty()
        || receipt.applied_at.len() > 128
        || receipt.verified_at.len() > 128
    {
        return Err(error(
            "error.knowledge_transport_cutover_receipt",
            "knowledge transport cutover receipt is invalid",
        ));
    }
    Ok(receipt)
}

fn load_current_marker_with_receipt(
    state_dir: &Path,
) -> CutoverResult<Option<KnowledgeTransportCutoverMarkerV1>> {
    let marker = load_knowledge_transport_cutover_marker_optional(state_dir)?;
    let receipt = read_state_file(
        &knowledge_transport_cutover_receipt_path(state_dir),
        MAX_KNOWLEDGE_TRANSPORT_CUTOVER_RECEIPT_BYTES,
        "knowledge transport cutover receipt",
    )?
    .map(|bytes| decode_receipt(&bytes))
    .transpose()?;
    match (marker, receipt) {
        (None, None) => Ok(None),
        (Some(marker), Some(receipt))
            if receipt.marker_checksum_sha256 == marker.checksum_sha256
                && receipt.report_artifact_hash == marker.report_artifact_hash
                && receipt.resolution_artifact_hash == marker.resolution_artifact_hash
                && receipt.applied_at == marker.applied_at
                && receipt.covered_project_count == marker.rows.len() as u64 =>
        {
            Ok(Some(marker))
        }
        (Some(_), None) => Err(error(
            "error.knowledge_transport_cutover_verify_required",
            "the current marker has no matching receipt; run cutover --verify",
        )),
        (None, Some(_)) => Err(error(
            "error.knowledge_transport_cutover_marker_missing",
            "a cutover receipt exists but the current marker is missing",
        )),
        (Some(_), Some(_)) => Err(error(
            "error.knowledge_transport_cutover_current_identity",
            "the current marker and receipt identify different cutovers",
        )),
    }
}

impl KnowledgeTransportCutoverRuntimeV1 {
    pub fn open(state_dir: &Path) -> CutoverResult<Self> {
        Ok(Self {
            marker: load_current_marker_with_receipt(state_dir)?,
        })
    }

    pub fn from_marker(marker: Option<KnowledgeTransportCutoverMarkerV1>) -> Self {
        Self { marker }
    }

    pub fn marker(&self) -> Option<&KnowledgeTransportCutoverMarkerV1> {
        self.marker.as_ref()
    }

    pub fn covers_project(&self, project_id: &ProjectId) -> bool {
        self.row(project_id).is_some()
    }

    pub fn covers_project_str(&self, project_id: &str) -> bool {
        ProjectId::parse(project_id.to_string())
            .ok()
            .is_some_and(|project_id| self.covers_project(&project_id))
    }

    fn row(&self, project_id: &ProjectId) -> Option<&PredictedKnowledgeTransportCutoverRowV1> {
        let marker = self.marker.as_ref()?;
        marker
            .rows
            .binary_search_by(|row| row.project_id.cmp(project_id))
            .ok()
            .map(|index| &marker.rows[index])
    }

    pub fn classify_project(
        &self,
        catalog: &bbox_corpus_core::project_catalog::CatalogSnapshotV2,
        assignments: &BTreeMap<PublishedScope, String>,
        project_id: &ProjectId,
        accepted: Option<&VerifiedAcceptedPublication>,
    ) -> KnowledgeTransportRuntimeCoverageV1 {
        let Some(row) = self.row(project_id) else {
            return KnowledgeTransportRuntimeCoverageV1::Uncovered;
        };
        let Some(project) = catalog.projects.get(project_id) else {
            return KnowledgeTransportRuntimeCoverageV1::ScopeMigrationPendingRecutover;
        };
        let ProjectScope::Published(scope) = &project.scope else {
            return KnowledgeTransportRuntimeCoverageV1::ScopeMigrationPendingRecutover;
        };
        if scope != &row.scope {
            return KnowledgeTransportRuntimeCoverageV1::ScopeMigrationPendingRecutover;
        }
        let Some(producer_id) = assignments.get(scope) else {
            return KnowledgeTransportRuntimeCoverageV1::CoveredProducerRemoved;
        };
        if producer_id != &row.producer_id
            || grant_commitment(project_id, scope, producer_id) != row.grant_commitment
        {
            return KnowledgeTransportRuntimeCoverageV1::ProducerAssignmentPendingRecutover;
        }
        let Some(accepted) = accepted else {
            return KnowledgeTransportRuntimeCoverageV1::AcceptedSourcePendingRecutover;
        };
        // Tolerant advancement (the same model 1c3d334b gave the code-source
        // marker): the row pins stable producer/scope authority, not an
        // immutable evidence generation. An accepted publication that
        // advanced through the same authenticated producer channel stays
        // current, because the transport is the only way content arrives.
        // The pinned generation/pointer hashes remain in the row as cutover
        // evidence but no longer gate currency; a missing accepted
        // publication or a non-producer binding still pend re-cutover.
        if accepted.content_stamp().accepted_scope() != &row.scope {
            return KnowledgeTransportRuntimeCoverageV1::AcceptedSourcePendingRecutover;
        }
        match accepted.binding_stamp().source() {
            AcceptedPublicationSourceBinding::Producer { producer_id, .. }
                if producer_id == &row.producer_id =>
            {
                KnowledgeTransportRuntimeCoverageV1::Current
            }
            _ => KnowledgeTransportRuntimeCoverageV1::AcceptedSourcePendingRecutover,
        }
    }
}

pub struct ProjectCatalogKnowledgeTransportCutoverFacadeV1;

impl ProjectCatalogKnowledgeTransportCutoverFacadeV1 {
    pub fn preflight(
        request: KnowledgeTransportCutoverPreflightRequestV1,
    ) -> CutoverResult<KnowledgeTransportCutoverPreflightReceiptV1> {
        validate_artifact_set(
            &request.layout,
            &request.report_path,
            &request.resolution_path,
            None,
        )
        .map_err(|cause| error("error.knowledge_transport_cutover_unsafe_layout", cause))?;
        validate_timestamp(&request.generated_at, "preflight")?;
        require_enabled(&request.config)?;
        let store = ProjectCatalogStore::open_existing(request.layout.projects_path())
            .map_err(|cause| error("error.knowledge_transport_cutover_catalog", cause))?;
        let state = store
            .snapshot()
            .map_err(|cause| error("error.knowledge_transport_cutover_catalog", cause))?;
        let assignments = configured_assignments(&request.config)?;
        validate_assignments_resolve(state.catalog(), &assignments)?;
        let predecessor = load_current_marker_with_receipt(&request.layout.state_dir)?;
        let checkout_health = CheckoutAccessObservations::open(
            request
                .layout
                .bro_home
                .join("checkout-access-observations.json"),
        )
        .map_err(|cause| error("error.knowledge_transport_cutover_observations", cause))?
        .health();
        let knowledge_observations =
            crate::knowledge_transport_observations::KnowledgeTransportObservationsV1::open(
                request
                    .layout
                    .bro_home
                    .join("knowledge-transport-observations.json"),
            )
            .map_err(|cause| error("error.knowledge_transport_cutover_observations", cause))?
            .snapshot();
        let accepted = AcceptedPublicationRuntime::open_global(request.layout.projects_path())
            .map_err(|cause| error("error.knowledge_transport_cutover_accepted", cause))?;
        let source_store = open_source_store(&request.layout, &request.config)?;
        let runtime = KnowledgeTransportCutoverRuntimeV1::from_marker(predecessor.clone());

        let mut projects = Vec::new();
        for (project_id, project) in &state.catalog().projects {
            let ProjectScope::Published(scope) = &project.scope else {
                continue;
            };
            let producer_id = assignments.get(scope).cloned();
            let existing_coverage = runtime.classify_project(
                state.catalog(),
                &assignments,
                project_id,
                accepted.load_verified(project_id).ok().as_ref(),
            );
            let Some(producer_id) = producer_id else {
                let mut evidence = project_evidence_without_assignment(
                    project_id,
                    scope,
                    existing_coverage,
                    &checkout_health.target_counters,
                    &knowledge_observations,
                );
                refuse_post_boundary_checkout_observations(&runtime, &mut evidence);
                projects.push(evidence);
                continue;
            };
            if existing_coverage == KnowledgeTransportRuntimeCoverageV1::Current {
                let mut evidence = project_evidence_base(
                    project_id,
                    scope,
                    Some(producer_id),
                    KnowledgeTransportCoverageStatusV1::CarriedForwardCurrent,
                    &checkout_health.target_counters,
                    &knowledge_observations,
                );
                refuse_post_boundary_checkout_observations(&runtime, &mut evidence);
                projects.push(evidence);
                continue;
            }
            let mut evidence = capture_project_evidence(
                project_id,
                scope,
                producer_id,
                existing_coverage,
                &accepted,
                source_store.as_ref(),
                &checkout_health.target_counters,
                &knowledge_observations,
            );
            refuse_post_boundary_checkout_observations(&runtime, &mut evidence);
            projects.push(evidence);
        }
        projects.sort_by(|left, right| left.project_id.cmp(&right.project_id));
        let replacement_ids = projects
            .iter()
            .filter(|project| {
                project.coverage_status == KnowledgeTransportCoverageStatusV1::Proposed
            })
            .map(|project| project.project_id.clone())
            .collect::<BTreeSet<_>>();
        let carried_forward_rows = predecessor
            .as_ref()
            .map(|marker| {
                marker
                    .rows
                    .iter()
                    .filter(|row| !replacement_ids.contains(&row.project_id))
                    .cloned()
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        let inventory_hash = inventory_hash(
            state.epoch(),
            state.catalog_sha256(),
            checkout_health.sequence,
            &knowledge_observations,
            &projects,
            &carried_forward_rows,
        )?;
        let (resolution, resolution_bytes, unresolved_blocked_projects) =
            load_or_create_resolution(&request.resolution_path, inventory_hash.clone(), &projects)?;
        let resolution_artifact_hash = Sha256ValueV1::digest(&resolution_bytes);
        let predicted_marker = predicted_marker(
            predecessor
                .as_ref()
                .map(|marker| marker.checksum_sha256.clone()),
            state.epoch(),
            inventory_hash.clone(),
            resolution_artifact_hash.clone(),
            &knowledge_observations,
            &projects,
            &carried_forward_rows,
        )?;
        let status = if !unresolved_blocked_projects.is_empty()
            || projects.iter().any(|project| {
                project.coverage_status == KnowledgeTransportCoverageStatusV1::Refused
            }) {
            KnowledgeTransportCutoverStatusV1::Refused
        } else {
            KnowledgeTransportCutoverStatusV1::Clean
        };
        let report = KnowledgeTransportCutoverReportV1 {
            version: REPORT_VERSION,
            generated_at: request.generated_at,
            status,
            catalog_epoch: state.epoch(),
            catalog_sha256: state.catalog_sha256().to_string(),
            checkout_observation_sequence: checkout_health.sequence,
            knowledge_observations,
            inventory_hash: inventory_hash.clone(),
            resolution_artifact_hash: resolution_artifact_hash.clone(),
            projects,
            carried_forward_rows,
            predicted_marker,
        };
        let report_bytes = serde_json::to_vec(&report)
            .map_err(|cause| error("error.knowledge_transport_cutover_artifact", cause))?;
        if report_bytes.len() > MAX_PROJECT_CATALOG_REPORT_BYTES {
            return Err(error(
                "error.knowledge_transport_cutover_artifact",
                "knowledge transport report exceeds the artifact bound",
            ));
        }
        write_artifact_if_absent(
            &request.resolution_path,
            &resolution_bytes,
            MAX_PROJECT_CATALOG_RESOLUTION_BYTES,
            "knowledge transport cutover resolution",
        )
        .map_err(|cause| error("error.knowledge_transport_cutover_artifact", cause))?;
        write_artifact_replacing(
            &request.report_path,
            &report_bytes,
            MAX_PROJECT_CATALOG_REPORT_BYTES,
            "knowledge transport cutover report",
        )
        .map_err(|cause| error("error.knowledge_transport_cutover_artifact", cause))?;
        let proposed_project_count = count_status(
            &report.projects,
            KnowledgeTransportCoverageStatusV1::Proposed,
        );
        let blocked_project_count = count_status(
            &report.projects,
            KnowledgeTransportCoverageStatusV1::BlockedPublishedNeverCovered,
        );
        let refused_project_count = count_status(
            &report.projects,
            KnowledgeTransportCoverageStatusV1::Refused,
        );
        let _ = resolution;
        Ok(KnowledgeTransportCutoverPreflightReceiptV1 {
            version: REPORT_VERSION,
            status: report.status,
            catalog_epoch: report.catalog_epoch,
            inventory_hash,
            report_artifact_hash: Sha256ValueV1::digest(&report_bytes),
            resolution_artifact_hash,
            proposed_project_count,
            blocked_project_count,
            refused_project_count,
        })
    }

    pub fn apply(
        request: KnowledgeTransportCutoverApplyRequestV1,
    ) -> CutoverResult<KnowledgeTransportCutoverVerificationReceiptV1> {
        validate_artifact_set(
            &request.layout,
            &request.report_path,
            &request.resolution_path,
            None,
        )
        .map_err(|cause| error("error.knowledge_transport_cutover_unsafe_layout", cause))?;
        validate_timestamp(&request.applied_at, "apply")?;
        let report_bytes = read_artifact_required(
            &request.report_path,
            MAX_PROJECT_CATALOG_REPORT_BYTES,
            "knowledge transport cutover report",
        )
        .map_err(|cause| error("error.knowledge_transport_cutover_artifact", cause))?;
        let resolution_bytes = read_artifact_required(
            &request.resolution_path,
            MAX_PROJECT_CATALOG_RESOLUTION_BYTES,
            "knowledge transport cutover resolution",
        )
        .map_err(|cause| error("error.knowledge_transport_cutover_artifact", cause))?;
        let report: KnowledgeTransportCutoverReportV1 = serde_json::from_slice(&report_bytes)
            .map_err(|cause| error("error.knowledge_transport_cutover_artifact", cause))?;
        let resolution: KnowledgeTransportCutoverResolutionV1 =
            serde_json::from_slice(&resolution_bytes)
                .map_err(|cause| error("error.knowledge_transport_cutover_resolution", cause))?;
        if report.version != REPORT_VERSION
            || resolution.version != RESOLUTION_VERSION
            || report.status != KnowledgeTransportCutoverStatusV1::Clean
            || resolution.inventory_hash != report.inventory_hash
            || Sha256ValueV1::digest(&resolution_bytes) != report.resolution_artifact_hash
        {
            return Err(error(
                "error.knowledge_transport_cutover_apply_refused",
                "reviewed artifacts are not a clean mutually bound cutover pair",
            ));
        }
        let unresolved = validate_blocked_project_acknowledgements(&resolution, &report.projects)?;
        if !unresolved.is_empty() {
            return Err(error(
                "error.knowledge_transport_cutover_apply_refused",
                format!(
                    "{} blocked Published projects lack explicit operator acknowledgement",
                    unresolved.len()
                ),
            ));
        }
        let store = ProjectCatalogStore::open_existing(request.layout.projects_path())
            .map_err(|cause| error("error.knowledge_transport_cutover_catalog", cause))?;
        let _mutation_lock = acquire_store_lock_nofollow(request.layout.projects_path())
            .map_err(|cause| error("error.knowledge_transport_cutover_lock", cause))?;
        let predecessor = load_current_marker_with_receipt(&request.layout.state_dir)?;
        if predecessor
            .as_ref()
            .map(|marker| marker.checksum_sha256.clone())
            != report.predicted_marker.predecessor_marker_checksum
        {
            return Err(error(
                "error.knowledge_transport_cutover_predecessor_changed",
                "the current knowledge transport marker changed after preflight",
            ));
        }
        let replacement_ids = report
            .projects
            .iter()
            .filter(|project| {
                project.coverage_status == KnowledgeTransportCoverageStatusV1::Proposed
            })
            .map(|project| project.project_id.clone())
            .collect::<BTreeSet<_>>();
        let expected_carried_rows = predecessor
            .as_ref()
            .map(|marker| {
                marker
                    .rows
                    .iter()
                    .filter(|row| !replacement_ids.contains(&row.project_id))
                    .cloned()
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        if report.carried_forward_rows != expected_carried_rows {
            return Err(error(
                "error.knowledge_transport_cutover_predecessor_changed",
                "reviewed carry-forward rows do not match the current predecessor",
            ));
        }
        let recomputed_inventory = inventory_hash(
            report.catalog_epoch,
            &report.catalog_sha256,
            report.checkout_observation_sequence,
            &report.knowledge_observations,
            &report.projects,
            &report.carried_forward_rows,
        )?;
        if recomputed_inventory != report.inventory_hash {
            return Err(error(
                "error.knowledge_transport_cutover_artifact_identity",
                "reviewed report inventory hash does not match its contents",
            ));
        }
        let recomputed_marker = predicted_marker(
            predecessor
                .as_ref()
                .map(|marker| marker.checksum_sha256.clone()),
            report.catalog_epoch,
            report.inventory_hash.clone(),
            report.resolution_artifact_hash.clone(),
            &report.knowledge_observations,
            &report.projects,
            &report.carried_forward_rows,
        )?;
        if recomputed_marker != report.predicted_marker {
            return Err(error(
                "error.knowledge_transport_cutover_artifact_identity",
                "reviewed predicted marker does not match report evidence",
            ));
        }
        recheck_report(&request.layout, &request.config, &store, &report)?;
        let mut marker = KnowledgeTransportCutoverMarkerV1 {
            version: MARKER_VERSION,
            applied_at: request.applied_at.clone(),
            report_artifact_hash: Sha256ValueV1::digest(&report_bytes),
            resolution_artifact_hash: Sha256ValueV1::digest(&resolution_bytes),
            predecessor_marker_checksum: report
                .predicted_marker
                .predecessor_marker_checksum
                .clone(),
            predecessor_catalog_epoch: report.predicted_marker.predecessor_catalog_epoch,
            inventory_hash: report.predicted_marker.inventory_hash.clone(),
            observation_snapshot_hash: report.predicted_marker.observation_snapshot_hash.clone(),
            rows: report.predicted_marker.rows.clone(),
            checksum_sha256: Sha256ValueV1::digest(b"pending"),
        };
        marker.checksum_sha256 = marker_checksum(&marker)?;
        let marker_bytes = serde_json::to_vec(&marker)
            .map_err(|cause| error("error.knowledge_transport_cutover_marker", cause))?;
        if marker_bytes.len() > MAX_KNOWLEDGE_TRANSPORT_CUTOVER_MARKER_BYTES
            || decode_knowledge_transport_cutover_marker_v1(&marker_bytes)? != marker
        {
            return Err(error(
                "error.knowledge_transport_cutover_marker_identity",
                "constructed marker failed its canonical identity check",
            ));
        }
        atomic_write_bytes_locked(
            &knowledge_transport_cutover_marker_path(&request.layout.state_dir),
            &marker_bytes,
        )
        .map_err(|cause| error("error.knowledge_transport_cutover_marker_write", cause))?;
        let verification = verify_marker(&request.layout, &request.config, &store, &marker)?;
        write_receipt(&request.layout.state_dir, &marker, &request.applied_at)?;
        Ok(verification)
    }

    pub fn verify(
        request: KnowledgeTransportCutoverVerifyRequestV1,
    ) -> CutoverResult<KnowledgeTransportCutoverVerificationReceiptV1> {
        validate_timestamp(&request.verified_at, "verify")?;
        let store = ProjectCatalogStore::open_existing(request.layout.projects_path())
            .map_err(|cause| error("error.knowledge_transport_cutover_catalog", cause))?;
        let _mutation_lock = acquire_store_lock_nofollow(request.layout.projects_path())
            .map_err(|cause| error("error.knowledge_transport_cutover_lock", cause))?;
        let marker = load_knowledge_transport_cutover_marker_optional(&request.layout.state_dir)?
            .ok_or_else(|| {
            error(
                "error.knowledge_transport_cutover_marker_missing",
                "there is no current knowledge transport marker",
            )
        })?;
        let verification = verify_marker(&request.layout, &request.config, &store, &marker)?;
        write_receipt(&request.layout.state_dir, &marker, &request.verified_at)?;
        let reopened =
            load_current_marker_with_receipt(&request.layout.state_dir)?.ok_or_else(|| {
                error(
                    "error.knowledge_transport_cutover_marker_missing",
                    "the verified marker disappeared",
                )
            })?;
        if reopened != marker {
            return Err(error(
                "error.knowledge_transport_cutover_current_identity",
                "the verified marker is not the selected current artifact",
            ));
        }
        Ok(verification)
    }
}

fn project_evidence_without_assignment(
    project_id: &ProjectId,
    scope: &PublishedScope,
    coverage: KnowledgeTransportRuntimeCoverageV1,
    target_counters: &[CheckoutAccessTargetCounter],
    observations: &KnowledgeTransportObservationSnapshotV1,
) -> KnowledgeTransportProjectEvidenceV1 {
    let coverage_status = match coverage {
        KnowledgeTransportRuntimeCoverageV1::Uncovered => {
            KnowledgeTransportCoverageStatusV1::BlockedPublishedNeverCovered
        }
        _ => KnowledgeTransportCoverageStatusV1::CoveredProducerRemoved,
    };
    project_evidence_base(
        project_id,
        scope,
        None,
        coverage_status,
        target_counters,
        observations,
    )
}

fn project_evidence_base(
    project_id: &ProjectId,
    scope: &PublishedScope,
    producer_id: Option<String>,
    coverage_status: KnowledgeTransportCoverageStatusV1,
    target_counters: &[CheckoutAccessTargetCounter],
    observations: &KnowledgeTransportObservationSnapshotV1,
) -> KnowledgeTransportProjectEvidenceV1 {
    let shadow_comparisons = observations
        .comparisons
        .iter()
        .filter(|comparison| comparison.project_id == project_id.as_str())
        .cloned()
        .collect::<Vec<_>>();
    let observation_window_start_sequence = observations
        .counters
        .iter()
        .filter(|counter| counter.project_id == project_id.as_str())
        .map(|counter| counter.first_sequence)
        .min()
        .unwrap_or(observations.sequence);
    KnowledgeTransportProjectEvidenceV1 {
        project_id: project_id.clone(),
        scope: scope.clone(),
        producer_id,
        coverage_status,
        publication_parity: None,
        prepared_upload_count: 0,
        unfinished_finalize_journal_count: 0,
        expired_workspace_ids: Vec::new(),
        workspace_parity: Vec::new(),
        shadow_comparisons,
        capability_baselines: capability_baselines(project_id, target_counters),
        observation_window_start_sequence,
        observation_window_end_sequence: observations.sequence,
        defects: Vec::new(),
    }
}

fn refuse_post_boundary_checkout_observations(
    runtime: &KnowledgeTransportCutoverRuntimeV1,
    evidence: &mut KnowledgeTransportProjectEvidenceV1,
) {
    let Some(row) = runtime.row(&evidence.project_id) else {
        return;
    };
    if evidence.capability_baselines != row.capability_baselines {
        evidence.coverage_status = KnowledgeTransportCoverageStatusV1::Refused;
        evidence
            .defects
            .push("a covered project recorded a post-boundary local checkout observation".into());
    }
}

fn capture_project_evidence(
    project_id: &ProjectId,
    scope: &PublishedScope,
    producer_id: String,
    existing_coverage: KnowledgeTransportRuntimeCoverageV1,
    accepted: &AcceptedPublicationRuntime,
    source_store: Option<&KnowledgeSourceStore>,
    target_counters: &[CheckoutAccessTargetCounter],
    observations: &KnowledgeTransportObservationSnapshotV1,
) -> KnowledgeTransportProjectEvidenceV1 {
    let mut evidence = project_evidence_base(
        project_id,
        scope,
        Some(producer_id.clone()),
        KnowledgeTransportCoverageStatusV1::Proposed,
        target_counters,
        observations,
    );
    match capture_publication_parity(project_id, scope, &producer_id, accepted, source_store) {
        Ok(parity) => evidence.publication_parity = Some(parity),
        Err(cause) => evidence.defects.push(cause.to_string()),
    }
    if let Err(cause) =
        capture_project_source_readiness(&mut evidence, accepted, source_store, observations)
    {
        evidence.defects.push(cause.to_string());
    }
    if !evidence.defects.is_empty() {
        evidence.coverage_status = match existing_coverage {
            KnowledgeTransportRuntimeCoverageV1::Uncovered => {
                KnowledgeTransportCoverageStatusV1::Refused
            }
            KnowledgeTransportRuntimeCoverageV1::Current => {
                KnowledgeTransportCoverageStatusV1::CarriedForwardCurrent
            }
            KnowledgeTransportRuntimeCoverageV1::CoveredProducerRemoved => {
                KnowledgeTransportCoverageStatusV1::CoveredProducerRemoved
            }
            KnowledgeTransportRuntimeCoverageV1::ScopeMigrationPendingRecutover => {
                KnowledgeTransportCoverageStatusV1::ScopeMigrationPendingRecutover
            }
            KnowledgeTransportRuntimeCoverageV1::ProducerAssignmentPendingRecutover => {
                KnowledgeTransportCoverageStatusV1::ProducerAssignmentPendingRecutover
            }
            KnowledgeTransportRuntimeCoverageV1::AcceptedSourcePendingRecutover => {
                KnowledgeTransportCoverageStatusV1::AcceptedSourcePendingRecutover
            }
        };
    }
    evidence
}

fn capture_project_source_readiness(
    evidence: &mut KnowledgeTransportProjectEvidenceV1,
    accepted: &AcceptedPublicationRuntime,
    source_store: Option<&KnowledgeSourceStore>,
    observations: &KnowledgeTransportObservationSnapshotV1,
) -> CutoverResult<()> {
    use crate::knowledge_transport_observations::KnowledgeTransportOperationV1 as Operation;

    let source_store = source_store.ok_or_else(|| {
        error(
            "error.knowledge_transport_cutover_source_missing",
            "the knowledge source store is absent",
        )
    })?;
    let readiness = source_store
        .project_cutover_readiness(evidence.project_id.as_str(), now_unix_secs())
        .map_err(|cause| error("error.knowledge_transport_cutover_source_readiness", cause))?;
    evidence.prepared_upload_count = readiness.prepared_upload_count;
    evidence.unfinished_finalize_journal_count = readiness.unfinished_finalize_journal_count;
    evidence.expired_workspace_ids = readiness
        .expired_workspace_ids
        .into_iter()
        .map(|workspace| workspace.as_str().to_string())
        .collect();
    if evidence.prepared_upload_count != 0 {
        evidence.defects.push(format!(
            "{} knowledge-source uploads remain prepared",
            evidence.prepared_upload_count
        ));
    }
    if evidence.unfinished_finalize_journal_count != 0 {
        evidence.defects.push(format!(
            "{} knowledge-source finalize journals remain unfinished",
            evidence.unfinished_finalize_journal_count
        ));
    }
    if !evidence.expired_workspace_ids.is_empty() {
        evidence.defects.push(format!(
            "{} selected provisional workspaces are expired",
            evidence.expired_workspace_ids.len()
        ));
    }

    let verified = accepted
        .load_verified(&evidence.project_id)
        .map_err(|cause| error("error.knowledge_transport_cutover_accepted", cause))?;
    for workspace in readiness.selected_workspaces {
        match capture_workspace_parity(&evidence.scope, &verified, workspace) {
            Ok(parity) => evidence.workspace_parity.push(parity),
            Err(cause) => evidence.defects.push(cause.to_string()),
        }
    }
    evidence
        .workspace_parity
        .sort_by(|left, right| left.workspace_id.cmp(&right.workspace_id));
    let selected_workspace_ids = evidence
        .workspace_parity
        .iter()
        .map(|workspace| workspace.workspace_id.as_str())
        .collect::<BTreeSet<_>>();
    evidence.shadow_comparisons.retain(|comparison| {
        comparison
            .workspace_id
            .as_deref()
            .is_some_and(|workspace_id| {
                selected_workspace_ids.contains(workspace_id)
                    && matches!(
                        comparison.operation,
                        Operation::ProvisionalOwnKnowledge
                            | Operation::ProvisionalOwnGaps
                            | Operation::ProvisionalAllKnowledge
                            | Operation::ProvisionalAllGaps
                    )
            })
    });

    validate_overlap_observations(evidence, observations);
    Ok(())
}

fn validate_overlap_observations(
    evidence: &mut KnowledgeTransportProjectEvidenceV1,
    observations: &KnowledgeTransportObservationSnapshotV1,
) {
    use crate::knowledge_transport_observations::{
        KnowledgeTransportOperationV1 as Operation, KnowledgeTransportOutcomeV1 as Outcome,
    };

    for operation in [Operation::PublishedKnowledge, Operation::PublishedGaps] {
        if !has_operation_counter(
            observations,
            evidence.project_id.as_str(),
            operation,
            Outcome::Remote,
        ) {
            evidence.defects.push(format!(
                "the remote {operation:?} view has not been exercised during overlap"
            ));
        }
    }
    if !evidence.workspace_parity.is_empty()
        && !has_operation_counter(
            observations,
            evidence.project_id.as_str(),
            Operation::WatcherRefresh,
            Outcome::Local,
        )
    {
        evidence
            .defects
            .push("no local watcher refresh was observed for selected overlap workspaces".into());
    }

    for parity in &evidence.workspace_parity {
        for (operation, expected_snapshot_id) in [
            (
                Operation::ProvisionalOwnKnowledge,
                parity.knowledge_snapshot_id.as_str(),
            ),
            (
                Operation::ProvisionalOwnGaps,
                parity.gap_snapshot_id.as_str(),
            ),
            (
                Operation::ProvisionalAllKnowledge,
                parity.knowledge_snapshot_id.as_str(),
            ),
            (
                Operation::ProvisionalAllGaps,
                parity.gap_snapshot_id.as_str(),
            ),
        ] {
            if !has_operation_counter(
                observations,
                evidence.project_id.as_str(),
                operation,
                Outcome::Local,
            ) {
                evidence.defects.push(format!(
                    "workspace {} has no local {operation:?} overlap observation",
                    parity.workspace_id
                ));
            }
            if !has_operation_counter(
                observations,
                evidence.project_id.as_str(),
                operation,
                Outcome::ShadowEqual,
            ) {
                evidence.defects.push(format!(
                    "workspace {} has no equal {operation:?} overlap observation",
                    parity.workspace_id
                ));
            }
            let comparison = evidence.shadow_comparisons.iter().find(|comparison| {
                comparison.operation == operation
                    && comparison.workspace_id.as_deref() == Some(parity.workspace_id.as_str())
            });
            match comparison {
                Some(comparison)
                    if comparison.equal
                        && comparison.transport_snapshot_id == expected_snapshot_id => {}
                Some(_) => evidence.defects.push(format!(
                    "workspace {} {operation:?} shadow evidence does not match the reopened remote snapshot",
                    parity.workspace_id
                )),
                None => evidence.defects.push(format!(
                    "workspace {} has no current {operation:?} shadow comparison",
                    parity.workspace_id
                )),
            }
        }
    }
}

fn has_operation_counter(
    observations: &KnowledgeTransportObservationSnapshotV1,
    project_id: &str,
    operation: crate::knowledge_transport_observations::KnowledgeTransportOperationV1,
    outcome: crate::knowledge_transport_observations::KnowledgeTransportOutcomeV1,
) -> bool {
    observations.counters.iter().any(|counter| {
        counter.project_id == project_id
            && counter.operation == operation
            && counter.outcome == outcome
            && counter.count != 0
    })
}

fn capture_workspace_parity(
    scope: &PublishedScope,
    verified: &VerifiedAcceptedPublication,
    source: ReadyProvisionalWorkspace,
) -> CutoverResult<KnowledgeTransportWorkspaceParityV1> {
    let accepted = verified.content_stamp();
    if source.project_id != accepted.project_id().as_str()
        || &source.descriptor.scope != scope
        || source.descriptor.accepted_generation != accepted.generation_id()
        || source.descriptor.accepted_commit != accepted.accepted_commit()
    {
        return Err(error(
            "error.knowledge_transport_cutover_provisional_stale",
            "selected provisional workspace does not target the current accepted publication",
        ));
    }
    let workspace_id = source.descriptor.workspace_id.as_str().to_string();
    let baseline_knowledge = bbox_knowledge::overlay::BaselineKnowledgeSnapshot::new(
        provisional_file_map(&source.baseline_knowledge, "knowledge")
            .map_err(|cause| error("error.knowledge_transport_cutover_workspace", cause))?,
    )
    .map_err(|cause| error("error.knowledge_transport_cutover_workspace", cause))?;
    let working_knowledge = bbox_knowledge::overlay::WorkingKnowledgeSnapshot::new(
        provisional_file_map(&source.working_knowledge, "knowledge")
            .map_err(|cause| error("error.knowledge_transport_cutover_workspace", cause))?,
    )
    .map_err(|cause| error("error.knowledge_transport_cutover_workspace", cause))?;
    let knowledge_digests = bbox_knowledge::overlay::AcceptedPublishedDigests(
        verified
            .knowledge_manifest()
            .iter()
            .filter_map(|(filename, manifest)| {
                Some((
                    basename(filename.as_str())?,
                    manifest.source_content_sha256.as_str().to_string(),
                ))
            })
            .collect(),
    );
    let knowledge = bbox_knowledge::overlay::recompute_catalog_overlay_from_sources(
        bbox_knowledge::overlay::CatalogOverlayPublished {
            published_scope: scope,
            checkout_id: &workspace_id,
            full_ref: accepted.full_ref(),
            accepted_commit: accepted.accepted_commit(),
            accepted_generation: accepted.generation_id(),
            published: &knowledge_digests,
        },
        &source.descriptor.checkout_head,
        &source.descriptor.merge_base,
        &baseline_knowledge,
        &working_knowledge,
    )
    .map_err(|cause| error("error.knowledge_transport_cutover_workspace", cause))?;
    if knowledge.status != bbox_knowledge::overlay::OverlayStatus::Valid {
        return Err(error(
            "error.knowledge_transport_cutover_workspace_invalid",
            format!(
                "workspace {workspace_id} knowledge overlay is not valid: {}",
                knowledge.diagnostics.join("; ")
            ),
        ));
    }

    let baseline_gaps = bbox_gaps::overlay::BaselineGapSnapshot::new(
        provisional_file_map(&source.baseline_gaps, "gap")
            .map_err(|cause| error("error.knowledge_transport_cutover_workspace", cause))?,
    )
    .map_err(|cause| error("error.knowledge_transport_cutover_workspace", cause))?;
    let working_gaps = bbox_gaps::overlay::WorkingGapSnapshot::new(
        provisional_file_map(&source.working_gaps, "gap")
            .map_err(|cause| error("error.knowledge_transport_cutover_workspace", cause))?,
    )
    .map_err(|cause| error("error.knowledge_transport_cutover_workspace", cause))?;
    let gap_digests = bbox_gaps::overlay::AcceptedPublishedGapDigests(
        verified
            .gap_manifest()
            .iter()
            .filter_map(|(filename, manifest)| {
                Some((
                    basename(filename.as_str())?,
                    manifest.source_content_sha256.as_str().to_string(),
                ))
            })
            .collect(),
    );
    let gaps = bbox_gaps::overlay::recompute_catalog_overlay_from_sources(
        bbox_gaps::overlay::CatalogGapOverlayPublished {
            published_scope: scope,
            checkout_id: &workspace_id,
            full_ref: accepted.full_ref(),
            accepted_commit: accepted.accepted_commit(),
            accepted_generation: accepted.generation_id(),
            published: &gap_digests,
        },
        &source.descriptor.checkout_head,
        &source.descriptor.merge_base,
        &baseline_gaps,
        &working_gaps,
    )
    .map_err(|cause| error("error.knowledge_transport_cutover_workspace", cause))?;
    if gaps.status != bbox_gaps::overlay::GapOverlayStatus::Valid {
        return Err(error(
            "error.knowledge_transport_cutover_workspace_invalid",
            format!(
                "workspace {workspace_id} gap overlay is not valid: {}",
                gaps.diagnostics.join("; ")
            ),
        ));
    }

    Ok(KnowledgeTransportWorkspaceParityV1 {
        workspace_id,
        source_generation_id: source.source_generation_id,
        sequence: source.descriptor.sequence,
        accepted_generation_id: source.descriptor.accepted_generation,
        lease_expires_unix_secs: source.lease_expires_unix_secs,
        knowledge_snapshot_id: knowledge.snapshot_id,
        gap_snapshot_id: gaps.snapshot_id,
    })
}

fn provisional_file_map(
    files: &[ReadyPublicationFile],
    lane: &str,
) -> anyhow::Result<BTreeMap<String, Vec<u8>>> {
    let mut mapped = BTreeMap::new();
    for file in files {
        let filename = basename(&file.manifest.repository_relative_filename)
            .ok_or_else(|| anyhow::anyhow!("provisional {lane} filename has no basename"))?;
        if mapped.insert(filename, file.source_bytes.clone()).is_some() {
            anyhow::bail!("provisional {lane} snapshot contains duplicate basenames");
        }
    }
    Ok(mapped)
}

fn basename(repository_relative: &str) -> Option<String> {
    Path::new(repository_relative)
        .file_name()
        .and_then(|name| name.to_str())
        .map(str::to_owned)
}

fn capture_publication_parity(
    project_id: &ProjectId,
    scope: &PublishedScope,
    producer_id: &str,
    accepted: &AcceptedPublicationRuntime,
    source_store: Option<&KnowledgeSourceStore>,
) -> CutoverResult<KnowledgeTransportPublicationParityV1> {
    let source_store = source_store.ok_or_else(|| {
        error(
            "error.knowledge_transport_cutover_source_missing",
            "the knowledge source store is absent",
        )
    })?;
    let verified = accepted
        .load_verified(project_id)
        .map_err(|cause| error("error.knowledge_transport_cutover_accepted", cause))?;
    if verified.content_stamp().accepted_scope() != scope {
        return Err(error(
            "error.knowledge_transport_cutover_scope",
            "accepted publication scope does not match the catalog",
        ));
    }
    let (source_generation_id, source_generation_sha256) = match verified.binding_stamp().source() {
        AcceptedPublicationSourceBinding::Producer {
            producer_id: bound_producer,
            source_generation_id,
            source_generation_sha256,
        } if bound_producer == producer_id => (
            source_generation_id.clone(),
            source_generation_sha256.clone(),
        ),
        _ => {
            return Err(error(
                "error.knowledge_transport_cutover_remote_source_required",
                "accepted publication is not bound to the configured remote producer",
            ));
        }
    };
    let pinned = source_store
        .pin_ready_publication_candidate(&source_generation_id)
        .map_err(|cause| error("error.knowledge_transport_cutover_source", cause))?;
    let candidate = pinned.candidate();
    validate_candidate_identity(
        candidate,
        project_id,
        scope,
        producer_id,
        &source_generation_sha256,
        &verified,
    )?;
    // This is a content-parity rebuild, not a pointer transition. An advance
    // would attach the installed generation as the prior arm, then correctly
    // reject the rebuilt current content because a prior arm must name a
    // distinct generation. Establish preparation carries no prior arm and a
    // dry run never attempts the pointer-absence commit precondition.
    let rebuilt = accepted
        .prepare_publish(
            PublishRequest {
                mode: PublisherPublishMode::Establish,
                project_id: project_id.clone(),
                source: AcceptedPublicationSourceBinding::Producer {
                    producer_id: producer_id.to_string(),
                    source_generation_id: source_generation_id.clone(),
                    source_generation_sha256: source_generation_sha256.clone(),
                },
                scope: scope.clone(),
                full_ref: candidate.descriptor.full_ref.clone(),
                accepted_commit: candidate.descriptor.publisher_commit.clone(),
                dry_run: true,
                // A parity rebuild must not touch the standing grant, and a
                // dry run installs no pointer to touch it on.
                auto_advance: Default::default(),
            },
            PublishSources {
                knowledge: candidate
                    .knowledge
                    .iter()
                    .map(|file| PublishSourceFile {
                        repository_relative_filename: file
                            .manifest
                            .repository_relative_filename
                            .clone(),
                        source_bytes: file.source_bytes.clone(),
                    })
                    .collect(),
                gaps: candidate
                    .gaps
                    .iter()
                    .map(|file| PublishSourceFile {
                        repository_relative_filename: file
                            .manifest
                            .repository_relative_filename
                            .clone(),
                        source_bytes: file.source_bytes.clone(),
                    })
                    .collect(),
                graphs: candidate
                    .graphs
                    .iter()
                    .map(|file| PublishSourceFile {
                        repository_relative_filename: file
                            .manifest
                            .repository_relative_filename
                            .clone(),
                        source_bytes: file.source_bytes.clone(),
                    })
                    .collect(),
                evidence: candidate
                    .evidence
                    .iter()
                    .map(|file| PublishSourceFile {
                        repository_relative_filename: file
                            .manifest
                            .repository_relative_filename
                            .clone(),
                        source_bytes: file.source_bytes.clone(),
                    })
                    .collect(),
            },
        )
        .map_err(|cause| error("error.knowledge_transport_cutover_parity", cause))?;
    let equal = rebuilt.generation_id() == verified.content_stamp().generation_id()
        && rebuilt.generation_hash() == verified.content_stamp().generation_hash();
    if !equal {
        return Err(error(
            "error.knowledge_transport_cutover_parity",
            "rebuilding accepted content from the remote candidate produced a different generation",
        ));
    }
    Ok(KnowledgeTransportPublicationParityV1 {
        accepted_generation_id: verified.content_stamp().generation_id().to_string(),
        accepted_generation_sha256: verified.content_stamp().generation_hash().to_string(),
        accepted_pointer_sha256: verified.binding_stamp().pointer_sha256().to_string(),
        source_generation_id,
        source_generation_sha256,
        knowledge_manifest_sha256: candidate.descriptor.knowledge.manifest_sha256.clone(),
        gap_manifest_sha256: candidate.descriptor.gaps.manifest_sha256.clone(),
        rebuilt_generation_id: rebuilt.generation_id().to_string(),
        rebuilt_generation_sha256: rebuilt.generation_hash().to_string(),
        equal,
    })
}

fn validate_candidate_identity(
    candidate: &ReadyPublicationCandidate,
    project_id: &ProjectId,
    scope: &PublishedScope,
    producer_id: &str,
    expected_source_hash: &str,
    verified: &VerifiedAcceptedPublication,
) -> CutoverResult<()> {
    if candidate.project_id != project_id.as_str()
        || candidate.producer_id != producer_id
        || &candidate.descriptor.scope != scope
        || candidate.descriptor.full_ref != verified.content_stamp().full_ref()
        || candidate.descriptor.publisher_commit != verified.content_stamp().accepted_commit()
        || candidate.source_generation_sha256 != expected_source_hash
    {
        return Err(error(
            "error.knowledge_transport_cutover_source_changed",
            "ready source candidate does not match accepted immutable evidence",
        ));
    }
    Ok(())
}

fn capability_baselines(
    project_id: &ProjectId,
    target_counters: &[CheckoutAccessTargetCounter],
) -> Vec<KnowledgeTransportCapabilityBaselineV1> {
    CUTOVER_CAPABILITIES
        .into_iter()
        .map(|capability| {
            let count = |outcome| {
                target_counters
                    .iter()
                    .filter(|counter| {
                        counter.project_id == project_id.as_str()
                            && counter.kind == capability
                            && counter.outcome == outcome
                    })
                    .map(|counter| counter.count)
                    .sum()
            };
            KnowledgeTransportCapabilityBaselineV1 {
                capability,
                granted: count(CheckoutAccessOutcome::Granted),
                denied: count(CheckoutAccessOutcome::Denied),
            }
        })
        .collect()
}

fn predicted_marker(
    predecessor_marker_checksum: Option<Sha256ValueV1>,
    catalog_epoch: u64,
    inventory_hash: Sha256ValueV1,
    resolution_artifact_hash: Sha256ValueV1,
    observations: &KnowledgeTransportObservationSnapshotV1,
    projects: &[KnowledgeTransportProjectEvidenceV1],
    carried_forward_rows: &[PredictedKnowledgeTransportCutoverRowV1],
) -> CutoverResult<PredictedKnowledgeTransportCutoverMarkerV1> {
    let mut rows = projects
        .iter()
        .filter(|project| project.coverage_status == KnowledgeTransportCoverageStatusV1::Proposed)
        .map(|project| {
            let producer_id = project.producer_id.clone().ok_or_else(|| {
                error(
                    "error.knowledge_transport_cutover_artifact_identity",
                    "a proposed row has no producer",
                )
            })?;
            let parity = project.publication_parity.as_ref().ok_or_else(|| {
                error(
                    "error.knowledge_transport_cutover_artifact_identity",
                    "a proposed row has no publication parity evidence",
                )
            })?;
            let parity_workspace_ids = project
                .workspace_parity
                .iter()
                .map(|workspace| workspace.workspace_id.clone())
                .collect::<Vec<_>>();
            Ok(PredictedKnowledgeTransportCutoverRowV1 {
                project_id: project.project_id.clone(),
                scope: project.scope.clone(),
                producer_id: producer_id.clone(),
                grant_commitment: grant_commitment(
                    &project.project_id,
                    &project.scope,
                    &producer_id,
                ),
                accepted_generation_id: parity.accepted_generation_id.clone(),
                accepted_generation_sha256: parity.accepted_generation_sha256.clone(),
                accepted_pointer_sha256: parity.accepted_pointer_sha256.clone(),
                source_generation_id: parity.source_generation_id.clone(),
                source_generation_sha256: parity.source_generation_sha256.clone(),
                publication_parity_commitment: Sha256ValueV1::digest(
                    &serde_json::to_vec(parity).map_err(|cause| {
                        error("error.knowledge_transport_cutover_artifact", cause)
                    })?,
                ),
                parity_workspace_ids,
                workspace_parity_commitment: Sha256ValueV1::digest(
                    &serde_json::to_vec(&project.workspace_parity).map_err(|cause| {
                        error("error.knowledge_transport_cutover_artifact", cause)
                    })?,
                ),
                shadow_observation_commitment: Sha256ValueV1::digest(
                    &serde_json::to_vec(&project.shadow_comparisons).map_err(|cause| {
                        error("error.knowledge_transport_cutover_artifact", cause)
                    })?,
                ),
                capability_baselines: project.capability_baselines.clone(),
                observation_window_start_sequence: project.observation_window_start_sequence,
                observation_window_end_sequence: project.observation_window_end_sequence,
            })
        })
        .collect::<CutoverResult<Vec<_>>>()?;
    rows.extend(carried_forward_rows.iter().cloned());
    rows.sort_by(|left, right| left.project_id.cmp(&right.project_id));
    Ok(PredictedKnowledgeTransportCutoverMarkerV1 {
        version: MARKER_VERSION,
        predecessor_marker_checksum,
        predecessor_catalog_epoch: catalog_epoch,
        inventory_hash,
        resolution_artifact_hash,
        observation_snapshot_hash: Sha256ValueV1::digest(
            &serde_json::to_vec(observations)
                .map_err(|cause| error("error.knowledge_transport_cutover_artifact", cause))?,
        ),
        rows,
    })
}

fn inventory_hash(
    catalog_epoch: u64,
    catalog_sha256: &str,
    checkout_observation_sequence: u64,
    observations: &KnowledgeTransportObservationSnapshotV1,
    projects: &[KnowledgeTransportProjectEvidenceV1],
    carried_forward_rows: &[PredictedKnowledgeTransportCutoverRowV1],
) -> CutoverResult<Sha256ValueV1> {
    serde_json::to_vec(&(
        catalog_epoch,
        catalog_sha256,
        checkout_observation_sequence,
        observations,
        projects,
        carried_forward_rows,
    ))
    .map(|bytes| Sha256ValueV1::digest(&bytes))
    .map_err(|cause| error("error.knowledge_transport_cutover_artifact", cause))
}

fn load_or_create_resolution(
    path: &Path,
    inventory_hash: Sha256ValueV1,
    projects: &[KnowledgeTransportProjectEvidenceV1],
) -> CutoverResult<(
    KnowledgeTransportCutoverResolutionV1,
    Vec<u8>,
    BTreeSet<ProjectId>,
)> {
    let existing = read_artifact_optional(
        path,
        MAX_PROJECT_CATALOG_RESOLUTION_BYTES,
        "knowledge transport cutover resolution",
    )
    .map_err(|cause| error("error.knowledge_transport_cutover_artifact", cause))?;
    let (resolution, bytes) = match existing {
        Some(bytes) => {
            let resolution: KnowledgeTransportCutoverResolutionV1 = serde_json::from_slice(&bytes)
                .map_err(|cause| error("error.knowledge_transport_cutover_resolution", cause))?;
            (resolution, bytes)
        }
        None => {
            let resolution = KnowledgeTransportCutoverResolutionV1 {
                version: RESOLUTION_VERSION,
                inventory_hash: inventory_hash.clone(),
                blocked_project_acknowledgements: BTreeMap::new(),
            };
            let bytes = serde_json::to_vec(&resolution)
                .map_err(|cause| error("error.knowledge_transport_cutover_resolution", cause))?;
            (resolution, bytes)
        }
    };
    if resolution.version != RESOLUTION_VERSION || resolution.inventory_hash != inventory_hash {
        return Err(error(
            "error.knowledge_transport_cutover_resolution",
            "resolution is bound to a different inventory",
        ));
    }
    let unresolved = validate_blocked_project_acknowledgements(&resolution, projects)?;
    Ok((resolution, bytes, unresolved))
}

fn validate_blocked_project_acknowledgements(
    resolution: &KnowledgeTransportCutoverResolutionV1,
    projects: &[KnowledgeTransportProjectEvidenceV1],
) -> CutoverResult<BTreeSet<ProjectId>> {
    let blocked = projects
        .iter()
        .filter(|project| {
            project.coverage_status
                == KnowledgeTransportCoverageStatusV1::BlockedPublishedNeverCovered
        })
        .map(|project| project.project_id.clone())
        .collect::<BTreeSet<_>>();
    for (project_id, acknowledgement) in &resolution.blocked_project_acknowledgements {
        if !blocked.contains(project_id) {
            return Err(error(
                "error.knowledge_transport_cutover_resolution",
                "resolution acknowledges a project that is not blocked",
            ));
        }
        if acknowledgement.trim().is_empty() || acknowledgement.len() > 1024 {
            return Err(error(
                "error.knowledge_transport_cutover_resolution",
                format!(
                    "blocked project {project_id} acknowledgement must contain at most 1024 bytes of operator rationale"
                ),
            ));
        }
    }
    let acknowledged = resolution
        .blocked_project_acknowledgements
        .keys()
        .cloned()
        .collect::<BTreeSet<_>>();
    Ok(blocked.difference(&acknowledged).cloned().collect())
}

fn recheck_report(
    layout: &ProjectCatalogMigrationResolvedLayoutV1,
    config: &Config,
    store: &ProjectCatalogStore,
    report: &KnowledgeTransportCutoverReportV1,
) -> CutoverResult<()> {
    require_enabled(config)?;
    let state = store
        .snapshot()
        .map_err(|cause| error("error.knowledge_transport_cutover_catalog", cause))?;
    if state.epoch() != report.catalog_epoch || state.catalog_sha256() != report.catalog_sha256 {
        return Err(error(
            "error.knowledge_transport_cutover_capture_changed",
            "catalog identity changed after preflight",
        ));
    }
    let assignments = configured_assignments(config)?;
    validate_assignments_resolve(state.catalog(), &assignments)?;
    let catalog_projects = state
        .catalog()
        .projects
        .iter()
        .filter_map(|(project_id, project)| match &project.scope {
            ProjectScope::Published(scope) => Some((project_id, scope)),
            // Knowledge transport is a published-scope lane; a connector
            // project has no committed .bbox tree to carry.
            ProjectScope::LegacyLocal | ProjectScope::Connector(_) => None,
        })
        .collect::<BTreeMap<_, _>>();
    let report_projects = report
        .projects
        .iter()
        .map(|project| (&project.project_id, &project.scope))
        .collect::<BTreeMap<_, _>>();
    if catalog_projects != report_projects {
        return Err(error(
            "error.knowledge_transport_cutover_capture_changed",
            "the set or scope of Published projects changed after preflight",
        ));
    }
    for evidence in &report.projects {
        if evidence.producer_id.as_ref() != assignments.get(&evidence.scope) {
            return Err(error(
                "error.knowledge_transport_cutover_capture_changed",
                "a reviewed project producer assignment changed after preflight",
            ));
        }
    }
    let checkout_health =
        CheckoutAccessObservations::open(layout.bro_home.join("checkout-access-observations.json"))
            .map_err(|cause| error("error.knowledge_transport_cutover_observations", cause))?
            .health();
    let observations =
        crate::knowledge_transport_observations::KnowledgeTransportObservationsV1::open(
            layout
                .bro_home
                .join("knowledge-transport-observations.json"),
        )
        .map_err(|cause| error("error.knowledge_transport_cutover_observations", cause))?
        .snapshot();
    if checkout_health.sequence != report.checkout_observation_sequence
        || observations != report.knowledge_observations
    {
        return Err(error(
            "error.knowledge_transport_cutover_observations_changed",
            "local or shadow observations changed after preflight; rerun preflight",
        ));
    }
    let accepted = AcceptedPublicationRuntime::open_global(layout.projects_path())
        .map_err(|cause| error("error.knowledge_transport_cutover_accepted", cause))?;
    let source_store = open_source_store(layout, config)?;
    let predecessor = load_current_marker_with_receipt(&layout.state_dir)?;
    let runtime = KnowledgeTransportCutoverRuntimeV1::from_marker(predecessor);
    for evidence in &report.projects {
        let verified = accepted.load_verified(&evidence.project_id).ok();
        let existing_coverage = runtime.classify_project(
            state.catalog(),
            &assignments,
            &evidence.project_id,
            verified.as_ref(),
        );
        let mut current = match assignments.get(&evidence.scope) {
            None => project_evidence_without_assignment(
                &evidence.project_id,
                &evidence.scope,
                existing_coverage,
                &checkout_health.target_counters,
                &observations,
            ),
            Some(producer_id)
                if existing_coverage == KnowledgeTransportRuntimeCoverageV1::Current =>
            {
                project_evidence_base(
                    &evidence.project_id,
                    &evidence.scope,
                    Some(producer_id.clone()),
                    KnowledgeTransportCoverageStatusV1::CarriedForwardCurrent,
                    &checkout_health.target_counters,
                    &observations,
                )
            }
            Some(producer_id) => capture_project_evidence(
                &evidence.project_id,
                &evidence.scope,
                producer_id.clone(),
                existing_coverage,
                &accepted,
                source_store.as_ref(),
                &checkout_health.target_counters,
                &observations,
            ),
        };
        refuse_post_boundary_checkout_observations(&runtime, &mut current);
        if &current != evidence {
            return Err(error(
                "error.knowledge_transport_cutover_capture_changed",
                "reviewed knowledge transport parity changed after preflight",
            ));
        }
    }
    Ok(())
}

fn verify_marker(
    layout: &ProjectCatalogMigrationResolvedLayoutV1,
    config: &Config,
    store: &ProjectCatalogStore,
    marker: &KnowledgeTransportCutoverMarkerV1,
) -> CutoverResult<KnowledgeTransportCutoverVerificationReceiptV1> {
    let state = store
        .snapshot()
        .map_err(|cause| error("error.knowledge_transport_cutover_catalog", cause))?;
    let assignments = configured_assignments(config)?;
    let accepted = AcceptedPublicationRuntime::open_global(layout.projects_path())
        .map_err(|cause| error("error.knowledge_transport_cutover_accepted", cause))?;
    let checkout_health =
        CheckoutAccessObservations::open(layout.bro_home.join("checkout-access-observations.json"))
            .map_err(|cause| error("error.knowledge_transport_cutover_observations", cause))?
            .health();
    let runtime = KnowledgeTransportCutoverRuntimeV1::from_marker(Some(marker.clone()));
    let mut rows = Vec::with_capacity(marker.rows.len());
    for row in &marker.rows {
        let verified = accepted.load_verified(&row.project_id).ok();
        let coverage = runtime.classify_project(
            state.catalog(),
            &assignments,
            &row.project_id,
            verified.as_ref(),
        );
        let current = capability_baselines(&row.project_id, &checkout_health.target_counters);
        if current != row.capability_baselines {
            return Err(error(
                "error.knowledge_transport_cutover_observation_delta",
                format!(
                    "covered project {} recorded a post-boundary local checkout observation",
                    row.project_id
                ),
            ));
        }
        rows.push(KnowledgeTransportCutoverVerificationRowV1 {
            project_id: row.project_id.clone(),
            coverage,
            capability_observations: current,
        });
    }
    Ok(KnowledgeTransportCutoverVerificationReceiptV1 {
        version: RECEIPT_VERSION,
        marker_checksum_sha256: marker.checksum_sha256.clone(),
        covered_project_count: rows.len() as u64,
        current_project_count: rows.iter().filter(|row| row.coverage.current()).count() as u64,
        rows,
    })
}

fn write_receipt(
    state_dir: &Path,
    marker: &KnowledgeTransportCutoverMarkerV1,
    verified_at: &str,
) -> CutoverResult<()> {
    let receipt = KnowledgeTransportCutoverReceiptV1 {
        version: RECEIPT_VERSION,
        applied_at: marker.applied_at.clone(),
        verified_at: verified_at.to_string(),
        marker_checksum_sha256: marker.checksum_sha256.clone(),
        report_artifact_hash: marker.report_artifact_hash.clone(),
        resolution_artifact_hash: marker.resolution_artifact_hash.clone(),
        covered_project_count: marker.rows.len() as u64,
    };
    let bytes = serde_json::to_vec(&receipt)
        .map_err(|cause| error("error.knowledge_transport_cutover_receipt", cause))?;
    if bytes.len() > MAX_KNOWLEDGE_TRANSPORT_CUTOVER_RECEIPT_BYTES {
        return Err(error(
            "error.knowledge_transport_cutover_receipt",
            "knowledge transport cutover receipt exceeds its bound",
        ));
    }
    atomic_write_bytes_locked(&knowledge_transport_cutover_receipt_path(state_dir), &bytes)
        .map_err(|cause| error("error.knowledge_transport_cutover_receipt_write", cause))
}

fn configured_assignments(config: &Config) -> CutoverResult<BTreeMap<PublishedScope, String>> {
    let mut assignments = BTreeMap::new();
    for producer in &config.code_collection.producers {
        if producer.producer_id.trim().is_empty() || producer.scopes.is_empty() {
            return Err(error(
                "error.knowledge_transport_cutover_config",
                "every producer requires an id and at least one scope",
            ));
        }
        for scope in &producer.scopes {
            if assignments
                .insert(scope.clone(), producer.producer_id.clone())
                .is_some()
            {
                return Err(error(
                    "error.knowledge_transport_cutover_config",
                    "a published scope is assigned to more than one producer",
                ));
            }
        }
    }
    Ok(assignments)
}

fn validate_assignments_resolve(
    catalog: &bbox_corpus_core::project_catalog::CatalogSnapshotV2,
    assignments: &BTreeMap<PublishedScope, String>,
) -> CutoverResult<()> {
    for scope in assignments.keys() {
        let count = catalog
            .projects
            .values()
            .filter(|project| {
                matches!(&project.scope, ProjectScope::Published(candidate) if candidate == scope)
            })
            .count();
        if count != 1 {
            return Err(error(
                "error.knowledge_transport_cutover_config",
                format!("configured scope {scope:?} resolves to {count} projects"),
            ));
        }
    }
    Ok(())
}

fn require_enabled(config: &Config) -> CutoverResult<()> {
    if !config.code_collection.enabled || !config.code_collection.knowledge_transport_enabled {
        return Err(error(
            "error.knowledge_transport_cutover_disabled",
            "code collection and knowledge transport must both be enabled",
        ));
    }
    Ok(())
}

fn source_store_limits(config: &Config) -> StoreLimits {
    StoreLimits {
        max_open_uploads_per_authority: config.code_collection.max_open_uploads_per_producer,
        retained_publication_generations: config.code_collection.retained_generations,
        unreferenced_blob_grace_secs: config
            .code_collection
            .unreferenced_blob_grace_hours
            .saturating_mul(60 * 60),
        ..StoreLimits::default()
    }
}

fn open_source_store(
    layout: &ProjectCatalogMigrationResolvedLayoutV1,
    config: &Config,
) -> CutoverResult<Option<KnowledgeSourceStore>> {
    let root = layout.state_dir.join("knowledge-sources");
    match std::fs::symlink_metadata(&root) {
        Ok(metadata) if metadata.file_type().is_dir() => {
            KnowledgeSourceStore::open_existing(root, source_store_limits(config))
                .map(Some)
                .map_err(|cause| error("error.knowledge_transport_cutover_source", cause))
        }
        Ok(_) => Err(error(
            "error.knowledge_transport_cutover_source",
            "knowledge source root is not a real directory",
        )),
        Err(cause) if cause.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(cause) => Err(error("error.knowledge_transport_cutover_source", cause)),
    }
}

fn grant_commitment(
    project_id: &ProjectId,
    scope: &PublishedScope,
    producer_id: &str,
) -> Sha256ValueV1 {
    Sha256ValueV1::digest(
        &serde_json::to_vec(&(project_id, scope, producer_id))
            .expect("knowledge transport grant identity is serializable"),
    )
}

fn validate_timestamp(value: &str, operation: &str) -> CutoverResult<()> {
    if value.trim().is_empty() || value.len() > 128 {
        return Err(error(
            "error.knowledge_transport_cutover_timestamp",
            format!("{operation} timestamp is invalid"),
        ));
    }
    Ok(())
}

fn now_unix_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn count_status(
    projects: &[KnowledgeTransportProjectEvidenceV1],
    status: KnowledgeTransportCoverageStatusV1,
) -> u64 {
    projects
        .iter()
        .filter(|project| project.coverage_status == status)
        .count() as u64
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::knowledge_transport_observations::{
        KnowledgeTransportObservationsV1, KnowledgeTransportOperationV1 as Operation,
        KnowledgeTransportOutcomeV1 as Outcome,
    };
    use bbox_corpus_core::project_catalog::{CatalogSnapshotV2, CorpusProject};

    fn project_id() -> ProjectId {
        ProjectId::parse("p_00000000000000000000000000000001").unwrap()
    }

    fn row() -> PredictedKnowledgeTransportCutoverRowV1 {
        PredictedKnowledgeTransportCutoverRowV1 {
            project_id: project_id(),
            scope: PublishedScope::try_new("repo", ".").unwrap(),
            producer_id: "producer-1".into(),
            grant_commitment: Sha256ValueV1::digest(b"grant"),
            accepted_generation_id: "a".repeat(64),
            accepted_generation_sha256: "b".repeat(64),
            accepted_pointer_sha256: "c".repeat(64),
            source_generation_id: format!("kps_{}", "d".repeat(64)),
            source_generation_sha256: "e".repeat(64),
            publication_parity_commitment: Sha256ValueV1::digest(b"parity"),
            parity_workspace_ids: Vec::new(),
            workspace_parity_commitment: Sha256ValueV1::digest(b"workspace-parity"),
            shadow_observation_commitment: Sha256ValueV1::digest(b"shadow"),
            capability_baselines: CUTOVER_CAPABILITIES
                .into_iter()
                .map(|capability| KnowledgeTransportCapabilityBaselineV1 {
                    capability,
                    granted: 0,
                    denied: 0,
                })
                .collect(),
            observation_window_start_sequence: 0,
            observation_window_end_sequence: 0,
        }
    }

    fn coverage_fixture() -> (
        CatalogSnapshotV2,
        PublishedScope,
        PredictedKnowledgeTransportCutoverRowV1,
    ) {
        let scope = PublishedScope::try_new("repo", ".").unwrap();
        let mut catalog = CatalogSnapshotV2::empty(1).unwrap();
        catalog.projects.insert(
            project_id(),
            CorpusProject {
                project_id: project_id(),
                scope: ProjectScope::Published(scope.clone()),
                operator_aliases: BTreeSet::new(),
                nominated_aliases: BTreeSet::new(),
                display_name: "Neutral fixture".into(),
                created_at: "unix:1".into(),
                registered_at_compat: None,
                repo_history: None,
                languages: BTreeSet::new(),
            },
        );
        catalog.validate().unwrap();
        let mut row = row();
        row.scope = scope.clone();
        row.grant_commitment = grant_commitment(&project_id(), &scope, "producer-1");
        (catalog, scope, row)
    }

    fn overlap_evidence(
        observations: &KnowledgeTransportObservationSnapshotV1,
    ) -> KnowledgeTransportProjectEvidenceV1 {
        let mut evidence = project_evidence_base(
            &project_id(),
            &PublishedScope::try_new("repo", ".").unwrap(),
            Some("producer-1".into()),
            KnowledgeTransportCoverageStatusV1::Proposed,
            &[],
            observations,
        );
        evidence.workspace_parity = vec![KnowledgeTransportWorkspaceParityV1 {
            workspace_id: "workspace-1".into(),
            source_generation_id: format!("kws_{}", "d".repeat(64)),
            sequence: 7,
            accepted_generation_id: "a".repeat(64),
            lease_expires_unix_secs: u64::MAX,
            knowledge_snapshot_id: "knowledge-snapshot".into(),
            gap_snapshot_id: "gap-snapshot".into(),
        }];
        evidence
    }

    #[test]
    fn overlap_gate_requires_every_selected_workspace_lane_and_reopened_identity() {
        let observations = KnowledgeTransportObservationsV1::in_memory();
        for operation in [Operation::PublishedKnowledge, Operation::PublishedGaps] {
            observations
                .record(project_id().as_str(), operation, Outcome::Remote)
                .unwrap();
        }
        observations
            .record(
                project_id().as_str(),
                Operation::WatcherRefresh,
                Outcome::Local,
            )
            .unwrap();
        for (operation, snapshot_id) in [
            (Operation::ProvisionalOwnKnowledge, "knowledge-snapshot"),
            (Operation::ProvisionalOwnGaps, "gap-snapshot"),
            (Operation::ProvisionalAllKnowledge, "knowledge-snapshot"),
            (Operation::ProvisionalAllGaps, "gap-snapshot"),
        ] {
            observations
                .record(project_id().as_str(), operation, Outcome::Local)
                .unwrap();
            observations
                .record_shadow(
                    project_id().as_str(),
                    operation,
                    Some("workspace-1"),
                    snapshot_id,
                    snapshot_id,
                )
                .unwrap();
        }
        let snapshot = observations.snapshot();
        let mut evidence = overlap_evidence(&snapshot);

        validate_overlap_observations(&mut evidence, &snapshot);

        assert!(evidence.defects.is_empty(), "{:?}", evidence.defects);
    }

    #[test]
    fn overlap_gate_refuses_absent_and_stale_workspace_evidence() {
        let observations = KnowledgeTransportObservationsV1::in_memory();
        observations
            .record(
                project_id().as_str(),
                Operation::PublishedKnowledge,
                Outcome::Remote,
            )
            .unwrap();
        observations
            .record(
                project_id().as_str(),
                Operation::ProvisionalOwnKnowledge,
                Outcome::Local,
            )
            .unwrap();
        observations
            .record_shadow(
                project_id().as_str(),
                Operation::ProvisionalOwnKnowledge,
                Some("workspace-1"),
                "stale-local",
                "stale-remote",
            )
            .unwrap();
        let snapshot = observations.snapshot();
        let mut evidence = overlap_evidence(&snapshot);

        validate_overlap_observations(&mut evidence, &snapshot);

        assert!(
            evidence
                .defects
                .iter()
                .any(|defect| defect.contains("PublishedGaps"))
        );
        assert!(
            evidence
                .defects
                .iter()
                .any(|defect| defect.contains("watcher refresh"))
        );
        assert!(evidence.defects.iter().any(|defect| {
            defect.contains("ProvisionalOwnKnowledge")
                && defect.contains("reopened remote snapshot")
        }));
        assert!(
            evidence
                .defects
                .iter()
                .any(|defect| defect.contains("ProvisionalAllGaps"))
        );
    }

    #[test]
    fn blocked_projects_require_exact_nonempty_operator_acknowledgement() {
        let observations = KnowledgeTransportObservationsV1::in_memory().snapshot();
        let blocked = vec![project_evidence_base(
            &project_id(),
            &PublishedScope::try_new("repo", ".").unwrap(),
            None,
            KnowledgeTransportCoverageStatusV1::BlockedPublishedNeverCovered,
            &[],
            &observations,
        )];
        let mut resolution = KnowledgeTransportCutoverResolutionV1 {
            version: RESOLUTION_VERSION,
            inventory_hash: Sha256ValueV1::digest(b"inventory"),
            blocked_project_acknowledgements: BTreeMap::new(),
        };

        assert_eq!(
            validate_blocked_project_acknowledgements(&resolution, &blocked).unwrap(),
            BTreeSet::from([project_id()])
        );
        resolution.blocked_project_acknowledgements.insert(
            project_id(),
            "operator accepts that this project remains uncovered".into(),
        );
        assert!(
            validate_blocked_project_acknowledgements(&resolution, &blocked)
                .unwrap()
                .is_empty()
        );
        resolution
            .blocked_project_acknowledgements
            .insert(project_id(), " ".into());
        assert!(
            validate_blocked_project_acknowledgements(&resolution, &blocked)
                .unwrap_err()
                .message
                .contains("operator rationale")
        );
    }

    #[test]
    fn marker_checksum_covers_rows_and_requires_receipt_on_open() {
        let root = tempfile::tempdir().unwrap();
        let mut marker = KnowledgeTransportCutoverMarkerV1 {
            version: MARKER_VERSION,
            applied_at: "2026-08-09T00:00:00Z".into(),
            report_artifact_hash: Sha256ValueV1::digest(b"report"),
            resolution_artifact_hash: Sha256ValueV1::digest(b"resolution"),
            predecessor_marker_checksum: None,
            predecessor_catalog_epoch: 1,
            inventory_hash: Sha256ValueV1::digest(b"inventory"),
            observation_snapshot_hash: Sha256ValueV1::digest(b"observations"),
            rows: vec![row()],
            checksum_sha256: Sha256ValueV1::digest(b"pending"),
        };
        marker.checksum_sha256 = marker_checksum(&marker).unwrap();
        let bytes = serde_json::to_vec(&marker).unwrap();
        assert_eq!(
            decode_knowledge_transport_cutover_marker_v1(&bytes).unwrap(),
            marker
        );
        atomic_write_bytes_locked(
            &knowledge_transport_cutover_marker_path(root.path()),
            &bytes,
        )
        .unwrap();
        let failure = KnowledgeTransportCutoverRuntimeV1::open(root.path()).unwrap_err();
        assert_eq!(
            failure.code,
            "error.knowledge_transport_cutover_verify_required"
        );
    }

    #[test]
    fn marker_row_is_a_monotonic_no_fallback_boundary() {
        let marker = KnowledgeTransportCutoverMarkerV1 {
            version: MARKER_VERSION,
            applied_at: "2026-08-09T00:00:00Z".into(),
            report_artifact_hash: Sha256ValueV1::digest(b"report"),
            resolution_artifact_hash: Sha256ValueV1::digest(b"resolution"),
            predecessor_marker_checksum: None,
            predecessor_catalog_epoch: 1,
            inventory_hash: Sha256ValueV1::digest(b"inventory"),
            observation_snapshot_hash: Sha256ValueV1::digest(b"observations"),
            rows: vec![row()],
            checksum_sha256: Sha256ValueV1::digest(b"unused"),
        };
        let runtime = KnowledgeTransportCutoverRuntimeV1::from_marker(Some(marker));
        assert!(runtime.covers_project(&project_id()));
        assert!(runtime.covers_project_str(project_id().as_str()));
        assert!(!runtime.covers_project_str("p_missing"));
    }

    #[test]
    fn every_runtime_drift_state_preserves_the_no_fallback_boundary() {
        let (mut catalog, scope, row) = coverage_fixture();
        let row_evidence = (
            row.accepted_generation_id.clone(),
            row.accepted_generation_sha256.clone(),
            row.accepted_pointer_sha256.clone(),
            row.source_generation_id.clone(),
            row.source_generation_sha256.clone(),
        );
        let runtime = KnowledgeTransportCutoverRuntimeV1::from_marker(Some(
            KnowledgeTransportCutoverMarkerV1 {
                version: MARKER_VERSION,
                applied_at: "2026-08-09T00:00:00Z".into(),
                report_artifact_hash: Sha256ValueV1::digest(b"report"),
                resolution_artifact_hash: Sha256ValueV1::digest(b"resolution"),
                predecessor_marker_checksum: None,
                predecessor_catalog_epoch: 1,
                inventory_hash: Sha256ValueV1::digest(b"inventory"),
                observation_snapshot_hash: Sha256ValueV1::digest(b"observations"),
                rows: vec![row],
                checksum_sha256: Sha256ValueV1::digest(b"unused"),
            },
        ));

        let cases = [
            (
                BTreeMap::new(),
                KnowledgeTransportRuntimeCoverageV1::CoveredProducerRemoved,
            ),
            (
                BTreeMap::from([(scope.clone(), "producer-2".into())]),
                KnowledgeTransportRuntimeCoverageV1::ProducerAssignmentPendingRecutover,
            ),
            (
                BTreeMap::from([(scope.clone(), "producer-1".into())]),
                KnowledgeTransportRuntimeCoverageV1::AcceptedSourcePendingRecutover,
            ),
        ];
        for (assignments, expected) in cases {
            let actual = runtime.classify_project(&catalog, &assignments, &project_id(), None);
            assert_eq!(actual, expected);
            assert!(actual.transport_governed());
            assert!(!actual.current());
        }

        // Tolerant advancement: an accepted publication that advanced through
        // the same producer stays current even though every pinned evidence
        // hash now differs from the row. A different producer or a missing
        // accepted publication still pend re-cutover.
        let assignments = BTreeMap::from([(scope.clone(), "producer-1".to_string())]);
        let accepted_at_row =
            crate::accepted_publication_runtime::VerifiedAcceptedPublication::for_test(
                &project_id(),
                &scope,
                &row_evidence.0,
                &row_evidence.1,
                &row_evidence.2,
                crate::accepted_publication_runtime::AcceptedPublicationSourceBinding::Producer {
                    producer_id: "producer-1".into(),
                    source_generation_id: row_evidence.3.clone(),
                    source_generation_sha256: row_evidence.4.clone(),
                },
            );
        let current = runtime.classify_project(
            &catalog,
            &assignments,
            &project_id(),
            Some(&accepted_at_row),
        );
        assert_eq!(current, KnowledgeTransportRuntimeCoverageV1::Current);

        let advanced = crate::accepted_publication_runtime::VerifiedAcceptedPublication::for_test(
            &project_id(),
            &scope,
            &"9".repeat(64),
            &"8".repeat(64),
            &"7".repeat(64),
            crate::accepted_publication_runtime::AcceptedPublicationSourceBinding::Producer {
                producer_id: "producer-1".into(),
                source_generation_id: format!("kps_{}", "6".repeat(64)),
                source_generation_sha256: "5".repeat(64),
            },
        );
        let after_advance =
            runtime.classify_project(&catalog, &assignments, &project_id(), Some(&advanced));
        assert_eq!(
            after_advance,
            KnowledgeTransportRuntimeCoverageV1::Current,
            "an accepted advance through the same producer must stay current"
        );

        let foreign_producer =
            crate::accepted_publication_runtime::VerifiedAcceptedPublication::for_test(
                &project_id(),
                &scope,
                &"9".repeat(64),
                &"8".repeat(64),
                &"7".repeat(64),
                crate::accepted_publication_runtime::AcceptedPublicationSourceBinding::Producer {
                    producer_id: "producer-2".into(),
                    source_generation_id: format!("kps_{}", "6".repeat(64)),
                    source_generation_sha256: "5".repeat(64),
                },
            );
        let foreign = runtime.classify_project(
            &catalog,
            &assignments,
            &project_id(),
            Some(&foreign_producer),
        );
        assert_eq!(
            foreign,
            KnowledgeTransportRuntimeCoverageV1::AcceptedSourcePendingRecutover
        );

        catalog.projects.get_mut(&project_id()).unwrap().scope =
            ProjectScope::Published(PublishedScope::try_new("repo", "moved").unwrap());
        let migrated = runtime.classify_project(
            &catalog,
            &BTreeMap::from([(scope, "producer-1".into())]),
            &project_id(),
            None,
        );
        assert_eq!(
            migrated,
            KnowledgeTransportRuntimeCoverageV1::ScopeMigrationPendingRecutover
        );
        assert!(migrated.transport_governed());
    }

    #[test]
    fn marker_tampering_fails_closed() {
        let mut marker = KnowledgeTransportCutoverMarkerV1 {
            version: MARKER_VERSION,
            applied_at: "2026-08-09T00:00:00Z".into(),
            report_artifact_hash: Sha256ValueV1::digest(b"report"),
            resolution_artifact_hash: Sha256ValueV1::digest(b"resolution"),
            predecessor_marker_checksum: None,
            predecessor_catalog_epoch: 1,
            inventory_hash: Sha256ValueV1::digest(b"inventory"),
            observation_snapshot_hash: Sha256ValueV1::digest(b"observations"),
            rows: vec![row()],
            checksum_sha256: Sha256ValueV1::digest(b"pending"),
        };
        marker.checksum_sha256 = marker_checksum(&marker).unwrap();
        marker.rows[0].producer_id = "tampered".into();

        let error =
            decode_knowledge_transport_cutover_marker_v1(&serde_json::to_vec(&marker).unwrap())
                .unwrap_err();
        assert_eq!(
            error.code,
            "error.knowledge_transport_cutover_marker_identity"
        );
    }
}
