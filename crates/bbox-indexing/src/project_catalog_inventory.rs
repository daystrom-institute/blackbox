//! Pure P1-C inventory, report, resolution, and deterministic plan contracts.
//!
//! This module accepts immutable observations only. It never opens a store,
//! reads a checkout, consults process state, or mutates migration participants.
//! Literal checkout and legacy selectors are confined to non-serializable
//! runtime bindings and the explicitly sensitive local-path report.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use bbox_code_source_store::MAX_SNAPSHOT_ID_BYTES;
use bbox_corpus_core::identity::PublishedScope;
use bbox_corpus_core::language::Language;
use bbox_corpus_core::project_catalog::{
    AttachmentId, CommitNamespace, LegacyPathBindingId, LegacyProjectRecordV1,
    MAX_PROJECT_CATALOG_ENTRIES, ProjectCatalogTransactionId, ProjectId, RecordedRepoAuthority,
    RepoHistoryId,
};
use serde::de::{self, DeserializeOwned};
use serde::{Deserialize, Deserializer, Serialize};
use sha2::{Digest, Sha256};

pub const PROJECT_CATALOG_INVENTORY_VERSION_V1: u32 = 1;
pub const PROJECT_CATALOG_MIGRATION_REPORT_VERSION_V1: u32 = 1;
pub const PROJECT_CATALOG_MIGRATION_RESOLUTION_VERSION_V1: u32 = 1;
pub const SENSITIVE_LOCAL_PATH_REPORT_VERSION_V1: u32 = 1;
pub const MAX_PROJECT_CATALOG_INVENTORY_BYTES: usize = 64 * 1024 * 1024;
pub const MAX_PROJECT_CATALOG_REPORT_BYTES: usize = 16 * 1024 * 1024;
pub const MAX_PROJECT_CATALOG_RESOLUTION_BYTES: usize = 8 * 1024 * 1024;

const MAX_SOURCE_STORE_BYTES: usize = 8 * 1024 * 1024;
const MAX_PATH_BYTES: usize = 4096;
const MAX_ID_BYTES: usize = 256;
const MAX_REF_BYTES: usize = 4096;
const MAX_DIAGNOSTIC_BYTES: usize = 512;
const MAX_OPERATOR_NOTE_BYTES: usize = 4096;
const INVENTORY_HASH_DOMAIN: &[u8] = b"blackbox.project-catalog.inventory.v1\0";
const PLAN_HASH_DOMAIN: &[u8] = b"blackbox.project-catalog.plan.v1\0";
const PATH_DIGEST_DOMAIN: &[u8] = b"blackbox.project-catalog.legacy-path.v1\0";
const GROUP_ID_DOMAIN: &[u8] = b"blackbox.project-catalog.repo-group.v1\0";
const IMMUTABLE_OWNER_ROW_SET_DOMAIN: &[u8] =
    b"blackbox.project-catalog.immutable-owner-row-set.v1\0";
const IMMUTABLE_LANE_FINGERPRINT_DOMAIN: &[u8] =
    b"blackbox.project-catalog.immutable-lane-fingerprint.v1\0";
const IMMUTABLE_LANE_CONTENT_DOMAIN: &[u8] =
    b"blackbox.project-catalog.immutable-lane-content.v1\0";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectCatalogInventoryError {
    code: &'static str,
    detail: String,
}

impl ProjectCatalogInventoryError {
    fn new(code: &'static str, detail: impl Into<String>) -> Self {
        Self {
            code,
            detail: bounded_detail(detail.into()),
        }
    }

    pub fn code(&self) -> &'static str {
        self.code
    }
}

impl fmt::Display for ProjectCatalogInventoryError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}: {}", self.code, self.detail)
    }
}

impl std::error::Error for ProjectCatalogInventoryError {}

pub type InventoryResult<T> = Result<T, ProjectCatalogInventoryError>;

fn bounded_detail(value: String) -> String {
    value
        .chars()
        .map(|ch| if ch.is_control() { ' ' } else { ch })
        .take(MAX_DIAGNOSTIC_BYTES)
        .collect()
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(transparent)]
pub struct Sha256ValueV1(String);

impl Sha256ValueV1 {
    pub fn parse(value: impl Into<String>) -> InventoryResult<Self> {
        let value = value.into();
        if value.len() != 64
            || !value
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
        {
            return Err(ProjectCatalogInventoryError::new(
                "error.project_catalog_inventory_invalid_hash",
                "expected a lowercase SHA-256 value",
            ));
        }
        Ok(Self(value))
    }

    pub fn digest(bytes: &[u8]) -> Self {
        Self(hex::encode(Sha256::digest(bytes)))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for Sha256ValueV1 {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for Sha256ValueV1 {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Self::parse(String::deserialize(deserializer)?).map_err(de::Error::custom)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LegacyProjectRecordInventoryV1 {
    pub project_id: String,
    pub repo_id: Option<String>,
    pub canonical_path_digest: Sha256ValueV1,
    pub registered_at: String,
    pub is_git_repo: bool,
    pub languages: BTreeSet<Language>,
    pub aliases: BTreeSet<String>,
}

impl LegacyProjectRecordInventoryV1 {
    pub fn from_legacy(record: LegacyProjectRecordV1) -> Self {
        Self {
            project_id: record.project_id,
            repo_id: record.repo_id,
            canonical_path_digest: digest_path(&record.canonical_path),
            registered_at: record.registered_at,
            is_git_repo: record.is_git_repo,
            languages: record.languages,
            aliases: record.aliases,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LegacyProjectPathStatusV1 {
    Present,
    Missing,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CommittedAuthorityObservationV1 {
    pub observation_id: String,
    pub authority: RecordedRepoAuthority,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LegacyProjectObservationV1 {
    pub observation_id: String,
    pub record: LegacyProjectRecordInventoryV1,
    pub path_status: LegacyProjectPathStatusV1,
    pub committed_authority: Option<CommittedAuthorityObservationV1>,
    pub committed_scope: Option<PublishedScope>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CollectedGenerationRoleV1 {
    Active,
    Retained,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "authority", rename_all = "snake_case", deny_unknown_fields)]
pub enum DurableSelectorEvidenceV1 {
    ExactMaterialized { selector_hash: Sha256ValueV1 },
    NoDurableSelector,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case", deny_unknown_fields)]
pub enum ImmutableCollectedDescriptorV1 {
    Valid {
        descriptor_hash: Sha256ValueV1,
        published_scope: PublishedScope,
    },
    Missing,
    Corrupt {
        diagnostic_code: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case", deny_unknown_fields)]
pub enum ImmutableArtifactObservationV1 {
    Valid { content_hash: Sha256ValueV1 },
    Missing,
    Corrupt { diagnostic_code: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CollectedGenerationObservationV1 {
    pub observation_id: String,
    pub project_id: ProjectId,
    pub role: CollectedGenerationRoleV1,
    pub generation_id: String,
    pub activation_scope: Option<PublishedScope>,
    pub descriptor: ImmutableCollectedDescriptorV1,
    pub manifest: ImmutableArtifactObservationV1,
    pub selector_evidence: DurableSelectorEvidenceV1,
    pub checkout_missing: bool,
    pub planned_metadata_v2_hash: Sha256ValueV1,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QuarantinedGenerationObservationV1 {
    pub observation_id: String,
    pub project_id: ProjectId,
    pub generation_id: String,
    pub descriptor: ImmutableCollectedDescriptorV1,
    pub manifest: ImmutableArtifactObservationV1,
    pub manifest_hash: Sha256ValueV1,
    pub planned_metadata_v2_hash: Sha256ValueV1,
    pub collision_lifecycle: CollisionLifecycleObservationV1,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CollisionLifecycleStateObservationV1 {
    Pending,
    Queued,
    Completed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CollisionLifecycleObservationV1 {
    pub version: u32,
    pub state: CollisionLifecycleStateObservationV1,
    pub project_id: ProjectId,
    pub former_scope: PublishedScope,
    pub generation_id: String,
    pub selector_evidence: DurableSelectorEvidenceV1,
    pub snapshot_id: String,
    pub manifest_sha256: Sha256ValueV1,
    pub inventory_hash: Sha256ValueV1,
    pub plan_hash: Sha256ValueV1,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CodeSourceObservationV1 {
    pub observation_id: String,
    pub project_id: ProjectId,
    pub generations: Vec<CollectedGenerationObservationV1>,
    pub quarantine: Vec<QuarantinedGenerationObservationV1>,
    pub effective_manifest_hash: Sha256ValueV1,
    pub planned_activation_v2_hash: Option<Sha256ValueV1>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RetainedGenerationOwnerResolutionObservationV1 {
    pub observation_id: String,
    pub generation_id: String,
    pub published_scope: PublishedScope,
    pub candidate_project_ids: BTreeSet<ProjectId>,
    pub descriptor: ImmutableCollectedDescriptorV1,
    pub manifest: ImmutableArtifactObservationV1,
    pub selector_evidence: DurableSelectorEvidenceV1,
    pub planned_metadata_v2_hash: Sha256ValueV1,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case", deny_unknown_fields)]
pub enum InventorySourceStateV1 {
    Present {
        fingerprint: Sha256ValueV1,
        content_hash: Sha256ValueV1,
        byte_len: u64,
    },
    Missing {
        fingerprint: Sha256ValueV1,
    },
    Corrupt {
        fingerprint: Sha256ValueV1,
        content_hash: Option<Sha256ValueV1>,
        diagnostic_code: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MutableInventorySourceKindV1 {
    LegacyProjectStore,
    PublisherRefStore,
    EffectiveSourceManifest,
    CodeSourceActivation,
    CodeSourceGenerationMetadata,
    CodeSourceGenerationManifest,
    CodeSourceCollisionLifecycle,
    CommittedAuthorityProbe,
    CheckoutRoot,
    CheckoutMarker,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum MutableInventorySourceLocatorV1 {
    LegacyProjectStore,
    PublisherRefStore,
    CodeSourceAnchor,
    CodeSourceActivation {
        project_id: ProjectId,
    },
    CodeSourceGenerationMetadata {
        scope_hash: Sha256ValueV1,
        generation_id: String,
    },
    CodeSourceGenerationManifest {
        scope_hash: Sha256ValueV1,
        generation_id: String,
    },
    CodeSourceCollisionLifecycle {
        project_id: ProjectId,
    },
    CommittedProjectConfig {
        project_id: ProjectId,
        commit_oid: String,
        repo_relative_path: String,
    },
    CommittedProjectConfigUnavailable {
        project_id: ProjectId,
    },
    CheckoutRoot {
        canonical_root_digest: Sha256ValueV1,
    },
    CheckoutMarker {
        canonical_root_digest: Sha256ValueV1,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MutableInventorySourceEvidenceV1 {
    pub source_id: String,
    pub source_kind: MutableInventorySourceKindV1,
    pub source_locator: MutableInventorySourceLocatorV1,
    pub state: InventorySourceStateV1,
    pub row_observation_ids: BTreeSet<String>,
    pub row_set_sha256: Sha256ValueV1,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ImmutableInventoryLaneKindV1 {
    ProjectScopedRefs,
    EdgeWorkspaces,
    GitMetadata,
    Checkouts,
    AttachmentCandidates,
    InventoryTargets,
    MaterializedAliases,
    LegacyPathObservations,
    RepoGroupingProofs,
    LegacyNamespaceClusters,
}

impl ImmutableInventoryLaneKindV1 {
    fn all() -> BTreeSet<Self> {
        BTreeSet::from([
            Self::ProjectScopedRefs,
            Self::EdgeWorkspaces,
            Self::GitMetadata,
            Self::Checkouts,
            Self::AttachmentCandidates,
            Self::InventoryTargets,
            Self::MaterializedAliases,
            Self::LegacyPathObservations,
            Self::RepoGroupingProofs,
            Self::LegacyNamespaceClusters,
        ])
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ImmutableInventoryLaneCompletenessV1 {
    Complete,
    Missing,
    Corrupt,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ImmutableInventoryLaneEvidenceV1 {
    pub lane_kind: ImmutableInventoryLaneKindV1,
    pub source_id: String,
    pub source_state: InventorySourceStateV1,
    pub completeness: ImmutableInventoryLaneCompletenessV1,
    pub row_count: u64,
    pub owner_subsources: Vec<OwnerSubsourceEvidenceV1>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ImmutableInventoryOwnerKindV1 {
    Tantivy,
    VectorMetadata,
    EdgeManifest,
    GitMetadata,
    Checkout,
    LegacyProjectStore,
    Knowledge,
    Gap,
    Thread,
    Note,
    Pin,
    Roadmap,
    Packet,
    Task,
    Proposal,
    SlackBinding,
    Whiteboard,
    Artifact,
    Provenance,
    TranscriptEdge,
    DerivedRepoGrouping,
    DerivedLegacyNamespaceClusters,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OwnerSubsourceEvidenceV1 {
    pub owner_kind: ImmutableInventoryOwnerKindV1,
    pub source_id: String,
    pub source_state: InventorySourceStateV1,
    pub row_observation_ids: BTreeSet<String>,
    pub row_set_sha256: Sha256ValueV1,
}

impl ImmutableInventoryLaneEvidenceV1 {
    pub fn from_owner_subsources(
        lane_kind: ImmutableInventoryLaneKindV1,
        source_id: impl Into<String>,
        row_count: u64,
        mut owner_subsources: Vec<OwnerSubsourceEvidenceV1>,
    ) -> InventoryResult<Self> {
        owner_subsources.sort_by(|left, right| {
            left.owner_kind
                .cmp(&right.owner_kind)
                .then_with(|| left.source_id.cmp(&right.source_id))
        });
        let completeness = lane_completeness(&owner_subsources);
        let source_state = aggregate_lane_source_state(lane_kind, &owner_subsources)?;
        let evidence = Self {
            lane_kind,
            source_id: source_id.into(),
            source_state,
            completeness,
            row_count,
            owner_subsources,
        };
        validate_immutable_lane_evidence(&evidence)?;
        Ok(evidence)
    }
}

impl OwnerSubsourceEvidenceV1 {
    pub fn new(
        owner_kind: ImmutableInventoryOwnerKindV1,
        source_id: impl Into<String>,
        source_state: InventorySourceStateV1,
        row_observation_ids: BTreeSet<String>,
    ) -> Self {
        let row_set_sha256 = immutable_owner_row_set_hash(&row_observation_ids);
        Self {
            owner_kind,
            source_id: source_id.into(),
            source_state,
            row_observation_ids,
            row_set_sha256,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PublisherPinObservationV1 {
    pub observation_id: String,
    pub project_id: ProjectId,
    pub expected_scope: PublishedScope,
    pub full_ref: String,
    pub candidate_attachment_ids: BTreeSet<AttachmentId>,
    pub resolved_commit: Option<String>,
    pub resolved_scope: Option<PublishedScope>,
    pub source_observation_ids: BTreeSet<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProjectScopedRefStoreKindV1 {
    Tantivy,
    VectorMetadata,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProjectScopedRefObservationV1 {
    pub observation_id: String,
    pub store_kind: ProjectScopedRefStoreKindV1,
    pub project_id: ProjectId,
    pub stable_row_id: String,
    pub entity_ref_hash: Sha256ValueV1,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EdgeWorkspaceObservationV1 {
    pub observation_id: String,
    pub workspace_id: String,
    pub project_ids: BTreeSet<ProjectId>,
    pub manifest_hash: Sha256ValueV1,
    pub active_selector_hash: Sha256ValueV1,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GitMetadataObservationV1 {
    pub observation_id: String,
    pub project_id: ProjectId,
    pub checkout_observation_id: String,
    pub common_directory_digest: Option<Sha256ValueV1>,
    pub full_first_commit: Option<String>,
    pub materialized_commit_namespaces: BTreeSet<String>,
    pub last_ingested_sha: Option<String>,
    pub resolved_refs: BTreeMap<String, String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case", deny_unknown_fields)]
pub enum CheckoutMarkerStateV1 {
    Valid { checkout_id: String },
    MissingOrEmpty,
    Malformed { diagnostic_code: String },
    Unreadable { diagnostic_code: String },
    Symlinked,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CheckoutObservationV1 {
    pub observation_id: String,
    pub canonical_root_digest: Sha256ValueV1,
    pub marker_state: CheckoutMarkerStateV1,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AttachmentCandidateObservationV1 {
    pub observation_id: String,
    pub attachment_id: AttachmentId,
    pub project_id: ProjectId,
    pub checkout_observation_id: String,
    pub base_relpath: String,
    pub observed_scope: Option<PublishedScope>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InventoryTargetKindV1 {
    ProjectArtifact,
    ProvenanceNote,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InventoryTargetObservationV1 {
    pub observation_id: String,
    pub target_kind: InventoryTargetKindV1,
    pub project_id: ProjectId,
    pub stable_target_id: String,
    pub target_hash: Sha256ValueV1,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MaterializedAliasObservationV1 {
    pub observation_id: String,
    pub alias: String,
    pub project_id: ProjectId,
    pub registered_at: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LegacyPathStoreKindV1 {
    Knowledge,
    Gap,
    Thread,
    Note,
    Pin,
    Roadmap,
    Packet,
    Task,
    Proposal,
    SlackBinding,
    Whiteboard,
    Artifact,
    Provenance,
    TranscriptEdge,
}

impl LegacyPathStoreKindV1 {
    fn all() -> BTreeSet<Self> {
        BTreeSet::from([
            Self::Knowledge,
            Self::Gap,
            Self::Thread,
            Self::Note,
            Self::Pin,
            Self::Roadmap,
            Self::Packet,
            Self::Task,
            Self::Proposal,
            Self::SlackBinding,
            Self::Whiteboard,
            Self::Artifact,
            Self::Provenance,
            Self::TranscriptEdge,
        ])
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LegacySelectorKindV1 {
    Project,
    ProjectAndRelativePath,
    AbsolutePath,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LegacyPathObservationV1 {
    pub observation_id: String,
    pub store_kind: LegacyPathStoreKindV1,
    pub stable_row_id: String,
    pub selector_kind: LegacySelectorKindV1,
    pub selector_digest: Sha256ValueV1,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RecordedAuthorityEvidenceMemberV1 {
    pub project_id: ProjectId,
    pub authority: RecordedRepoAuthority,
    pub authority_observation_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GitEvidenceMemberV1 {
    pub project_id: ProjectId,
    pub git_observation_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CollectedEvidenceMemberV1 {
    pub project_id: ProjectId,
    pub generation_observation_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "evidence_class", rename_all = "snake_case", deny_unknown_fields)]
pub enum RepoGroupingProofV1 {
    IdenticalCommittedRecordedAuthority {
        proof_id: String,
        members: Vec<RecordedAuthorityEvidenceMemberV1>,
    },
    SharedGitCommonDirectoryAndFirstCommit {
        proof_id: String,
        members: Vec<GitEvidenceMemberV1>,
    },
    CollectedDescriptorActivationAgreement {
        proof_id: String,
        members: Vec<CollectedEvidenceMemberV1>,
    },
}

impl RepoGroupingProofV1 {
    pub fn proof_id(&self) -> &str {
        match self {
            Self::IdenticalCommittedRecordedAuthority { proof_id, .. }
            | Self::SharedGitCommonDirectoryAndFirstCommit { proof_id, .. }
            | Self::CollectedDescriptorActivationAgreement { proof_id, .. } => proof_id,
        }
    }

    pub fn project_ids(&self) -> BTreeSet<ProjectId> {
        match self {
            Self::IdenticalCommittedRecordedAuthority { members, .. } => members
                .iter()
                .map(|member| member.project_id.clone())
                .collect(),
            Self::SharedGitCommonDirectoryAndFirstCommit { members, .. } => members
                .iter()
                .map(|member| member.project_id.clone())
                .collect(),
            Self::CollectedDescriptorActivationAgreement { members, .. } => members
                .iter()
                .map(|member| member.project_id.clone())
                .collect(),
        }
    }

    pub fn source_observation_ids(&self) -> BTreeSet<String> {
        match self {
            Self::IdenticalCommittedRecordedAuthority { members, .. } => members
                .iter()
                .map(|member| member.authority_observation_id.clone())
                .collect(),
            Self::SharedGitCommonDirectoryAndFirstCommit { members, .. } => members
                .iter()
                .map(|member| member.git_observation_id.clone())
                .collect(),
            Self::CollectedDescriptorActivationAgreement { members, .. } => members
                .iter()
                .map(|member| member.generation_observation_id.clone())
                .collect(),
        }
    }

    fn canonicalize(&mut self) {
        match self {
            Self::IdenticalCommittedRecordedAuthority { members, .. } => {
                members.sort_by(|left, right| left.project_id.cmp(&right.project_id));
            }
            Self::SharedGitCommonDirectoryAndFirstCommit { members, .. } => {
                members.sort_by(|left, right| left.project_id.cmp(&right.project_id));
            }
            Self::CollectedDescriptorActivationAgreement { members, .. } => {
                members.sort_by(|left, right| left.project_id.cmp(&right.project_id));
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LegacyNamespaceClusterObservationV1 {
    pub observation_id: String,
    pub cluster_id: String,
    pub materialized_namespace: String,
    pub project_ids: BTreeSet<ProjectId>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case", deny_unknown_fields)]
pub enum LegacyCommitNamespaceAttributionV1 {
    Proved {
        project_ids: BTreeSet<ProjectId>,
    },
    Ambiguous {
        candidate_project_ids: BTreeSet<ProjectId>,
    },
    Unclaimed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LegacyCommitNamespaceInventoryV1 {
    pub observation_id: String,
    pub namespace: CommitNamespace,
    pub commit_document_count: u64,
    pub commit_document_set_sha256: Sha256ValueV1,
    pub vector_key_count: u64,
    pub vector_key_set_sha256: Sha256ValueV1,
    pub attribution: LegacyCommitNamespaceAttributionV1,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct V1ProjectCatalogInventory {
    pub version: u32,
    pub source_store_hash: Sha256ValueV1,
    pub publisher_ref_source_hash: Sha256ValueV1,
    pub publisher_ref_source_bytes: Vec<u8>,
    pub code_source_inventory_hash: Sha256ValueV1,
    pub code_source_generation_count: u64,
    pub code_source_generation_set_sha256: Sha256ValueV1,
    pub mutable_source_evidence: Vec<MutableInventorySourceEvidenceV1>,
    pub immutable_lane_evidence: Vec<ImmutableInventoryLaneEvidenceV1>,
    pub legacy_projects: Vec<LegacyProjectObservationV1>,
    pub code_sources: Vec<CodeSourceObservationV1>,
    pub retained_owner_resolutions: Vec<RetainedGenerationOwnerResolutionObservationV1>,
    pub publisher_pins: Vec<PublisherPinObservationV1>,
    pub project_scoped_refs: Vec<ProjectScopedRefObservationV1>,
    pub edge_workspaces: Vec<EdgeWorkspaceObservationV1>,
    pub git_metadata: Vec<GitMetadataObservationV1>,
    pub checkouts: Vec<CheckoutObservationV1>,
    pub attachment_candidates: Vec<AttachmentCandidateObservationV1>,
    pub inventory_targets: Vec<InventoryTargetObservationV1>,
    pub materialized_aliases: Vec<MaterializedAliasObservationV1>,
    pub legacy_path_observations: Vec<LegacyPathObservationV1>,
    pub repo_grouping_proofs: Vec<RepoGroupingProofV1>,
    pub legacy_namespace_clusters: Vec<LegacyNamespaceClusterObservationV1>,
    pub legacy_commit_namespaces: Vec<LegacyCommitNamespaceInventoryV1>,
}

impl V1ProjectCatalogInventory {
    pub fn validate(&self) -> InventoryResult<()> {
        if self.version != PROJECT_CATALOG_INVENTORY_VERSION_V1 {
            return Err(invalid("unsupported inventory version"));
        }
        if self.publisher_ref_source_bytes.len() > MAX_SOURCE_STORE_BYTES {
            return Err(limit("captured source store"));
        }
        if self.publisher_ref_source_hash != Sha256ValueV1::digest(&self.publisher_ref_source_bytes)
        {
            return Err(ProjectCatalogInventoryError::new(
                "error.project_catalog_inventory_source_hash_mismatch",
                "captured source bytes do not match their recorded hash",
            ));
        }
        let materialized_generation_count = self
            .code_sources
            .iter()
            .map(|source| source.generations.len() + source.quarantine.len())
            .sum::<usize>()
            + self.retained_owner_resolutions.len();
        let materialized_generation_count = materialized_generation_count as u64;
        if self.code_source_generation_count < materialized_generation_count {
            return Err(invalid(
                "code-source complete-set count omits a materialized generation",
            ));
        }
        if self.mutable_source_evidence.len() > MAX_PROJECT_CATALOG_ENTRIES
            || self.immutable_lane_evidence.len() > ImmutableInventoryLaneKindV1::all().len()
        {
            return Err(limit("inventory source evidence"));
        }
        let mut source_ids = BTreeSet::new();
        for evidence in &self.mutable_source_evidence {
            validate_stable_id(&evidence.source_id, "mutable source id")?;
            if !source_ids.insert(evidence.source_id.as_str()) {
                return Err(duplicate("mutable source id"));
            }
            validate_mutable_source_locator(evidence.source_kind, &evidence.source_locator)?;
            validate_inventory_source_state(&evidence.state)?;
            for observation_id in &evidence.row_observation_ids {
                validate_stable_id(observation_id, "mutable source row observation id")?;
            }
            if evidence.row_set_sha256 != mutable_source_row_set_hash(&evidence.row_observation_ids)
            {
                return Err(invalid("mutable source row-set hash mismatch"));
            }
        }
        let source_kind_counts = self.mutable_source_evidence.iter().fold(
            BTreeMap::<MutableInventorySourceKindV1, usize>::new(),
            |mut counts, evidence| {
                *counts.entry(evidence.source_kind).or_default() += 1;
                counts
            },
        );
        let generation_count = self
            .code_sources
            .iter()
            .map(|source| source.generations.len() + source.quarantine.len())
            .sum::<usize>()
            + self.retained_owner_resolutions.len();
        for (kind, expected) in [
            (MutableInventorySourceKindV1::LegacyProjectStore, 1),
            (MutableInventorySourceKindV1::PublisherRefStore, 1),
            (MutableInventorySourceKindV1::EffectiveSourceManifest, 1),
            (
                MutableInventorySourceKindV1::CodeSourceActivation,
                self.code_sources.len(),
            ),
            (
                MutableInventorySourceKindV1::CodeSourceGenerationMetadata,
                generation_count,
            ),
            (
                MutableInventorySourceKindV1::CodeSourceGenerationManifest,
                generation_count,
            ),
            (
                MutableInventorySourceKindV1::CodeSourceCollisionLifecycle,
                self.code_sources
                    .iter()
                    .filter(|source| !source.quarantine.is_empty())
                    .count(),
            ),
            (
                MutableInventorySourceKindV1::CommittedAuthorityProbe,
                self.legacy_projects.len(),
            ),
            (
                MutableInventorySourceKindV1::CheckoutRoot,
                self.checkouts.len(),
            ),
            (
                MutableInventorySourceKindV1::CheckoutMarker,
                self.checkouts.len(),
            ),
        ] {
            if source_kind_counts.get(&kind).copied().unwrap_or_default() != expected {
                return Err(invalid("mutable inventory source coverage is incomplete"));
            }
        }
        for (kind, expected_hash) in [
            (
                MutableInventorySourceKindV1::LegacyProjectStore,
                &self.source_store_hash,
            ),
            (
                MutableInventorySourceKindV1::PublisherRefStore,
                &self.publisher_ref_source_hash,
            ),
        ] {
            let evidence = self
                .mutable_source_evidence
                .iter()
                .find(|evidence| evidence.source_kind == kind)
                .ok_or_else(|| invalid("required mutable source evidence is missing"))?;
            if let InventorySourceStateV1::Present { content_hash, .. } = &evidence.state
                && content_hash != expected_hash
            {
                return Err(invalid("mutable source evidence hash mismatch"));
            }
        }
        let mut lane_kinds = BTreeSet::new();
        for evidence in &self.immutable_lane_evidence {
            validate_immutable_lane_evidence(evidence)?;
            if !lane_kinds.insert(evidence.lane_kind) {
                return Err(duplicate("immutable inventory lane"));
            }
        }
        if lane_kinds != ImmutableInventoryLaneKindV1::all() {
            return Err(invalid("immutable inventory lane coverage is incomplete"));
        }
        let expected_lane_counts = BTreeMap::from([
            (
                ImmutableInventoryLaneKindV1::ProjectScopedRefs,
                self.project_scoped_refs.len() as u64,
            ),
            (
                ImmutableInventoryLaneKindV1::EdgeWorkspaces,
                self.edge_workspaces.len() as u64,
            ),
            (
                ImmutableInventoryLaneKindV1::GitMetadata,
                (self.git_metadata.len() + self.legacy_commit_namespaces.len()) as u64,
            ),
            (
                ImmutableInventoryLaneKindV1::Checkouts,
                self.checkouts.len() as u64,
            ),
            (
                ImmutableInventoryLaneKindV1::AttachmentCandidates,
                self.attachment_candidates.len() as u64,
            ),
            (
                ImmutableInventoryLaneKindV1::InventoryTargets,
                self.inventory_targets.len() as u64,
            ),
            (
                ImmutableInventoryLaneKindV1::MaterializedAliases,
                self.materialized_aliases.len() as u64,
            ),
            (
                ImmutableInventoryLaneKindV1::LegacyPathObservations,
                self.legacy_path_observations.len() as u64,
            ),
            (
                ImmutableInventoryLaneKindV1::RepoGroupingProofs,
                self.repo_grouping_proofs.len() as u64,
            ),
            (
                ImmutableInventoryLaneKindV1::LegacyNamespaceClusters,
                self.legacy_namespace_clusters.len() as u64,
            ),
        ]);
        for evidence in &self.immutable_lane_evidence {
            if evidence.row_count != expected_lane_counts[&evidence.lane_kind] {
                return Err(invalid("immutable lane row count does not match inventory"));
            }
        }

        for (kind, count) in [
            ("legacy projects", self.legacy_projects.len()),
            ("code sources", self.code_sources.len()),
            (
                "retained owner resolutions",
                self.retained_owner_resolutions.len(),
            ),
            ("publisher pins", self.publisher_pins.len()),
            ("project refs", self.project_scoped_refs.len()),
            ("edge workspaces", self.edge_workspaces.len()),
            ("git metadata", self.git_metadata.len()),
            ("checkouts", self.checkouts.len()),
            ("attachment candidates", self.attachment_candidates.len()),
            ("inventory targets", self.inventory_targets.len()),
            ("materialized aliases", self.materialized_aliases.len()),
            (
                "legacy path observations",
                self.legacy_path_observations.len(),
            ),
            ("repo grouping proofs", self.repo_grouping_proofs.len()),
            (
                "legacy namespace clusters",
                self.legacy_namespace_clusters.len(),
            ),
            (
                "legacy commit namespaces",
                self.legacy_commit_namespaces.len(),
            ),
        ] {
            if count > MAX_PROJECT_CATALOG_ENTRIES {
                return Err(limit(kind));
            }
        }

        let mut observations = BTreeSet::new();
        let mut projects = BTreeSet::new();
        let mut authority_observations = BTreeMap::new();
        for row in &self.legacy_projects {
            insert_observation(&mut observations, &row.observation_id)?;
            validate_timestamp(&row.record.registered_at)?;
            let project_id = ProjectId::parse(row.record.project_id.clone())
                .map_err(|_| invalid("legacy project id is invalid"))?;
            if !projects.insert(project_id.clone()) {
                return Err(duplicate("legacy project id"));
            }
            validate_optional_token(row.record.repo_id.as_deref(), "legacy repo id")?;
            for alias in &row.record.aliases {
                validate_token(alias, "legacy alias")?;
            }
            if let Some(authority) = &row.committed_authority {
                insert_observation(&mut observations, &authority.observation_id)?;
                authority_observations.insert(
                    authority.observation_id.as_str(),
                    (project_id, &authority.authority),
                );
            }
            if let Some(scope) = &row.committed_scope {
                scope
                    .validate()
                    .map_err(|_| invalid("legacy committed scope is invalid"))?;
            }
        }

        let mut generations = BTreeMap::new();
        for source in &self.code_sources {
            insert_observation(&mut observations, &source.observation_id)?;
            ensure_known_project(&projects, &source.project_id)?;
            if source.generations.len() > MAX_PROJECT_CATALOG_ENTRIES
                || source.quarantine.len() > MAX_PROJECT_CATALOG_ENTRIES
            {
                return Err(limit("code source generations"));
            }
            let active_count = source
                .generations
                .iter()
                .filter(|generation| generation.role == CollectedGenerationRoleV1::Active)
                .count();
            if active_count > 1
                || source.planned_activation_v2_hash.is_some() != (active_count == 1)
            {
                return Err(invalid(
                    "code source active generation and planned activation are not exact",
                ));
            }
            let mut per_source_generations = BTreeSet::new();
            for generation in &source.generations {
                insert_observation(&mut observations, &generation.observation_id)?;
                ensure_known_project(&projects, &generation.project_id)?;
                if generation.project_id != source.project_id {
                    return Err(invalid("code source generation project mismatch"));
                }
                validate_token(&generation.generation_id, "generation id")?;
                if !per_source_generations.insert(generation.generation_id.as_str()) {
                    return Err(duplicate("code source generation"));
                }
                validate_descriptor(&generation.descriptor)?;
                validate_artifact(&generation.manifest)?;
                if generation.role == CollectedGenerationRoleV1::Active
                    && self
                        .legacy_projects
                        .iter()
                        .find(|row| row.record.project_id == generation.project_id.as_str())
                        .and_then(|row| row.committed_scope.as_ref())
                        .is_some_and(|scope| {
                            generation.activation_scope.as_ref() != Some(scope)
                                || !matches!(
                                    &generation.descriptor,
                                    ImmutableCollectedDescriptorV1::Valid {
                                        published_scope,
                                        ..
                                    } if published_scope == scope
                                )
                        })
                {
                    return Err(invalid(
                        "active descriptor and activation disagree with committed scope",
                    ));
                }
                match (&generation.role, &generation.selector_evidence) {
                    (
                        CollectedGenerationRoleV1::Active,
                        DurableSelectorEvidenceV1::ExactMaterialized { .. },
                    )
                    | (
                        CollectedGenerationRoleV1::Retained,
                        DurableSelectorEvidenceV1::NoDurableSelector,
                    ) => {}
                    _ => {
                        return Err(invalid(
                            "collected generation selector authority disagrees with role",
                        ));
                    }
                }
                match (&generation.role, &generation.activation_scope) {
                    (CollectedGenerationRoleV1::Active, Some(scope)) => scope
                        .validate()
                        .map_err(|_| invalid("active generation scope is invalid"))?,
                    (CollectedGenerationRoleV1::Retained, None) => {}
                    _ => {
                        return Err(invalid(
                            "collected generation activation scope disagrees with role",
                        ));
                    }
                }
                generations.insert(generation.observation_id.as_str(), generation);
            }
            for generation in &source.quarantine {
                insert_observation(&mut observations, &generation.observation_id)?;
                ensure_known_project(&projects, &generation.project_id)?;
                if generation.project_id != source.project_id {
                    return Err(invalid("quarantine generation project mismatch"));
                }
                validate_token(&generation.generation_id, "quarantine generation id")?;
                if per_source_generations.contains(generation.generation_id.as_str()) {
                    return Err(invalid("quarantined generation is also active or retained"));
                }
                validate_descriptor(&generation.descriptor)?;
                validate_artifact(&generation.manifest)?;
                validate_collision_lifecycle(&generation.collision_lifecycle, generation)?;
            }
        }
        for generation in &self.retained_owner_resolutions {
            insert_observation(&mut observations, &generation.observation_id)?;
            validate_token(&generation.generation_id, "retained generation id")?;
            generation
                .published_scope
                .validate()
                .map_err(|_| invalid("retained generation scope is invalid"))?;
            if generation.candidate_project_ids.len() < 2 {
                return Err(invalid(
                    "ambiguous retained generation has fewer than two owner candidates",
                ));
            }
            for project_id in &generation.candidate_project_ids {
                ensure_known_project(&projects, project_id)?;
            }
            validate_descriptor(&generation.descriptor)?;
            validate_artifact(&generation.manifest)?;
            if generation.selector_evidence != DurableSelectorEvidenceV1::NoDurableSelector {
                return Err(invalid(
                    "ambiguous retained generation fabricates selector authority",
                ));
            }
        }

        let attachment_ids = self
            .attachment_candidates
            .iter()
            .map(|row| row.attachment_id.clone())
            .collect::<BTreeSet<_>>();
        let mut publisher_pin_keys = BTreeSet::new();
        for pin in &self.publisher_pins {
            insert_observation(&mut observations, &pin.observation_id)?;
            ensure_known_project(&projects, &pin.project_id)?;
            pin.expected_scope
                .validate()
                .map_err(|_| invalid("publisher pin scope is invalid"))?;
            validate_full_ref(&pin.full_ref)?;
            if let Some(commit) = &pin.resolved_commit {
                validate_full_commit(commit)?;
            }
            if let Some(scope) = &pin.resolved_scope {
                scope
                    .validate()
                    .map_err(|_| invalid("resolved publisher scope is invalid"))?;
            }
            for attachment_id in &pin.candidate_attachment_ids {
                if !attachment_ids.contains(attachment_id) {
                    return Err(unknown("publisher candidate attachment"));
                }
            }
            if pin.source_observation_ids.is_empty() {
                return Err(invalid("publisher pin has no source observations"));
            }
            for observation_id in &pin.source_observation_ids {
                validate_stable_id(observation_id, "publisher source observation id")?;
            }
            if !publisher_pin_keys.insert(publisher_pin_key(pin)?) {
                return Err(duplicate("publisher pin identity"));
            }
        }

        for row in &self.project_scoped_refs {
            insert_observation(&mut observations, &row.observation_id)?;
            ensure_known_project(&projects, &row.project_id)?;
            validate_stable_id(&row.stable_row_id, "project ref row id")?;
        }
        for row in &self.edge_workspaces {
            insert_observation(&mut observations, &row.observation_id)?;
            validate_stable_id(&row.workspace_id, "edge workspace id")?;
            for project_id in &row.project_ids {
                ensure_known_project(&projects, project_id)?;
            }
        }

        let mut git_observations = BTreeMap::new();
        for row in &self.git_metadata {
            insert_observation(&mut observations, &row.observation_id)?;
            ensure_known_project(&projects, &row.project_id)?;
            validate_stable_id(&row.checkout_observation_id, "Git checkout observation id")?;
            if let Some(commit) = &row.full_first_commit {
                validate_full_commit(commit)?;
            }
            if let Some(commit) = &row.last_ingested_sha {
                validate_full_commit(commit)?;
            }
            for namespace in &row.materialized_commit_namespaces {
                validate_token(namespace, "materialized commit namespace")?;
            }
            for (full_ref, commit) in &row.resolved_refs {
                validate_full_ref(full_ref)?;
                validate_full_commit(commit)?;
            }
            git_observations.insert(row.observation_id.as_str(), row);
        }

        let mut checkout_observations = BTreeSet::new();
        let mut canonical_checkout_root_digests = BTreeSet::new();
        for row in &self.checkouts {
            insert_observation(&mut observations, &row.observation_id)?;
            checkout_observations.insert(row.observation_id.as_str());
            if !canonical_checkout_root_digests.insert(&row.canonical_root_digest) {
                return Err(duplicate("canonical checkout root"));
            }
            validate_marker_state(&row.marker_state)?;
        }
        for row in &self.git_metadata {
            if !checkout_observations.contains(row.checkout_observation_id.as_str()) {
                return Err(unknown("Git checkout observation"));
            }
        }

        let mut seen_attachment_ids = BTreeSet::new();
        for row in &self.attachment_candidates {
            insert_observation(&mut observations, &row.observation_id)?;
            ensure_known_project(&projects, &row.project_id)?;
            if !seen_attachment_ids.insert(row.attachment_id.clone()) {
                return Err(duplicate("attachment candidate id"));
            }
            if !checkout_observations.contains(row.checkout_observation_id.as_str()) {
                return Err(unknown("attachment checkout observation"));
            }
            validate_relative_path(&row.base_relpath)?;
            if let Some(scope) = &row.observed_scope {
                scope
                    .validate()
                    .map_err(|_| invalid("attachment observed scope is invalid"))?;
            }
        }

        for row in &self.inventory_targets {
            insert_observation(&mut observations, &row.observation_id)?;
            ensure_known_project(&projects, &row.project_id)?;
            validate_stable_id(&row.stable_target_id, "inventory target id")?;
        }
        for row in &self.materialized_aliases {
            insert_observation(&mut observations, &row.observation_id)?;
            ensure_known_project(&projects, &row.project_id)?;
            validate_token(&row.alias, "materialized alias")?;
            if let Some(timestamp) = &row.registered_at {
                validate_timestamp(timestamp)?;
            }
        }
        for row in &self.legacy_path_observations {
            insert_observation(&mut observations, &row.observation_id)?;
            validate_stable_id(&row.stable_row_id, "legacy path row id")?;
        }
        for row in &self.legacy_namespace_clusters {
            insert_observation(&mut observations, &row.observation_id)?;
            validate_stable_id(&row.cluster_id, "legacy namespace cluster id")?;
            validate_token(&row.materialized_namespace, "materialized namespace")?;
            if row.project_ids.len() < 2 {
                return Err(invalid(
                    "legacy namespace cluster must contain two projects",
                ));
            }
            for project_id in &row.project_ids {
                ensure_known_project(&projects, project_id)?;
            }
        }
        let mut commit_namespaces = BTreeSet::new();
        for row in &self.legacy_commit_namespaces {
            insert_observation(&mut observations, &row.observation_id)?;
            if !commit_namespaces.insert(row.namespace.clone()) {
                return Err(duplicate("legacy commit namespace"));
            }
            match &row.attribution {
                LegacyCommitNamespaceAttributionV1::Proved { project_ids } => {
                    if project_ids.is_empty() {
                        return Err(invalid(
                            "proved legacy commit namespace has no project owner",
                        ));
                    }
                    for project_id in project_ids {
                        ensure_known_project(&projects, project_id)?;
                    }
                }
                LegacyCommitNamespaceAttributionV1::Ambiguous {
                    candidate_project_ids,
                } => {
                    if candidate_project_ids.len() < 2 {
                        return Err(invalid(
                            "ambiguous legacy commit namespace has fewer than two candidates",
                        ));
                    }
                    for project_id in candidate_project_ids {
                        ensure_known_project(&projects, project_id)?;
                    }
                }
                LegacyCommitNamespaceAttributionV1::Unclaimed => {}
            }
        }

        validate_publisher_source_membership(self)?;
        let mut proof_ids = BTreeSet::new();
        for proof in &self.repo_grouping_proofs {
            validate_stable_id(proof.proof_id(), "repo grouping proof id")?;
            if !proof_ids.insert(proof.proof_id()) {
                return Err(duplicate("repo grouping proof id"));
            }
            validate_grouping_proof(
                proof,
                &projects,
                &authority_observations,
                &git_observations,
                &generations,
            )?;
        }
        validate_mutable_source_row_coverage(self, &observations)?;
        Ok(())
    }

    pub fn canonical_json(&self) -> InventoryResult<Vec<u8>> {
        self.validate()?;
        let mut canonical = self.clone();
        canonical.canonicalize();
        let bytes = serde_json::to_vec(&canonical).map_err(|error| {
            ProjectCatalogInventoryError::new(
                "error.project_catalog_inventory_encode",
                error.to_string(),
            )
        })?;
        if bytes.len() > MAX_PROJECT_CATALOG_INVENTORY_BYTES {
            return Err(limit("canonical inventory"));
        }
        Ok(bytes)
    }

    pub fn inventory_hash(&self) -> InventoryResult<Sha256ValueV1> {
        Ok(domain_hash(INVENTORY_HASH_DOMAIN, &self.canonical_json()?))
    }

    pub fn hard_refusals(&self) -> Vec<InventoryRefusalV1> {
        let mut refusals = Vec::new();
        for source in &self.mutable_source_evidence {
            let required_code_evidence_missing =
                matches!(
                    source.source_kind,
                    MutableInventorySourceKindV1::CodeSourceGenerationMetadata
                        | MutableInventorySourceKindV1::CodeSourceGenerationManifest
                        | MutableInventorySourceKindV1::CodeSourceCollisionLifecycle
                ) && matches!(&source.state, InventorySourceStateV1::Missing { .. });
            if matches!(&source.state, InventorySourceStateV1::Corrupt { .. })
                || required_code_evidence_missing
            {
                refusals.push(InventoryRefusalV1 {
                    record_id: source.source_id.clone(),
                    diagnostic_code: if required_code_evidence_missing {
                        "required_code_source_evidence_missing".to_string()
                    } else {
                        "mutable_source_corrupt".to_string()
                    },
                });
            }
        }
        for lane in &self.immutable_lane_evidence {
            let diagnostic_code = match lane.completeness {
                ImmutableInventoryLaneCompletenessV1::Complete => None,
                ImmutableInventoryLaneCompletenessV1::Missing => Some("immutable_lane_missing"),
                ImmutableInventoryLaneCompletenessV1::Corrupt => Some("immutable_lane_corrupt"),
            };
            if let Some(diagnostic_code) = diagnostic_code {
                refusals.push(InventoryRefusalV1 {
                    record_id: lane.source_id.clone(),
                    diagnostic_code: diagnostic_code.to_string(),
                });
            }
        }
        for source in &self.code_sources {
            for generation in &source.generations {
                let diagnostic_code = match &generation.descriptor {
                    ImmutableCollectedDescriptorV1::Missing => {
                        Some("active_or_retained_descriptor_missing")
                    }
                    ImmutableCollectedDescriptorV1::Corrupt { .. } => {
                        Some("active_or_retained_descriptor_corrupt")
                    }
                    ImmutableCollectedDescriptorV1::Valid {
                        published_scope, ..
                    } if generation.role == CollectedGenerationRoleV1::Active
                        && generation.activation_scope.as_ref() != Some(published_scope) =>
                    {
                        Some("descriptor_activation_scope_mismatch")
                    }
                    ImmutableCollectedDescriptorV1::Valid { .. } => match &generation.manifest {
                        ImmutableArtifactObservationV1::Valid { .. } => None,
                        ImmutableArtifactObservationV1::Missing => {
                            Some("active_or_retained_manifest_missing")
                        }
                        ImmutableArtifactObservationV1::Corrupt { .. } => {
                            Some("active_or_retained_manifest_corrupt")
                        }
                    },
                };
                if let Some(diagnostic_code) = diagnostic_code {
                    refusals.push(InventoryRefusalV1 {
                        record_id: generation.observation_id.clone(),
                        diagnostic_code: diagnostic_code.to_string(),
                    });
                }
            }
            for generation in &source.quarantine {
                let diagnostic_code = match &generation.descriptor {
                    ImmutableCollectedDescriptorV1::Missing => {
                        Some("quarantined_descriptor_missing")
                    }
                    ImmutableCollectedDescriptorV1::Corrupt { .. } => {
                        Some("quarantined_descriptor_corrupt")
                    }
                    ImmutableCollectedDescriptorV1::Valid { .. } => match &generation.manifest {
                        ImmutableArtifactObservationV1::Valid { .. } => None,
                        ImmutableArtifactObservationV1::Missing => {
                            Some("quarantined_manifest_missing")
                        }
                        ImmutableArtifactObservationV1::Corrupt { .. } => {
                            Some("quarantined_manifest_corrupt")
                        }
                    },
                };
                if let Some(diagnostic_code) = diagnostic_code {
                    refusals.push(InventoryRefusalV1 {
                        record_id: generation.observation_id.clone(),
                        diagnostic_code: diagnostic_code.to_string(),
                    });
                }
            }
        }
        for generation in &self.retained_owner_resolutions {
            let diagnostic_code = match &generation.descriptor {
                ImmutableCollectedDescriptorV1::Missing => {
                    Some("retained_owner_descriptor_missing")
                }
                ImmutableCollectedDescriptorV1::Corrupt { .. } => {
                    Some("retained_owner_descriptor_corrupt")
                }
                ImmutableCollectedDescriptorV1::Valid { .. } => match &generation.manifest {
                    ImmutableArtifactObservationV1::Valid { .. } => None,
                    ImmutableArtifactObservationV1::Missing => {
                        Some("retained_owner_manifest_missing")
                    }
                    ImmutableArtifactObservationV1::Corrupt { .. } => {
                        Some("retained_owner_manifest_corrupt")
                    }
                },
            };
            if let Some(diagnostic_code) = diagnostic_code {
                refusals.push(InventoryRefusalV1 {
                    record_id: generation.observation_id.clone(),
                    diagnostic_code: diagnostic_code.to_string(),
                });
            }
        }
        refusals.sort_by(|left, right| left.record_id.cmp(&right.record_id));
        refusals
    }

    fn canonicalize(&mut self) {
        self.mutable_source_evidence
            .sort_by(|left, right| left.source_id.cmp(&right.source_id));
        self.immutable_lane_evidence
            .sort_by(|left, right| left.lane_kind.cmp(&right.lane_kind));
        self.legacy_projects
            .sort_by(|left, right| left.observation_id.cmp(&right.observation_id));
        self.code_sources
            .sort_by(|left, right| left.observation_id.cmp(&right.observation_id));
        for source in &mut self.code_sources {
            source
                .generations
                .sort_by(|left, right| left.observation_id.cmp(&right.observation_id));
            source
                .quarantine
                .sort_by(|left, right| left.observation_id.cmp(&right.observation_id));
        }
        self.retained_owner_resolutions
            .sort_by(|left, right| left.observation_id.cmp(&right.observation_id));
        self.publisher_pins
            .sort_by(|left, right| left.observation_id.cmp(&right.observation_id));
        self.project_scoped_refs
            .sort_by(|left, right| left.observation_id.cmp(&right.observation_id));
        self.edge_workspaces
            .sort_by(|left, right| left.observation_id.cmp(&right.observation_id));
        self.git_metadata
            .sort_by(|left, right| left.observation_id.cmp(&right.observation_id));
        self.checkouts
            .sort_by(|left, right| left.observation_id.cmp(&right.observation_id));
        self.attachment_candidates
            .sort_by(|left, right| left.observation_id.cmp(&right.observation_id));
        self.inventory_targets
            .sort_by(|left, right| left.observation_id.cmp(&right.observation_id));
        self.materialized_aliases
            .sort_by(|left, right| left.observation_id.cmp(&right.observation_id));
        self.legacy_path_observations
            .sort_by(|left, right| left.observation_id.cmp(&right.observation_id));
        self.repo_grouping_proofs
            .sort_by(|left, right| left.proof_id().cmp(right.proof_id()));
        for proof in &mut self.repo_grouping_proofs {
            proof.canonicalize();
        }
        self.legacy_namespace_clusters
            .sort_by(|left, right| left.observation_id.cmp(&right.observation_id));
        self.legacy_commit_namespaces
            .sort_by(|left, right| left.namespace.cmp(&right.namespace));
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InventoryRefusalV1 {
    pub record_id: String,
    pub diagnostic_code: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RepoGroupingEvidenceClassV1 {
    IdenticalCommittedRecordedAuthority,
    SharedGitCommonDirectoryAndFirstCommit,
    CollectedDescriptorActivationAgreement,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeterministicRepoHistoryGroupV1 {
    pub group_id: String,
    pub planned_history_id: RepoHistoryId,
    pub planned_primary_namespace: CommitNamespace,
    pub planned_compatibility_namespaces: BTreeSet<CommitNamespace>,
    pub project_ids: BTreeSet<ProjectId>,
    pub evidence_classes: BTreeSet<RepoGroupingEvidenceClassV1>,
    pub proof_ids: BTreeSet<String>,
    pub source_observation_ids: BTreeSet<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlannedRepoHistoryIdentityV1 {
    pub planned_history_id: RepoHistoryId,
    pub planned_primary_namespace: CommitNamespace,
    pub planned_compatibility_namespaces: BTreeSet<CommitNamespace>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProjectMigrationReportRowV1 {
    pub observation_id: String,
    pub project_id: ProjectId,
    pub path_status: LegacyProjectPathStatusV1,
    pub path_digest: Sha256ValueV1,
    pub committed_authority_present: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AttachmentMigrationReportRowV1 {
    pub observation_id: String,
    pub attachment_id: AttachmentId,
    pub project_id: ProjectId,
    pub checkout_observation_id: String,
    pub scope_digest: Option<Sha256ValueV1>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CheckoutIdentityActionV1 {
    pub observation_id: String,
    pub canonical_root_digest: Sha256ValueV1,
    pub planned_checkout_id: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LegacyPathRelationshipV1 {
    ExactRoot,
    Contained,
    Ambiguous,
    Unscoped,
    MissingProject,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LegacyPathBindingStatusV1 {
    Planned,
    ResolutionRequired,
    UnscopedPreserved,
    Refused,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LegacyPathBindingReportV1 {
    pub observation_id: String,
    pub planned_binding_id: LegacyPathBindingId,
    pub store_kind: LegacyPathStoreKindV1,
    pub relationship: LegacyPathRelationshipV1,
    pub status: LegacyPathBindingStatusV1,
    pub path_digest: Sha256ValueV1,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConflictReportV1 {
    pub conflict_id: String,
    pub affected_record_ids: BTreeSet<String>,
    pub diagnostic_code: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PublisherBindingReportStatusV1 {
    SeedG1Predicted,
    NoPublishedContentAcknowledged,
    ResolutionRequired,
    Refused,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PublisherBindingReportV1 {
    pub pin_observation_id: String,
    pub project_id: ProjectId,
    pub expected_scope_digest: Sha256ValueV1,
    pub full_ref_digest: Sha256ValueV1,
    pub status: PublisherBindingReportStatusV1,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PredictedAssetV1 {
    pub asset_id: String,
    pub content_hash: Sha256ValueV1,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MissingPathReportV1 {
    pub project_id: ProjectId,
    pub path_digest: Sha256ValueV1,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RequiredResolutionKindV1 {
    ScopeOwner,
    RepoHistoryGroupMerge,
    RepoHistoryGroupSplit,
    ExcludeAttachment,
    QuarantineCollected,
    PublisherBindingDisposition,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RequiredResolutionV1 {
    pub resolution_id: String,
    pub kind: RequiredResolutionKindV1,
    pub candidate_record_ids: BTreeSet<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProjectCatalogMigrationStatusV1 {
    Clean,
    ResolutionRequired,
    Refused,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProjectCatalogMigrationPlanKindV1 {
    Executable,
    AssessmentOnly,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProjectCatalogMigrationReportV1 {
    pub version: u32,
    pub transaction_id: ProjectCatalogTransactionId,
    pub inventory_hash: Sha256ValueV1,
    pub plan_hash: Sha256ValueV1,
    pub resolution_artifact_hash: Sha256ValueV1,
    pub source_store_hash: Sha256ValueV1,
    pub publisher_ref_source_hash: Sha256ValueV1,
    pub generated_at: String,
    pub status: ProjectCatalogMigrationStatusV1,
    pub plan_kind: ProjectCatalogMigrationPlanKindV1,
    pub projects: Vec<ProjectMigrationReportRowV1>,
    pub repo_history_groups: Vec<DeterministicRepoHistoryGroupV1>,
    pub attachments: Vec<AttachmentMigrationReportRowV1>,
    pub checkout_identity_actions: Vec<CheckoutIdentityActionV1>,
    pub legacy_path_bindings: Vec<LegacyPathBindingReportV1>,
    pub namespace_conflicts: Vec<ConflictReportV1>,
    pub scope_conflicts: Vec<ConflictReportV1>,
    pub alias_conflicts: Vec<ConflictReportV1>,
    pub activation_conflicts: Vec<ConflictReportV1>,
    pub publisher_bindings: Vec<PublisherBindingReportV1>,
    pub publisher_binding_conflicts: Vec<ConflictReportV1>,
    pub predicted_g1_assets: Vec<PredictedAssetV1>,
    pub predicted_accepted_pointer_hashes: BTreeMap<ProjectId, Sha256ValueV1>,
    pub missing_paths: Vec<MissingPathReportV1>,
    pub unscoped_legacy_counts: BTreeMap<LegacyPathStoreKindV1, u64>,
    pub required_resolutions: Vec<RequiredResolutionV1>,
    pub predicted_catalog_hash: Sha256ValueV1,
    pub predicted_attachment_hash: Sha256ValueV1,
    pub predicted_participant_hashes: BTreeMap<String, Sha256ValueV1>,
}

impl ProjectCatalogMigrationReportV1 {
    pub fn validate(&self) -> InventoryResult<()> {
        if self.version != PROJECT_CATALOG_MIGRATION_REPORT_VERSION_V1 {
            return Err(invalid("unsupported migration report version"));
        }
        for (kind, count) in [
            ("report projects", self.projects.len()),
            ("report repo history groups", self.repo_history_groups.len()),
            ("report attachments", self.attachments.len()),
            (
                "report checkout identity actions",
                self.checkout_identity_actions.len(),
            ),
            (
                "report legacy path bindings",
                self.legacy_path_bindings.len(),
            ),
            ("report namespace conflicts", self.namespace_conflicts.len()),
            ("report scope conflicts", self.scope_conflicts.len()),
            ("report alias conflicts", self.alias_conflicts.len()),
            (
                "report activation conflicts",
                self.activation_conflicts.len(),
            ),
            ("report publisher bindings", self.publisher_bindings.len()),
            (
                "report publisher conflicts",
                self.publisher_binding_conflicts.len(),
            ),
            ("report predicted G1 assets", self.predicted_g1_assets.len()),
            ("report missing paths", self.missing_paths.len()),
            (
                "report required resolutions",
                self.required_resolutions.len(),
            ),
        ] {
            if count > MAX_PROJECT_CATALOG_ENTRIES {
                return Err(limit(kind));
            }
        }
        validate_timestamp(&self.generated_at)?;
        validate_unique_by(
            self.projects.iter().map(|row| row.observation_id.as_str()),
            "report project observation",
        )?;
        validate_unique_by(
            self.repo_history_groups
                .iter()
                .map(|row| row.group_id.as_str()),
            "report repo history group",
        )?;
        if self
            .repo_history_groups
            .iter()
            .map(|row| row.planned_history_id.clone())
            .collect::<BTreeSet<_>>()
            .len()
            != self.repo_history_groups.len()
        {
            return Err(duplicate("report planned repo history id"));
        }
        validate_unique_by(
            self.attachments
                .iter()
                .map(|row| row.observation_id.as_str()),
            "report attachment observation",
        )?;
        validate_unique_by(
            self.checkout_identity_actions
                .iter()
                .map(|row| row.observation_id.as_str()),
            "checkout identity action",
        )?;
        validate_unique_by(
            self.legacy_path_bindings
                .iter()
                .map(|row| row.observation_id.as_str()),
            "legacy path binding report row",
        )?;
        if self
            .legacy_path_bindings
            .iter()
            .map(|row| row.planned_binding_id.clone())
            .collect::<BTreeSet<_>>()
            .len()
            != self.legacy_path_bindings.len()
        {
            return Err(duplicate("report planned legacy path binding id"));
        }
        validate_unique_by(
            self.publisher_bindings
                .iter()
                .map(|row| row.pin_observation_id.as_str()),
            "publisher binding report row",
        )?;
        validate_unique_by(
            self.predicted_g1_assets
                .iter()
                .map(|row| row.asset_id.as_str()),
            "predicted G1 asset",
        )?;
        validate_unique_by(
            self.required_resolutions
                .iter()
                .map(|row| row.resolution_id.as_str()),
            "required resolution",
        )?;
        validate_unique_by(
            self.namespace_conflicts
                .iter()
                .chain(&self.scope_conflicts)
                .chain(&self.alias_conflicts)
                .chain(&self.activation_conflicts)
                .chain(&self.publisher_binding_conflicts)
                .map(|row| row.conflict_id.as_str()),
            "report conflict",
        )?;
        for row in &self.checkout_identity_actions {
            validate_checkout_id(&row.planned_checkout_id)?;
        }
        for row in self
            .namespace_conflicts
            .iter()
            .chain(&self.scope_conflicts)
            .chain(&self.alias_conflicts)
            .chain(&self.activation_conflicts)
            .chain(&self.publisher_binding_conflicts)
        {
            validate_stable_id(&row.conflict_id, "conflict id")?;
            validate_diagnostic_code(&row.diagnostic_code)?;
            if row.affected_record_ids.is_empty() {
                return Err(invalid("conflict has no affected records"));
            }
            for record_id in &row.affected_record_ids {
                validate_stable_id(record_id, "conflict record id")?;
            }
        }
        for row in &self.required_resolutions {
            validate_stable_id(&row.resolution_id, "resolution id")?;
            if row.candidate_record_ids.is_empty() {
                return Err(invalid("required resolution has no candidates"));
            }
            for record_id in &row.candidate_record_ids {
                validate_stable_id(record_id, "resolution candidate record id")?;
            }
        }
        for group in &self.repo_history_groups {
            if group.project_ids.is_empty() {
                return Err(invalid("report repo history group is empty"));
            }
            if group
                .planned_compatibility_namespaces
                .contains(&group.planned_primary_namespace)
            {
                return Err(invalid(
                    "report primary namespace is repeated as compatibility namespace",
                ));
            }
            for proof_id in &group.proof_ids {
                validate_stable_id(proof_id, "report repo grouping proof id")?;
            }
            for observation_id in &group.source_observation_ids {
                validate_stable_id(observation_id, "report repo grouping observation id")?;
            }
        }
        for role in self.predicted_participant_hashes.keys() {
            validate_stable_id(role, "predicted participant role")?;
        }
        match self.status {
            ProjectCatalogMigrationStatusV1::ResolutionRequired
                if self.required_resolutions.is_empty() =>
            {
                Err(invalid("resolution-required report has no requirements"))
            }
            ProjectCatalogMigrationStatusV1::Clean
                if self.plan_kind != ProjectCatalogMigrationPlanKindV1::Executable =>
            {
                Err(invalid("clean report lacks an executable plan"))
            }
            ProjectCatalogMigrationStatusV1::ResolutionRequired
            | ProjectCatalogMigrationStatusV1::Refused
                if self.plan_kind != ProjectCatalogMigrationPlanKindV1::AssessmentOnly =>
            {
                Err(invalid("non-clean report claims an executable plan"))
            }
            _ => Ok(()),
        }
    }

    pub fn validate_against_inventory(
        &self,
        inventory: &V1ProjectCatalogInventory,
    ) -> InventoryResult<()> {
        self.validate()?;
        inventory.validate()?;
        if self.inventory_hash != inventory.inventory_hash()?
            || self.source_store_hash != inventory.source_store_hash
            || self.publisher_ref_source_hash != inventory.publisher_ref_source_hash
        {
            return Err(ProjectCatalogInventoryError::new(
                "error.project_catalog_inventory_stale_report",
                "report does not match the captured inventory",
            ));
        }
        let project_rows = self
            .projects
            .iter()
            .map(|row| (row.observation_id.as_str(), row))
            .collect::<BTreeMap<_, _>>();
        if project_rows.len() != inventory.legacy_projects.len() {
            return Err(invalid("report project inventory is incomplete"));
        }
        for observed in &inventory.legacy_projects {
            let row = project_rows
                .get(observed.observation_id.as_str())
                .ok_or_else(|| unknown("report project observation"))?;
            let project_id = ProjectId::parse(observed.record.project_id.clone())
                .map_err(|_| invalid("legacy project id is invalid"))?;
            if row.project_id != project_id
                || row.path_status != observed.path_status
                || row.path_digest != observed.record.canonical_path_digest
                || row.committed_authority_present != observed.committed_authority.is_some()
            {
                return Err(invalid("report project row disagrees with inventory"));
            }
        }
        let attachment_rows = self
            .attachments
            .iter()
            .map(|row| (row.observation_id.as_str(), row))
            .collect::<BTreeMap<_, _>>();
        if attachment_rows.len() != inventory.attachment_candidates.len() {
            return Err(invalid("report attachment inventory is incomplete"));
        }
        for observed in &inventory.attachment_candidates {
            let row = attachment_rows
                .get(observed.observation_id.as_str())
                .ok_or_else(|| unknown("report attachment observation"))?;
            let scope_digest = observed
                .observed_scope
                .as_ref()
                .map(digest_published_scope)
                .transpose()?;
            if row.attachment_id != observed.attachment_id
                || row.project_id != observed.project_id
                || row.checkout_observation_id != observed.checkout_observation_id
                || row.scope_digest != scope_digest
            {
                return Err(invalid("report attachment row disagrees with inventory"));
            }
        }
        let path_rows = self
            .legacy_path_bindings
            .iter()
            .map(|row| (row.observation_id.as_str(), row))
            .collect::<BTreeMap<_, _>>();
        if path_rows.len() != inventory.legacy_path_observations.len() {
            return Err(invalid("report legacy-path inventory is incomplete"));
        }
        for observed in &inventory.legacy_path_observations {
            let row = path_rows
                .get(observed.observation_id.as_str())
                .ok_or_else(|| unknown("report legacy-path observation"))?;
            if row.store_kind != observed.store_kind || row.path_digest != observed.selector_digest
            {
                return Err(invalid("report legacy-path row disagrees with inventory"));
            }
        }
        let expected_unscoped_counts = self
            .legacy_path_bindings
            .iter()
            .filter(|row| row.status == LegacyPathBindingStatusV1::UnscopedPreserved)
            .fold(BTreeMap::new(), |mut counts, row| {
                *counts.entry(row.store_kind).or_insert(0) += 1;
                counts
            });
        if self.unscoped_legacy_counts != expected_unscoped_counts {
            return Err(invalid(
                "unscoped legacy counts are not derived from report rows",
            ));
        }
        let expected_missing = inventory
            .legacy_projects
            .iter()
            .filter(|row| row.path_status == LegacyProjectPathStatusV1::Missing)
            .map(|row| {
                Ok(MissingPathReportV1 {
                    project_id: ProjectId::parse(row.record.project_id.clone())
                        .map_err(|_| invalid("legacy project id is invalid"))?,
                    path_digest: row.record.canonical_path_digest.clone(),
                })
            })
            .collect::<InventoryResult<Vec<_>>>()?;
        let expected_missing = expected_missing
            .iter()
            .map(|row| (row.project_id.clone(), row.path_digest.clone()))
            .collect::<BTreeSet<_>>();
        let reported_missing = self
            .missing_paths
            .iter()
            .map(|row| (row.project_id.clone(), row.path_digest.clone()))
            .collect::<BTreeSet<_>>();
        if expected_missing != reported_missing
            || reported_missing.len() != self.missing_paths.len()
        {
            return Err(invalid("missing-path report rows are not exact"));
        }
        if !inventory.hard_refusals().is_empty()
            && self.status != ProjectCatalogMigrationStatusV1::Refused
        {
            return Err(ProjectCatalogInventoryError::new(
                "error.project_catalog_inventory_refusal_suppressed",
                "report suppresses a non-overridable inventory refusal",
            ));
        }
        for retained in &inventory.retained_owner_resolutions {
            let expected_resolution_id = canonical_scope_resolution_id(
                &retained.published_scope,
                &retained.candidate_project_ids,
            )?;
            let expected_candidates = retained
                .candidate_project_ids
                .iter()
                .map(ToString::to_string)
                .collect::<BTreeSet<_>>();
            if !self.required_resolutions.iter().any(|resolution| {
                resolution.resolution_id == expected_resolution_id
                    && resolution.kind == RequiredResolutionKindV1::ScopeOwner
                    && resolution.candidate_record_ids == expected_candidates
            }) {
                return Err(invalid(
                    "ambiguous retained owner lacks its exact bounded resolution",
                ));
            }
        }
        self.validate_path_redaction(inventory)
    }

    pub fn validate_path_redaction(
        &self,
        inventory: &V1ProjectCatalogInventory,
    ) -> InventoryResult<()> {
        let _encoded = serde_json::to_vec(self).map_err(|error| {
            ProjectCatalogInventoryError::new(
                "error.project_catalog_report_encode",
                error.to_string(),
            )
        })?;
        inventory.canonical_json()?;
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SensitiveLocalPathWarningV1 {
    HostLocalSensitiveDoNotCommit,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SensitiveLocalPathRowV1 {
    pub observation_id: String,
    pub store_kind: LegacyPathStoreKindV1,
    pub stable_row_id: String,
    pub literal_selector: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SensitiveLocalPathReportV1 {
    pub version: u32,
    pub inventory_hash: Sha256ValueV1,
    pub local_paths_included: bool,
    pub warning: SensitiveLocalPathWarningV1,
    pub rows: Vec<SensitiveLocalPathRowV1>,
}

impl SensitiveLocalPathReportV1 {
    pub fn from_runtime_rows(
        inventory: &V1ProjectCatalogInventory,
        rows: Vec<SensitiveLocalPathRowV1>,
    ) -> InventoryResult<Self> {
        inventory.validate()?;
        if rows.len() != inventory.legacy_path_observations.len() {
            return Err(invalid(
                "sensitive local-path rows do not cover canonical observations",
            ));
        }
        let canonical = inventory
            .legacy_path_observations
            .iter()
            .map(|row| (row.observation_id.as_str(), row))
            .collect::<BTreeMap<_, _>>();
        for row in &rows {
            let observed = canonical
                .get(row.observation_id.as_str())
                .ok_or_else(|| unknown("sensitive local-path observation"))?;
            if row.store_kind != observed.store_kind
                || row.stable_row_id != observed.stable_row_id
                || digest_path(&row.literal_selector) != observed.selector_digest
            {
                return Err(invalid(
                    "sensitive local-path row disagrees with canonical digest",
                ));
            }
        }
        let mut rows = rows;
        rows.sort_by(|left, right| left.observation_id.cmp(&right.observation_id));
        Ok(Self {
            version: SENSITIVE_LOCAL_PATH_REPORT_VERSION_V1,
            inventory_hash: inventory.inventory_hash()?,
            local_paths_included: true,
            warning: SensitiveLocalPathWarningV1::HostLocalSensitiveDoNotCommit,
            rows,
        })
    }

    pub fn validate(&self) -> InventoryResult<()> {
        if self.version != SENSITIVE_LOCAL_PATH_REPORT_VERSION_V1
            || !self.local_paths_included
            || self.warning != SensitiveLocalPathWarningV1::HostLocalSensitiveDoNotCommit
        {
            return Err(invalid("sensitive local-path report marker is invalid"));
        }
        if self.rows.len() > MAX_PROJECT_CATALOG_ENTRIES {
            return Err(limit("sensitive local-path rows"));
        }
        validate_unique_by(
            self.rows.iter().map(|row| row.observation_id.as_str()),
            "sensitive local-path row",
        )?;
        for row in &self.rows {
            validate_stable_id(&row.stable_row_id, "sensitive local-path row id")?;
            validate_literal_selector(&row.literal_selector)?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SelectedScopeOwnerV1 {
    pub resolution_id: String,
    pub scope: PublishedScope,
    pub owner_project_id: ProjectId,
    pub losing_project_ids: BTreeSet<ProjectId>,
    pub owned_aliases: BTreeSet<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RepoHistoryGroupMergeV1 {
    pub resolution_id: String,
    pub target_group_id: String,
    pub source_group_ids: BTreeSet<String>,
    pub proof_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RepoHistorySplitPartitionV1 {
    pub target_group_id: String,
    pub project_ids: BTreeSet<ProjectId>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RepoHistoryGroupSplitV1 {
    pub resolution_id: String,
    pub source_cluster_id: String,
    pub partitions: Vec<RepoHistorySplitPartitionV1>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExcludedAttachmentV1 {
    pub resolution_id: String,
    pub attachment_id: AttachmentId,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QuarantineCollectedV1 {
    pub resolution_id: String,
    pub project_id: ProjectId,
    pub generation_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PublicationPayloadHashesV1 {
    pub knowledge_manifest_hash: Sha256ValueV1,
    pub gap_manifest_hash: Sha256ValueV1,
    pub knowledge_payload_hash: Sha256ValueV1,
    pub gap_payload_hash: Sha256ValueV1,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "disposition", rename_all = "snake_case", deny_unknown_fields)]
pub enum PublisherBindingDispositionV1 {
    SeedG1 {
        project_id: ProjectId,
        attachment_id: AttachmentId,
        expected_scope: PublishedScope,
        full_ref: String,
        accepted_commit: String,
        generation_id: String,
        payload_hashes: PublicationPayloadHashesV1,
        pointer_hash: Sha256ValueV1,
    },
    NoPublishedContentAcknowledged {
        project_id: ProjectId,
        expected_scope: PublishedScope,
        full_ref: String,
        bounded_reason: String,
    },
}

impl PublisherBindingDispositionV1 {
    pub fn project_id(&self) -> &ProjectId {
        match self {
            Self::SeedG1 { project_id, .. }
            | Self::NoPublishedContentAcknowledged { project_id, .. } => project_id,
        }
    }

    pub fn expected_scope(&self) -> &PublishedScope {
        match self {
            Self::SeedG1 { expected_scope, .. }
            | Self::NoPublishedContentAcknowledged { expected_scope, .. } => expected_scope,
        }
    }

    pub fn full_ref(&self) -> &str {
        match self {
            Self::SeedG1 { full_ref, .. }
            | Self::NoPublishedContentAcknowledged { full_ref, .. } => full_ref,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OperatorResolutionNoteV1 {
    pub note_id: String,
    pub bounded_note: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProjectCatalogMigrationResolutionV1 {
    pub version: u32,
    pub inventory_hash: Sha256ValueV1,
    pub selected_scope_owners: Vec<SelectedScopeOwnerV1>,
    pub repo_history_group_merges: Vec<RepoHistoryGroupMergeV1>,
    pub repo_history_group_splits: Vec<RepoHistoryGroupSplitV1>,
    pub excluded_attachments: Vec<ExcludedAttachmentV1>,
    pub quarantine_collected: Vec<QuarantineCollectedV1>,
    pub publisher_binding_dispositions: Vec<PublisherBindingDispositionV1>,
    pub operator_notes: Vec<OperatorResolutionNoteV1>,
}

impl ProjectCatalogMigrationResolutionV1 {
    pub fn empty(inventory_hash: Sha256ValueV1) -> Self {
        Self {
            version: PROJECT_CATALOG_MIGRATION_RESOLUTION_VERSION_V1,
            inventory_hash,
            selected_scope_owners: Vec::new(),
            repo_history_group_merges: Vec::new(),
            repo_history_group_splits: Vec::new(),
            excluded_attachments: Vec::new(),
            quarantine_collected: Vec::new(),
            publisher_binding_dispositions: Vec::new(),
            operator_notes: Vec::new(),
        }
    }

    pub fn validate(&self) -> InventoryResult<()> {
        if self.version != PROJECT_CATALOG_MIGRATION_RESOLUTION_VERSION_V1 {
            return Err(invalid("unsupported migration resolution version"));
        }
        for (kind, count) in [
            ("selected scope owners", self.selected_scope_owners.len()),
            (
                "repo history group merges",
                self.repo_history_group_merges.len(),
            ),
            (
                "repo history group splits",
                self.repo_history_group_splits.len(),
            ),
            ("excluded attachments", self.excluded_attachments.len()),
            ("quarantine dispositions", self.quarantine_collected.len()),
            (
                "publisher dispositions",
                self.publisher_binding_dispositions.len(),
            ),
            ("operator notes", self.operator_notes.len()),
        ] {
            if count > MAX_PROJECT_CATALOG_ENTRIES {
                return Err(limit(kind));
            }
        }
        let mut resolution_ids = BTreeSet::new();
        for (resolution_id, kind) in self
            .selected_scope_owners
            .iter()
            .map(|row| (row.resolution_id.as_str(), "scope owner"))
            .chain(
                self.repo_history_group_merges
                    .iter()
                    .map(|row| (row.resolution_id.as_str(), "group merge")),
            )
            .chain(
                self.repo_history_group_splits
                    .iter()
                    .map(|row| (row.resolution_id.as_str(), "group split")),
            )
            .chain(
                self.excluded_attachments
                    .iter()
                    .map(|row| (row.resolution_id.as_str(), "excluded attachment")),
            )
            .chain(
                self.quarantine_collected
                    .iter()
                    .map(|row| (row.resolution_id.as_str(), "quarantine")),
            )
        {
            validate_stable_id(resolution_id, kind)?;
            if !resolution_ids.insert(resolution_id) {
                return Err(duplicate("resolution disposition"));
            }
        }
        for row in &self.selected_scope_owners {
            row.scope
                .validate()
                .map_err(|_| invalid("selected scope is invalid"))?;
            if row.losing_project_ids.contains(&row.owner_project_id) {
                return Err(invalid("selected scope owner also appears as a loser"));
            }
            for alias in &row.owned_aliases {
                validate_token(alias, "selected alias")?;
            }
        }
        if self
            .selected_scope_owners
            .iter()
            .map(|row| row.scope.clone())
            .collect::<BTreeSet<_>>()
            .len()
            != self.selected_scope_owners.len()
        {
            return Err(duplicate("selected scope owner target"));
        }
        for row in &self.repo_history_group_merges {
            validate_stable_id(&row.target_group_id, "merge target group id")?;
            validate_stable_id(&row.proof_id, "merge proof id")?;
            if row.source_group_ids.len() < 2 {
                return Err(invalid("group merge requires at least two source groups"));
            }
            for group_id in &row.source_group_ids {
                validate_stable_id(group_id, "merge source group id")?;
            }
        }
        for row in &self.repo_history_group_splits {
            validate_stable_id(&row.source_cluster_id, "split source cluster id")?;
            if row.partitions.len() < 2 {
                return Err(invalid("group split requires at least two partitions"));
            }
            validate_unique_by(
                row.partitions
                    .iter()
                    .map(|partition| partition.target_group_id.as_str()),
                "split target group",
            )?;
            for partition in &row.partitions {
                validate_stable_id(&partition.target_group_id, "split target group id")?;
                if partition.project_ids.is_empty() {
                    return Err(invalid("group split partition is empty"));
                }
            }
        }
        for row in &self.quarantine_collected {
            validate_token(&row.generation_id, "quarantined generation id")?;
        }
        if self
            .repo_history_group_merges
            .iter()
            .map(|row| row.target_group_id.as_str())
            .collect::<BTreeSet<_>>()
            .len()
            != self.repo_history_group_merges.len()
            || self
                .repo_history_group_splits
                .iter()
                .map(|row| row.source_cluster_id.as_str())
                .collect::<BTreeSet<_>>()
                .len()
                != self.repo_history_group_splits.len()
            || self
                .excluded_attachments
                .iter()
                .map(|row| row.attachment_id.clone())
                .collect::<BTreeSet<_>>()
                .len()
                != self.excluded_attachments.len()
            || self
                .quarantine_collected
                .iter()
                .map(|row| (row.project_id.clone(), row.generation_id.as_str()))
                .collect::<BTreeSet<_>>()
                .len()
                != self.quarantine_collected.len()
        {
            return Err(duplicate("resolution target"));
        }
        let mut publisher_keys = BTreeSet::new();
        for disposition in &self.publisher_binding_dispositions {
            validate_publisher_disposition(disposition)?;
            let key = publisher_disposition_key(disposition)?;
            if !publisher_keys.insert(key) {
                return Err(duplicate("publisher binding disposition"));
            }
        }
        validate_unique_by(
            self.operator_notes.iter().map(|row| row.note_id.as_str()),
            "operator note",
        )?;
        for row in &self.operator_notes {
            validate_stable_id(&row.note_id, "operator note id")?;
            validate_bounded_text(&row.bounded_note, MAX_OPERATOR_NOTE_BYTES, "operator note")?;
        }
        Ok(())
    }

    fn canonicalize(&mut self) {
        self.selected_scope_owners
            .sort_by(|left, right| left.resolution_id.cmp(&right.resolution_id));
        self.repo_history_group_merges
            .sort_by(|left, right| left.resolution_id.cmp(&right.resolution_id));
        self.repo_history_group_splits
            .sort_by(|left, right| left.resolution_id.cmp(&right.resolution_id));
        for split in &mut self.repo_history_group_splits {
            split
                .partitions
                .sort_by(|left, right| left.target_group_id.cmp(&right.target_group_id));
        }
        self.excluded_attachments
            .sort_by(|left, right| left.resolution_id.cmp(&right.resolution_id));
        self.quarantine_collected
            .sort_by(|left, right| left.resolution_id.cmp(&right.resolution_id));
        self.publisher_binding_dispositions
            .sort_by_key(|row| publisher_disposition_sort_key(row));
        self.operator_notes
            .sort_by(|left, right| left.note_id.cmp(&right.note_id));
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResolvedProjectScopeInputV1 {
    pub project_id: ProjectId,
    pub published_scope: Option<PublishedScope>,
    pub created_at: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AttachmentPostImageInputV1 {
    pub attachment_id: AttachmentId,
    pub project_id: ProjectId,
    pub checkout_observation_id: String,
    pub checkout_id: String,
    pub expected_scope: Option<PublishedScope>,
    pub attached_at: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LegacyPathBindingPostImageInputV1 {
    pub observation_id: String,
    pub planned_binding_id: LegacyPathBindingId,
    pub attachment_id: Option<AttachmentId>,
    pub literal_selector: String,
    pub relationship: LegacyPathRelationshipV1,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QuarantinePostImageInputV1 {
    pub project_id: ProjectId,
    pub generation_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValidatedQuarantineBindingsV1 {
    plan_hash: Sha256ValueV1,
    transaction_id: ProjectCatalogTransactionId,
    generation_owners: BTreeMap<Sha256ValueV1, ProjectId>,
    bindings: BTreeSet<(ProjectId, Sha256ValueV1)>,
}

impl ValidatedQuarantineBindingsV1 {
    pub fn plan_hash(&self) -> &Sha256ValueV1 {
        &self.plan_hash
    }

    pub fn bindings(&self) -> &BTreeSet<(ProjectId, Sha256ValueV1)> {
        &self.bindings
    }

    pub fn transaction_id(&self) -> &ProjectCatalogTransactionId {
        &self.transaction_id
    }

    pub fn generation_owners(&self) -> &BTreeMap<Sha256ValueV1, ProjectId> {
        &self.generation_owners
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PredictedPostImageHashesV1 {
    pub catalog_hash: Sha256ValueV1,
    pub attachment_hash: Sha256ValueV1,
    pub participant_hashes: BTreeMap<String, Sha256ValueV1>,
    pub g1_assets: Vec<PredictedAssetV1>,
    pub accepted_pointer_hashes: BTreeMap<ProjectId, Sha256ValueV1>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeterministicPostImageInputV1 {
    pub version: u32,
    pub transaction_id: ProjectCatalogTransactionId,
    pub inventory_hash: Sha256ValueV1,
    pub resolved_project_scopes: Vec<ResolvedProjectScopeInputV1>,
    pub repo_history_groups: Vec<DeterministicRepoHistoryGroupV1>,
    pub attachments: Vec<AttachmentPostImageInputV1>,
    pub checkout_identity_actions: Vec<CheckoutIdentityActionV1>,
    pub legacy_path_bindings: Vec<LegacyPathBindingPostImageInputV1>,
    pub quarantined_collected: Vec<QuarantinePostImageInputV1>,
    pub publisher_binding_dispositions: Vec<PublisherBindingDispositionV1>,
    pub predicted_hashes: PredictedPostImageHashesV1,
}

impl DeterministicPostImageInputV1 {
    pub fn validate(&self, inventory: &V1ProjectCatalogInventory) -> InventoryResult<()> {
        if self.version != PROJECT_CATALOG_MIGRATION_REPORT_VERSION_V1 {
            return Err(invalid(
                "unsupported deterministic post-image input version",
            ));
        }
        if self.inventory_hash != inventory.inventory_hash()? {
            return Err(ProjectCatalogInventoryError::new(
                "error.project_catalog_inventory_stale_post_image",
                "post-image input does not match inventory",
            ));
        }
        let known_projects = inventory
            .legacy_projects
            .iter()
            .map(|row| ProjectId::parse(row.record.project_id.clone()))
            .collect::<Result<BTreeSet<_>, _>>()
            .map_err(|_| invalid("legacy project id is invalid"))?;
        let registered_at = inventory
            .legacy_projects
            .iter()
            .map(|row| {
                Ok((
                    ProjectId::parse(row.record.project_id.clone())
                        .map_err(|_| invalid("legacy project id is invalid"))?,
                    row.record.registered_at.as_str(),
                ))
            })
            .collect::<InventoryResult<BTreeMap<_, _>>>()?;
        let known_attachments = inventory
            .attachment_candidates
            .iter()
            .map(|row| row.attachment_id.clone())
            .collect::<BTreeSet<_>>();
        let known_checkout_observations = inventory
            .checkouts
            .iter()
            .map(|row| row.observation_id.as_str())
            .collect::<BTreeSet<_>>();
        let known_path_observations = inventory
            .legacy_path_observations
            .iter()
            .map(|row| (row.observation_id.as_str(), &row.selector_digest))
            .collect::<BTreeMap<_, _>>();

        let mut resolved_projects = BTreeSet::new();
        for row in &self.resolved_project_scopes {
            ensure_known_project(&known_projects, &row.project_id)?;
            if !resolved_projects.insert(row.project_id.clone()) {
                return Err(duplicate("resolved project scope"));
            }
            if let Some(scope) = &row.published_scope {
                scope
                    .validate()
                    .map_err(|_| invalid("resolved project scope is invalid"))?;
                if !observed_scopes_for_project(inventory, &row.project_id).contains(scope) {
                    return Err(ProjectCatalogInventoryError::new(
                        "error.project_catalog_inventory_invented_scope",
                        "resolved project scope is not proved by inventory",
                    ));
                }
            }
            if registered_at.get(&row.project_id).copied() != Some(row.created_at.as_str()) {
                return Err(ProjectCatalogInventoryError::new(
                    "error.project_catalog_inventory_migration_timestamp_mismatch",
                    "project creation timestamp does not preserve legacy registered_at",
                ));
            }
            validate_timestamp(&row.created_at)?;
        }
        if resolved_projects != known_projects {
            return Err(invalid("resolved project scopes are incomplete"));
        }

        let mut attachment_ids = BTreeSet::new();
        for row in &self.attachments {
            ensure_known_project(&known_projects, &row.project_id)?;
            if !known_attachments.contains(&row.attachment_id) {
                return Err(unknown("post-image attachment"));
            }
            if !attachment_ids.insert(row.attachment_id.clone()) {
                return Err(duplicate("post-image attachment"));
            }
            if !known_checkout_observations.contains(row.checkout_observation_id.as_str()) {
                return Err(unknown("post-image checkout observation"));
            }
            if registered_at.get(&row.project_id).copied() != Some(row.attached_at.as_str()) {
                return Err(ProjectCatalogInventoryError::new(
                    "error.project_catalog_inventory_migration_timestamp_mismatch",
                    "attachment timestamp does not preserve legacy registered_at",
                ));
            }
            validate_timestamp(&row.attached_at)?;
            validate_checkout_id(&row.checkout_id)?;
            let checkout = inventory
                .checkouts
                .iter()
                .find(|candidate| candidate.observation_id == row.checkout_observation_id)
                .ok_or_else(|| unknown("post-image checkout observation"))?;
            match &checkout.marker_state {
                CheckoutMarkerStateV1::Valid { checkout_id } if checkout_id == &row.checkout_id => {
                }
                CheckoutMarkerStateV1::MissingOrEmpty => {
                    let action = self
                        .checkout_identity_actions
                        .iter()
                        .find(|action| action.observation_id == checkout.observation_id)
                        .ok_or_else(|| invalid("missing checkout marker has no planned id"))?;
                    if action.planned_checkout_id != row.checkout_id {
                        return Err(invalid(
                            "attachment does not use the persisted planned checkout id",
                        ));
                    }
                }
                CheckoutMarkerStateV1::Valid { .. } => {
                    return Err(invalid("attachment rewrites a valid checkout id"));
                }
                CheckoutMarkerStateV1::Malformed { .. }
                | CheckoutMarkerStateV1::Unreadable { .. }
                | CheckoutMarkerStateV1::Symlinked => {
                    return Err(ProjectCatalogInventoryError::new(
                        "error.project_catalog_inventory_checkout_marker_refused",
                        "unsafe checkout marker state cannot produce an attachment",
                    ));
                }
            }
        }
        validate_unique_by(
            self.checkout_identity_actions
                .iter()
                .map(|row| row.observation_id.as_str()),
            "post-image checkout action",
        )?;
        for row in &self.checkout_identity_actions {
            let checkout = inventory
                .checkouts
                .iter()
                .find(|candidate| candidate.observation_id == row.observation_id)
                .ok_or_else(|| unknown("checkout identity action"))?;
            if !matches!(checkout.marker_state, CheckoutMarkerStateV1::MissingOrEmpty)
                || checkout.canonical_root_digest != row.canonical_root_digest
            {
                return Err(invalid("checkout identity action is not eligible"));
            }
            validate_checkout_id(&row.planned_checkout_id)?;
        }
        validate_unique_by(
            self.legacy_path_bindings
                .iter()
                .map(|row| row.observation_id.as_str()),
            "post-image legacy path binding",
        )?;
        if self
            .legacy_path_bindings
            .iter()
            .map(|row| row.planned_binding_id.clone())
            .collect::<BTreeSet<_>>()
            .len()
            != self.legacy_path_bindings.len()
        {
            return Err(duplicate("post-image planned legacy path binding id"));
        }
        for row in &self.legacy_path_bindings {
            let expected = known_path_observations
                .get(row.observation_id.as_str())
                .ok_or_else(|| unknown("legacy path binding observation"))?;
            if **expected != digest_path(&row.literal_selector) {
                return Err(invalid("legacy path binding literal was rewritten"));
            }
            if let Some(attachment_id) = &row.attachment_id
                && !attachment_ids.contains(attachment_id)
            {
                return Err(unknown("legacy path binding attachment"));
            }
        }
        for row in &self.quarantined_collected {
            ensure_known_project(&known_projects, &row.project_id)?;
            validate_token(&row.generation_id, "post-image quarantine generation")?;
        }
        let unique_quarantine = self
            .quarantined_collected
            .iter()
            .map(|row| (row.project_id.clone(), row.generation_id.as_str()))
            .collect::<BTreeSet<_>>();
        if unique_quarantine.len() != self.quarantined_collected.len() {
            return Err(duplicate("post-image quarantine generation"));
        }
        if self.legacy_path_bindings.len() != known_path_observations.len() {
            return Err(invalid(
                "post-image legacy path bindings do not cover every observation",
            ));
        }
        validate_unique_by(
            self.repo_history_groups
                .iter()
                .map(|row| row.group_id.as_str()),
            "post-image repo history group",
        )?;
        if self
            .repo_history_groups
            .iter()
            .map(|row| row.planned_history_id.clone())
            .collect::<BTreeSet<_>>()
            .len()
            != self.repo_history_groups.len()
        {
            return Err(duplicate("post-image planned repo history id"));
        }
        validate_post_image_groups(
            inventory,
            &known_projects,
            &self.resolved_project_scopes,
            &self.repo_history_groups,
        )?;
        let mut publisher_keys = BTreeSet::new();
        for disposition in &self.publisher_binding_dispositions {
            validate_publisher_disposition(disposition)?;
            if !publisher_keys.insert(publisher_disposition_key(disposition)?) {
                return Err(duplicate("post-image publisher disposition"));
            }
        }
        for key in self.predicted_hashes.participant_hashes.keys() {
            validate_stable_id(key, "predicted participant role")?;
        }
        Ok(())
    }

    fn canonicalize(&mut self) {
        self.resolved_project_scopes
            .sort_by(|left, right| left.project_id.cmp(&right.project_id));
        self.repo_history_groups
            .sort_by(|left, right| left.group_id.cmp(&right.group_id));
        self.attachments
            .sort_by(|left, right| left.attachment_id.cmp(&right.attachment_id));
        self.checkout_identity_actions
            .sort_by(|left, right| left.observation_id.cmp(&right.observation_id));
        self.legacy_path_bindings
            .sort_by(|left, right| left.observation_id.cmp(&right.observation_id));
        self.quarantined_collected.sort_by(|left, right| {
            (&left.project_id, &left.generation_id).cmp(&(&right.project_id, &right.generation_id))
        });
        self.publisher_binding_dispositions
            .sort_by_key(|row| publisher_disposition_sort_key(row));
        self.predicted_hashes
            .g1_assets
            .sort_by(|left, right| left.asset_id.cmp(&right.asset_id));
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
struct CanonicalPlanHashInputV1<'a> {
    version: u32,
    inventory_hash: &'a Sha256ValueV1,
    resolution: &'a ProjectCatalogMigrationResolutionV1,
    post_image: CanonicalSemanticPostImageV1<'a>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
struct CanonicalSemanticPostImageV1<'a> {
    version: u32,
    transaction_id: &'a ProjectCatalogTransactionId,
    inventory_hash: &'a Sha256ValueV1,
    resolved_project_scopes: &'a [ResolvedProjectScopeInputV1],
    repo_history_groups: &'a [DeterministicRepoHistoryGroupV1],
    attachments: &'a [AttachmentPostImageInputV1],
    checkout_identity_actions: &'a [CheckoutIdentityActionV1],
    legacy_path_bindings: &'a [LegacyPathBindingPostImageInputV1],
    quarantined_collected: &'a [QuarantinePostImageInputV1],
    publisher_binding_dispositions: &'a [PublisherBindingDispositionV1],
}

pub fn canonical_plan_hash(
    inventory: &V1ProjectCatalogInventory,
    resolution: &ProjectCatalogMigrationResolutionV1,
    post_image: &DeterministicPostImageInputV1,
) -> InventoryResult<Sha256ValueV1> {
    inventory.validate()?;
    resolution.validate()?;
    post_image.validate(inventory)?;
    let inventory_hash = inventory.inventory_hash()?;
    if resolution.inventory_hash != inventory_hash || post_image.inventory_hash != inventory_hash {
        return Err(ProjectCatalogInventoryError::new(
            "error.project_catalog_inventory_stale_plan_input",
            "plan input does not match inventory",
        ));
    }
    let mut canonical_resolution = resolution.clone();
    canonical_resolution.canonicalize();
    let mut canonical_post_image = post_image.clone();
    canonical_post_image.canonicalize();
    let input = CanonicalPlanHashInputV1 {
        version: PROJECT_CATALOG_MIGRATION_REPORT_VERSION_V1,
        inventory_hash: &inventory_hash,
        resolution: &canonical_resolution,
        // Participant bytes such as collision-retirement records embed this
        // semantic plan hash. Their predicted hashes are therefore excluded
        // from semantic intent and bound later by exact report/marker bytes.
        post_image: CanonicalSemanticPostImageV1 {
            version: canonical_post_image.version,
            transaction_id: &canonical_post_image.transaction_id,
            inventory_hash: &canonical_post_image.inventory_hash,
            resolved_project_scopes: &canonical_post_image.resolved_project_scopes,
            repo_history_groups: &canonical_post_image.repo_history_groups,
            attachments: &canonical_post_image.attachments,
            checkout_identity_actions: &canonical_post_image.checkout_identity_actions,
            legacy_path_bindings: &canonical_post_image.legacy_path_bindings,
            quarantined_collected: &canonical_post_image.quarantined_collected,
            publisher_binding_dispositions: &canonical_post_image.publisher_binding_dispositions,
        },
    };
    let bytes = serde_json::to_vec(&input).map_err(|error| {
        ProjectCatalogInventoryError::new("error.project_catalog_plan_encode", error.to_string())
    })?;
    Ok(domain_hash(PLAN_HASH_DOMAIN, &bytes))
}

pub fn validate_supported_resolution(
    inventory: &V1ProjectCatalogInventory,
    report: &ProjectCatalogMigrationReportV1,
    resolution: &ProjectCatalogMigrationResolutionV1,
    post_image: &DeterministicPostImageInputV1,
) -> InventoryResult<()> {
    inventory.validate()?;
    report.validate_against_inventory(inventory)?;
    resolution.validate()?;
    post_image.validate(inventory)?;
    let inventory_hash = inventory.inventory_hash()?;
    if resolution.inventory_hash != inventory_hash {
        return Err(ProjectCatalogInventoryError::new(
            "error.project_catalog_inventory_stale_resolution",
            "resolution does not match the captured inventory",
        ));
    }
    if report.plan_hash != canonical_plan_hash(inventory, resolution, post_image)? {
        return Err(ProjectCatalogInventoryError::new(
            "error.project_catalog_inventory_plan_hash_mismatch",
            "report plan hash does not bind the supplied resolution and post-images",
        ));
    }
    if report.transaction_id != post_image.transaction_id {
        return Err(ProjectCatalogInventoryError::new(
            "error.project_catalog_inventory_transaction_id_mismatch",
            "report and deterministic post-image use different migration transaction ids",
        ));
    }
    if report.repo_history_groups != post_image.repo_history_groups {
        return Err(ProjectCatalogInventoryError::new(
            "error.project_catalog_inventory_history_plan_mismatch",
            "report and deterministic post-image use different repository-history plans",
        ));
    }
    let report_binding_ids = report
        .legacy_path_bindings
        .iter()
        .map(|row| (row.observation_id.as_str(), &row.planned_binding_id))
        .collect::<BTreeMap<_, _>>();
    let post_image_binding_ids = post_image
        .legacy_path_bindings
        .iter()
        .map(|row| (row.observation_id.as_str(), &row.planned_binding_id))
        .collect::<BTreeMap<_, _>>();
    if report_binding_ids != post_image_binding_ids {
        return Err(ProjectCatalogInventoryError::new(
            "error.project_catalog_inventory_legacy_path_plan_mismatch",
            "report and deterministic post-image use different legacy-path binding ids",
        ));
    }
    if report.predicted_catalog_hash != post_image.predicted_hashes.catalog_hash
        || report.predicted_attachment_hash != post_image.predicted_hashes.attachment_hash
        || report.predicted_participant_hashes != post_image.predicted_hashes.participant_hashes
        || report.predicted_g1_assets != post_image.predicted_hashes.g1_assets
        || report.predicted_accepted_pointer_hashes
            != post_image.predicted_hashes.accepted_pointer_hashes
    {
        return Err(ProjectCatalogInventoryError::new(
            "error.project_catalog_inventory_prediction_mismatch",
            "report predicted hashes do not match the bound post-images",
        ));
    }

    let projects = inventory
        .legacy_projects
        .iter()
        .map(|row| ProjectId::parse(row.record.project_id.clone()))
        .collect::<Result<BTreeSet<_>, _>>()
        .map_err(|_| invalid("legacy project id is invalid"))?;
    let attachments = inventory
        .attachment_candidates
        .iter()
        .map(|row| (row.attachment_id.clone(), row))
        .collect::<BTreeMap<_, _>>();
    let requirements = report
        .required_resolutions
        .iter()
        .map(|row| (row.resolution_id.as_str(), row))
        .collect::<BTreeMap<_, _>>();
    let mut satisfied = BTreeSet::new();
    let mut expected_quarantines = BTreeMap::new();
    let canonical_scope_conflicts = canonical_scope_conflicts(inventory)?;
    let selected_scope_owners = resolution
        .selected_scope_owners
        .iter()
        .map(|row| (row.scope.clone(), row))
        .collect::<BTreeMap<_, _>>();
    if selected_scope_owners.len() != resolution.selected_scope_owners.len()
        || selected_scope_owners.len() != canonical_scope_conflicts.len()
        || selected_scope_owners.keys().collect::<BTreeSet<_>>()
            != canonical_scope_conflicts.keys().collect::<BTreeSet<_>>()
    {
        return Err(ProjectCatalogInventoryError::new(
            "error.project_catalog_inventory_incomplete_scope_conflicts",
            "scope-owner dispositions do not equal every inventoried duplicate scope",
        ));
    }
    let resolved_scopes = post_image
        .resolved_project_scopes
        .iter()
        .map(|row| (row.project_id.clone(), row.published_scope.as_ref()))
        .collect::<BTreeMap<_, _>>();

    for row in &resolution.selected_scope_owners {
        let canonical_candidates = &canonical_scope_conflicts[&row.scope];
        let canonical_resolution_id =
            canonical_scope_resolution_id(&row.scope, canonical_candidates)?;
        if row.resolution_id != canonical_resolution_id {
            return Err(invalid(
                "scope-owner disposition does not use its canonical conflict id",
            ));
        }
        require_resolution_kind(
            &requirements,
            &mut satisfied,
            &row.resolution_id,
            RequiredResolutionKindV1::ScopeOwner,
        )?;
        ensure_known_project(&projects, &row.owner_project_id)?;
        if !observed_scopes_for_project(inventory, &row.owner_project_id).contains(&row.scope) {
            return Err(ProjectCatalogInventoryError::new(
                "error.project_catalog_inventory_invented_scope",
                "selected scope owner has no matching inventoried scope",
            ));
        }
        let requirement = requirements[row.resolution_id.as_str()];
        let mut selected = row
            .losing_project_ids
            .iter()
            .cloned()
            .collect::<BTreeSet<_>>();
        selected.insert(row.owner_project_id.clone());
        if &selected != canonical_candidates {
            return Err(invalid(
                "scope-owner resolution does not cover exact inventoried candidates",
            ));
        }
        let canonical_candidate_ids = canonical_candidates
            .iter()
            .map(ToString::to_string)
            .collect::<BTreeSet<_>>();
        if requirement.candidate_record_ids != canonical_candidate_ids
            || !report.scope_conflicts.iter().any(|conflict| {
                conflict.conflict_id == row.resolution_id
                    && conflict.affected_record_ids == canonical_candidate_ids
                    && conflict.diagnostic_code == "duplicate_published_scope"
            })
        {
            return Err(invalid(
                "scope-owner requirement and conflict do not match the inventoried candidates",
            ));
        }
        if resolved_scopes.get(&row.owner_project_id) != Some(&Some(&row.scope))
            || row
                .losing_project_ids
                .iter()
                .any(|project_id| resolved_scopes.get(project_id) == Some(&Some(&row.scope)))
        {
            return Err(invalid(
                "scope-owner disposition is not reflected exactly in the post-image",
            ));
        }
        for source in &inventory.code_sources {
            for generation in &source.generations {
                if row.losing_project_ids.contains(&generation.project_id)
                    && expected_quarantines
                        .insert(
                            (
                                generation.project_id.clone(),
                                generation.generation_id.clone(),
                            ),
                            generation.observation_id.clone(),
                        )
                        .is_some()
                {
                    return Err(duplicate("canonical quarantine candidate"));
                }
            }
        }
        for alias in &row.owned_aliases {
            let alias_owners = inventory
                .legacy_projects
                .iter()
                .filter(|candidate| candidate.record.aliases.contains(alias))
                .map(|candidate| ProjectId::parse(candidate.record.project_id.clone()))
                .collect::<Result<BTreeSet<_>, _>>()
                .map_err(|_| invalid("legacy project id is invalid"))?
                .into_iter()
                .chain(
                    inventory
                        .materialized_aliases
                        .iter()
                        .filter(|candidate| candidate.alias == *alias)
                        .map(|candidate| candidate.project_id.clone()),
                )
                .collect::<BTreeSet<_>>();
            let mut selected_alias_owners = row.losing_project_ids.clone();
            selected_alias_owners.insert(row.owner_project_id.clone());
            if alias_owners.len() < 2
                || alias_owners != selected_alias_owners
                || !alias_owners.contains(&row.owner_project_id)
            {
                return Err(unknown("selected alias conflict"));
            }
            let conflicted_owner_ids = alias_owners
                .iter()
                .map(ToString::to_string)
                .collect::<BTreeSet<_>>();
            if !report
                .alias_conflicts
                .iter()
                .any(|conflict| conflict.affected_record_ids == conflicted_owner_ids)
            {
                return Err(unknown("selected alias conflict"));
            }
        }
    }
    let scope_resolution_ids = resolution
        .selected_scope_owners
        .iter()
        .map(|row| row.resolution_id.as_str())
        .collect::<BTreeSet<_>>();
    let reported_scope_conflict_ids = report
        .scope_conflicts
        .iter()
        .map(|row| row.conflict_id.as_str())
        .collect::<BTreeSet<_>>();
    let required_scope_resolution_ids = report
        .required_resolutions
        .iter()
        .filter(|row| row.kind == RequiredResolutionKindV1::ScopeOwner)
        .map(|row| row.resolution_id.as_str())
        .collect::<BTreeSet<_>>();
    let canonical_scope_resolution_ids = canonical_scope_conflicts
        .iter()
        .map(|(scope, candidates)| canonical_scope_resolution_id(scope, candidates))
        .collect::<InventoryResult<BTreeSet<_>>>()?;
    if reported_scope_conflict_ids != required_scope_resolution_ids
        || scope_resolution_ids != required_scope_resolution_ids
        || scope_resolution_ids
            != canonical_scope_resolution_ids
                .iter()
                .map(String::as_str)
                .collect::<BTreeSet<_>>()
    {
        return Err(invalid(
            "scope conflicts, requirements, and dispositions are not exact",
        ));
    }

    let report_groups = report
        .repo_history_groups
        .iter()
        .map(|group| (group.group_id.as_str(), group))
        .collect::<BTreeMap<_, _>>();
    let proofs = inventory
        .repo_grouping_proofs
        .iter()
        .map(|proof| (proof.proof_id(), proof))
        .collect::<BTreeMap<_, _>>();
    for row in &resolution.repo_history_group_merges {
        require_resolution_kind(
            &requirements,
            &mut satisfied,
            &row.resolution_id,
            RequiredResolutionKindV1::RepoHistoryGroupMerge,
        )?;
        let proof = proofs
            .get(row.proof_id.as_str())
            .ok_or_else(|| unknown("repo history merge proof"))?;
        let mut merged_projects = BTreeSet::new();
        for group_id in &row.source_group_ids {
            let group = report_groups
                .get(group_id.as_str())
                .ok_or_else(|| unknown("repo history merge source group"))?;
            merged_projects.extend(group.project_ids.iter().cloned());
        }
        if !proof.project_ids().is_superset(&merged_projects) {
            return Err(ProjectCatalogInventoryError::new(
                "error.project_catalog_inventory_unsupported_merge",
                "repo history merge is not covered by stronger same-repo evidence",
            ));
        }
        let target = post_image
            .repo_history_groups
            .iter()
            .find(|group| group.group_id == row.target_group_id)
            .ok_or_else(|| unknown("repo history merge target group"))?;
        if target.project_ids != merged_projects {
            return Err(invalid(
                "repo history merge target does not equal source group union",
            ));
        }
    }

    let namespace_clusters = inventory
        .legacy_namespace_clusters
        .iter()
        .map(|row| (row.cluster_id.as_str(), row))
        .collect::<BTreeMap<_, _>>();
    for row in &resolution.repo_history_group_splits {
        require_resolution_kind(
            &requirements,
            &mut satisfied,
            &row.resolution_id,
            RequiredResolutionKindV1::RepoHistoryGroupSplit,
        )?;
        let cluster = namespace_clusters
            .get(row.source_cluster_id.as_str())
            .ok_or_else(|| unknown("repo history split cluster"))?;
        let mut partitioned = BTreeSet::new();
        for partition in &row.partitions {
            for project_id in &partition.project_ids {
                if !partitioned.insert(project_id.clone()) {
                    return Err(duplicate("repo history split project"));
                }
            }
        }
        if partitioned != cluster.project_ids {
            return Err(invalid(
                "repo history split does not exactly partition cluster",
            ));
        }
        for partition in &row.partitions {
            let target = post_image
                .repo_history_groups
                .iter()
                .find(|group| group.group_id == partition.target_group_id)
                .ok_or_else(|| unknown("repo history split target group"))?;
            if target.project_ids != partition.project_ids {
                return Err(invalid(
                    "repo history split target does not match its partition",
                ));
            }
        }
    }

    for row in &resolution.excluded_attachments {
        require_resolution_kind(
            &requirements,
            &mut satisfied,
            &row.resolution_id,
            RequiredResolutionKindV1::ExcludeAttachment,
        )?;
        let attachment = attachments
            .get(&row.attachment_id)
            .ok_or_else(|| unknown("excluded attachment"))?;
        if !requirements[row.resolution_id.as_str()]
            .candidate_record_ids
            .contains(&attachment.observation_id)
        {
            return Err(invalid("excluded attachment is not a conflict candidate"));
        }
        if post_image
            .attachments
            .iter()
            .any(|candidate| candidate.attachment_id == row.attachment_id)
        {
            return Err(invalid("excluded attachment remains in post-image input"));
        }
    }
    let excluded_attachment_ids = resolution
        .excluded_attachments
        .iter()
        .map(|row| row.attachment_id.clone())
        .collect::<BTreeSet<_>>();
    let expected_attachment_ids = attachments
        .keys()
        .filter(|attachment_id| !excluded_attachment_ids.contains(*attachment_id))
        .cloned()
        .collect::<BTreeSet<_>>();
    let post_attachment_ids = post_image
        .attachments
        .iter()
        .map(|row| row.attachment_id.clone())
        .collect::<BTreeSet<_>>();
    if expected_attachment_ids != post_attachment_ids {
        return Err(ProjectCatalogInventoryError::new(
            "error.project_catalog_inventory_suppressed_attachment",
            "post-image attachments differ without an exact exclusion resolution",
        ));
    }

    let actual_quarantines = resolution
        .quarantine_collected
        .iter()
        .map(|row| {
            (
                (row.project_id.clone(), row.generation_id.clone()),
                row.resolution_id.as_str(),
            )
        })
        .collect::<BTreeMap<_, _>>();
    if actual_quarantines.keys().cloned().collect::<BTreeSet<_>>()
        != expected_quarantines
            .keys()
            .cloned()
            .collect::<BTreeSet<_>>()
    {
        return Err(ProjectCatalogInventoryError::new(
            "error.project_catalog_inventory_unsupported_quarantine",
            "quarantine dispositions do not equal the losing inventoried generations",
        ));
    }
    for row in &resolution.quarantine_collected {
        require_resolution_kind(
            &requirements,
            &mut satisfied,
            &row.resolution_id,
            RequiredResolutionKindV1::QuarantineCollected,
        )?;
        let candidate_record_ids = BTreeSet::from([expected_quarantines
            .get(&(row.project_id.clone(), row.generation_id.clone()))
            .ok_or_else(|| unknown("canonical quarantine candidate"))?
            .clone()]);
        let requirement = requirements[row.resolution_id.as_str()];
        if requirement.candidate_record_ids != candidate_record_ids
            || !report.activation_conflicts.iter().any(|conflict| {
                conflict.conflict_id == row.resolution_id
                    && conflict.affected_record_ids == candidate_record_ids
            })
        {
            return Err(invalid(
                "quarantine requirement and conflict do not match its inventoried generation",
            ));
        }
        if !post_image.quarantined_collected.iter().any(|candidate| {
            candidate.project_id == row.project_id && candidate.generation_id == row.generation_id
        }) {
            return Err(invalid(
                "selected quarantine is absent from post-image input",
            ));
        }
    }
    let quarantine_resolution_ids = resolution
        .quarantine_collected
        .iter()
        .map(|row| row.resolution_id.as_str())
        .collect::<BTreeSet<_>>();
    let reported_activation_conflict_ids = report
        .activation_conflicts
        .iter()
        .map(|row| row.conflict_id.as_str())
        .collect::<BTreeSet<_>>();
    let required_quarantine_resolution_ids = report
        .required_resolutions
        .iter()
        .filter(|row| row.kind == RequiredResolutionKindV1::QuarantineCollected)
        .map(|row| row.resolution_id.as_str())
        .collect::<BTreeSet<_>>();
    if reported_activation_conflict_ids != required_quarantine_resolution_ids
        || quarantine_resolution_ids != required_quarantine_resolution_ids
    {
        return Err(invalid(
            "activation conflicts, quarantine requirements, and dispositions are not exact",
        ));
    }
    for quarantined in &post_image.quarantined_collected {
        let resolved = resolution.quarantine_collected.iter().any(|row| {
            row.project_id == quarantined.project_id
                && row.generation_id == quarantined.generation_id
        });
        let already_quarantined = inventory.code_sources.iter().any(|source| {
            source.quarantine.iter().any(|row| {
                row.project_id == quarantined.project_id
                    && row.generation_id == quarantined.generation_id
            })
        });
        if !resolved && !already_quarantined {
            return Err(ProjectCatalogInventoryError::new(
                "error.project_catalog_inventory_unsupported_quarantine",
                "post-image quarantines a generation without inventoried or operator authority",
            ));
        }
    }

    validate_publisher_disposition_set(inventory, report, resolution, post_image, &mut satisfied)?;

    let expected_nonpublisher = report
        .required_resolutions
        .iter()
        .filter(|row| row.kind != RequiredResolutionKindV1::PublisherBindingDisposition)
        .map(|row| row.resolution_id.as_str())
        .collect::<BTreeSet<_>>();
    let actual_nonpublisher = satisfied
        .iter()
        .copied()
        .filter(|resolution_id| {
            requirements.get(resolution_id).is_some_and(|row| {
                row.kind != RequiredResolutionKindV1::PublisherBindingDisposition
            })
        })
        .collect::<BTreeSet<_>>();
    if expected_nonpublisher != actual_nonpublisher {
        return Err(ProjectCatalogInventoryError::new(
            "error.project_catalog_inventory_incomplete_resolution",
            "resolution does not satisfy every required conflict exactly once",
        ));
    }
    Ok(())
}

pub fn validated_quarantine_bindings(
    inventory: &V1ProjectCatalogInventory,
    report: &ProjectCatalogMigrationReportV1,
    resolution: &ProjectCatalogMigrationResolutionV1,
    post_image: &DeterministicPostImageInputV1,
) -> InventoryResult<ValidatedQuarantineBindingsV1> {
    validate_supported_resolution(inventory, report, resolution, post_image)?;
    let mut generation_owners = BTreeMap::new();
    for source in &inventory.code_sources {
        for generation in &source.generations {
            let generation_id = Sha256ValueV1::parse(generation.generation_id.clone())?;
            if generation_owners
                .insert(generation_id, generation.project_id.clone())
                .is_some()
            {
                return Err(duplicate("canonical generation owner"));
            }
        }
        for generation in &source.quarantine {
            let generation_id = Sha256ValueV1::parse(generation.generation_id.clone())?;
            if generation_owners
                .insert(generation_id, generation.project_id.clone())
                .is_some()
            {
                return Err(duplicate("canonical generation owner"));
            }
        }
    }
    for generation in &inventory.retained_owner_resolutions {
        let quarantined_owner = post_image
            .quarantined_collected
            .iter()
            .find(|row| row.generation_id == generation.generation_id)
            .map(|row| row.project_id.clone());
        let owner = match quarantined_owner {
            Some(owner) => owner,
            None => {
                let candidates = post_image
                    .resolved_project_scopes
                    .iter()
                    .filter(|row| {
                        generation.candidate_project_ids.contains(&row.project_id)
                            && row.published_scope.as_ref() == Some(&generation.published_scope)
                    })
                    .map(|row| row.project_id.clone())
                    .collect::<BTreeSet<_>>();
                if candidates.len() != 1 {
                    return Err(invalid(
                        "validated retained generation lacks one canonical owner",
                    ));
                }
                candidates.into_iter().next().expect("one canonical owner")
            }
        };
        let generation_id = Sha256ValueV1::parse(generation.generation_id.clone())?;
        if generation_owners.insert(generation_id, owner).is_some() {
            return Err(duplicate("canonical generation owner"));
        }
    }
    let bindings = post_image
        .quarantined_collected
        .iter()
        .map(|row| {
            Ok((
                row.project_id.clone(),
                Sha256ValueV1::parse(row.generation_id.clone())?,
            ))
        })
        .collect::<InventoryResult<BTreeSet<_>>>()?;
    Ok(ValidatedQuarantineBindingsV1 {
        plan_hash: report.plan_hash.clone(),
        transaction_id: post_image.transaction_id.clone(),
        generation_owners,
        bindings,
    })
}

fn validate_publisher_disposition_set<'a>(
    inventory: &'a V1ProjectCatalogInventory,
    report: &'a ProjectCatalogMigrationReportV1,
    resolution: &'a ProjectCatalogMigrationResolutionV1,
    post_image: &'a DeterministicPostImageInputV1,
    satisfied: &mut BTreeSet<&'a str>,
) -> InventoryResult<()> {
    let resolution_dispositions = resolution
        .publisher_binding_dispositions
        .iter()
        .map(|row| Ok((publisher_disposition_key(row)?, row)))
        .collect::<InventoryResult<BTreeMap<_, _>>>()?;
    let post_dispositions = post_image
        .publisher_binding_dispositions
        .iter()
        .map(|row| Ok((publisher_disposition_key(row)?, row)))
        .collect::<InventoryResult<BTreeMap<_, _>>>()?;
    if post_dispositions.len() != inventory.publisher_pins.len() {
        return Err(ProjectCatalogInventoryError::new(
            "error.project_catalog_inventory_incomplete_publisher_disposition",
            "every legacy publisher pin requires exactly one disposition",
        ));
    }
    let report_bindings = report
        .publisher_bindings
        .iter()
        .map(|row| (row.pin_observation_id.as_str(), row))
        .collect::<BTreeMap<_, _>>();
    for pin in &inventory.publisher_pins {
        let key = publisher_pin_key(pin)?;
        let disposition = post_dispositions
            .get(&key)
            .ok_or_else(|| unknown("publisher disposition for legacy pin"))?;
        validate_publisher_disposition_against_pin(pin, disposition, inventory)?;
        let report_binding = report_bindings
            .get(pin.observation_id.as_str())
            .ok_or_else(|| unknown("publisher report binding"))?;
        if report_binding.project_id != pin.project_id
            || report_binding.expected_scope_digest != digest_published_scope(&pin.expected_scope)?
            || report_binding.full_ref_digest != digest_publisher_full_ref(&pin.full_ref)?
        {
            return Err(invalid("publisher report binding disagrees with inventory"));
        }
        match report_binding.status {
            PublisherBindingReportStatusV1::ResolutionRequired => {
                let selected = resolution_dispositions.get(&key).ok_or_else(|| {
                    ProjectCatalogInventoryError::new(
                        "error.project_catalog_inventory_incomplete_publisher_disposition",
                        "required publisher disposition is missing",
                    )
                })?;
                if *selected != *disposition {
                    return Err(invalid(
                        "resolved publisher disposition disagrees with post-image input",
                    ));
                }
                let requirement = report
                    .required_resolutions
                    .iter()
                    .find(|row| {
                        row.kind == RequiredResolutionKindV1::PublisherBindingDisposition
                            && row.candidate_record_ids.contains(&pin.observation_id)
                    })
                    .ok_or_else(|| invalid("publisher resolution requirement is missing"))?;
                if !satisfied.insert(requirement.resolution_id.as_str()) {
                    return Err(duplicate("publisher resolution requirement"));
                }
            }
            PublisherBindingReportStatusV1::SeedG1Predicted
            | PublisherBindingReportStatusV1::NoPublishedContentAcknowledged => {
                if resolution_dispositions.contains_key(&key) {
                    return Err(ProjectCatalogInventoryError::new(
                        "error.project_catalog_inventory_unrequested_resolution",
                        "resolution attempts to replace an automatic publisher disposition",
                    ));
                }
            }
            PublisherBindingReportStatusV1::Refused => {
                return Err(ProjectCatalogInventoryError::new(
                    "error.project_catalog_inventory_refused",
                    "publisher binding is refused and cannot be resolved",
                ));
            }
        }
    }
    for key in resolution_dispositions.keys() {
        if !post_dispositions.contains_key(key) {
            return Err(unknown("publisher disposition"));
        }
    }
    Ok(())
}

pub fn deterministic_repo_history_group_ids(
    inventory: &V1ProjectCatalogInventory,
    resolved_projects: &[ResolvedProjectScopeInputV1],
) -> InventoryResult<Vec<String>> {
    Ok(
        deterministic_repo_history_group_memberships(inventory, resolved_projects)?
            .into_iter()
            .map(|(group_id, _)| group_id)
            .collect(),
    )
}

pub(crate) fn deterministic_repo_history_group_memberships(
    inventory: &V1ProjectCatalogInventory,
    resolved_projects: &[ResolvedProjectScopeInputV1],
) -> InventoryResult<Vec<(String, BTreeSet<ProjectId>)>> {
    let resolved_scopes = resolved_projects
        .iter()
        .map(|row| (row.project_id.clone(), row.published_scope.as_ref()))
        .collect::<BTreeMap<_, _>>();
    Ok(build_repo_history_group_evidence(inventory)?
        .into_iter()
        .filter(|group| {
            group.project_ids.iter().any(|project_id| {
                resolved_scopes
                    .get(project_id)
                    .is_some_and(|scope| scope.is_some())
                    || project_has_repo_history_evidence(inventory, project_id)
            })
        })
        .map(|group| (group.group_id, group.project_ids))
        .collect())
}

pub fn build_deterministic_repo_history_groups(
    inventory: &V1ProjectCatalogInventory,
    resolved_projects: &[ResolvedProjectScopeInputV1],
    planned_identities: &BTreeMap<String, PlannedRepoHistoryIdentityV1>,
) -> InventoryResult<Vec<DeterministicRepoHistoryGroupV1>> {
    let required_group_ids = deterministic_repo_history_group_ids(inventory, resolved_projects)?
        .into_iter()
        .collect::<BTreeSet<_>>();
    let evidence_groups = build_repo_history_group_evidence(inventory)?
        .into_iter()
        .filter(|group| required_group_ids.contains(&group.group_id))
        .collect::<Vec<_>>();
    if evidence_groups.len() != planned_identities.len() {
        return Err(invalid(
            "planned repository-history identities are incomplete",
        ));
    }
    let mut output = Vec::with_capacity(evidence_groups.len());
    for evidence in evidence_groups {
        let planned = planned_identities
            .get(&evidence.group_id)
            .ok_or_else(|| unknown("planned repository-history group"))?;
        output.push(DeterministicRepoHistoryGroupV1 {
            group_id: evidence.group_id,
            planned_history_id: planned.planned_history_id.clone(),
            planned_primary_namespace: planned.planned_primary_namespace.clone(),
            planned_compatibility_namespaces: planned.planned_compatibility_namespaces.clone(),
            project_ids: evidence.project_ids,
            evidence_classes: evidence.evidence_classes,
            proof_ids: evidence.proof_ids,
            source_observation_ids: evidence.source_observation_ids,
        });
    }
    for group in &output {
        validate_planned_namespaces(inventory, group)?;
    }
    let known_projects = inventory
        .legacy_projects
        .iter()
        .map(|row| ProjectId::parse(row.record.project_id.clone()))
        .collect::<Result<BTreeSet<_>, _>>()
        .map_err(|_| invalid("legacy project id is invalid"))?;
    validate_post_image_groups(inventory, &known_projects, resolved_projects, &output)?;
    Ok(output)
}

#[derive(Debug)]
struct RepoHistoryGroupEvidenceV1 {
    group_id: String,
    project_ids: BTreeSet<ProjectId>,
    evidence_classes: BTreeSet<RepoGroupingEvidenceClassV1>,
    proof_ids: BTreeSet<String>,
    source_observation_ids: BTreeSet<String>,
}

fn build_repo_history_group_evidence(
    inventory: &V1ProjectCatalogInventory,
) -> InventoryResult<Vec<RepoHistoryGroupEvidenceV1>> {
    inventory.validate()?;
    let projects = inventory
        .legacy_projects
        .iter()
        .map(|row| ProjectId::parse(row.record.project_id.clone()))
        .collect::<Result<BTreeSet<_>, _>>()
        .map_err(|_| invalid("legacy project id is invalid"))?;
    let mut parent = projects
        .iter()
        .map(|project_id| (project_id.clone(), project_id.clone()))
        .collect::<BTreeMap<_, _>>();
    for proof in &inventory.repo_grouping_proofs {
        let proof_projects = proof.project_ids();
        let Some(first) = proof_projects.first() else {
            return Err(invalid("repo grouping proof is empty"));
        };
        for project_id in proof_projects.iter().skip(1) {
            union_projects(&mut parent, first, project_id)?;
        }
    }

    let mut grouped = BTreeMap::<ProjectId, BTreeSet<ProjectId>>::new();
    for project_id in &projects {
        let root = find_project_root(&parent, project_id)?;
        grouped.entry(root).or_default().insert(project_id.clone());
    }

    let mut output = Vec::new();
    for project_ids in grouped.into_values() {
        let mut proof_ids = BTreeSet::new();
        let mut evidence_classes = BTreeSet::new();
        let mut source_observation_ids = BTreeSet::new();
        for proof in &inventory.repo_grouping_proofs {
            let proof_projects = proof.project_ids();
            if project_ids.is_superset(&proof_projects) {
                proof_ids.insert(proof.proof_id().to_string());
                source_observation_ids.extend(proof.source_observation_ids());
                evidence_classes.insert(match proof {
                    RepoGroupingProofV1::IdenticalCommittedRecordedAuthority { .. } => {
                        RepoGroupingEvidenceClassV1::IdenticalCommittedRecordedAuthority
                    }
                    RepoGroupingProofV1::SharedGitCommonDirectoryAndFirstCommit { .. } => {
                        RepoGroupingEvidenceClassV1::SharedGitCommonDirectoryAndFirstCommit
                    }
                    RepoGroupingProofV1::CollectedDescriptorActivationAgreement { .. } => {
                        RepoGroupingEvidenceClassV1::CollectedDescriptorActivationAgreement
                    }
                });
            }
        }
        let id_bytes =
            serde_json::to_vec(&(project_ids.clone(), proof_ids.clone())).map_err(|error| {
                ProjectCatalogInventoryError::new(
                    "error.project_catalog_inventory_encode",
                    error.to_string(),
                )
            })?;
        let digest = domain_hash(GROUP_ID_DOMAIN, &id_bytes);
        output.push(RepoHistoryGroupEvidenceV1 {
            group_id: format!("group_{}", &digest.as_str()[..32]),
            project_ids,
            evidence_classes,
            proof_ids,
            source_observation_ids,
        });
    }
    output.sort_by(|left, right| left.group_id.cmp(&right.group_id));
    Ok(output)
}

pub fn decode_inventory_v1(bytes: &[u8]) -> InventoryResult<V1ProjectCatalogInventory> {
    let inventory: V1ProjectCatalogInventory =
        decode_capped(bytes, MAX_PROJECT_CATALOG_INVENTORY_BYTES, "inventory")?;
    inventory.validate()?;
    Ok(inventory)
}

pub fn encode_inventory_v1(inventory: &V1ProjectCatalogInventory) -> InventoryResult<Vec<u8>> {
    inventory.canonical_json()
}

pub fn decode_migration_report_v1(
    bytes: &[u8],
) -> InventoryResult<ProjectCatalogMigrationReportV1> {
    let report: ProjectCatalogMigrationReportV1 =
        decode_capped(bytes, MAX_PROJECT_CATALOG_REPORT_BYTES, "migration report")?;
    report.validate()?;
    Ok(report)
}

pub fn encode_migration_report_v1(
    report: &ProjectCatalogMigrationReportV1,
    inventory: &V1ProjectCatalogInventory,
) -> InventoryResult<Vec<u8>> {
    report.validate_against_inventory(inventory)?;
    encode_capped(report, MAX_PROJECT_CATALOG_REPORT_BYTES, "migration report")
}

pub fn decode_migration_resolution_v1(
    bytes: &[u8],
) -> InventoryResult<ProjectCatalogMigrationResolutionV1> {
    let resolution: ProjectCatalogMigrationResolutionV1 = decode_capped(
        bytes,
        MAX_PROJECT_CATALOG_RESOLUTION_BYTES,
        "migration resolution",
    )?;
    resolution.validate()?;
    Ok(resolution)
}

pub fn encode_migration_resolution_v1(
    resolution: &ProjectCatalogMigrationResolutionV1,
) -> InventoryResult<Vec<u8>> {
    resolution.validate()?;
    let mut canonical = resolution.clone();
    canonical.canonicalize();
    encode_capped(
        &canonical,
        MAX_PROJECT_CATALOG_RESOLUTION_BYTES,
        "migration resolution",
    )
}

pub fn decode_sensitive_local_path_report_v1(
    bytes: &[u8],
) -> InventoryResult<SensitiveLocalPathReportV1> {
    let report: SensitiveLocalPathReportV1 =
        decode_capped(bytes, MAX_PROJECT_CATALOG_REPORT_BYTES, "sensitive report")?;
    report.validate()?;
    Ok(report)
}

pub fn encode_sensitive_local_path_report_v1(
    report: &SensitiveLocalPathReportV1,
) -> InventoryResult<Vec<u8>> {
    report.validate()?;
    encode_capped(report, MAX_PROJECT_CATALOG_REPORT_BYTES, "sensitive report")
}

pub fn digest_path(path: &str) -> Sha256ValueV1 {
    domain_hash(PATH_DIGEST_DOMAIN, path.as_bytes())
}

pub fn digest_published_scope(scope: &PublishedScope) -> InventoryResult<Sha256ValueV1> {
    scope
        .validate()
        .map_err(|_| invalid("published scope is invalid"))?;
    digest_json(scope)
}

pub fn digest_publisher_full_ref(full_ref: &str) -> InventoryResult<Sha256ValueV1> {
    validate_full_ref(full_ref)?;
    Ok(domain_hash(
        b"blackbox.project-catalog.publisher-full-ref.v1\0",
        full_ref.as_bytes(),
    ))
}

pub(crate) fn canonical_scope_resolution_id(
    scope: &PublishedScope,
    candidates: &BTreeSet<ProjectId>,
) -> InventoryResult<String> {
    let bytes = serde_json::to_vec(&(scope, candidates)).map_err(|error| {
        ProjectCatalogInventoryError::new(
            "error.project_catalog_inventory_encode",
            error.to_string(),
        )
    })?;
    let digest = domain_hash(b"blackbox.project-catalog.scope-conflict.v1\0", &bytes);
    Ok(format!("scope_conflict_{}", &digest.as_str()[..32]))
}

fn observed_scopes_for_project(
    inventory: &V1ProjectCatalogInventory,
    project_id: &ProjectId,
) -> BTreeSet<PublishedScope> {
    let mut scopes = BTreeSet::new();
    for project in &inventory.legacy_projects {
        if project.record.project_id == project_id.as_str()
            && let Some(scope) = &project.committed_scope
        {
            scopes.insert(scope.clone());
        }
    }
    for source in &inventory.code_sources {
        if source.project_id != *project_id {
            continue;
        }
        for generation in &source.generations {
            if let Some(scope) = &generation.activation_scope {
                scopes.insert(scope.clone());
            }
            if let ImmutableCollectedDescriptorV1::Valid {
                published_scope, ..
            } = &generation.descriptor
            {
                scopes.insert(published_scope.clone());
            }
        }
    }
    for retained in &inventory.retained_owner_resolutions {
        if retained.candidate_project_ids.contains(project_id) {
            scopes.insert(retained.published_scope.clone());
        }
    }
    for attachment in &inventory.attachment_candidates {
        if attachment.project_id == *project_id
            && let Some(scope) = &attachment.observed_scope
        {
            scopes.insert(scope.clone());
        }
    }
    for pin in &inventory.publisher_pins {
        if pin.project_id == *project_id {
            scopes.insert(pin.expected_scope.clone());
            if let Some(scope) = &pin.resolved_scope {
                scopes.insert(scope.clone());
            }
        }
    }
    scopes
}

fn validate_post_image_groups(
    inventory: &V1ProjectCatalogInventory,
    known_projects: &BTreeSet<ProjectId>,
    resolved_projects: &[ResolvedProjectScopeInputV1],
    groups: &[DeterministicRepoHistoryGroupV1],
) -> InventoryResult<()> {
    let proofs = inventory
        .repo_grouping_proofs
        .iter()
        .map(|proof| (proof.proof_id(), proof))
        .collect::<BTreeMap<_, _>>();
    let mut assigned = BTreeSet::new();
    for group in groups {
        validate_stable_id(&group.group_id, "post-image repo history group id")?;
        if group.project_ids.is_empty() {
            return Err(invalid("post-image repo history group is empty"));
        }
        validate_planned_namespaces(inventory, group)?;
        let mut expected_classes = BTreeSet::new();
        let mut expected_observations = BTreeSet::new();
        let mut parent = group
            .project_ids
            .iter()
            .map(|project_id| (project_id.clone(), project_id.clone()))
            .collect::<BTreeMap<_, _>>();
        for project_id in &group.project_ids {
            ensure_known_project(known_projects, project_id)?;
            if !assigned.insert(project_id.clone()) {
                return Err(duplicate("post-image repo history project"));
            }
        }
        for proof_id in &group.proof_ids {
            let proof = proofs
                .get(proof_id.as_str())
                .ok_or_else(|| unknown("post-image repo grouping proof"))?;
            let proof_projects = proof.project_ids();
            if !group.project_ids.is_superset(&proof_projects) {
                return Err(invalid("repo grouping proof crosses post-image groups"));
            }
            let first = proof_projects
                .first()
                .ok_or_else(|| invalid("repo grouping proof is empty"))?;
            for project_id in proof_projects.iter().skip(1) {
                union_projects(&mut parent, first, project_id)?;
            }
            expected_observations.extend(proof.source_observation_ids());
            expected_classes.insert(match proof {
                RepoGroupingProofV1::IdenticalCommittedRecordedAuthority { .. } => {
                    RepoGroupingEvidenceClassV1::IdenticalCommittedRecordedAuthority
                }
                RepoGroupingProofV1::SharedGitCommonDirectoryAndFirstCommit { .. } => {
                    RepoGroupingEvidenceClassV1::SharedGitCommonDirectoryAndFirstCommit
                }
                RepoGroupingProofV1::CollectedDescriptorActivationAgreement { .. } => {
                    RepoGroupingEvidenceClassV1::CollectedDescriptorActivationAgreement
                }
            });
        }
        if expected_classes != group.evidence_classes
            || expected_observations != group.source_observation_ids
        {
            return Err(invalid("repo history group evidence summary is not exact"));
        }
        if group.project_ids.len() > 1 {
            let roots = group
                .project_ids
                .iter()
                .map(|project_id| find_project_root(&parent, project_id))
                .collect::<InventoryResult<BTreeSet<_>>>()?;
            if roots.len() != 1 {
                return Err(ProjectCatalogInventoryError::new(
                    "error.project_catalog_inventory_weak_grouping",
                    "multi-project repo history group lacks connected strong evidence",
                ));
            }
        }
    }
    let resolved_scopes = resolved_projects
        .iter()
        .map(|row| (row.project_id.clone(), row.published_scope.as_ref()))
        .collect::<BTreeMap<_, _>>();
    let required_projects = known_projects
        .iter()
        .filter(|project_id| {
            resolved_scopes
                .get(*project_id)
                .is_some_and(|scope| scope.is_some())
                || project_has_repo_history_evidence(inventory, project_id)
        })
        .cloned()
        .collect::<BTreeSet<_>>();
    if assigned != required_projects {
        return Err(invalid(
            "post-image repo history groups do not exactly cover projects with history",
        ));
    }
    Ok(())
}

fn validate_planned_namespaces(
    inventory: &V1ProjectCatalogInventory,
    group: &DeterministicRepoHistoryGroupV1,
) -> InventoryResult<()> {
    if group
        .planned_compatibility_namespaces
        .contains(&group.planned_primary_namespace)
    {
        return Err(invalid(
            "primary namespace is repeated as a compatibility namespace",
        ));
    }
    let inventoried = inventory
        .git_metadata
        .iter()
        .filter(|row| group.project_ids.contains(&row.project_id))
        .flat_map(|row| row.materialized_commit_namespaces.iter())
        .map(|namespace| {
            CommitNamespace::parse(namespace.clone())
                .map_err(|_| invalid("inventoried commit namespace is invalid"))
        })
        .collect::<InventoryResult<BTreeSet<_>>>()?
        .difference(
            &inventory
                .legacy_namespace_clusters
                .iter()
                .map(|cluster| {
                    CommitNamespace::parse(cluster.materialized_namespace.clone())
                        .map_err(|_| invalid("legacy namespace cluster is invalid"))
                })
                .collect::<InventoryResult<BTreeSet<_>>>()?,
        )
        .cloned()
        .collect::<BTreeSet<_>>();
    if !inventoried.is_empty() {
        let mut planned = group.planned_compatibility_namespaces.clone();
        planned.insert(group.planned_primary_namespace.clone());
        if planned != inventoried {
            return Err(ProjectCatalogInventoryError::new(
                "error.project_catalog_inventory_namespace_plan_mismatch",
                "planned primary and compatibility namespaces do not exactly match inventory",
            ));
        }
    } else if !group.planned_compatibility_namespaces.is_empty() {
        return Err(ProjectCatalogInventoryError::new(
            "error.project_catalog_inventory_namespace_plan_mismatch",
            "compatibility namespaces were invented without materialized inventory evidence",
        ));
    }
    Ok(())
}

fn project_has_repo_history_evidence(
    inventory: &V1ProjectCatalogInventory,
    project_id: &ProjectId,
) -> bool {
    inventory.git_metadata.iter().any(|row| {
        row.project_id == *project_id
            && (row.common_directory_digest.is_some()
                || row.full_first_commit.is_some()
                || !row.materialized_commit_namespaces.is_empty()
                || row.last_ingested_sha.is_some())
    }) || inventory
        .repo_grouping_proofs
        .iter()
        .any(|proof| proof.project_ids().contains(project_id))
}

fn validate_grouping_proof<'a>(
    proof: &RepoGroupingProofV1,
    projects: &BTreeSet<ProjectId>,
    authority_observations: &BTreeMap<&'a str, (ProjectId, &'a RecordedRepoAuthority)>,
    git_observations: &BTreeMap<&'a str, &'a GitMetadataObservationV1>,
    generations: &BTreeMap<&'a str, &'a CollectedGenerationObservationV1>,
) -> InventoryResult<()> {
    let proof_projects = proof.project_ids();
    if proof_projects.len() < 2 {
        return Err(invalid(
            "repo grouping proof must cover at least two projects",
        ));
    }
    for project_id in &proof_projects {
        ensure_known_project(projects, project_id)?;
    }
    match proof {
        RepoGroupingProofV1::IdenticalCommittedRecordedAuthority { members, .. } => {
            if members.len() != proof_projects.len() {
                return Err(duplicate("recorded-authority proof project"));
            }
            let mut authority = None;
            for member in members {
                let observed = authority_observations
                    .get(member.authority_observation_id.as_str())
                    .ok_or_else(|| unknown("recorded authority proof observation"))?;
                if observed.0 != member.project_id || observed.1 != &member.authority {
                    return Err(invalid(
                        "recorded-authority proof disagrees with observation",
                    ));
                }
                match authority {
                    None => authority = Some(&member.authority),
                    Some(expected) if expected == &member.authority => {}
                    Some(_) => {
                        return Err(ProjectCatalogInventoryError::new(
                            "error.project_catalog_inventory_weak_grouping",
                            "recorded-authority grouping members do not share authority",
                        ));
                    }
                }
            }
        }
        RepoGroupingProofV1::SharedGitCommonDirectoryAndFirstCommit { members, .. } => {
            if members.len() != proof_projects.len() {
                return Err(duplicate("Git proof project"));
            }
            let mut shared: Option<(&Sha256ValueV1, &str)> = None;
            for member in members {
                let observed = git_observations
                    .get(member.git_observation_id.as_str())
                    .ok_or_else(|| unknown("Git grouping observation"))?;
                if observed.project_id != member.project_id {
                    return Err(invalid("Git proof project disagrees with observation"));
                }
                let common_directory = observed
                    .common_directory_digest
                    .as_ref()
                    .ok_or_else(|| invalid("Git proof lacks a canonical common directory"))?;
                let first_commit = observed
                    .full_first_commit
                    .as_deref()
                    .ok_or_else(|| invalid("Git proof lacks a full first commit"))?;
                match shared {
                    None => shared = Some((common_directory, first_commit)),
                    Some((expected_directory, expected_commit))
                        if expected_directory == common_directory
                            && expected_commit == first_commit => {}
                    Some(_) => {
                        return Err(ProjectCatalogInventoryError::new(
                            "error.project_catalog_inventory_weak_grouping",
                            "Git grouping members do not share common directory and first commit",
                        ));
                    }
                }
            }
        }
        RepoGroupingProofV1::CollectedDescriptorActivationAgreement { members, .. } => {
            if members.len() != proof_projects.len() {
                return Err(duplicate("collected proof project"));
            }
            let mut shared_authority = None;
            for member in members {
                let generation = generations
                    .get(member.generation_observation_id.as_str())
                    .ok_or_else(|| unknown("collected grouping observation"))?;
                if generation.project_id != member.project_id || !generation.checkout_missing {
                    return Err(invalid(
                        "collected proof does not describe the missing-checkout project",
                    ));
                }
                let descriptor_scope = match &generation.descriptor {
                    ImmutableCollectedDescriptorV1::Valid {
                        published_scope, ..
                    } => published_scope,
                    _ => {
                        return Err(invalid(
                            "collected proof requires a valid immutable descriptor",
                        ));
                    }
                };
                if generation.activation_scope.as_ref() != Some(descriptor_scope) {
                    return Err(ProjectCatalogInventoryError::new(
                        "error.project_catalog_inventory_weak_grouping",
                        "collected descriptor and activation authority disagree",
                    ));
                }
                match shared_authority {
                    None => shared_authority = Some(descriptor_scope.repo_id()),
                    Some(expected) if expected == descriptor_scope.repo_id() => {}
                    Some(_) => {
                        return Err(ProjectCatalogInventoryError::new(
                            "error.project_catalog_inventory_weak_grouping",
                            "collected grouping members do not share repository authority",
                        ));
                    }
                }
            }
        }
    }
    Ok(())
}

fn validate_publisher_disposition_against_pin(
    pin: &PublisherPinObservationV1,
    disposition: &PublisherBindingDispositionV1,
    inventory: &V1ProjectCatalogInventory,
) -> InventoryResult<()> {
    if disposition.project_id() != &pin.project_id
        || disposition.expected_scope() != &pin.expected_scope
        || disposition.full_ref() != pin.full_ref
    {
        return Err(invalid("publisher disposition rewrites pin identity"));
    }
    match disposition {
        PublisherBindingDispositionV1::SeedG1 {
            attachment_id,
            accepted_commit,
            expected_scope,
            ..
        } => {
            if !pin.candidate_attachment_ids.contains(attachment_id)
                || pin.resolved_commit.as_deref() != Some(accepted_commit)
                || pin.resolved_scope.as_ref() != Some(expected_scope)
            {
                return Err(ProjectCatalogInventoryError::new(
                    "error.project_catalog_inventory_invented_publisher_field",
                    "SeedG1 contains a field not proved by inventory",
                ));
            }
            let attachment = inventory
                .attachment_candidates
                .iter()
                .find(|row| row.attachment_id == *attachment_id)
                .ok_or_else(|| unknown("publisher attachment"))?;
            if attachment.project_id != pin.project_id
                || attachment.observed_scope.as_ref() != Some(expected_scope)
            {
                return Err(invalid("publisher attachment disagrees with pin scope"));
            }
        }
        PublisherBindingDispositionV1::NoPublishedContentAcknowledged { .. } => {}
    }
    Ok(())
}

fn validate_publisher_disposition(
    disposition: &PublisherBindingDispositionV1,
) -> InventoryResult<()> {
    disposition
        .expected_scope()
        .validate()
        .map_err(|_| invalid("publisher disposition scope is invalid"))?;
    validate_full_ref(disposition.full_ref())?;
    match disposition {
        PublisherBindingDispositionV1::SeedG1 {
            accepted_commit,
            generation_id,
            ..
        } => {
            validate_full_commit(accepted_commit)?;
            validate_token(generation_id, "accepted generation id")?;
        }
        PublisherBindingDispositionV1::NoPublishedContentAcknowledged {
            bounded_reason, ..
        } => validate_bounded_text(
            bounded_reason,
            MAX_OPERATOR_NOTE_BYTES,
            "no-content acknowledgement",
        )?,
    }
    Ok(())
}

fn validate_publisher_source_membership(
    inventory: &V1ProjectCatalogInventory,
) -> InventoryResult<()> {
    let publisher_source = inventory
        .mutable_source_evidence
        .iter()
        .find(|row| row.source_kind == MutableInventorySourceKindV1::PublisherRefStore)
        .ok_or_else(|| invalid("publisher source evidence is missing"))?;
    let decoded = match &publisher_source.state {
        InventorySourceStateV1::Missing { .. } => {
            if !inventory.publisher_ref_source_bytes.is_empty() {
                return Err(invalid("missing publisher source carries bytes"));
            }
            Vec::new()
        }
        InventorySourceStateV1::Present { .. } => {
            crate::publisher::decode_publisher_ref_source_v1(&inventory.publisher_ref_source_bytes)
                .map_err(|_| invalid("publisher source bytes fail owner codec"))?
        }
        InventorySourceStateV1::Corrupt { .. } => {
            return Err(invalid("publisher source is corrupt"));
        }
    };
    let decoded_keys = decoded
        .iter()
        .map(|row| (row.scope.clone(), row.branch_ref.as_str()))
        .collect::<BTreeSet<_>>();
    let typed_keys = inventory
        .publisher_pins
        .iter()
        .map(|row| (row.expected_scope.clone(), row.full_ref.as_str()))
        .collect::<BTreeSet<_>>();
    if decoded_keys.len() != decoded.len()
        || typed_keys.len() != inventory.publisher_pins.len()
        || decoded_keys != typed_keys
    {
        return Err(invalid(
            "publisher typed pins do not exactly match owner-decoded source bytes",
        ));
    }
    let projects = inventory
        .legacy_projects
        .iter()
        .map(|row| {
            (
                ProjectId::parse(row.record.project_id.clone())
                    .expect("validated legacy project id remains valid"),
                row,
            )
        })
        .collect::<BTreeMap<_, _>>();
    let attachments = inventory
        .attachment_candidates
        .iter()
        .map(|row| (&row.attachment_id, row))
        .collect::<BTreeMap<_, _>>();
    let checkouts = inventory
        .checkouts
        .iter()
        .map(|row| (row.observation_id.as_str(), row))
        .collect::<BTreeMap<_, _>>();
    for pin in &inventory.publisher_pins {
        let project = projects
            .get(&pin.project_id)
            .ok_or_else(|| unknown("publisher project observation"))?;
        let authority_scope =
            project_authority_scope(inventory, &pin.project_id)?.ok_or_else(|| {
                invalid("publisher project lacks committed or active scope authority")
            })?;
        if authority_scope != &pin.expected_scope {
            return Err(invalid(
                "publisher expected scope disagrees with project authority",
            ));
        }
        let mut expected =
            BTreeSet::from([pin.observation_id.clone(), project.observation_id.clone()]);
        if pin.resolved_commit.is_some() != pin.resolved_scope.is_some()
            || pin
                .resolved_scope
                .as_ref()
                .is_some_and(|scope| scope != &pin.expected_scope)
        {
            return Err(invalid(
                "publisher resolved commit and scope evidence is incomplete",
            ));
        }
        if let Some(authority) = &project.committed_authority {
            expected.insert(authority.observation_id.clone());
        }
        let mut provenance_git_ids = BTreeSet::new();
        for attachment_id in &pin.candidate_attachment_ids {
            let attachment = attachments
                .get(attachment_id)
                .ok_or_else(|| unknown("publisher candidate attachment"))?;
            if attachment.project_id != pin.project_id
                || attachment.observed_scope.as_ref() != Some(&pin.expected_scope)
            {
                return Err(invalid(
                    "publisher candidate attachment crosses project or scope",
                ));
            }
            if !checkouts.contains_key(attachment.checkout_observation_id.as_str()) {
                return Err(unknown("publisher candidate checkout observation"));
            }
            let matching_git = inventory
                .git_metadata
                .iter()
                .filter(|row| {
                    row.project_id == pin.project_id
                        && row.checkout_observation_id == attachment.checkout_observation_id
                        && row.resolved_refs.get(&pin.full_ref) == pin.resolved_commit.as_ref()
                })
                .collect::<Vec<_>>();
            if matching_git.len() != 1 {
                return Err(invalid(
                    "publisher candidate lacks unique checkout Git ref provenance",
                ));
            }
            expected.insert(attachment.observation_id.clone());
            expected.insert(attachment.checkout_observation_id.clone());
            provenance_git_ids.insert(matching_git[0].observation_id.clone());
        }
        if pin.resolved_commit.is_some() {
            if provenance_git_ids.is_empty() {
                provenance_git_ids.extend(
                    inventory
                        .git_metadata
                        .iter()
                        .filter(|row| {
                            row.project_id == pin.project_id
                                && row.resolved_refs.get(&pin.full_ref)
                                    == pin.resolved_commit.as_ref()
                        })
                        .map(|row| row.observation_id.clone()),
                );
            }
            if provenance_git_ids.is_empty() {
                return Err(invalid(
                    "resolved publisher commit lacks Git observation evidence",
                ));
            }
        }
        expected.extend(provenance_git_ids);
        if pin.source_observation_ids != expected {
            return Err(invalid(
                "publisher source observations are incomplete or spliced",
            ));
        }
    }
    Ok(())
}

pub(crate) fn project_authority_scope<'a>(
    inventory: &'a V1ProjectCatalogInventory,
    project_id: &ProjectId,
) -> InventoryResult<Option<&'a PublishedScope>> {
    let committed = inventory
        .legacy_projects
        .iter()
        .find(|row| row.record.project_id == project_id.as_str())
        .and_then(|row| row.committed_scope.as_ref());
    let active_generations = inventory
        .code_sources
        .iter()
        .filter(|source| source.project_id == *project_id)
        .flat_map(|source| source.generations.iter())
        .filter(|generation| generation.role == CollectedGenerationRoleV1::Active)
        .collect::<Vec<_>>();
    if active_generations.len() > 1 {
        return Err(invalid(
            "publisher project has multiple active scope authorities",
        ));
    }
    let active = active_generations
        .first()
        .and_then(|generation| match &generation.descriptor {
            ImmutableCollectedDescriptorV1::Valid {
                published_scope, ..
            } if generation.activation_scope.as_ref() == Some(published_scope) => {
                Some((published_scope, generation.checkout_missing))
            }
            ImmutableCollectedDescriptorV1::Valid { .. }
            | ImmutableCollectedDescriptorV1::Missing
            | ImmutableCollectedDescriptorV1::Corrupt { .. } => None,
        });
    match (committed, active) {
        (Some(committed), Some((active, _))) if committed != active => Err(invalid(
            "publisher committed and active scope authorities disagree",
        )),
        (Some(committed), _) => Ok(Some(committed)),
        (None, Some((active, true))) => Ok(Some(active)),
        (None, Some((_, false)) | None) => Ok(None),
    }
}

pub(crate) fn canonical_scope_conflicts(
    inventory: &V1ProjectCatalogInventory,
) -> InventoryResult<BTreeMap<PublishedScope, BTreeSet<ProjectId>>> {
    inventory.validate()?;
    let mut conflicts = BTreeMap::<PublishedScope, BTreeSet<ProjectId>>::new();
    for project in &inventory.legacy_projects {
        let project_id = ProjectId::parse(project.record.project_id.clone())
            .map_err(|_| invalid("legacy project id is invalid"))?;
        if let Some(scope) = project_authority_scope(inventory, &project_id)? {
            conflicts
                .entry(scope.clone())
                .or_default()
                .insert(project_id);
        }
    }
    conflicts.retain(|_, candidates| candidates.len() > 1);
    Ok(conflicts)
}

fn publisher_pin_key(
    pin: &PublisherPinObservationV1,
) -> InventoryResult<(String, Sha256ValueV1, String)> {
    Ok((
        pin.project_id.to_string(),
        digest_published_scope(&pin.expected_scope)?,
        pin.full_ref.clone(),
    ))
}

fn publisher_disposition_key(
    disposition: &PublisherBindingDispositionV1,
) -> InventoryResult<(String, Sha256ValueV1, String)> {
    Ok((
        disposition.project_id().to_string(),
        digest_published_scope(disposition.expected_scope())?,
        disposition.full_ref().to_string(),
    ))
}

fn publisher_disposition_sort_key(
    disposition: &PublisherBindingDispositionV1,
) -> (String, String, String, String, String) {
    let variant_key = match disposition {
        PublisherBindingDispositionV1::SeedG1 { attachment_id, .. } => {
            format!("seed_g1:{attachment_id}")
        }
        PublisherBindingDispositionV1::NoPublishedContentAcknowledged { .. } => {
            "no_published_content".to_string()
        }
    };
    (
        disposition.project_id().to_string(),
        disposition.expected_scope().repo_id().to_string(),
        disposition.expected_scope().bbox_root_relpath().to_string(),
        disposition.full_ref().to_string(),
        variant_key,
    )
}

fn require_resolution_kind<'a>(
    requirements: &'a BTreeMap<&'a str, &'a RequiredResolutionV1>,
    satisfied: &mut BTreeSet<&'a str>,
    resolution_id: &'a str,
    expected: RequiredResolutionKindV1,
) -> InventoryResult<()> {
    let requirement = requirements
        .get(resolution_id)
        .ok_or_else(|| unknown("resolution id"))?;
    if requirement.kind != expected {
        return Err(invalid("resolution disposition has the wrong kind"));
    }
    if !satisfied.insert(resolution_id) {
        return Err(duplicate("resolution id"));
    }
    Ok(())
}

fn find_project_root(
    parent: &BTreeMap<ProjectId, ProjectId>,
    project_id: &ProjectId,
) -> InventoryResult<ProjectId> {
    let mut current = project_id;
    let mut steps = 0usize;
    loop {
        let next = parent
            .get(current)
            .ok_or_else(|| unknown("repo grouping project"))?;
        if next == current {
            return Ok(current.clone());
        }
        current = next;
        steps += 1;
        if steps > parent.len() {
            return Err(invalid("repo grouping parent cycle"));
        }
    }
}

fn union_projects(
    parent: &mut BTreeMap<ProjectId, ProjectId>,
    left: &ProjectId,
    right: &ProjectId,
) -> InventoryResult<()> {
    let left_root = find_project_root(parent, left)?;
    let right_root = find_project_root(parent, right)?;
    if left_root == right_root {
        return Ok(());
    }
    let (child, root) = if left_root < right_root {
        (right_root, left_root)
    } else {
        (left_root, right_root)
    };
    parent.insert(child, root);
    Ok(())
}

fn decode_capped<T: DeserializeOwned>(
    bytes: &[u8],
    max_bytes: usize,
    kind: &'static str,
) -> InventoryResult<T> {
    if bytes.len() > max_bytes {
        return Err(limit(kind));
    }
    serde_json::from_slice(bytes).map_err(|error| {
        ProjectCatalogInventoryError::new(
            "error.project_catalog_inventory_decode",
            format!("{kind} is not strict valid JSON: {error}"),
        )
    })
}

fn encode_capped(
    value: &impl Serialize,
    max_bytes: usize,
    kind: &'static str,
) -> InventoryResult<Vec<u8>> {
    let bytes = serde_json::to_vec(value).map_err(|error| {
        ProjectCatalogInventoryError::new(
            "error.project_catalog_inventory_encode",
            format!("{kind} could not be encoded: {error}"),
        )
    })?;
    if bytes.len() > max_bytes {
        return Err(limit(kind));
    }
    Ok(bytes)
}

fn domain_hash(domain: &[u8], bytes: &[u8]) -> Sha256ValueV1 {
    let mut hasher = Sha256::new();
    hasher.update(domain);
    hasher.update(bytes);
    Sha256ValueV1(hex::encode(hasher.finalize()))
}

fn immutable_owner_row_set_hash(observation_ids: &BTreeSet<String>) -> Sha256ValueV1 {
    let mut bytes = Vec::new();
    for observation_id in observation_ids {
        bytes.extend_from_slice(&(observation_id.len() as u64).to_be_bytes());
        bytes.extend_from_slice(observation_id.as_bytes());
    }
    domain_hash(IMMUTABLE_OWNER_ROW_SET_DOMAIN, &bytes)
}

fn lane_completeness(
    owner_subsources: &[OwnerSubsourceEvidenceV1],
) -> ImmutableInventoryLaneCompletenessV1 {
    if owner_subsources
        .iter()
        .any(|source| matches!(&source.source_state, InventorySourceStateV1::Corrupt { .. }))
    {
        ImmutableInventoryLaneCompletenessV1::Corrupt
    } else if owner_subsources
        .iter()
        .any(|source| matches!(&source.source_state, InventorySourceStateV1::Missing { .. }))
    {
        ImmutableInventoryLaneCompletenessV1::Missing
    } else {
        ImmutableInventoryLaneCompletenessV1::Complete
    }
}

fn aggregate_lane_source_state(
    lane_kind: ImmutableInventoryLaneKindV1,
    owner_subsources: &[OwnerSubsourceEvidenceV1],
) -> InventoryResult<InventorySourceStateV1> {
    let bytes = serde_json::to_vec(&(lane_kind, owner_subsources)).map_err(|error| {
        ProjectCatalogInventoryError::new(
            "error.project_catalog_inventory_encode",
            error.to_string(),
        )
    })?;
    let fingerprint = domain_hash(IMMUTABLE_LANE_FINGERPRINT_DOMAIN, &bytes);
    let content_hash = domain_hash(IMMUTABLE_LANE_CONTENT_DOMAIN, &bytes);
    let byte_len = owner_subsources.iter().try_fold(0u64, |total, source| {
        let source_len = match &source.source_state {
            InventorySourceStateV1::Present { byte_len, .. } => *byte_len,
            InventorySourceStateV1::Missing { .. } => 0,
            InventorySourceStateV1::Corrupt { .. } => 0,
        };
        total
            .checked_add(source_len)
            .ok_or_else(|| limit("immutable lane aggregate bytes"))
    })?;
    Ok(match lane_completeness(owner_subsources) {
        ImmutableInventoryLaneCompletenessV1::Complete => InventorySourceStateV1::Present {
            fingerprint,
            content_hash,
            byte_len,
        },
        ImmutableInventoryLaneCompletenessV1::Missing => {
            InventorySourceStateV1::Missing { fingerprint }
        }
        ImmutableInventoryLaneCompletenessV1::Corrupt => InventorySourceStateV1::Corrupt {
            fingerprint,
            content_hash: Some(content_hash),
            diagnostic_code: "owner_subsource_corrupt".to_string(),
        },
    })
}

fn legacy_path_owner_kind(kind: LegacyPathStoreKindV1) -> ImmutableInventoryOwnerKindV1 {
    match kind {
        LegacyPathStoreKindV1::Knowledge => ImmutableInventoryOwnerKindV1::Knowledge,
        LegacyPathStoreKindV1::Gap => ImmutableInventoryOwnerKindV1::Gap,
        LegacyPathStoreKindV1::Thread => ImmutableInventoryOwnerKindV1::Thread,
        LegacyPathStoreKindV1::Note => ImmutableInventoryOwnerKindV1::Note,
        LegacyPathStoreKindV1::Pin => ImmutableInventoryOwnerKindV1::Pin,
        LegacyPathStoreKindV1::Roadmap => ImmutableInventoryOwnerKindV1::Roadmap,
        LegacyPathStoreKindV1::Packet => ImmutableInventoryOwnerKindV1::Packet,
        LegacyPathStoreKindV1::Task => ImmutableInventoryOwnerKindV1::Task,
        LegacyPathStoreKindV1::Proposal => ImmutableInventoryOwnerKindV1::Proposal,
        LegacyPathStoreKindV1::SlackBinding => ImmutableInventoryOwnerKindV1::SlackBinding,
        LegacyPathStoreKindV1::Whiteboard => ImmutableInventoryOwnerKindV1::Whiteboard,
        LegacyPathStoreKindV1::Artifact => ImmutableInventoryOwnerKindV1::Artifact,
        LegacyPathStoreKindV1::Provenance => ImmutableInventoryOwnerKindV1::Provenance,
        LegacyPathStoreKindV1::TranscriptEdge => ImmutableInventoryOwnerKindV1::TranscriptEdge,
    }
}

fn expected_lane_owner_kinds(
    lane_kind: ImmutableInventoryLaneKindV1,
) -> BTreeSet<ImmutableInventoryOwnerKindV1> {
    use ImmutableInventoryOwnerKindV1 as Owner;
    match lane_kind {
        ImmutableInventoryLaneKindV1::ProjectScopedRefs => {
            BTreeSet::from([Owner::Tantivy, Owner::VectorMetadata])
        }
        ImmutableInventoryLaneKindV1::EdgeWorkspaces => BTreeSet::from([Owner::EdgeManifest]),
        ImmutableInventoryLaneKindV1::GitMetadata => {
            BTreeSet::from([Owner::GitMetadata, Owner::Tantivy, Owner::VectorMetadata])
        }
        ImmutableInventoryLaneKindV1::Checkouts => BTreeSet::from([Owner::Checkout]),
        ImmutableInventoryLaneKindV1::AttachmentCandidates => {
            BTreeSet::from([Owner::LegacyProjectStore, Owner::Checkout])
        }
        ImmutableInventoryLaneKindV1::InventoryTargets => {
            BTreeSet::from([Owner::Artifact, Owner::Provenance])
        }
        ImmutableInventoryLaneKindV1::MaterializedAliases => {
            BTreeSet::from([Owner::LegacyProjectStore])
        }
        ImmutableInventoryLaneKindV1::LegacyPathObservations => LegacyPathStoreKindV1::all()
            .into_iter()
            .map(legacy_path_owner_kind)
            .collect(),
        ImmutableInventoryLaneKindV1::RepoGroupingProofs => {
            BTreeSet::from([Owner::DerivedRepoGrouping])
        }
        ImmutableInventoryLaneKindV1::LegacyNamespaceClusters => {
            BTreeSet::from([Owner::DerivedLegacyNamespaceClusters])
        }
    }
}

fn validate_immutable_lane_evidence(
    evidence: &ImmutableInventoryLaneEvidenceV1,
) -> InventoryResult<()> {
    validate_stable_id(&evidence.source_id, "immutable lane source id")?;
    if evidence.owner_subsources.is_empty()
        || evidence.owner_subsources.len() > MAX_PROJECT_CATALOG_ENTRIES
    {
        return Err(invalid(
            "immutable lane owner subsource coverage is incomplete",
        ));
    }
    let mut source_ids = BTreeSet::new();
    let mut owner_kinds = BTreeSet::new();
    for source in &evidence.owner_subsources {
        validate_stable_id(&source.source_id, "immutable owner source id")?;
        validate_inventory_source_state(&source.source_state)?;
        if !source_ids.insert(source.source_id.as_str()) {
            return Err(duplicate("immutable owner source id"));
        }
        if !owner_kinds.insert(source.owner_kind) {
            return Err(duplicate("immutable owner kind"));
        }
        for observation_id in &source.row_observation_ids {
            validate_stable_id(observation_id, "immutable owner row observation id")?;
        }
        if source.row_set_sha256 != immutable_owner_row_set_hash(&source.row_observation_ids) {
            return Err(invalid("immutable owner row-set hash mismatch"));
        }
        if !matches!(&source.source_state, InventorySourceStateV1::Present { .. })
            && !source.row_observation_ids.is_empty()
        {
            return Err(invalid(
                "missing or corrupt immutable owner source carries rows",
            ));
        }
    }
    if owner_kinds != expected_lane_owner_kinds(evidence.lane_kind) {
        return Err(invalid(
            "immutable lane owner subsource coverage is incomplete",
        ));
    }
    let completeness = lane_completeness(&evidence.owner_subsources);
    if evidence.completeness != completeness
        || evidence.source_state
            != aggregate_lane_source_state(evidence.lane_kind, &evidence.owner_subsources)?
    {
        return Err(invalid(
            "immutable lane aggregate does not match owner subsources",
        ));
    }
    match evidence.completeness {
        ImmutableInventoryLaneCompletenessV1::Complete => {
            if !matches!(
                &evidence.source_state,
                InventorySourceStateV1::Present { .. }
            ) {
                return Err(invalid(
                    "complete immutable lane does not have present source evidence",
                ));
            }
        }
        ImmutableInventoryLaneCompletenessV1::Missing => {
            if evidence.row_count != 0
                || !matches!(
                    &evidence.source_state,
                    InventorySourceStateV1::Missing { .. }
                )
            {
                return Err(invalid(
                    "missing immutable lane carries rows or wrong state",
                ));
            }
        }
        ImmutableInventoryLaneCompletenessV1::Corrupt => {
            if evidence.row_count != 0
                || !matches!(
                    &evidence.source_state,
                    InventorySourceStateV1::Corrupt { .. }
                )
            {
                return Err(invalid(
                    "corrupt immutable lane carries rows or wrong state",
                ));
            }
        }
    }
    Ok(())
}

pub fn mutable_source_row_set_hash(observation_ids: &BTreeSet<String>) -> Sha256ValueV1 {
    let mut bytes = Vec::new();
    for observation_id in observation_ids {
        bytes.extend_from_slice(&(observation_id.len() as u64).to_be_bytes());
        bytes.extend_from_slice(observation_id.as_bytes());
    }
    domain_hash(
        b"blackbox.project-catalog.mutable-source-row-set.v1\0",
        &bytes,
    )
}

fn validate_mutable_source_locator(
    source_kind: MutableInventorySourceKindV1,
    locator: &MutableInventorySourceLocatorV1,
) -> InventoryResult<()> {
    let matches_kind = matches!(
        (source_kind, locator),
        (
            MutableInventorySourceKindV1::LegacyProjectStore,
            MutableInventorySourceLocatorV1::LegacyProjectStore
        ) | (
            MutableInventorySourceKindV1::PublisherRefStore,
            MutableInventorySourceLocatorV1::PublisherRefStore
        ) | (
            MutableInventorySourceKindV1::EffectiveSourceManifest,
            MutableInventorySourceLocatorV1::CodeSourceAnchor
        ) | (
            MutableInventorySourceKindV1::CodeSourceActivation,
            MutableInventorySourceLocatorV1::CodeSourceActivation { .. }
        ) | (
            MutableInventorySourceKindV1::CodeSourceGenerationMetadata,
            MutableInventorySourceLocatorV1::CodeSourceGenerationMetadata { .. }
        ) | (
            MutableInventorySourceKindV1::CodeSourceGenerationManifest,
            MutableInventorySourceLocatorV1::CodeSourceGenerationManifest { .. }
        ) | (
            MutableInventorySourceKindV1::CodeSourceCollisionLifecycle,
            MutableInventorySourceLocatorV1::CodeSourceCollisionLifecycle { .. }
        ) | (
            MutableInventorySourceKindV1::CommittedAuthorityProbe,
            MutableInventorySourceLocatorV1::CommittedProjectConfig { .. }
                | MutableInventorySourceLocatorV1::CommittedProjectConfigUnavailable { .. }
        ) | (
            MutableInventorySourceKindV1::CheckoutRoot,
            MutableInventorySourceLocatorV1::CheckoutRoot { .. }
        ) | (
            MutableInventorySourceKindV1::CheckoutMarker,
            MutableInventorySourceLocatorV1::CheckoutMarker { .. }
        )
    );
    if !matches_kind {
        return Err(invalid("mutable source kind and locator disagree"));
    }
    match locator {
        MutableInventorySourceLocatorV1::CodeSourceGenerationMetadata { generation_id, .. }
        | MutableInventorySourceLocatorV1::CodeSourceGenerationManifest { generation_id, .. } => {
            Sha256ValueV1::parse(generation_id.clone())
                .map_err(|_| invalid("generation source locator id is invalid"))?;
        }
        MutableInventorySourceLocatorV1::CommittedProjectConfig {
            commit_oid,
            repo_relative_path,
            ..
        } => {
            validate_full_commit(commit_oid)?;
            validate_relative_path(repo_relative_path)?;
        }
        MutableInventorySourceLocatorV1::LegacyProjectStore
        | MutableInventorySourceLocatorV1::PublisherRefStore
        | MutableInventorySourceLocatorV1::CodeSourceAnchor
        | MutableInventorySourceLocatorV1::CodeSourceActivation { .. }
        | MutableInventorySourceLocatorV1::CodeSourceCollisionLifecycle { .. }
        | MutableInventorySourceLocatorV1::CommittedProjectConfigUnavailable { .. }
        | MutableInventorySourceLocatorV1::CheckoutRoot { .. }
        | MutableInventorySourceLocatorV1::CheckoutMarker { .. } => {}
    }
    Ok(())
}

fn validate_mutable_source_row_coverage(
    inventory: &V1ProjectCatalogInventory,
    observations: &BTreeSet<&str>,
) -> InventoryResult<()> {
    let code_source_row_by_project = inventory
        .code_sources
        .iter()
        .map(|row| (row.project_id.clone(), row.observation_id.as_str()))
        .collect::<BTreeMap<_, _>>();
    let generation_row_by_identity = inventory
        .code_sources
        .iter()
        .flat_map(|source| {
            source
                .generations
                .iter()
                .map(|row| {
                    let scope = match &row.descriptor {
                        ImmutableCollectedDescriptorV1::Valid {
                            published_scope, ..
                        } => Some(published_scope),
                        ImmutableCollectedDescriptorV1::Missing
                        | ImmutableCollectedDescriptorV1::Corrupt { .. } => None,
                    };
                    (row.observation_id.as_str(), (&row.generation_id, scope))
                })
                .chain(source.quarantine.iter().map(|row| {
                    let scope = match &row.descriptor {
                        ImmutableCollectedDescriptorV1::Valid {
                            published_scope, ..
                        } => Some(published_scope),
                        ImmutableCollectedDescriptorV1::Missing
                        | ImmutableCollectedDescriptorV1::Corrupt { .. } => None,
                    };
                    (row.observation_id.as_str(), (&row.generation_id, scope))
                }))
        })
        .chain(inventory.retained_owner_resolutions.iter().map(|row| {
            (
                row.observation_id.as_str(),
                (&row.generation_id, Some(&row.published_scope)),
            )
        }))
        .collect::<BTreeMap<_, _>>();
    let authority_row_by_project = inventory
        .legacy_projects
        .iter()
        .map(|row| {
            (
                ProjectId::parse(row.record.project_id.clone())
                    .expect("validated legacy project id remains valid"),
                row.committed_authority
                    .as_ref()
                    .map_or(row.observation_id.as_str(), |authority| {
                        authority.observation_id.as_str()
                    }),
            )
        })
        .collect::<BTreeMap<_, _>>();
    let quarantine_row_by_project = inventory
        .code_sources
        .iter()
        .flat_map(|source| {
            source
                .quarantine
                .iter()
                .map(|row| (row.project_id.clone(), row.observation_id.as_str()))
        })
        .fold(
            BTreeMap::<ProjectId, BTreeSet<&str>>::new(),
            |mut rows, (project_id, observation_id)| {
                rows.entry(project_id).or_default().insert(observation_id);
                rows
            },
        );
    let checkout_row_by_digest = inventory
        .checkouts
        .iter()
        .map(|row| {
            (
                row.canonical_root_digest.clone(),
                row.observation_id.as_str(),
            )
        })
        .collect::<BTreeMap<_, _>>();
    let mut actual = BTreeMap::<MutableInventorySourceKindV1, BTreeSet<&str>>::new();
    for source in &inventory.mutable_source_evidence {
        for observation_id in &source.row_observation_ids {
            if !observations.contains(observation_id.as_str()) {
                return Err(unknown("mutable source row observation"));
            }
            actual
                .entry(source.source_kind)
                .or_default()
                .insert(observation_id);
        }
        if let MutableInventorySourceLocatorV1::CodeSourceCollisionLifecycle { project_id } =
            &source.source_locator
        {
            let expected = quarantine_row_by_project
                .get(project_id)
                .ok_or_else(|| unknown("collision lifecycle project"))?
                .iter()
                .map(|row| (*row).to_string())
                .collect::<BTreeSet<_>>();
            if source.row_observation_ids != expected {
                return Err(invalid(
                    "collision lifecycle source does not bind its complete generation set",
                ));
            }
            continue;
        }
        let expected_single_row = match &source.source_locator {
            MutableInventorySourceLocatorV1::CodeSourceActivation { project_id } => {
                code_source_row_by_project.get(project_id).copied()
            }
            MutableInventorySourceLocatorV1::CodeSourceGenerationMetadata {
                scope_hash,
                generation_id,
            }
            | MutableInventorySourceLocatorV1::CodeSourceGenerationManifest {
                scope_hash,
                generation_id,
            } => generation_row_by_identity.iter().find_map(
                |(observation_id, (observed_generation_id, scope))| {
                    (*observed_generation_id == generation_id
                        && scope.is_none_or(|scope| {
                            Sha256ValueV1::parse(bbox_code_source::scope_hash(scope))
                                .ok()
                                .as_ref()
                                == Some(scope_hash)
                        }))
                    .then_some(*observation_id)
                },
            ),
            MutableInventorySourceLocatorV1::CodeSourceCollisionLifecycle { .. } => unreachable!(),
            MutableInventorySourceLocatorV1::CommittedProjectConfig { project_id, .. }
            | MutableInventorySourceLocatorV1::CommittedProjectConfigUnavailable { project_id } => {
                authority_row_by_project.get(project_id).copied()
            }
            MutableInventorySourceLocatorV1::CheckoutRoot {
                canonical_root_digest,
            }
            | MutableInventorySourceLocatorV1::CheckoutMarker {
                canonical_root_digest,
            } => checkout_row_by_digest.get(canonical_root_digest).copied(),
            MutableInventorySourceLocatorV1::LegacyProjectStore
            | MutableInventorySourceLocatorV1::PublisherRefStore
            | MutableInventorySourceLocatorV1::CodeSourceAnchor => None,
        };
        if !matches!(
            &source.source_locator,
            MutableInventorySourceLocatorV1::LegacyProjectStore
                | MutableInventorySourceLocatorV1::PublisherRefStore
                | MutableInventorySourceLocatorV1::CodeSourceAnchor
        ) && expected_single_row
            .is_none_or(|row| source.row_observation_ids != BTreeSet::from([row.to_string()]))
        {
            return Err(invalid(
                "mutable source locator and bound observation disagree",
            ));
        }
    }
    let legacy_rows = inventory
        .legacy_projects
        .iter()
        .map(|row| row.observation_id.as_str())
        .collect::<BTreeSet<_>>();
    let authority_rows = inventory
        .legacy_projects
        .iter()
        .map(|row| {
            row.committed_authority
                .as_ref()
                .map_or(row.observation_id.as_str(), |authority| {
                    authority.observation_id.as_str()
                })
        })
        .collect::<BTreeSet<_>>();
    let code_source_rows = inventory
        .code_sources
        .iter()
        .map(|row| row.observation_id.as_str())
        .collect::<BTreeSet<_>>();
    let generation_rows = inventory
        .code_sources
        .iter()
        .flat_map(|source| {
            source
                .generations
                .iter()
                .map(|row| row.observation_id.as_str())
                .chain(
                    source
                        .quarantine
                        .iter()
                        .map(|row| row.observation_id.as_str()),
                )
        })
        .chain(
            inventory
                .retained_owner_resolutions
                .iter()
                .map(|row| row.observation_id.as_str()),
        )
        .collect::<BTreeSet<_>>();
    let effective_rows = code_source_rows
        .iter()
        .copied()
        .chain(generation_rows.iter().copied())
        .collect::<BTreeSet<_>>();
    let publisher_rows = inventory
        .publisher_pins
        .iter()
        .map(|row| row.observation_id.as_str())
        .collect::<BTreeSet<_>>();
    let checkout_rows = inventory
        .checkouts
        .iter()
        .map(|row| row.observation_id.as_str())
        .collect::<BTreeSet<_>>();
    for (kind, expected) in [
        (
            MutableInventorySourceKindV1::LegacyProjectStore,
            legacy_rows,
        ),
        (
            MutableInventorySourceKindV1::PublisherRefStore,
            publisher_rows,
        ),
        (
            MutableInventorySourceKindV1::EffectiveSourceManifest,
            effective_rows,
        ),
        (
            MutableInventorySourceKindV1::CodeSourceActivation,
            code_source_rows,
        ),
        (
            MutableInventorySourceKindV1::CodeSourceGenerationMetadata,
            generation_rows.clone(),
        ),
        (
            MutableInventorySourceKindV1::CodeSourceGenerationManifest,
            generation_rows,
        ),
        (
            MutableInventorySourceKindV1::CodeSourceCollisionLifecycle,
            inventory
                .code_sources
                .iter()
                .flat_map(|source| {
                    source
                        .quarantine
                        .iter()
                        .map(|row| row.observation_id.as_str())
                })
                .collect(),
        ),
        (
            MutableInventorySourceKindV1::CommittedAuthorityProbe,
            authority_rows,
        ),
        (
            MutableInventorySourceKindV1::CheckoutRoot,
            checkout_rows.clone(),
        ),
        (MutableInventorySourceKindV1::CheckoutMarker, checkout_rows),
    ] {
        if actual.remove(&kind).unwrap_or_default() != expected {
            return Err(invalid("mutable source row coverage is incomplete"));
        }
    }
    Ok(())
}

fn digest_json(value: &impl Serialize) -> InventoryResult<Sha256ValueV1> {
    let bytes = serde_json::to_vec(value).map_err(|error| {
        ProjectCatalogInventoryError::new(
            "error.project_catalog_inventory_encode",
            error.to_string(),
        )
    })?;
    Ok(domain_hash(b"blackbox.project-catalog.value.v1\0", &bytes))
}

fn insert_observation<'a>(
    observations: &mut BTreeSet<&'a str>,
    observation_id: &'a str,
) -> InventoryResult<()> {
    validate_stable_id(observation_id, "observation id")?;
    if !observations.insert(observation_id) {
        return Err(duplicate("observation id"));
    }
    Ok(())
}

fn ensure_known_project(
    projects: &BTreeSet<ProjectId>,
    project_id: &ProjectId,
) -> InventoryResult<()> {
    if !projects.contains(project_id) {
        return Err(unknown("project id"));
    }
    Ok(())
}

fn validate_unique_by<'a>(
    values: impl IntoIterator<Item = &'a str>,
    kind: &'static str,
) -> InventoryResult<()> {
    let mut seen = BTreeSet::new();
    for value in values {
        validate_stable_id(value, kind)?;
        if !seen.insert(value) {
            return Err(duplicate(kind));
        }
    }
    Ok(())
}

fn validate_descriptor(descriptor: &ImmutableCollectedDescriptorV1) -> InventoryResult<()> {
    match descriptor {
        ImmutableCollectedDescriptorV1::Valid {
            published_scope, ..
        } => published_scope
            .validate()
            .map_err(|_| invalid("collected descriptor scope is invalid")),
        ImmutableCollectedDescriptorV1::Missing => Ok(()),
        ImmutableCollectedDescriptorV1::Corrupt { diagnostic_code } => {
            validate_diagnostic_code(diagnostic_code)
        }
    }
}

fn validate_artifact(artifact: &ImmutableArtifactObservationV1) -> InventoryResult<()> {
    match artifact {
        ImmutableArtifactObservationV1::Valid { .. } | ImmutableArtifactObservationV1::Missing => {
            Ok(())
        }
        ImmutableArtifactObservationV1::Corrupt { diagnostic_code } => {
            validate_diagnostic_code(diagnostic_code)
        }
    }
}

fn validate_collision_lifecycle(
    lifecycle: &CollisionLifecycleObservationV1,
    generation: &QuarantinedGenerationObservationV1,
) -> InventoryResult<()> {
    if lifecycle.version != 1
        || lifecycle.project_id != generation.project_id
        || lifecycle.generation_id != generation.generation_id
        || lifecycle.manifest_sha256 != generation.manifest_hash
    {
        return Err(invalid(
            "collision lifecycle disagrees with quarantined generation",
        ));
    }
    lifecycle
        .former_scope
        .validate()
        .map_err(|_| invalid("collision lifecycle former scope is invalid"))?;
    if let ImmutableCollectedDescriptorV1::Valid {
        published_scope, ..
    } = &generation.descriptor
        && published_scope != &lifecycle.former_scope
    {
        return Err(invalid(
            "collision lifecycle former scope disagrees with descriptor",
        ));
    }
    match &lifecycle.selector_evidence {
        DurableSelectorEvidenceV1::ExactMaterialized { .. } => {}
        DurableSelectorEvidenceV1::NoDurableSelector => {}
    }
    validate_bounded_text(
        &lifecycle.snapshot_id,
        MAX_SNAPSHOT_ID_BYTES,
        "collision lifecycle snapshot id",
    )
}

fn validate_inventory_source_state(state: &InventorySourceStateV1) -> InventoryResult<()> {
    match state {
        InventorySourceStateV1::Present { byte_len, .. } => {
            if *byte_len > MAX_PROJECT_CATALOG_INVENTORY_BYTES as u64 {
                return Err(limit("inventory source bytes"));
            }
        }
        InventorySourceStateV1::Missing { .. } => {}
        InventorySourceStateV1::Corrupt {
            diagnostic_code, ..
        } => validate_diagnostic_code(diagnostic_code)?,
    }
    Ok(())
}

fn validate_marker_state(state: &CheckoutMarkerStateV1) -> InventoryResult<()> {
    match state {
        CheckoutMarkerStateV1::Valid { checkout_id } => validate_checkout_id(checkout_id),
        CheckoutMarkerStateV1::MissingOrEmpty | CheckoutMarkerStateV1::Symlinked => Ok(()),
        CheckoutMarkerStateV1::Malformed { diagnostic_code }
        | CheckoutMarkerStateV1::Unreadable { diagnostic_code } => {
            validate_diagnostic_code(diagnostic_code)
        }
    }
}

fn validate_checkout_id(value: &str) -> InventoryResult<()> {
    if value.len() != 32
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    {
        return Err(invalid(
            "checkout id is not 32 lowercase hexadecimal characters",
        ));
    }
    Ok(())
}

fn validate_full_commit(value: &str) -> InventoryResult<()> {
    if !matches!(value.len(), 40 | 64)
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    {
        return Err(invalid("Git commit is not a full lowercase object id"));
    }
    Ok(())
}

fn validate_full_ref(value: &str) -> InventoryResult<()> {
    validate_bounded_text(value, MAX_REF_BYTES, "full Git ref")?;
    if !value.starts_with("refs/")
        || value.contains("..")
        || value.ends_with('/')
        || value.contains('\\')
    {
        return Err(invalid("publisher ref is not a full Git ref"));
    }
    Ok(())
}

fn validate_relative_path(value: &str) -> InventoryResult<()> {
    validate_bounded_text(value, MAX_PATH_BYTES, "relative path")?;
    if value.starts_with('/')
        || value.contains('\\')
        || value.split('/').any(|segment| matches!(segment, "" | ".."))
    {
        return Err(invalid("path is not normalized relative form"));
    }
    Ok(())
}

fn validate_literal_selector(value: &str) -> InventoryResult<()> {
    validate_bounded_text(value, MAX_PATH_BYTES, "legacy selector")
}

fn validate_stable_id(value: &str, kind: &'static str) -> InventoryResult<()> {
    if value.is_empty()
        || value.len() > MAX_ID_BYTES
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.'))
    {
        return Err(invalid(kind));
    }
    Ok(())
}

fn validate_token(value: &str, kind: &'static str) -> InventoryResult<()> {
    validate_stable_id(value, kind)
}

fn validate_optional_token(value: Option<&str>, kind: &'static str) -> InventoryResult<()> {
    if let Some(value) = value {
        validate_token(value, kind)?;
    }
    Ok(())
}

fn validate_timestamp(value: &str) -> InventoryResult<()> {
    validate_bounded_text(value, 128, "timestamp")?;
    if !value
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b':' | b'.' | b'+'))
    {
        return Err(invalid("timestamp"));
    }
    Ok(())
}

fn validate_diagnostic_code(value: &str) -> InventoryResult<()> {
    validate_stable_id(value, "diagnostic code")
}

fn validate_bounded_text(value: &str, max_bytes: usize, kind: &'static str) -> InventoryResult<()> {
    if value.is_empty()
        || value.len() > max_bytes
        || value.trim() != value
        || value.chars().any(char::is_control)
    {
        return Err(invalid(kind));
    }
    Ok(())
}

fn invalid(detail: impl Into<String>) -> ProjectCatalogInventoryError {
    ProjectCatalogInventoryError::new("error.project_catalog_inventory_invalid", detail)
}

fn duplicate(kind: impl Into<String>) -> ProjectCatalogInventoryError {
    ProjectCatalogInventoryError::new(
        "error.project_catalog_inventory_duplicate",
        format!("duplicate {}", kind.into()),
    )
}

fn unknown(kind: impl Into<String>) -> ProjectCatalogInventoryError {
    ProjectCatalogInventoryError::new(
        "error.project_catalog_inventory_unknown_record",
        format!("unknown {}", kind.into()),
    )
}

fn limit(kind: impl Into<String>) -> ProjectCatalogInventoryError {
    ProjectCatalogInventoryError::new(
        "error.project_catalog_inventory_limit",
        format!("{} exceeds its limit", kind.into()),
    )
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    fn project_id(value: &str) -> ProjectId {
        ProjectId::parse(value.to_string()).unwrap()
    }

    fn attachment_id(hex_digit: char) -> AttachmentId {
        AttachmentId::parse(format!("att_{}", hex_digit.to_string().repeat(32))).unwrap()
    }

    fn transaction_id(hex_digit: char) -> ProjectCatalogTransactionId {
        ProjectCatalogTransactionId::parse(format!("pct_{}", hex_digit.to_string().repeat(32)))
            .unwrap()
    }

    fn history_id(hex_digit: char) -> RepoHistoryId {
        RepoHistoryId::parse(format!("rh_{}", hex_digit.to_string().repeat(32))).unwrap()
    }

    fn binding_id(hex_digit: char) -> LegacyPathBindingId {
        LegacyPathBindingId::parse(format!("lpb_{}", hex_digit.to_string().repeat(32))).unwrap()
    }

    fn scope(root: &str) -> PublishedScope {
        PublishedScope::try_new("acme_repo", root).unwrap()
    }

    fn hash(label: &str) -> Sha256ValueV1 {
        Sha256ValueV1::digest(label.as_bytes())
    }

    fn lane_evidence(
        lane_kind: ImmutableInventoryLaneKindV1,
        source_id: &str,
        row_count: u64,
    ) -> ImmutableInventoryLaneEvidenceV1 {
        let owner_subsources = expected_lane_owner_kinds(lane_kind)
            .into_iter()
            .enumerate()
            .map(|(index, owner_kind)| {
                let owner_source_id = format!("{source_id}_owner_{index}");
                OwnerSubsourceEvidenceV1::new(
                    owner_kind,
                    owner_source_id.clone(),
                    InventorySourceStateV1::Present {
                        fingerprint: hash(&format!("{owner_source_id}_fingerprint")),
                        content_hash: hash(&format!("{owner_source_id}_content")),
                        byte_len: 1,
                    },
                    BTreeSet::new(),
                )
            })
            .collect();
        ImmutableInventoryLaneEvidenceV1::from_owner_subsources(
            lane_kind,
            source_id,
            row_count,
            owner_subsources,
        )
        .unwrap()
    }

    fn mutable_evidence(
        source_id: &str,
        source_kind: MutableInventorySourceKindV1,
        content_hash: Sha256ValueV1,
    ) -> MutableInventorySourceEvidenceV1 {
        let (source_locator, row_observation_ids) = match source_id {
            "source_legacy_store" => (
                MutableInventorySourceLocatorV1::LegacyProjectStore,
                BTreeSet::from(["legacy_alpha".to_string(), "legacy_beta".to_string()]),
            ),
            "source_publisher_store" => (
                MutableInventorySourceLocatorV1::PublisherRefStore,
                BTreeSet::from(["pin_alpha".to_string()]),
            ),
            "source_effective_manifest" => (
                MutableInventorySourceLocatorV1::CodeSourceAnchor,
                BTreeSet::from(["source_alpha".to_string(), "generation_alpha".to_string()]),
            ),
            "source_activation" => (
                MutableInventorySourceLocatorV1::CodeSourceActivation {
                    project_id: project_id("alpha"),
                },
                BTreeSet::from(["source_alpha".to_string()]),
            ),
            "source_metadata" => (
                MutableInventorySourceLocatorV1::CodeSourceGenerationMetadata {
                    scope_hash: Sha256ValueV1::parse(bbox_code_source::scope_hash(&scope(
                        "services/alpha",
                    )))
                    .unwrap(),
                    generation_id: "e".repeat(64),
                },
                BTreeSet::from(["generation_alpha".to_string()]),
            ),
            "source_manifest" => (
                MutableInventorySourceLocatorV1::CodeSourceGenerationManifest {
                    scope_hash: Sha256ValueV1::parse(bbox_code_source::scope_hash(&scope(
                        "services/alpha",
                    )))
                    .unwrap(),
                    generation_id: "e".repeat(64),
                },
                BTreeSet::from(["generation_alpha".to_string()]),
            ),
            "source_authority_alpha" => (
                MutableInventorySourceLocatorV1::CommittedProjectConfig {
                    project_id: project_id("alpha"),
                    commit_oid: "a".repeat(40),
                    repo_relative_path: "alpha/.bbox/config.toml".to_string(),
                },
                BTreeSet::from(["authority_alpha".to_string()]),
            ),
            "source_authority_beta" => (
                MutableInventorySourceLocatorV1::CommittedProjectConfig {
                    project_id: project_id("beta"),
                    commit_oid: "b".repeat(40),
                    repo_relative_path: "beta/.bbox/config.toml".to_string(),
                },
                BTreeSet::from(["authority_beta".to_string()]),
            ),
            "source_checkout_root" => (
                MutableInventorySourceLocatorV1::CheckoutRoot {
                    canonical_root_digest: digest_path("/workspace/acme"),
                },
                BTreeSet::from(["checkout_acme".to_string()]),
            ),
            "source_checkout_marker" => (
                MutableInventorySourceLocatorV1::CheckoutMarker {
                    canonical_root_digest: digest_path("/workspace/acme"),
                },
                BTreeSet::from(["checkout_acme".to_string()]),
            ),
            _ => unreachable!("fixture source id is closed"),
        };
        let row_set_sha256 = mutable_source_row_set_hash(&row_observation_ids);
        MutableInventorySourceEvidenceV1 {
            source_id: source_id.to_string(),
            source_kind,
            source_locator,
            state: InventorySourceStateV1::Present {
                fingerprint: hash(&format!("{source_id}_fingerprint")),
                content_hash,
                byte_len: 1,
            },
            row_observation_ids,
            row_set_sha256,
        }
    }

    fn legacy_project(
        observation_id: &str,
        project: &str,
        path: &str,
        authority_observation_id: &str,
    ) -> LegacyProjectObservationV1 {
        LegacyProjectObservationV1 {
            observation_id: observation_id.to_string(),
            record: LegacyProjectRecordInventoryV1 {
                project_id: project.to_string(),
                repo_id: Some("weak_hint".to_string()),
                canonical_path_digest: digest_path(path),
                registered_at: "2026-01-02T03:04:05Z".to_string(),
                is_git_repo: true,
                languages: BTreeSet::new(),
                aliases: BTreeSet::new(),
            },
            path_status: LegacyProjectPathStatusV1::Present,
            committed_authority: Some(CommittedAuthorityObservationV1 {
                observation_id: authority_observation_id.to_string(),
                authority: RecordedRepoAuthority::parse("acme_repo".to_string()).unwrap(),
            }),
            committed_scope: Some(scope(&format!("services/{project}"))),
        }
    }

    pub(crate) fn fixture_inventory() -> V1ProjectCatalogInventory {
        let source_store_bytes = br#"{"version":1,"projects":[]}"#.to_vec();
        let publisher_ref_source_bytes = serde_json::to_vec(&serde_json::json!({
            "version": 1,
            "refs": [{
                "scope": {
                    "repo_id": "acme_repo",
                    "bbox_root_relpath": "services/alpha"
                },
                "branch_ref": "refs/heads/main"
            }]
        }))
        .unwrap();
        let source_store_hash = Sha256ValueV1::digest(&source_store_bytes);
        let publisher_ref_source_hash = Sha256ValueV1::digest(&publisher_ref_source_bytes);
        let alpha = project_id("alpha");
        let beta = project_id("beta");
        let alpha_attachment = attachment_id('1');
        let beta_attachment = attachment_id('2');
        V1ProjectCatalogInventory {
            version: PROJECT_CATALOG_INVENTORY_VERSION_V1,
            source_store_hash: source_store_hash.clone(),
            publisher_ref_source_hash: publisher_ref_source_hash.clone(),
            publisher_ref_source_bytes,
            code_source_inventory_hash: hash("code_source_inventory"),
            code_source_generation_count: 1,
            code_source_generation_set_sha256: hash("code_source_generation_set"),
            mutable_source_evidence: vec![
                mutable_evidence(
                    "source_legacy_store",
                    MutableInventorySourceKindV1::LegacyProjectStore,
                    source_store_hash,
                ),
                mutable_evidence(
                    "source_publisher_store",
                    MutableInventorySourceKindV1::PublisherRefStore,
                    publisher_ref_source_hash,
                ),
                mutable_evidence(
                    "source_effective_manifest",
                    MutableInventorySourceKindV1::EffectiveSourceManifest,
                    hash("effective_manifest"),
                ),
                mutable_evidence(
                    "source_activation",
                    MutableInventorySourceKindV1::CodeSourceActivation,
                    hash("activation"),
                ),
                mutable_evidence(
                    "source_metadata",
                    MutableInventorySourceKindV1::CodeSourceGenerationMetadata,
                    hash("metadata"),
                ),
                mutable_evidence(
                    "source_manifest",
                    MutableInventorySourceKindV1::CodeSourceGenerationManifest,
                    hash("manifest"),
                ),
                mutable_evidence(
                    "source_authority_alpha",
                    MutableInventorySourceKindV1::CommittedAuthorityProbe,
                    hash("authority_alpha"),
                ),
                mutable_evidence(
                    "source_authority_beta",
                    MutableInventorySourceKindV1::CommittedAuthorityProbe,
                    hash("authority_beta"),
                ),
                mutable_evidence(
                    "source_checkout_root",
                    MutableInventorySourceKindV1::CheckoutRoot,
                    hash("checkout_root"),
                ),
                mutable_evidence(
                    "source_checkout_marker",
                    MutableInventorySourceKindV1::CheckoutMarker,
                    hash("checkout_marker"),
                ),
            ],
            immutable_lane_evidence: vec![
                lane_evidence(
                    ImmutableInventoryLaneKindV1::ProjectScopedRefs,
                    "lane_project_refs",
                    2,
                ),
                lane_evidence(
                    ImmutableInventoryLaneKindV1::EdgeWorkspaces,
                    "lane_edge_workspaces",
                    1,
                ),
                lane_evidence(
                    ImmutableInventoryLaneKindV1::GitMetadata,
                    "lane_git_metadata",
                    2,
                ),
                lane_evidence(ImmutableInventoryLaneKindV1::Checkouts, "lane_checkouts", 1),
                lane_evidence(
                    ImmutableInventoryLaneKindV1::AttachmentCandidates,
                    "lane_attachments",
                    2,
                ),
                lane_evidence(
                    ImmutableInventoryLaneKindV1::InventoryTargets,
                    "lane_targets",
                    1,
                ),
                lane_evidence(
                    ImmutableInventoryLaneKindV1::MaterializedAliases,
                    "lane_aliases",
                    1,
                ),
                lane_evidence(
                    ImmutableInventoryLaneKindV1::LegacyPathObservations,
                    "lane_legacy_paths",
                    1,
                ),
                lane_evidence(
                    ImmutableInventoryLaneKindV1::RepoGroupingProofs,
                    "lane_repo_proofs",
                    1,
                ),
                lane_evidence(
                    ImmutableInventoryLaneKindV1::LegacyNamespaceClusters,
                    "lane_namespace_clusters",
                    0,
                ),
            ],
            legacy_projects: vec![
                legacy_project(
                    "legacy_alpha",
                    "alpha",
                    "/workspace/acme/alpha",
                    "authority_alpha",
                ),
                legacy_project(
                    "legacy_beta",
                    "beta",
                    "/workspace/acme/beta",
                    "authority_beta",
                ),
            ],
            code_sources: vec![CodeSourceObservationV1 {
                observation_id: "source_alpha".to_string(),
                project_id: alpha.clone(),
                generations: vec![CollectedGenerationObservationV1 {
                    observation_id: "generation_alpha".to_string(),
                    project_id: alpha.clone(),
                    role: CollectedGenerationRoleV1::Active,
                    generation_id: "e".repeat(64),
                    activation_scope: Some(scope("services/alpha")),
                    descriptor: ImmutableCollectedDescriptorV1::Valid {
                        descriptor_hash: hash("descriptor"),
                        published_scope: scope("services/alpha"),
                    },
                    manifest: ImmutableArtifactObservationV1::Valid {
                        content_hash: hash("manifest"),
                    },
                    selector_evidence: DurableSelectorEvidenceV1::ExactMaterialized {
                        selector_hash: hash("selector"),
                    },
                    checkout_missing: false,
                    planned_metadata_v2_hash: hash("metadata_v2"),
                }],
                quarantine: Vec::new(),
                effective_manifest_hash: hash("effective_manifest"),
                planned_activation_v2_hash: Some(hash("activation_v2")),
            }],
            retained_owner_resolutions: Vec::new(),
            publisher_pins: vec![PublisherPinObservationV1 {
                observation_id: "pin_alpha".to_string(),
                project_id: alpha.clone(),
                expected_scope: scope("services/alpha"),
                full_ref: "refs/heads/main".to_string(),
                candidate_attachment_ids: BTreeSet::from([alpha_attachment.clone()]),
                resolved_commit: Some("a".repeat(40)),
                resolved_scope: Some(scope("services/alpha")),
                source_observation_ids: BTreeSet::from([
                    "pin_alpha".to_string(),
                    "legacy_alpha".to_string(),
                    "authority_alpha".to_string(),
                    "attachment_alpha".to_string(),
                    "checkout_acme".to_string(),
                    "git_alpha".to_string(),
                ]),
            }],
            project_scoped_refs: vec![
                ProjectScopedRefObservationV1 {
                    observation_id: "tantivy_alpha".to_string(),
                    store_kind: ProjectScopedRefStoreKindV1::Tantivy,
                    project_id: alpha.clone(),
                    stable_row_id: "doc_alpha".to_string(),
                    entity_ref_hash: hash("entity"),
                },
                ProjectScopedRefObservationV1 {
                    observation_id: "vector_beta".to_string(),
                    store_kind: ProjectScopedRefStoreKindV1::VectorMetadata,
                    project_id: beta.clone(),
                    stable_row_id: "vector_beta".to_string(),
                    entity_ref_hash: hash("vector"),
                },
            ],
            edge_workspaces: vec![EdgeWorkspaceObservationV1 {
                observation_id: "edge_workspace".to_string(),
                workspace_id: "workspace_1".to_string(),
                project_ids: BTreeSet::from([alpha.clone(), beta.clone()]),
                manifest_hash: hash("edge_manifest"),
                active_selector_hash: hash("edge_selector"),
            }],
            git_metadata: vec![
                GitMetadataObservationV1 {
                    observation_id: "git_alpha".to_string(),
                    project_id: alpha.clone(),
                    checkout_observation_id: "checkout_acme".to_string(),
                    common_directory_digest: Some(digest_path("/workspace/acme/.git")),
                    full_first_commit: Some("b".repeat(40)),
                    materialized_commit_namespaces: BTreeSet::from(
                        ["legacy_namespace".to_string()],
                    ),
                    last_ingested_sha: Some("c".repeat(40)),
                    resolved_refs: BTreeMap::from([(
                        "refs/heads/main".to_string(),
                        "a".repeat(40),
                    )]),
                },
                GitMetadataObservationV1 {
                    observation_id: "git_beta".to_string(),
                    project_id: beta.clone(),
                    checkout_observation_id: "checkout_acme".to_string(),
                    common_directory_digest: Some(digest_path("/workspace/acme/.git")),
                    full_first_commit: Some("b".repeat(40)),
                    materialized_commit_namespaces: BTreeSet::from(
                        ["legacy_namespace".to_string()],
                    ),
                    last_ingested_sha: Some("d".repeat(40)),
                    resolved_refs: BTreeMap::from([(
                        "refs/heads/main".to_string(),
                        "d".repeat(40),
                    )]),
                },
            ],
            checkouts: vec![CheckoutObservationV1 {
                observation_id: "checkout_acme".to_string(),
                canonical_root_digest: digest_path("/workspace/acme"),
                marker_state: CheckoutMarkerStateV1::MissingOrEmpty,
            }],
            attachment_candidates: vec![
                AttachmentCandidateObservationV1 {
                    observation_id: "attachment_alpha".to_string(),
                    attachment_id: alpha_attachment,
                    project_id: alpha,
                    checkout_observation_id: "checkout_acme".to_string(),
                    base_relpath: "services/alpha".to_string(),
                    observed_scope: Some(scope("services/alpha")),
                },
                AttachmentCandidateObservationV1 {
                    observation_id: "attachment_beta".to_string(),
                    attachment_id: beta_attachment,
                    project_id: beta,
                    checkout_observation_id: "checkout_acme".to_string(),
                    base_relpath: "services/beta".to_string(),
                    observed_scope: Some(scope("services/beta")),
                },
            ],
            inventory_targets: vec![InventoryTargetObservationV1 {
                observation_id: "artifact_alpha".to_string(),
                target_kind: InventoryTargetKindV1::ProjectArtifact,
                project_id: project_id("alpha"),
                stable_target_id: "artifact_1".to_string(),
                target_hash: hash("artifact"),
            }],
            materialized_aliases: vec![MaterializedAliasObservationV1 {
                observation_id: "alias_alpha".to_string(),
                alias: "alpha_alias".to_string(),
                project_id: project_id("alpha"),
                registered_at: Some("2026-01-02T03:04:05Z".to_string()),
            }],
            legacy_path_observations: vec![LegacyPathObservationV1 {
                observation_id: "legacy_path_alpha".to_string(),
                store_kind: LegacyPathStoreKindV1::Knowledge,
                stable_row_id: "knowledge_1".to_string(),
                selector_kind: LegacySelectorKindV1::ProjectAndRelativePath,
                selector_digest: digest_path("/workspace/acme/alpha/src/Example.java"),
            }],
            repo_grouping_proofs: vec![RepoGroupingProofV1::IdenticalCommittedRecordedAuthority {
                proof_id: "proof_authority".to_string(),
                members: vec![
                    RecordedAuthorityEvidenceMemberV1 {
                        project_id: project_id("alpha"),
                        authority: RecordedRepoAuthority::parse("acme_repo".to_string()).unwrap(),
                        authority_observation_id: "authority_alpha".to_string(),
                    },
                    RecordedAuthorityEvidenceMemberV1 {
                        project_id: project_id("beta"),
                        authority: RecordedRepoAuthority::parse("acme_repo".to_string()).unwrap(),
                        authority_observation_id: "authority_beta".to_string(),
                    },
                ],
            }],
            legacy_namespace_clusters: Vec::new(),
            legacy_commit_namespaces: Vec::new(),
        }
    }

    fn seed_disposition() -> PublisherBindingDispositionV1 {
        PublisherBindingDispositionV1::SeedG1 {
            project_id: project_id("alpha"),
            attachment_id: attachment_id('1'),
            expected_scope: scope("services/alpha"),
            full_ref: "refs/heads/main".to_string(),
            accepted_commit: "a".repeat(40),
            generation_id: "accepted_generation_1".to_string(),
            payload_hashes: PublicationPayloadHashesV1 {
                knowledge_manifest_hash: hash("knowledge_manifest"),
                gap_manifest_hash: hash("gap_manifest"),
                knowledge_payload_hash: hash("knowledge_payload"),
                gap_payload_hash: hash("gap_payload"),
            },
            pointer_hash: hash("pointer"),
        }
    }

    #[test]
    fn immutable_lane_cannot_launder_an_omitted_owner_subsource() {
        let inventory = fixture_inventory();
        let lane = inventory
            .immutable_lane_evidence
            .iter()
            .find(|lane| lane.lane_kind == ImmutableInventoryLaneKindV1::LegacyPathObservations)
            .unwrap();
        let mut incomplete = lane.owner_subsources.clone();
        incomplete.retain(|source| source.owner_kind != ImmutableInventoryOwnerKindV1::Whiteboard);

        let error = ImmutableInventoryLaneEvidenceV1::from_owner_subsources(
            lane.lane_kind,
            lane.source_id.clone(),
            lane.row_count,
            incomplete,
        )
        .unwrap_err();
        assert_eq!(error.code(), "error.project_catalog_inventory_invalid");
    }

    #[test]
    fn legacy_path_store_kind_set_matches_the_closed_owner_contract() {
        assert_eq!(
            LegacyPathStoreKindV1::all(),
            BTreeSet::from([
                LegacyPathStoreKindV1::Knowledge,
                LegacyPathStoreKindV1::Gap,
                LegacyPathStoreKindV1::Thread,
                LegacyPathStoreKindV1::Note,
                LegacyPathStoreKindV1::Pin,
                LegacyPathStoreKindV1::Roadmap,
                LegacyPathStoreKindV1::Packet,
                LegacyPathStoreKindV1::Task,
                LegacyPathStoreKindV1::Proposal,
                LegacyPathStoreKindV1::SlackBinding,
                LegacyPathStoreKindV1::Whiteboard,
                LegacyPathStoreKindV1::Artifact,
                LegacyPathStoreKindV1::Provenance,
                LegacyPathStoreKindV1::TranscriptEdge,
            ])
        );
    }

    #[test]
    fn legacy_commit_namespace_commitments_are_canonical_inventory() {
        let mut inventory = fixture_inventory();
        inventory
            .legacy_commit_namespaces
            .push(LegacyCommitNamespaceInventoryV1 {
                observation_id: "commit_namespace_legacy".to_string(),
                namespace: CommitNamespace::parse("legacy_namespace".to_string()).unwrap(),
                commit_document_count: 7,
                commit_document_set_sha256: hash("commit_documents"),
                vector_key_count: 7,
                vector_key_set_sha256: hash("commit_vectors"),
                attribution: LegacyCommitNamespaceAttributionV1::Ambiguous {
                    candidate_project_ids: BTreeSet::from([
                        project_id("alpha"),
                        project_id("beta"),
                    ]),
                },
            });
        let lane = inventory
            .immutable_lane_evidence
            .iter_mut()
            .find(|lane| lane.lane_kind == ImmutableInventoryLaneKindV1::GitMetadata)
            .unwrap();
        let mut owner_subsources = lane.owner_subsources.clone();
        let git_owner = owner_subsources
            .iter_mut()
            .find(|source| source.owner_kind == ImmutableInventoryOwnerKindV1::GitMetadata)
            .unwrap();
        git_owner
            .row_observation_ids
            .insert("commit_namespace_legacy".to_string());
        git_owner.row_set_sha256 = immutable_owner_row_set_hash(&git_owner.row_observation_ids);
        *lane = ImmutableInventoryLaneEvidenceV1::from_owner_subsources(
            lane.lane_kind,
            lane.source_id.clone(),
            lane.row_count + 1,
            owner_subsources,
        )
        .unwrap();

        let original_hash = inventory.inventory_hash().unwrap();
        inventory.legacy_commit_namespaces[0].commit_document_set_sha256 =
            hash("changed_commit_documents");
        let changed_hash = inventory.inventory_hash().unwrap();
        assert_ne!(original_hash, changed_hash);

        inventory.legacy_commit_namespaces.clear();
        assert!(inventory.validate().is_err());
    }

    #[test]
    fn collected_scope_authority_is_only_a_missing_checkout_fallback() {
        let mut inventory = fixture_inventory();
        let project_id = project_id("alpha");
        inventory
            .legacy_projects
            .iter_mut()
            .find(|project| project.record.project_id == project_id.as_str())
            .unwrap()
            .committed_scope = None;
        inventory
            .code_sources
            .iter_mut()
            .find(|source| source.project_id == project_id)
            .unwrap()
            .generations
            .first_mut()
            .unwrap()
            .checkout_missing = false;
        assert_eq!(
            project_authority_scope(&inventory, &project_id).unwrap(),
            None
        );

        inventory
            .code_sources
            .iter_mut()
            .find(|source| source.project_id == project_id)
            .unwrap()
            .generations
            .first_mut()
            .unwrap()
            .checkout_missing = true;
        assert_eq!(
            project_authority_scope(&inventory, &project_id).unwrap(),
            Some(&scope("services/alpha"))
        );

        inventory
            .legacy_projects
            .iter_mut()
            .find(|project| project.record.project_id == project_id.as_str())
            .unwrap()
            .committed_scope = Some(scope("services/different"));
        assert!(project_authority_scope(&inventory, &project_id).is_err());
    }

    pub(crate) fn fixture_post_image(
        inventory: &V1ProjectCatalogInventory,
    ) -> DeterministicPostImageInputV1 {
        let inventory_hash = inventory.inventory_hash().unwrap();
        let checkout_id = "e".repeat(32);
        let registered_at = "2026-01-02T03:04:05Z".to_string();
        let resolved_project_scopes = vec![
            ResolvedProjectScopeInputV1 {
                project_id: project_id("alpha"),
                published_scope: Some(scope("services/alpha")),
                created_at: registered_at.clone(),
            },
            ResolvedProjectScopeInputV1 {
                project_id: project_id("beta"),
                published_scope: Some(scope("services/beta")),
                created_at: registered_at.clone(),
            },
        ];
        let group_ids =
            deterministic_repo_history_group_ids(inventory, &resolved_project_scopes).unwrap();
        let planned_groups = BTreeMap::from([(
            group_ids[0].clone(),
            PlannedRepoHistoryIdentityV1 {
                planned_history_id: history_id('3'),
                planned_primary_namespace: CommitNamespace::parse("legacy_namespace").unwrap(),
                planned_compatibility_namespaces: BTreeSet::new(),
            },
        )]);
        DeterministicPostImageInputV1 {
            version: PROJECT_CATALOG_MIGRATION_REPORT_VERSION_V1,
            transaction_id: transaction_id('4'),
            inventory_hash,
            repo_history_groups: build_deterministic_repo_history_groups(
                inventory,
                &resolved_project_scopes,
                &planned_groups,
            )
            .unwrap(),
            resolved_project_scopes,
            attachments: vec![
                AttachmentPostImageInputV1 {
                    attachment_id: attachment_id('1'),
                    project_id: project_id("alpha"),
                    checkout_observation_id: "checkout_acme".to_string(),
                    checkout_id: checkout_id.clone(),
                    expected_scope: Some(scope("services/alpha")),
                    attached_at: registered_at.clone(),
                },
                AttachmentPostImageInputV1 {
                    attachment_id: attachment_id('2'),
                    project_id: project_id("beta"),
                    checkout_observation_id: "checkout_acme".to_string(),
                    checkout_id: checkout_id.clone(),
                    expected_scope: Some(scope("services/beta")),
                    attached_at: registered_at,
                },
            ],
            checkout_identity_actions: vec![CheckoutIdentityActionV1 {
                observation_id: "checkout_acme".to_string(),
                canonical_root_digest: digest_path("/workspace/acme"),
                planned_checkout_id: checkout_id,
            }],
            legacy_path_bindings: vec![LegacyPathBindingPostImageInputV1 {
                observation_id: "legacy_path_alpha".to_string(),
                planned_binding_id: binding_id('5'),
                attachment_id: Some(attachment_id('1')),
                literal_selector: "/workspace/acme/alpha/src/Example.java".to_string(),
                relationship: LegacyPathRelationshipV1::Contained,
            }],
            quarantined_collected: Vec::new(),
            publisher_binding_dispositions: vec![seed_disposition()],
            predicted_hashes: PredictedPostImageHashesV1 {
                catalog_hash: hash("catalog"),
                attachment_hash: hash("attachments"),
                participant_hashes: BTreeMap::from([(
                    "accepted_pointer_alpha".to_string(),
                    hash("pointer"),
                )]),
                g1_assets: vec![PredictedAssetV1 {
                    asset_id: "g1_alpha".to_string(),
                    content_hash: hash("g1"),
                }],
                accepted_pointer_hashes: BTreeMap::from([(project_id("alpha"), hash("pointer"))]),
            },
        }
    }

    pub(crate) fn fixture_report(
        inventory: &V1ProjectCatalogInventory,
        resolution: &ProjectCatalogMigrationResolutionV1,
        post_image: &DeterministicPostImageInputV1,
    ) -> ProjectCatalogMigrationReportV1 {
        let pin = &inventory.publisher_pins[0];
        ProjectCatalogMigrationReportV1 {
            version: PROJECT_CATALOG_MIGRATION_REPORT_VERSION_V1,
            transaction_id: post_image.transaction_id.clone(),
            inventory_hash: inventory.inventory_hash().unwrap(),
            plan_hash: canonical_plan_hash(inventory, resolution, post_image).unwrap(),
            resolution_artifact_hash: Sha256ValueV1::digest(
                &encode_migration_resolution_v1(resolution).unwrap(),
            ),
            source_store_hash: inventory.source_store_hash.clone(),
            publisher_ref_source_hash: inventory.publisher_ref_source_hash.clone(),
            generated_at: "2026-01-02T03:04:05Z".to_string(),
            status: ProjectCatalogMigrationStatusV1::Clean,
            plan_kind: ProjectCatalogMigrationPlanKindV1::Executable,
            projects: inventory
                .legacy_projects
                .iter()
                .map(|row| ProjectMigrationReportRowV1 {
                    observation_id: row.observation_id.clone(),
                    project_id: ProjectId::parse(row.record.project_id.clone()).unwrap(),
                    path_status: row.path_status,
                    path_digest: row.record.canonical_path_digest.clone(),
                    committed_authority_present: row.committed_authority.is_some(),
                })
                .collect(),
            repo_history_groups: post_image.repo_history_groups.clone(),
            attachments: inventory
                .attachment_candidates
                .iter()
                .map(|row| AttachmentMigrationReportRowV1 {
                    observation_id: row.observation_id.clone(),
                    attachment_id: row.attachment_id.clone(),
                    project_id: row.project_id.clone(),
                    checkout_observation_id: row.checkout_observation_id.clone(),
                    scope_digest: row
                        .observed_scope
                        .as_ref()
                        .map(|scope| digest_published_scope(scope).unwrap()),
                })
                .collect(),
            checkout_identity_actions: post_image.checkout_identity_actions.clone(),
            legacy_path_bindings: vec![LegacyPathBindingReportV1 {
                observation_id: "legacy_path_alpha".to_string(),
                planned_binding_id: binding_id('5'),
                store_kind: LegacyPathStoreKindV1::Knowledge,
                relationship: LegacyPathRelationshipV1::Contained,
                status: LegacyPathBindingStatusV1::Planned,
                path_digest: digest_path("/workspace/acme/alpha/src/Example.java"),
            }],
            namespace_conflicts: Vec::new(),
            scope_conflicts: Vec::new(),
            alias_conflicts: Vec::new(),
            activation_conflicts: Vec::new(),
            publisher_bindings: vec![PublisherBindingReportV1 {
                pin_observation_id: pin.observation_id.clone(),
                project_id: pin.project_id.clone(),
                expected_scope_digest: digest_published_scope(&pin.expected_scope).unwrap(),
                full_ref_digest: digest_publisher_full_ref(&pin.full_ref).unwrap(),
                status: PublisherBindingReportStatusV1::SeedG1Predicted,
            }],
            publisher_binding_conflicts: Vec::new(),
            predicted_g1_assets: post_image.predicted_hashes.g1_assets.clone(),
            predicted_accepted_pointer_hashes: post_image
                .predicted_hashes
                .accepted_pointer_hashes
                .clone(),
            missing_paths: Vec::new(),
            unscoped_legacy_counts: BTreeMap::new(),
            required_resolutions: Vec::new(),
            predicted_catalog_hash: post_image.predicted_hashes.catalog_hash.clone(),
            predicted_attachment_hash: post_image.predicted_hashes.attachment_hash.clone(),
            predicted_participant_hashes: post_image.predicted_hashes.participant_hashes.clone(),
        }
    }

    pub(crate) fn validated_quarantine_bindings_fixture(
        transaction_id: ProjectCatalogTransactionId,
        generation_owners: BTreeMap<Sha256ValueV1, ProjectId>,
        bindings: BTreeSet<(ProjectId, Sha256ValueV1)>,
    ) -> ValidatedQuarantineBindingsV1 {
        let mut inventory = fixture_inventory();
        inventory.code_sources.clear();
        inventory.code_source_generation_count = generation_owners.len() as u64;
        inventory.code_source_generation_set_sha256 = hash("authority_generation_set");

        let bindings_by_project = bindings.iter().fold(
            BTreeMap::<ProjectId, BTreeSet<Sha256ValueV1>>::new(),
            |mut by_project, (project_id, generation_id)| {
                assert_eq!(generation_owners.get(generation_id), Some(project_id));
                by_project
                    .entry(project_id.clone())
                    .or_default()
                    .insert(generation_id.clone());
                by_project
            },
        );
        let generations_by_project = generation_owners.iter().fold(
            BTreeMap::<ProjectId, Vec<Sha256ValueV1>>::new(),
            |mut by_project, (generation_id, project_id)| {
                by_project
                    .entry(project_id.clone())
                    .or_default()
                    .push(generation_id.clone());
                by_project
            },
        );
        let mut bound_scopes = BTreeMap::new();
        let mut winner_projects = BTreeMap::new();
        for (index, project_id) in bindings_by_project.keys().enumerate() {
            bound_scopes.insert(
                project_id.clone(),
                PublishedScope::try_new("authority-fixture", format!("collision-{index}")).unwrap(),
            );
            winner_projects.insert(
                project_id.clone(),
                ProjectId::parse(format!("authority-winner-{index}")).unwrap(),
            );
        }

        let mut generation_observations = BTreeMap::new();
        for (project_index, (project_id, generation_ids)) in
            generations_by_project.iter().enumerate()
        {
            if !inventory
                .legacy_projects
                .iter()
                .any(|row| row.record.project_id == project_id.as_str())
            {
                let mut project = legacy_project(
                    &format!("authority_legacy_{project_index}"),
                    project_id.as_str(),
                    &format!("/authority/{project_index}"),
                    &format!("authority_probe_{project_index}"),
                );
                project.committed_scope = bound_scopes.get(project_id).cloned();
                inventory.legacy_projects.push(project);
            }
            let generations = generation_ids
                .iter()
                .enumerate()
                .map(|(generation_index, generation_id)| {
                    let observation_id =
                        format!("authority_generation_{project_index}_{generation_index}");
                    generation_observations.insert(
                        (project_id.clone(), generation_id.clone()),
                        observation_id.clone(),
                    );
                    let published_scope =
                        if bindings.contains(&(project_id.clone(), generation_id.clone())) {
                            bound_scopes[project_id].clone()
                        } else {
                            PublishedScope::try_new(
                                "authority-fixture",
                                format!("retained-{project_index}-{generation_index}"),
                            )
                            .unwrap()
                        };
                    CollectedGenerationObservationV1 {
                        observation_id,
                        project_id: project_id.clone(),
                        role: CollectedGenerationRoleV1::Retained,
                        generation_id: generation_id.to_string(),
                        activation_scope: None,
                        descriptor: ImmutableCollectedDescriptorV1::Valid {
                            descriptor_hash: hash(&format!(
                                "authority_descriptor_{project_index}_{generation_index}"
                            )),
                            published_scope,
                        },
                        manifest: ImmutableArtifactObservationV1::Valid {
                            content_hash: hash(&format!(
                                "authority_manifest_{project_index}_{generation_index}"
                            )),
                        },
                        selector_evidence: DurableSelectorEvidenceV1::NoDurableSelector,
                        checkout_missing: false,
                        planned_metadata_v2_hash: hash(&format!(
                            "authority_metadata_{project_index}_{generation_index}"
                        )),
                    }
                })
                .collect::<Vec<_>>();
            inventory.code_sources.push(CodeSourceObservationV1 {
                observation_id: format!("authority_source_{project_index}"),
                project_id: project_id.clone(),
                generations,
                quarantine: Vec::new(),
                effective_manifest_hash: hash(&format!("authority_effective_{project_index}")),
                planned_activation_v2_hash: None,
            });
        }
        for (index, (loser, winner)) in winner_projects.iter().enumerate() {
            let mut project = legacy_project(
                &format!("authority_winner_legacy_{index}"),
                winner.as_str(),
                &format!("/authority/winner/{index}"),
                &format!("authority_winner_probe_{index}"),
            );
            project.committed_scope = Some(bound_scopes[loser].clone());
            inventory.legacy_projects.push(project);
        }

        let evidence = |source_id: String,
                        source_kind: MutableInventorySourceKindV1,
                        source_locator: MutableInventorySourceLocatorV1,
                        row_observation_ids: BTreeSet<String>,
                        content_hash: Sha256ValueV1| {
            MutableInventorySourceEvidenceV1 {
                row_set_sha256: mutable_source_row_set_hash(&row_observation_ids),
                source_id,
                source_kind,
                source_locator,
                state: InventorySourceStateV1::Present {
                    fingerprint: hash("authority_source_fingerprint"),
                    content_hash,
                    byte_len: 1,
                },
                row_observation_ids,
            }
        };
        let legacy_rows = inventory
            .legacy_projects
            .iter()
            .map(|row| row.observation_id.clone())
            .collect::<BTreeSet<_>>();
        let publisher_rows = inventory
            .publisher_pins
            .iter()
            .map(|row| row.observation_id.clone())
            .collect::<BTreeSet<_>>();
        let code_source_rows = inventory
            .code_sources
            .iter()
            .map(|row| row.observation_id.clone())
            .collect::<BTreeSet<_>>();
        let generation_rows = inventory
            .code_sources
            .iter()
            .flat_map(|source| {
                source
                    .generations
                    .iter()
                    .map(|generation| generation.observation_id.clone())
            })
            .collect::<BTreeSet<_>>();
        let mut mutable_source_evidence = vec![
            evidence(
                "authority_legacy_store".to_string(),
                MutableInventorySourceKindV1::LegacyProjectStore,
                MutableInventorySourceLocatorV1::LegacyProjectStore,
                legacy_rows,
                inventory.source_store_hash.clone(),
            ),
            evidence(
                "authority_publisher_store".to_string(),
                MutableInventorySourceKindV1::PublisherRefStore,
                MutableInventorySourceLocatorV1::PublisherRefStore,
                publisher_rows,
                inventory.publisher_ref_source_hash.clone(),
            ),
            evidence(
                "authority_effective_manifest".to_string(),
                MutableInventorySourceKindV1::EffectiveSourceManifest,
                MutableInventorySourceLocatorV1::CodeSourceAnchor,
                code_source_rows
                    .iter()
                    .cloned()
                    .chain(generation_rows.iter().cloned())
                    .collect(),
                hash("authority_effective_manifest"),
            ),
        ];
        for (index, source) in inventory.code_sources.iter().enumerate() {
            mutable_source_evidence.push(evidence(
                format!("authority_activation_{index}"),
                MutableInventorySourceKindV1::CodeSourceActivation,
                MutableInventorySourceLocatorV1::CodeSourceActivation {
                    project_id: source.project_id.clone(),
                },
                BTreeSet::from([source.observation_id.clone()]),
                hash(&format!("authority_activation_{index}")),
            ));
            for (generation_index, generation) in source.generations.iter().enumerate() {
                let ImmutableCollectedDescriptorV1::Valid {
                    published_scope, ..
                } = &generation.descriptor
                else {
                    unreachable!();
                };
                let scope_hash =
                    Sha256ValueV1::parse(bbox_code_source::scope_hash(published_scope)).unwrap();
                for (kind, suffix) in [
                    (
                        MutableInventorySourceKindV1::CodeSourceGenerationMetadata,
                        "metadata",
                    ),
                    (
                        MutableInventorySourceKindV1::CodeSourceGenerationManifest,
                        "manifest",
                    ),
                ] {
                    let locator = match kind {
                        MutableInventorySourceKindV1::CodeSourceGenerationMetadata => {
                            MutableInventorySourceLocatorV1::CodeSourceGenerationMetadata {
                                scope_hash: scope_hash.clone(),
                                generation_id: generation.generation_id.clone(),
                            }
                        }
                        MutableInventorySourceKindV1::CodeSourceGenerationManifest => {
                            MutableInventorySourceLocatorV1::CodeSourceGenerationManifest {
                                scope_hash: scope_hash.clone(),
                                generation_id: generation.generation_id.clone(),
                            }
                        }
                        _ => unreachable!(),
                    };
                    mutable_source_evidence.push(evidence(
                        format!("authority_{suffix}_{index}_{generation_index}"),
                        kind,
                        locator,
                        BTreeSet::from([generation.observation_id.clone()]),
                        hash(&format!("authority_{suffix}_{index}_{generation_index}")),
                    ));
                }
            }
        }
        for (index, project) in inventory.legacy_projects.iter().enumerate() {
            let authority = project.committed_authority.as_ref().unwrap();
            mutable_source_evidence.push(evidence(
                format!("authority_probe_source_{index}"),
                MutableInventorySourceKindV1::CommittedAuthorityProbe,
                MutableInventorySourceLocatorV1::CommittedProjectConfig {
                    project_id: ProjectId::parse(project.record.project_id.clone()).unwrap(),
                    commit_oid: format!("{:040x}", index + 1),
                    repo_relative_path: format!("project-{index}/.bbox/config.toml"),
                },
                BTreeSet::from([authority.observation_id.clone()]),
                hash(&format!("authority_probe_source_{index}")),
            ));
        }
        for (index, checkout) in inventory.checkouts.iter().enumerate() {
            for (kind, suffix) in [
                (MutableInventorySourceKindV1::CheckoutRoot, "root"),
                (MutableInventorySourceKindV1::CheckoutMarker, "marker"),
            ] {
                let locator = match kind {
                    MutableInventorySourceKindV1::CheckoutRoot => {
                        MutableInventorySourceLocatorV1::CheckoutRoot {
                            canonical_root_digest: checkout.canonical_root_digest.clone(),
                        }
                    }
                    MutableInventorySourceKindV1::CheckoutMarker => {
                        MutableInventorySourceLocatorV1::CheckoutMarker {
                            canonical_root_digest: checkout.canonical_root_digest.clone(),
                        }
                    }
                    _ => unreachable!(),
                };
                mutable_source_evidence.push(evidence(
                    format!("authority_checkout_{suffix}_{index}"),
                    kind,
                    locator,
                    BTreeSet::from([checkout.observation_id.clone()]),
                    hash(&format!("authority_checkout_{suffix}_{index}")),
                ));
            }
        }
        inventory.mutable_source_evidence = mutable_source_evidence;
        inventory.validate().unwrap();

        let mut resolution =
            ProjectCatalogMigrationResolutionV1::empty(inventory.inventory_hash().unwrap());
        let mut post_image = fixture_post_image(&inventory);
        post_image.transaction_id = transaction_id;
        for project_id in generations_by_project.keys() {
            post_image
                .resolved_project_scopes
                .push(ResolvedProjectScopeInputV1 {
                    project_id: project_id.clone(),
                    published_scope: None,
                    created_at: "2026-01-02T03:04:05Z".to_string(),
                });
            if let Some(winner) = winner_projects.get(project_id) {
                post_image
                    .resolved_project_scopes
                    .push(ResolvedProjectScopeInputV1 {
                        project_id: winner.clone(),
                        published_scope: Some(bound_scopes[project_id].clone()),
                        created_at: "2026-01-02T03:04:05Z".to_string(),
                    });
                let candidates = BTreeSet::from([winner.clone(), project_id.clone()]);
                resolution.selected_scope_owners.push(SelectedScopeOwnerV1 {
                    resolution_id: canonical_scope_resolution_id(
                        &bound_scopes[project_id],
                        &candidates,
                    )
                    .unwrap(),
                    scope: bound_scopes[project_id].clone(),
                    owner_project_id: winner.clone(),
                    losing_project_ids: BTreeSet::from([project_id.clone()]),
                    owned_aliases: BTreeSet::new(),
                });
            }
        }
        let existing_history_identities = post_image
            .repo_history_groups
            .iter()
            .map(|group| {
                (
                    group.group_id.clone(),
                    PlannedRepoHistoryIdentityV1 {
                        planned_history_id: group.planned_history_id.clone(),
                        planned_primary_namespace: group.planned_primary_namespace.clone(),
                        planned_compatibility_namespaces: group
                            .planned_compatibility_namespaces
                            .clone(),
                    },
                )
            })
            .collect::<BTreeMap<_, _>>();
        let history_group_ids =
            deterministic_repo_history_group_ids(&inventory, &post_image.resolved_project_scopes)
                .unwrap();
        let planned_history_identities = history_group_ids
            .into_iter()
            .enumerate()
            .map(|(index, group_id)| {
                let identity = existing_history_identities
                    .get(&group_id)
                    .cloned()
                    .unwrap_or_else(|| PlannedRepoHistoryIdentityV1 {
                        planned_history_id: RepoHistoryId::parse(format!("rh_{:032x}", index + 16))
                            .unwrap(),
                        planned_primary_namespace: CommitNamespace::parse(format!(
                            "authority_namespace_{index}"
                        ))
                        .unwrap(),
                        planned_compatibility_namespaces: BTreeSet::new(),
                    });
                (group_id, identity)
            })
            .collect::<BTreeMap<_, _>>();
        post_image.repo_history_groups = build_deterministic_repo_history_groups(
            &inventory,
            &post_image.resolved_project_scopes,
            &planned_history_identities,
        )
        .unwrap();
        for (index, (project_id, generation_id)) in bindings.iter().enumerate() {
            resolution.quarantine_collected.push(QuarantineCollectedV1 {
                resolution_id: format!("authority_quarantine_{index}"),
                project_id: project_id.clone(),
                generation_id: generation_id.to_string(),
            });
            post_image
                .quarantined_collected
                .push(QuarantinePostImageInputV1 {
                    project_id: project_id.clone(),
                    generation_id: generation_id.to_string(),
                });
        }
        let mut report = fixture_report(&inventory, &resolution, &post_image);
        for row in &resolution.selected_scope_owners {
            let candidate_record_ids = BTreeSet::from([
                row.owner_project_id.to_string(),
                row.losing_project_ids.iter().next().unwrap().to_string(),
            ]);
            report.scope_conflicts.push(ConflictReportV1 {
                conflict_id: row.resolution_id.clone(),
                affected_record_ids: candidate_record_ids.clone(),
                diagnostic_code: "duplicate_published_scope".to_string(),
            });
            report.required_resolutions.push(RequiredResolutionV1 {
                resolution_id: row.resolution_id.clone(),
                kind: RequiredResolutionKindV1::ScopeOwner,
                candidate_record_ids,
            });
        }
        for row in &resolution.quarantine_collected {
            let candidate_record_ids = BTreeSet::from([generation_observations[&(
                row.project_id.clone(),
                Sha256ValueV1::parse(row.generation_id.clone()).unwrap(),
            )]
                .clone()]);
            report.activation_conflicts.push(ConflictReportV1 {
                conflict_id: row.resolution_id.clone(),
                affected_record_ids: candidate_record_ids.clone(),
                diagnostic_code: "authority_generation_quarantine".to_string(),
            });
            report.required_resolutions.push(RequiredResolutionV1 {
                resolution_id: row.resolution_id.clone(),
                kind: RequiredResolutionKindV1::QuarantineCollected,
                candidate_record_ids,
            });
        }
        if !report.required_resolutions.is_empty() {
            report.status = ProjectCatalogMigrationStatusV1::ResolutionRequired;
        }
        let authority =
            validated_quarantine_bindings(&inventory, &report, &resolution, &post_image).unwrap();
        assert_eq!(authority.generation_owners(), &generation_owners);
        assert_eq!(authority.bindings(), &bindings);
        authority
    }

    #[test]
    fn inventory_hash_is_independent_of_adapter_enumeration_order() {
        let inventory = fixture_inventory();
        let expected = inventory.inventory_hash().unwrap();
        let mut permuted = inventory.clone();
        permuted.legacy_projects.reverse();
        permuted.project_scoped_refs.reverse();
        permuted.git_metadata.reverse();
        permuted.attachment_candidates.reverse();
        if let RepoGroupingProofV1::IdenticalCommittedRecordedAuthority { members, .. } =
            &mut permuted.repo_grouping_proofs[0]
        {
            members.reverse();
        }
        assert_eq!(permuted.inventory_hash().unwrap(), expected);
    }

    #[test]
    fn grouping_uses_only_cross_checked_strong_evidence() {
        let inventory = fixture_inventory();
        let post_image = fixture_post_image(&inventory);
        let groups = post_image.repo_history_groups;
        assert_eq!(groups.len(), 1);
        assert_eq!(
            groups[0].project_ids,
            BTreeSet::from([project_id("alpha"), project_id("beta")])
        );
        assert_eq!(
            groups[0].evidence_classes,
            BTreeSet::from([RepoGroupingEvidenceClassV1::IdenticalCommittedRecordedAuthority])
        );

        let mut forged = inventory;
        let RepoGroupingProofV1::IdenticalCommittedRecordedAuthority { members, .. } =
            &mut forged.repo_grouping_proofs[0]
        else {
            unreachable!();
        };
        members[1].authority = RecordedRepoAuthority::parse("different_repo".to_string()).unwrap();
        assert_eq!(
            forged.validate().unwrap_err().code(),
            "error.project_catalog_inventory_invalid"
        );
    }

    #[test]
    fn missing_active_descriptor_is_a_non_overridable_refusal() {
        let mut inventory = fixture_inventory();
        let source = &mut inventory.code_sources[0];
        source.generations[0].role = CollectedGenerationRoleV1::Retained;
        source.generations[0].activation_scope = None;
        source.generations[0].selector_evidence = DurableSelectorEvidenceV1::NoDurableSelector;
        source.generations[0].descriptor = ImmutableCollectedDescriptorV1::Missing;
        source.planned_activation_v2_hash = None;
        inventory.validate().unwrap();
        assert_eq!(
            inventory.hard_refusals(),
            vec![InventoryRefusalV1 {
                record_id: "generation_alpha".to_string(),
                diagnostic_code: "active_or_retained_descriptor_missing".to_string(),
            }]
        );
    }

    #[test]
    fn retained_generation_cannot_carry_fabricated_selector_authority() {
        let mut inventory = fixture_inventory();
        let source = &mut inventory.code_sources[0];
        source.generations[0].role = CollectedGenerationRoleV1::Retained;
        source.generations[0].activation_scope = None;
        source.planned_activation_v2_hash = None;
        assert_eq!(
            inventory.validate().unwrap_err().code(),
            "error.project_catalog_inventory_invalid"
        );

        inventory.code_sources[0].generations[0].selector_evidence =
            DurableSelectorEvidenceV1::NoDurableSelector;
        inventory.validate().unwrap();
    }

    #[test]
    fn publisher_sources_reject_cross_project_and_unknown_splices() {
        let mut cross_project = fixture_inventory();
        cross_project.attachment_candidates[0].project_id = project_id("beta");
        assert_eq!(
            cross_project.validate().unwrap_err().code(),
            "error.project_catalog_inventory_invalid"
        );

        let mut coherent_splice = fixture_inventory();
        let checkout_beta_digest = digest_path("/workspace/beta");
        coherent_splice.checkouts.push(CheckoutObservationV1 {
            observation_id: "checkout_beta".to_string(),
            canonical_root_digest: checkout_beta_digest.clone(),
            marker_state: CheckoutMarkerStateV1::MissingOrEmpty,
        });
        let checkout_lane = coherent_splice
            .immutable_lane_evidence
            .iter_mut()
            .find(|lane| lane.lane_kind == ImmutableInventoryLaneKindV1::Checkouts)
            .unwrap();
        checkout_lane.row_count += 1;
        if let InventorySourceStateV1::Present { byte_len, .. } = &mut checkout_lane.source_state {
            *byte_len += 1;
        }
        for (source_id, source_kind, locator) in [
            (
                "source_checkout_root_beta",
                MutableInventorySourceKindV1::CheckoutRoot,
                MutableInventorySourceLocatorV1::CheckoutRoot {
                    canonical_root_digest: checkout_beta_digest.clone(),
                },
            ),
            (
                "source_checkout_marker_beta",
                MutableInventorySourceKindV1::CheckoutMarker,
                MutableInventorySourceLocatorV1::CheckoutMarker {
                    canonical_root_digest: checkout_beta_digest.clone(),
                },
            ),
        ] {
            let row_observation_ids = BTreeSet::from(["checkout_beta".to_string()]);
            coherent_splice
                .mutable_source_evidence
                .push(MutableInventorySourceEvidenceV1 {
                    source_id: source_id.to_string(),
                    source_kind,
                    source_locator: locator,
                    state: InventorySourceStateV1::Present {
                        fingerprint: hash(&format!("{source_id}_fingerprint")),
                        content_hash: hash(&format!("{source_id}_content")),
                        byte_len: 1,
                    },
                    row_set_sha256: mutable_source_row_set_hash(&row_observation_ids),
                    row_observation_ids,
                });
        }
        let beta_attachment_id = {
            let beta_attachment = coherent_splice
                .attachment_candidates
                .iter_mut()
                .find(|attachment| attachment.project_id == project_id("beta"))
                .unwrap();
            beta_attachment.checkout_observation_id = "checkout_beta".to_string();
            beta_attachment.attachment_id.clone()
        };
        let beta_scope = scope("services/beta");
        let beta_commit = "d".repeat(40);
        let pin_full_ref = {
            let pin = &mut coherent_splice.publisher_pins[0];
            pin.project_id = project_id("beta");
            pin.expected_scope = beta_scope.clone();
            pin.candidate_attachment_ids = BTreeSet::from([beta_attachment_id]);
            pin.resolved_commit = Some(beta_commit);
            pin.resolved_scope = Some(beta_scope.clone());
            pin.source_observation_ids = BTreeSet::from([
                pin.observation_id.clone(),
                "legacy_beta".to_string(),
                "authority_beta".to_string(),
                "attachment_beta".to_string(),
                "checkout_beta".to_string(),
                "git_beta".to_string(),
            ]);
            pin.full_ref.clone()
        };
        coherent_splice.publisher_ref_source_bytes = serde_json::to_vec(&serde_json::json!({
            "version": 1,
            "refs": [{
                "scope": {
                    "repo_id": beta_scope.repo_id(),
                    "bbox_root_relpath": beta_scope.bbox_root_relpath()
                },
                "branch_ref": pin_full_ref
            }]
        }))
        .unwrap();
        coherent_splice.publisher_ref_source_hash =
            Sha256ValueV1::digest(&coherent_splice.publisher_ref_source_bytes);
        let publisher_source = coherent_splice
            .mutable_source_evidence
            .iter_mut()
            .find(|source| source.source_kind == MutableInventorySourceKindV1::PublisherRefStore)
            .unwrap();
        if let InventorySourceStateV1::Present { content_hash, .. } = &mut publisher_source.state {
            *content_hash = coherent_splice.publisher_ref_source_hash.clone();
        }
        assert_eq!(
            coherent_splice.validate().unwrap_err().code(),
            "error.project_catalog_inventory_invalid"
        );

        let mut unknown = fixture_inventory();
        unknown.publisher_pins[0]
            .source_observation_ids
            .insert("unknown_observation".to_string());
        assert_eq!(
            unknown.validate().unwrap_err().code(),
            "error.project_catalog_inventory_invalid"
        );

        let mut rewritten_pin = fixture_inventory();
        rewritten_pin.publisher_pins[0].full_ref = "refs/heads/other".to_string();
        assert_eq!(
            rewritten_pin.validate().unwrap_err().code(),
            "error.project_catalog_inventory_invalid"
        );
    }

    #[test]
    fn mutable_source_rows_reject_omission_and_substitution() {
        let mut omitted = fixture_inventory();
        let metadata = omitted
            .mutable_source_evidence
            .iter_mut()
            .find(|row| {
                row.source_kind == MutableInventorySourceKindV1::CodeSourceGenerationMetadata
            })
            .unwrap();
        metadata.row_observation_ids.clear();
        metadata.row_set_sha256 = mutable_source_row_set_hash(&metadata.row_observation_ids);
        assert_eq!(
            omitted.validate().unwrap_err().code(),
            "error.project_catalog_inventory_invalid"
        );

        let mut substituted = fixture_inventory();
        let metadata = substituted
            .mutable_source_evidence
            .iter_mut()
            .find(|row| {
                row.source_kind == MutableInventorySourceKindV1::CodeSourceGenerationMetadata
            })
            .unwrap();
        metadata.row_observation_ids = BTreeSet::from(["legacy_alpha".to_string()]);
        metadata.row_set_sha256 = mutable_source_row_set_hash(&metadata.row_observation_ids);
        assert_eq!(
            substituted.validate().unwrap_err().code(),
            "error.project_catalog_inventory_invalid"
        );
    }

    #[test]
    fn corrupt_quarantine_manifest_is_a_non_overridable_refusal() {
        let mut inventory = fixture_inventory();
        let active = inventory.code_sources[0].generations.remove(0);
        inventory.code_sources[0].planned_activation_v2_hash = None;
        inventory.code_sources[0]
            .quarantine
            .push(QuarantinedGenerationObservationV1 {
                observation_id: active.observation_id,
                project_id: active.project_id.clone(),
                generation_id: active.generation_id.clone(),
                descriptor: active.descriptor,
                manifest: ImmutableArtifactObservationV1::Corrupt {
                    diagnostic_code: "manifest_invalid".to_string(),
                },
                manifest_hash: hash("manifest"),
                planned_metadata_v2_hash: active.planned_metadata_v2_hash,
                collision_lifecycle: CollisionLifecycleObservationV1 {
                    version: 1,
                    state: CollisionLifecycleStateObservationV1::Pending,
                    project_id: active.project_id,
                    former_scope: scope("services/alpha"),
                    generation_id: active.generation_id,
                    selector_evidence: DurableSelectorEvidenceV1::ExactMaterialized {
                        selector_hash: hash("selector"),
                    },
                    snapshot_id: format!("collected-{}", "e".repeat(32)),
                    manifest_sha256: hash("manifest"),
                    inventory_hash: hash("inventory"),
                    plan_hash: hash("plan"),
                },
            });
        assert!(inventory.hard_refusals().iter().any(|refusal| {
            refusal.record_id == "generation_alpha"
                && refusal.diagnostic_code == "quarantined_manifest_corrupt"
        }));
    }

    #[test]
    fn canonical_inventory_contains_only_path_digests() {
        let inventory = fixture_inventory();
        let encoded = String::from_utf8(inventory.canonical_json().unwrap()).unwrap();
        for forbidden in [
            "/workspace/acme",
            "Example.java",
            "src/Example.java",
            "\"canonical_path\":\"",
            "literal_selector",
        ] {
            assert!(
                !encoded.contains(forbidden),
                "canonical inventory leaked {forbidden}"
            );
        }
        assert!(encoded.contains(digest_path("/workspace/acme").as_str()));
    }

    #[test]
    fn default_report_contains_no_literal_checkout_or_legacy_path() {
        let inventory = fixture_inventory();
        let resolution =
            ProjectCatalogMigrationResolutionV1::empty(inventory.inventory_hash().unwrap());
        let post_image = fixture_post_image(&inventory);
        let report = fixture_report(&inventory, &resolution, &post_image);
        report.validate_against_inventory(&inventory).unwrap();
        let encoded =
            String::from_utf8(encode_migration_report_v1(&report, &inventory).unwrap()).unwrap();
        for forbidden in [
            "/workspace/acme",
            "Example.java",
            "src/Example.java",
            "canonical_checkout_root",
            "literal_selector",
        ] {
            assert!(
                !encoded.contains(forbidden),
                "default report leaked {forbidden}"
            );
        }
        assert!(encoded.contains(digest_path("/workspace/acme").as_str()));
    }

    #[test]
    fn local_path_report_requires_explicit_sensitive_shape() {
        let inventory = fixture_inventory();
        let report = SensitiveLocalPathReportV1::from_runtime_rows(
            &inventory,
            vec![SensitiveLocalPathRowV1 {
                observation_id: "legacy_path_alpha".to_string(),
                store_kind: LegacyPathStoreKindV1::Knowledge,
                stable_row_id: "knowledge_1".to_string(),
                literal_selector: "/workspace/acme/alpha/src/Example.java".to_string(),
            }],
        )
        .unwrap();
        report.validate().unwrap();
        let encoded = serde_json::to_string(&report).unwrap();
        assert!(encoded.contains("\"local_paths_included\":true"));
        assert!(encoded.contains("/workspace/acme/alpha/src/Example.java"));

        let mut unmarked = report;
        unmarked.local_paths_included = false;
        assert!(unmarked.validate().is_err());
    }

    #[test]
    fn strict_decoders_reject_unknown_fields_at_every_owned_boundary() {
        let inventory = fixture_inventory();
        let mut value = serde_json::to_value(&inventory).unwrap();
        value
            .as_object_mut()
            .unwrap()
            .insert("future_field".to_string(), serde_json::json!(true));
        assert!(decode_inventory_v1(&serde_json::to_vec(&value).unwrap()).is_err());

        let mut value = serde_json::to_value(&inventory).unwrap();
        value["legacy_projects"][0]["record"]
            .as_object_mut()
            .unwrap()
            .insert("future_field".to_string(), serde_json::json!(true));
        assert!(decode_inventory_v1(&serde_json::to_vec(&value).unwrap()).is_err());

        let resolution =
            ProjectCatalogMigrationResolutionV1::empty(inventory.inventory_hash().unwrap());
        let mut value = serde_json::to_value(&resolution).unwrap();
        value
            .as_object_mut()
            .unwrap()
            .insert("future_field".to_string(), serde_json::json!(true));
        assert!(decode_migration_resolution_v1(&serde_json::to_vec(&value).unwrap()).is_err());

        let mut resolution =
            ProjectCatalogMigrationResolutionV1::empty(inventory.inventory_hash().unwrap());
        resolution.publisher_binding_dispositions = vec![seed_disposition()];
        let mut value = serde_json::to_value(&resolution).unwrap();
        value["publisher_binding_dispositions"][0]
            .as_object_mut()
            .unwrap()
            .insert("future_field".to_string(), serde_json::json!(true));
        assert!(decode_migration_resolution_v1(&serde_json::to_vec(&value).unwrap()).is_err());
    }

    #[test]
    fn supported_plan_binds_inventory_resolution_and_all_predictions() {
        let inventory = fixture_inventory();
        let resolution =
            ProjectCatalogMigrationResolutionV1::empty(inventory.inventory_hash().unwrap());
        let post_image = fixture_post_image(&inventory);
        let report = fixture_report(&inventory, &resolution, &post_image);
        validate_supported_resolution(&inventory, &report, &resolution, &post_image).unwrap();

        let mut changed = post_image.clone();
        changed.checkout_identity_actions[0].planned_checkout_id = "f".repeat(32);
        changed.attachments[0].checkout_id = "f".repeat(32);
        changed.attachments[1].checkout_id = "f".repeat(32);
        assert_ne!(
            canonical_plan_hash(&inventory, &resolution, &changed).unwrap(),
            report.plan_hash
        );

        let mut changed = post_image;
        changed.predicted_hashes.catalog_hash = hash("different_catalog");
        assert_eq!(
            canonical_plan_hash(&inventory, &resolution, &changed).unwrap(),
            report.plan_hash
        );
    }

    #[test]
    fn quarantine_bindings_cannot_be_laundered_with_coordinated_post_image_and_hash_changes() {
        let inventory = fixture_inventory();
        let clean_resolution =
            ProjectCatalogMigrationResolutionV1::empty(inventory.inventory_hash().unwrap());
        let post_image = fixture_post_image(&inventory);
        let report = fixture_report(&inventory, &clean_resolution, &post_image);
        let authority =
            validated_quarantine_bindings(&inventory, &report, &clean_resolution, &post_image)
                .unwrap();
        assert!(authority.bindings().is_empty());

        let scope_resolution_id = "resolve_scope_alpha".to_string();
        let quarantine_resolution_id = "quarantine_generation_alpha".to_string();
        let mut resolution =
            ProjectCatalogMigrationResolutionV1::empty(inventory.inventory_hash().unwrap());
        resolution.selected_scope_owners.push(SelectedScopeOwnerV1 {
            resolution_id: scope_resolution_id.clone(),
            scope: scope("services/alpha"),
            owner_project_id: project_id("alpha"),
            losing_project_ids: BTreeSet::from([project_id("beta")]),
            owned_aliases: BTreeSet::new(),
        });
        resolution.quarantine_collected.push(QuarantineCollectedV1 {
            resolution_id: quarantine_resolution_id.clone(),
            project_id: project_id("alpha"),
            generation_id: "e".repeat(64),
        });
        let mut laundered_post_image = post_image;
        laundered_post_image
            .quarantined_collected
            .push(QuarantinePostImageInputV1 {
                project_id: project_id("alpha"),
                generation_id: "e".repeat(64),
            });
        let mut laundered_report = fixture_report(&inventory, &resolution, &laundered_post_image);
        laundered_report.status = ProjectCatalogMigrationStatusV1::ResolutionRequired;
        laundered_report.scope_conflicts = vec![ConflictReportV1 {
            conflict_id: scope_resolution_id.clone(),
            affected_record_ids: BTreeSet::from([
                project_id("alpha").to_string(),
                project_id("beta").to_string(),
            ]),
            diagnostic_code: "collected_scope_owner_conflict".to_string(),
        }];
        laundered_report.activation_conflicts = vec![ConflictReportV1 {
            conflict_id: quarantine_resolution_id.clone(),
            affected_record_ids: BTreeSet::from(["generation_alpha".to_string()]),
            diagnostic_code: "collected_generation_quarantine".to_string(),
        }];
        laundered_report.required_resolutions = vec![
            RequiredResolutionV1 {
                resolution_id: scope_resolution_id,
                kind: RequiredResolutionKindV1::ScopeOwner,
                candidate_record_ids: BTreeSet::from([
                    project_id("alpha").to_string(),
                    project_id("beta").to_string(),
                ]),
            },
            RequiredResolutionV1 {
                resolution_id: quarantine_resolution_id,
                kind: RequiredResolutionKindV1::QuarantineCollected,
                candidate_record_ids: BTreeSet::from(["generation_alpha".to_string()]),
            },
        ];
        assert_eq!(
            validated_quarantine_bindings(
                &inventory,
                &laundered_report,
                &resolution,
                &laundered_post_image,
            )
            .unwrap_err()
            .code(),
            "error.project_catalog_inventory_incomplete_scope_conflicts"
        );
    }

    #[test]
    fn duplicate_scope_conflict_cannot_be_suppressed_with_coordinated_post_image_and_hash() {
        let mut inventory = fixture_inventory();
        inventory
            .legacy_projects
            .iter_mut()
            .find(|project| project.record.project_id == "beta")
            .unwrap()
            .committed_scope = Some(scope("services/alpha"));
        inventory.validate().unwrap();
        let resolution =
            ProjectCatalogMigrationResolutionV1::empty(inventory.inventory_hash().unwrap());
        let mut post_image = fixture_post_image(&inventory);
        post_image
            .resolved_project_scopes
            .iter_mut()
            .find(|project| project.project_id == project_id("beta"))
            .unwrap()
            .published_scope = None;
        let report = fixture_report(&inventory, &resolution, &post_image);

        assert_eq!(
            validate_supported_resolution(&inventory, &report, &resolution, &post_image)
                .unwrap_err()
                .code(),
            "error.project_catalog_inventory_incomplete_scope_conflicts"
        );
    }

    #[test]
    fn separate_preflight_and_apply_preserve_all_planned_identities() {
        let inventory = fixture_inventory();
        let resolution =
            ProjectCatalogMigrationResolutionV1::empty(inventory.inventory_hash().unwrap());
        let post_image = fixture_post_image(&inventory);
        let report = fixture_report(&inventory, &resolution, &post_image);
        let persisted = serde_json::to_vec(&post_image).unwrap();
        let reopened: DeterministicPostImageInputV1 = serde_json::from_slice(&persisted).unwrap();

        assert_eq!(reopened.transaction_id, report.transaction_id);
        assert_eq!(reopened.repo_history_groups, report.repo_history_groups);
        assert_eq!(
            reopened.legacy_path_bindings[0].planned_binding_id,
            report.legacy_path_bindings[0].planned_binding_id
        );
        assert_eq!(
            canonical_plan_hash(&inventory, &resolution, &reopened).unwrap(),
            report.plan_hash
        );
        validate_supported_resolution(&inventory, &report, &resolution, &reopened).unwrap();
    }

    #[test]
    fn report_cannot_substitute_any_planned_identity() {
        let inventory = fixture_inventory();
        let resolution =
            ProjectCatalogMigrationResolutionV1::empty(inventory.inventory_hash().unwrap());
        let post_image = fixture_post_image(&inventory);

        let mut report = fixture_report(&inventory, &resolution, &post_image);
        report.transaction_id = transaction_id('9');
        assert_eq!(
            validate_supported_resolution(&inventory, &report, &resolution, &post_image)
                .unwrap_err()
                .code(),
            "error.project_catalog_inventory_transaction_id_mismatch"
        );

        let mut report = fixture_report(&inventory, &resolution, &post_image);
        report.repo_history_groups[0].planned_history_id = history_id('9');
        assert_eq!(
            validate_supported_resolution(&inventory, &report, &resolution, &post_image)
                .unwrap_err()
                .code(),
            "error.project_catalog_inventory_history_plan_mismatch"
        );

        let mut report = fixture_report(&inventory, &resolution, &post_image);
        report.legacy_path_bindings[0].planned_binding_id = binding_id('9');
        assert_eq!(
            validate_supported_resolution(&inventory, &report, &resolution, &post_image)
                .unwrap_err()
                .code(),
            "error.project_catalog_inventory_legacy_path_plan_mismatch"
        );
    }

    #[test]
    fn predicted_timestamps_must_preserve_registered_at() {
        let inventory = fixture_inventory();
        let mut post_image = fixture_post_image(&inventory);
        post_image.resolved_project_scopes[0].created_at = "2026-01-02T03:04:06Z".to_string();
        assert_eq!(
            post_image.validate(&inventory).unwrap_err().code(),
            "error.project_catalog_inventory_migration_timestamp_mismatch"
        );

        let mut post_image = fixture_post_image(&inventory);
        post_image.attachments[0].attached_at = "2026-01-02T03:04:06Z".to_string();
        assert_eq!(
            post_image.validate(&inventory).unwrap_err().code(),
            "error.project_catalog_inventory_migration_timestamp_mismatch"
        );
    }

    #[test]
    fn explicit_no_content_disposition_satisfies_one_ambiguous_publisher_pin() {
        let inventory = fixture_inventory();
        let inventory_hash = inventory.inventory_hash().unwrap();
        let no_content = PublisherBindingDispositionV1::NoPublishedContentAcknowledged {
            project_id: project_id("alpha"),
            expected_scope: scope("services/alpha"),
            full_ref: "refs/heads/main".to_string(),
            bounded_reason: "no_published_rows_observed".to_string(),
        };
        let mut resolution = ProjectCatalogMigrationResolutionV1::empty(inventory_hash);
        resolution.publisher_binding_dispositions = vec![no_content.clone()];
        let mut post_image = fixture_post_image(&inventory);
        post_image.publisher_binding_dispositions = vec![no_content];
        post_image.predicted_hashes.g1_assets.clear();
        post_image.predicted_hashes.accepted_pointer_hashes.clear();
        post_image.predicted_hashes.participant_hashes.clear();
        let mut report = fixture_report(&inventory, &resolution, &post_image);
        report.status = ProjectCatalogMigrationStatusV1::ResolutionRequired;
        report.publisher_bindings[0].status = PublisherBindingReportStatusV1::ResolutionRequired;
        report.required_resolutions = vec![RequiredResolutionV1 {
            resolution_id: "resolve_pin_alpha".to_string(),
            kind: RequiredResolutionKindV1::PublisherBindingDisposition,
            candidate_record_ids: BTreeSet::from(["pin_alpha".to_string()]),
        }];
        validate_supported_resolution(&inventory, &report, &resolution, &post_image).unwrap();
    }

    #[test]
    fn stale_resolution_and_invented_publisher_fields_fail_closed() {
        let inventory = fixture_inventory();
        let mut resolution =
            ProjectCatalogMigrationResolutionV1::empty(inventory.inventory_hash().unwrap());
        resolution.inventory_hash = hash("stale");
        let post_image = fixture_post_image(&inventory);
        let report = fixture_report(
            &inventory,
            &ProjectCatalogMigrationResolutionV1::empty(inventory.inventory_hash().unwrap()),
            &post_image,
        );
        assert_eq!(
            validate_supported_resolution(&inventory, &report, &resolution, &post_image)
                .unwrap_err()
                .code(),
            "error.project_catalog_inventory_stale_resolution"
        );

        let resolution =
            ProjectCatalogMigrationResolutionV1::empty(inventory.inventory_hash().unwrap());
        let mut invented = post_image;
        let PublisherBindingDispositionV1::SeedG1 {
            accepted_commit, ..
        } = &mut invented.publisher_binding_dispositions[0]
        else {
            unreachable!();
        };
        *accepted_commit = "9".repeat(40);
        let mut invented_report = fixture_report(&inventory, &resolution, &invented);
        invented_report.plan_hash =
            canonical_plan_hash(&inventory, &resolution, &invented).unwrap();
        assert_eq!(
            validate_supported_resolution(&inventory, &invented_report, &resolution, &invented,)
                .unwrap_err()
                .code(),
            "error.project_catalog_inventory_invented_publisher_field"
        );
    }

    #[test]
    fn monorepo_attachments_share_one_persisted_planned_checkout_id() {
        let inventory = fixture_inventory();
        let mut post_image = fixture_post_image(&inventory);
        post_image.attachments[1].checkout_id = "f".repeat(32);
        assert_eq!(
            post_image.validate(&inventory).unwrap_err().code(),
            "error.project_catalog_inventory_invalid"
        );
    }

    #[test]
    fn literal_legacy_selector_cannot_be_rewritten_in_post_image() {
        let inventory = fixture_inventory();
        let mut post_image = fixture_post_image(&inventory);
        post_image.legacy_path_bindings[0].literal_selector =
            "/workspace/acme/beta/src/Example.java".to_string();
        assert!(post_image.validate(&inventory).is_err());
    }
}
